//! Activity status: Discord RPC for games and music apps, and detection of running games.
//!
//! Off by default. While off, no pipe is open, no process is read and nothing is sent.
//! While on, it only looks at processes every 20 seconds.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::Method;
use serde_json::{json, Map, Value};
use slint::ComponentHandle;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::gateway::GatewayCommand;
use crate::http::DiscordHttpClient;
use crate::utils::video_settings;
use crate::AppWindow;

const SCAN_EVERY: Duration = Duration::from_secs(20);
const MAX_FRAME: usize = 64 * 1024;
const OP_HANDSHAKE: u32 = 0;
const OP_FRAME: u32 = 1;
const OP_CLOSE: u32 = 2;
const OP_PING: u32 = 3;
const OP_PONG: u32 = 4;
/// Names too common to identify a game without their folder.
const GENERIC_EXES: [&str; 8] = ["game.exe", "launcher.exe", "start.exe", "main.exe", "app.exe", "client.exe", "play.exe", "setup.exe"];

type Client = Arc<Mutex<Option<DiscordHttpClient>>>;
type Commands = Arc<Mutex<Option<mpsc::Sender<GatewayCommand>>>>;

#[derive(Default)]
struct State {
    /// Activities set through RPC, newest last.
    rpc: Vec<(u64, Value)>,
    game: Option<(u64, Value)>,
    published: Option<Value>,
    commands: Option<Commands>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
static RUNNING: AtomicBool = AtomicBool::new(false);
static GENERATION: AtomicU64 = AtomicU64::new(0);
static NEXT_CONNECTION: AtomicU64 = AtomicU64::new(1);

fn with_state<R>(job: impl FnOnce(&mut State) -> R) -> R {
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    job(guard.get_or_insert_with(State::default))
}

fn current(generation: u64) -> bool {
    GENERATION.load(Ordering::SeqCst) == generation
}

fn publish() {
    with_state(|state| {
        let activity = state.rpc.last().map(|(_, a)| a.clone()).or_else(|| state.game.as_ref().map(|(_, a)| a.clone()));
        let wanted = json!(activity.into_iter().collect::<Vec<_>>());
        if state.published.as_ref() == Some(&wanted) {
            return;
        }
        let Some(tx) = state.commands.as_ref().and_then(|c| c.lock().ok()?.as_ref().cloned()) else { return };
        if tx.try_send(GatewayCommand::UpdatePresence { activities: wanted.clone() }).is_ok() {
            state.published = Some(wanted);
        }
    });
}

/// A new gateway session starts with no activity: it is sent again.
pub fn session_started() {
    if RUNNING.load(Ordering::SeqCst) {
        with_state(|state| state.published = None);
        publish();
    }
}

fn milliseconds(value: &Value) -> Option<u64> {
    let number = value.as_u64()?;
    Some(if number < 100_000_000_000 { number * 1000 } else { number })
}

/// What an app sent, in the form the gateway takes. Secrets and unknown fields stay out.
fn to_gateway_activity(activity: &Value, application_id: &str, name: &str) -> Value {
    let mut out = Map::new();
    let kind = activity["type"].as_u64().filter(|t| matches!(t, 0 | 2 | 3 | 5)).unwrap_or(0);
    out.insert("type".into(), json!(kind));
    out.insert("name".into(), json!(name));
    out.insert("application_id".into(), json!(application_id));
    for key in ["details", "state", "assets", "party", "instance"] {
        if !activity[key].is_null() {
            out.insert(key.into(), activity[key].clone());
        }
    }
    let mut times = Map::new();
    for key in ["start", "end"] {
        if let Some(at) = milliseconds(&activity["timestamps"][key]) {
            times.insert(key.into(), json!(at));
        }
    }
    if !times.is_empty() {
        out.insert("timestamps".into(), Value::Object(times));
    }
    if let Some(buttons) = activity["buttons"].as_array().filter(|b| !b.is_empty()) {
        let labels: Vec<&str> = buttons.iter().filter_map(|b| b["label"].as_str()).take(2).collect();
        let urls: Vec<&str> = buttons.iter().filter_map(|b| b["url"].as_str()).take(2).collect();
        if labels.len() == urls.len() {
            out.insert("buttons".into(), json!(labels));
            out.insert("metadata".into(), json!({ "button_urls": urls }));
        }
    }
    Value::Object(out)
}

async fn application_name(http: &Client, id: &str) -> String {
    let client = http.lock().unwrap().as_ref().cloned();
    let Some(client) = client else { return "Game".to_owned() };
    client
        .discord_call(Method::GET, &format!("/oauth2/applications/{id}/rpc"), None)
        .await
        .ok()
        .and_then(|body| body["name"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "Game".to_owned())
}

async fn read_frame<S: AsyncRead + Unpin>(stream: &mut S) -> Option<(u32, Value)> {
    let mut header = [0u8; 8];
    stream.read_exact(&mut header).await.ok()?;
    let op = u32::from_le_bytes(header[..4].try_into().ok()?);
    let length = u32::from_le_bytes(header[4..].try_into().ok()?) as usize;
    if length > MAX_FRAME {
        return None;
    }
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body).await.ok()?;
    Some((op, serde_json::from_slice(&body).ok()?))
}

async fn write_frame<S: AsyncWrite + Unpin>(stream: &mut S, op: u32, body: &Value) -> Option<()> {
    let text = body.to_string();
    let mut out = Vec::with_capacity(text.len() + 8);
    out.extend_from_slice(&op.to_le_bytes());
    out.extend_from_slice(&(text.len() as u32).to_le_bytes());
    out.extend_from_slice(text.as_bytes());
    stream.write_all(&out).await.ok()?;
    stream.flush().await.ok()
}

fn ready_frame() -> Value {
    json!({
        "cmd": "DISPATCH",
        "evt": "READY",
        "data": {
            "v": 1,
            "config": { "cdn_host": "cdn.discordapp.com", "api_endpoint": "//discord.com/api", "environment": "production" },
            "user": { "id": crate::gateway::get_my_user_id().to_string(), "username": crate::gateway::get_my_username(), "discriminator": "0", "bot": false },
        },
    })
}

async fn handle_connection<S: AsyncRead + AsyncWrite + Unpin>(mut stream: S, http: Client, generation: u64) {
    let connection = NEXT_CONNECTION.fetch_add(1, Ordering::SeqCst);
    let Some((OP_HANDSHAKE, hello)) = read_frame(&mut stream).await else { return };
    let application_id = hello["client_id"].as_str().unwrap_or("").to_owned();
    if application_id.is_empty() || !application_id.bytes().all(|b| b.is_ascii_digit()) {
        let _ = write_frame(&mut stream, OP_CLOSE, &json!({ "code": 4000, "message": "Invalid client id" })).await;
        return;
    }
    if write_frame(&mut stream, OP_FRAME, &ready_frame()).await.is_none() {
        return;
    }
    let name = application_name(&http, &application_id).await;
    while current(generation) {
        let Ok(frame) = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut stream)).await else { continue };
        let Some((op, message)) = frame else { break };
        match op {
            OP_PING => {
                if write_frame(&mut stream, OP_PONG, &message).await.is_none() {
                    break;
                }
            }
            OP_CLOSE => break,
            OP_FRAME => {
                let nonce = message["nonce"].clone();
                let command = message["cmd"].as_str().unwrap_or("");
                let reply = if command == "SET_ACTIVITY" {
                    let activity = &message["args"]["activity"];
                    with_state(|state| {
                        state.rpc.retain(|(id, _)| *id != connection);
                        if activity.is_object() {
                            state.rpc.push((connection, to_gateway_activity(activity, &application_id, &name)));
                        }
                    });
                    publish();
                    json!({ "cmd": "SET_ACTIVITY", "nonce": nonce, "evt": null, "data": activity })
                } else if command == "SUBSCRIBE" || command == "UNSUBSCRIBE" {
                    json!({ "cmd": command, "nonce": nonce, "evt": message["evt"], "data": { "evt": message["evt"] } })
                } else {
                    json!({ "cmd": command, "nonce": nonce, "evt": "ERROR", "data": { "code": 1000, "message": "Not supported" } })
                };
                if write_frame(&mut stream, OP_FRAME, &reply).await.is_none() {
                    break;
                }
            }
            _ => break,
        }
    }
    with_state(|state| state.rpc.retain(|(id, _)| *id != connection));
    publish();
}

#[cfg(windows)]
async fn serve(http: Client, generation: u64) {
    use tokio::net::windows::named_pipe::ServerOptions;
    let mut opened = None;
    for index in 0..10 {
        let name = format!(r"\\.\pipe\discord-ipc-{index}");
        if let Ok(server) = ServerOptions::new().first_pipe_instance(true).create(&name) {
            opened = Some((name, server));
            break;
        }
    }
    let Some((name, mut server)) = opened else {
        log::warn!("RPC: nenhum pipe discord-ipc livre");
        return;
    };
    log::info!("RPC ouvindo em {name}");
    while current(generation) {
        match tokio::time::timeout(Duration::from_secs(2), server.connect()).await {
            Err(_) => continue,
            Ok(Err(_)) => tokio::time::sleep(Duration::from_millis(500)).await,
            Ok(Ok(())) => {
                let Ok(next) = ServerOptions::new().create(&name) else { break };
                let connected = std::mem::replace(&mut server, next);
                tokio::spawn(handle_connection(connected, Arc::clone(&http), generation));
            }
        }
    }
}

#[cfg(unix)]
async fn serve(http: Client, generation: u64) {
    use tokio::net::{UnixListener, UnixStream};
    let folder = std::env::var("XDG_RUNTIME_DIR").or_else(|_| std::env::var("TMPDIR")).unwrap_or_else(|_| "/tmp".to_owned());
    let mut opened = None;
    for index in 0..10 {
        let path = std::path::Path::new(&folder).join(format!("discord-ipc-{index}"));
        if path.exists() {
            if UnixStream::connect(&path).await.is_ok() {
                continue;
            }
            let _ = std::fs::remove_file(&path);
        }
        if let Ok(listener) = UnixListener::bind(&path) {
            opened = Some((path, listener));
            break;
        }
    }
    let Some((path, listener)) = opened else { return };
    while current(generation) {
        if let Ok(Ok((stream, _))) = tokio::time::timeout(Duration::from_secs(2), listener.accept()).await {
            tokio::spawn(handle_connection(stream, Arc::clone(&http), generation));
        }
    }
    let _ = std::fs::remove_file(path);
}

fn name_hash(name: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    name.hash(&mut hasher);
    hasher.finish()
}

fn normalize(path: &str) -> String {
    path.replace('\\', "/").to_lowercase()
}

/// The last one, two and three parts of a path.
fn path_suffixes(path: &str) -> Vec<&str> {
    let mut out = Vec::with_capacity(3);
    let mut end = path.len();
    for _ in 0..3 {
        let start = path[..end].rfind('/');
        out.push(&path[start.map_or(0, |s| s + 1)..]);
        match start {
            Some(s) => end = s,
            None => break,
        }
    }
    out
}

const PLATFORM: &str = if cfg!(windows) { "win32" } else { "linux" };

/// Compact lookup of Discord's list of known games: hashed executable to application.
fn detectable_map(body: &Value) -> HashMap<u64, u64> {
    let mut map = HashMap::new();
    for app in body.as_array().into_iter().flatten() {
        let Some(id) = app["id"].as_str().and_then(|id| id.parse::<u64>().ok()) else { continue };
        for exe in app["executables"].as_array().into_iter().flatten() {
            if exe["os"].as_str() != Some(PLATFORM) || exe["is_launcher"].as_bool() == Some(true) {
                continue;
            }
            let Some(name) = exe["name"].as_str() else { continue };
            let name = normalize(name);
            let name = name.trim_start_matches('>');
            if name.is_empty() || GENERIC_EXES.contains(&name) {
                continue;
            }
            map.insert(name_hash(name), id);
        }
    }
    map
}

fn find_game(paths: &[String], games: &HashMap<u64, u64>) -> Option<u64> {
    paths.iter().find_map(|path| path_suffixes(path).iter().find_map(|suffix| games.get(&name_hash(suffix)).copied()))
}

#[cfg(windows)]
fn running_paths() -> Vec<String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::ProcessStatus::EnumProcesses;
    use windows_sys::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};

    let mut ids = vec![0u32; 4096];
    let mut used = 0u32;
    if unsafe { EnumProcesses(ids.as_mut_ptr(), (ids.len() * 4) as u32, &mut used) } == 0 {
        return Vec::new();
    }
    ids.truncate(used as usize / 4);
    let mut buffer = [0u16; 1024];
    ids.into_iter()
        .filter(|id| *id != 0 && *id != std::process::id())
        .filter_map(|id| unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, id);
            if process.is_null() {
                return None;
            }
            let mut length = buffer.len() as u32;
            let ok = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, buffer.as_mut_ptr(), &mut length);
            CloseHandle(process);
            (ok != 0).then(|| normalize(&String::from_utf16_lossy(&buffer[..length as usize])))
        })
        .collect()
}

#[cfg(unix)]
fn running_paths() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir("/proc") else { return Vec::new() };
    entries
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()))
        .filter_map(|entry| std::fs::read_link(entry.path().join("exe")).ok())
        .map(|path| normalize(&path.to_string_lossy()))
        .collect()
}

async fn detect_games(http: Client, generation: u64) {
    let mut games: Option<HashMap<u64, u64>> = None;
    while current(generation) {
        if games.is_none() {
            let client = http.lock().unwrap().as_ref().cloned();
            if let Some(client) = client {
                if let Ok(body) = client.discord_call(Method::GET, "/applications/detectable", None).await {
                    games = tokio::task::spawn_blocking(move || detectable_map(&body)).await.ok();
                }
            }
        }
        if let Some(known) = games.take() {
            let (known, found) = tokio::task::spawn_blocking(move || {
                let found = find_game(&running_paths(), &known);
                (known, found)
            })
            .await
            .unwrap_or_default();
            games = Some(known);
            let same = with_state(|state| state.game.as_ref().map(|(id, _)| *id) == found);
            if !same {
                let activity = match found {
                    Some(id) => {
                        let name = application_name(&http, &id.to_string()).await;
                        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
                        Some((id, json!({ "type": 0, "name": name, "application_id": id.to_string(), "timestamps": { "start": now } })))
                    }
                    None => None,
                };
                if current(generation) {
                    with_state(|state| state.game = activity);
                    publish();
                }
            }
        }
        tokio::time::sleep(SCAN_EVERY).await;
    }
}

fn start(http: Client) {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    tokio::spawn(serve(Arc::clone(&http), generation));
    tokio::spawn(detect_games(http, generation));
}

fn stop() {
    if !RUNNING.swap(false, Ordering::SeqCst) {
        return;
    }
    GENERATION.fetch_add(1, Ordering::SeqCst);
    with_state(|state| {
        state.rpc.clear();
        state.game = None;
    });
    publish();
}

pub fn install(ui: &AppWindow, http: Client, commands: Commands) {
    with_state(|state| state.commands = Some(commands));
    let enabled = video_settings::get_rich_presence();
    ui.set_rich_presence_enabled(enabled);
    if enabled {
        start(Arc::clone(&http));
    }
    let app = ui.as_weak();
    ui.on_toggle_rich_presence(move || {
        let Some(ui) = app.upgrade() else { return };
        let enabled = !ui.get_rich_presence_enabled();
        ui.set_rich_presence_enabled(enabled);
        video_settings::set_rich_presence(enabled);
        if enabled {
            start(Arc::clone(&http));
        } else {
            stop();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_what_the_gateway_takes_and_drops_secrets() {
        let activity = to_gateway_activity(&json!({
            "details": "Ranked", "state": "In match", "secrets": { "join": "x" },
            "timestamps": { "start": 1_700_000_000 },
            "buttons": [{ "label": "Site", "url": "https://example.com" }],
        }), "123", "Chess");
        assert_eq!((activity["name"].as_str(), activity["application_id"].as_str()), (Some("Chess"), Some("123")));
        assert!(activity["secrets"].is_null());
        assert_eq!(activity["timestamps"]["start"], 1_700_000_000_000u64);
        assert_eq!(activity["buttons"][0], "Site");
        assert_eq!(activity["metadata"]["button_urls"][0], "https://example.com");
    }

    #[test]
    fn unsupported_activity_types_become_playing() {
        assert_eq!(to_gateway_activity(&json!({ "type": 4 }), "1", "x")["type"], 0);
        assert_eq!(to_gateway_activity(&json!({ "type": 2 }), "1", "x")["type"], 2);
    }

    #[test]
    fn matches_games_by_the_end_of_their_path() {
        let body = json!([
            { "id": "10", "executables": [{ "os": PLATFORM, "name": "Hades/Hades.exe" }] },
            { "id": "20", "executables": [{ "os": PLATFORM, "name": "game.exe" }] },
            { "id": "30", "executables": [{ "os": PLATFORM, "name": "store.exe", "is_launcher": true }] },
        ]);
        let games = detectable_map(&body);
        assert_eq!(find_game(&[normalize(r"C:\Steam\common\Hades\Hades.exe")], &games), Some(10));
        assert_eq!(find_game(&[normalize(r"C:\x\game.exe")], &games), None);
        assert_eq!(find_game(&[normalize(r"C:\x\store.exe")], &games), None);
    }

    #[test]
    fn frames_round_trip() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let (mut a, mut b) = tokio::io::duplex(1024);
            write_frame(&mut a, OP_FRAME, &json!({ "cmd": "SET_ACTIVITY" })).await.unwrap();
            let (op, body) = read_frame(&mut b).await.unwrap();
            assert_eq!((op, body["cmd"].as_str()), (OP_FRAME, Some("SET_ACTIVITY")));
        });
    }

    #[test]
    fn oversized_frames_are_refused() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let (mut a, mut b) = tokio::io::duplex(64);
            a.write_all(&1u32.to_le_bytes()).await.unwrap();
            a.write_all(&(MAX_FRAME as u32 + 1).to_le_bytes()).await.unwrap();
            assert!(read_frame(&mut b).await.is_none());
        });
    }
}
