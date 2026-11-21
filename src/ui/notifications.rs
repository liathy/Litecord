//! Message notifications: DMs and mentions, with a sound and a desktop notification.
//!
//! The account's Discord mute settings decide what stays silent. Delivery runs on one
//! small thread that sleeps until there is something to show.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

const MAX_BODY_CHARS: usize = 140;
const MIN_SOUND_GAP: Duration = Duration::from_millis(700);
const QUEUED_NOTIFICATIONS: usize = 8;
const NOTHING: u64 = 2;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct GuildMutes {
    muted: bool,
    suppress_everyone: bool,
    channels: HashSet<String>,
    /// Muted by the user, as opposed to only set to "Nothing".
    is_muted: bool,
    muted_channels: HashSet<String>,
    levels: HashMap<String, u64>,
}

static SETTINGS: Mutex<Option<HashMap<String, GuildMutes>>> = Mutex::new(None);
static OPEN_CHANNEL: Mutex<String> = Mutex::new(String::new());
/// Channel to category, so a muted category also mutes what is inside it.
static PARENTS: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);
static SENDER: OnceLock<SyncSender<(String, String)>> = OnceLock::new();

fn mute_is_active(entry: &Value, now: chrono::DateTime<chrono::Utc>) -> bool {
    if entry["muted"].as_bool() != Some(true) {
        return false;
    }
    entry["mute_config"]["end_time"]
        .as_str()
        .and_then(|end| chrono::DateTime::parse_from_rfc3339(end).ok())
        .map_or(true, |end| end > now)
}

fn silences(entry: &Value, now: chrono::DateTime<chrono::Utc>) -> bool {
    mute_is_active(entry, now) || entry["message_notifications"].as_u64() == Some(NOTHING)
}

fn parse_entry(entry: &Value, now: chrono::DateTime<chrono::Utc>) -> (String, GuildMutes) {
    let channels = entry["channel_overrides"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|channel| silences(channel, now))
        .filter_map(|channel| channel["channel_id"].as_str().map(str::to_owned))
        .collect();
    let overrides = || entry["channel_overrides"].as_array().into_iter().flatten();
    let muted_channels = overrides()
        .filter(|channel| mute_is_active(channel, now))
        .filter_map(|channel| channel["channel_id"].as_str().map(str::to_owned))
        .collect();
    let levels = overrides()
        .filter_map(|channel| Some((channel["channel_id"].as_str()?.to_owned(), channel["message_notifications"].as_u64()?)))
        .collect();
    (
        entry["guild_id"].as_str().unwrap_or("").to_owned(),
        GuildMutes {
            muted: silences(entry, now),
            suppress_everyone: entry["suppress_everyone"].as_bool().unwrap_or(false),
            channels,
            is_muted: mute_is_active(entry, now),
            muted_channels,
            levels,
        },
    )
}

/// `user_guild_settings` from READY: a list, or an object holding one in `entries`.
pub fn load_settings(settings: &Value) {
    let now = chrono::Utc::now();
    let list = settings.as_array().or_else(|| settings["entries"].as_array());
    let parsed = list.into_iter().flatten().map(|entry| parse_entry(entry, now)).collect();
    *SETTINGS.lock().unwrap_or_else(|e| e.into_inner()) = Some(parsed);
    crate::ui::presence::schedule_refresh();
}

/// USER_GUILD_SETTINGS_UPDATE: one server's settings changed.
pub fn update_settings(entry: &Value) {
    let (guild, mutes) = parse_entry(entry, chrono::Utc::now());
    SETTINGS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(guild, mutes);
    crate::ui::presence::schedule_refresh();
}

/// DMs are stored under an empty server id.
fn settings_key(guild: &str) -> &str {
    if guild == "@me" { "" } else { guild }
}

pub fn remember_parents<'a>(pairs: impl Iterator<Item = (&'a str, &'a str)>) {
    let mut store = PARENTS.lock().unwrap_or_else(|e| e.into_inner());
    let map = store.get_or_insert_with(HashMap::new);
    for (channel, parent) in pairs {
        map.insert(channel.to_owned(), parent.to_owned());
    }
}

fn parent_of(channel: &str) -> Option<String> {
    PARENTS.lock().unwrap_or_else(|e| e.into_inner()).as_ref()?.get(channel).cloned()
}

fn holds(set: &HashSet<String>, channel: &str, parent: Option<&str>) -> bool {
    set.contains(channel) || parent.is_some_and(|id| set.contains(id))
}

pub fn is_guild_muted(guild: &str) -> bool {
    SETTINGS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|map| map.get(settings_key(guild)))
        .is_some_and(|entry| entry.is_muted)
}

pub fn is_channel_muted(guild: &str, channel: &str) -> bool {
    let parent = parent_of(channel);
    SETTINGS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|map| map.get(settings_key(guild)))
        .is_some_and(|entry| holds(&entry.muted_channels, channel, parent.as_deref()))
}

/// Muted in any server or DM: new messages there don't count as unread badges.
pub fn is_muted_anywhere(channel: &str) -> bool {
    let parent = parent_of(channel);
    SETTINGS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|map| map.values().any(|entry| holds(&entry.muted_channels, channel, parent.as_deref())))
}

pub fn channel_level(guild: &str, channel: &str) -> u64 {
    SETTINGS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|map| map.get(settings_key(guild))?.levels.get(channel).copied())
        .unwrap_or(3)
}

/// Shows a change at once; Discord's settings update confirms it afterwards.
pub fn set_local_mute(guild: &str, channel: &str, muted: bool) {
    let mut store = SETTINGS.lock().unwrap_or_else(|e| e.into_inner());
    let entry = store.get_or_insert_with(HashMap::new).entry(settings_key(guild).to_owned()).or_default();
    if muted {
        entry.muted_channels.insert(channel.to_owned());
        entry.channels.insert(channel.to_owned());
    } else {
        entry.muted_channels.remove(channel);
        if entry.levels.get(channel) != Some(&NOTHING) {
            entry.channels.remove(channel);
        }
    }
}

pub fn set_local_level(guild: &str, channel: &str, level: u64) {
    let mut store = SETTINGS.lock().unwrap_or_else(|e| e.into_inner());
    let entry = store.get_or_insert_with(HashMap::new).entry(settings_key(guild).to_owned()).or_default();
    entry.levels.insert(channel.to_owned(), level);
    if level == NOTHING {
        entry.channels.insert(channel.to_owned());
    } else if !entry.muted_channels.contains(channel) {
        entry.channels.remove(channel);
    }
}

pub fn set_open_channel(channel_id: &str) {
    *OPEN_CHANNEL.lock().unwrap_or_else(|e| e.into_inner()) = channel_id.to_owned();
}

fn should_notify(message: &Value, my_id: &str, mutes: &HashMap<String, GuildMutes>, looking_at_it: bool) -> bool {
    if looking_at_it || my_id.is_empty() || message["author"]["id"].as_str() == Some(my_id) {
        return false;
    }
    let channel = message["channel_id"].as_str().unwrap_or("");
    let guild = message["guild_id"].as_str().unwrap_or("");
    let settings = mutes.get(guild);
    if settings.is_some_and(|s| s.channels.contains(channel)) {
        return false;
    }
    if guild.is_empty() {
        return true;
    }
    if settings.is_some_and(|s| s.muted) {
        return false;
    }
    let mentioned = message["mentions"]
        .as_array()
        .is_some_and(|users| users.iter().any(|user| user["id"].as_str() == Some(my_id)));
    let everyone = message["mention_everyone"].as_bool() == Some(true)
        && !settings.is_some_and(|s| s.suppress_everyone);
    mentioned || everyone
}

#[cfg(windows)]
fn window_is_in_front() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
    let mut process = 0u32;
    unsafe {
        let window = GetForegroundWindow();
        if window.is_null() {
            return false;
        }
        GetWindowThreadProcessId(window, &mut process);
    }
    process == std::process::id()
}

#[cfg(not(windows))]
fn window_is_in_front() -> bool {
    crate::ui::helpers::APP_IS_VISIBLE.load(Ordering::Relaxed)
}

fn short_body(text: &str) -> String {
    let clean = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if clean.chars().count() <= MAX_BODY_CHARS {
        return clean;
    }
    let mut cut: String = clean.chars().take(MAX_BODY_CHARS).collect();
    cut.push('…');
    cut
}

/// A new message from the gateway.
pub fn on_message(message: &Value, my_id: &str, author: &str, text: &str) {
    let open = *OPEN_CHANNEL.lock().unwrap_or_else(|e| e.into_inner()) == message["channel_id"].as_str().unwrap_or("");
    let visible = crate::ui::helpers::APP_IS_VISIBLE.load(Ordering::Relaxed);
    let looking_at_it = open && visible && window_is_in_front();
    if is_muted_anywhere(message["channel_id"].as_str().unwrap_or("")) {
        return;
    }
    let notify = {
        let settings = SETTINGS.lock().unwrap_or_else(|e| e.into_inner());
        let empty = HashMap::new();
        should_notify(message, my_id, settings.as_ref().unwrap_or(&empty), looking_at_it)
    };
    if !notify {
        return;
    }
    let body = if text.trim().is_empty() { "📎".to_owned() } else { short_body(text) };
    let sender = SENDER.get_or_init(start_worker);
    let _ = sender.try_send((author.to_owned(), body));
}

fn start_worker() -> SyncSender<(String, String)> {
    let (tx, rx) = sync_channel::<(String, String)>(QUEUED_NOTIFICATIONS);
    let _ = std::thread::Builder::new().name("notifications".into()).stack_size(256 * 1024).spawn(move || {
        prepare_system();
        let mut last_sound: Option<Instant> = None;
        for (title, body) in rx {
            if last_sound.map_or(true, |at| at.elapsed() >= MIN_SOUND_GAP) {
                last_sound = Some(Instant::now());
                crate::audio::sound_effects::play_ui_sound(crate::audio::sound_effects::UiSound::Message);
            }
            if let Err(e) = show(&title, &body) {
                log::warn!("Notificação não exibida: {}", e);
            }
        }
    });
    tx
}

#[cfg(windows)]
const APP_ID: &str = "Litecord.Desktop";

/// Windows shows a notification only for an app id it knows; this names it for the user.
#[cfg(windows)]
fn prepare_system() {
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
    };
    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let path = wide(&format!("Software\\Classes\\AppUserModelId\\{APP_ID}"));
    let (name, value) = (wide("DisplayName"), wide("Litecord"));
    unsafe {
        let mut key = std::ptr::null_mut();
        let created = RegCreateKeyExW(
            HKEY_CURRENT_USER, path.as_ptr(), 0, std::ptr::null(), REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE, std::ptr::null(), &mut key, std::ptr::null_mut(),
        );
        if created == 0 {
            RegSetValueExW(key, name.as_ptr(), 0, REG_SZ, value.as_ptr().cast(), (value.len() * 2) as u32);
            RegCloseKey(key);
        }
        let _ = windows::Win32::System::WinRT::RoInitialize(windows::Win32::System::WinRT::RO_INIT_MULTITHREADED);
    }
}

#[cfg(windows)]
fn show(title: &str, body: &str) -> windows::core::Result<()> {
    use windows::core::{Interface, HSTRING};
    use windows::Data::Xml::Dom::IXmlNode;
    use windows::UI::Notifications::{ToastNotification, ToastNotificationManager, ToastTemplateType};

    // Text nodes, never XML built from strings: a message cannot change the notification.
    let xml = ToastNotificationManager::GetTemplateContent(ToastTemplateType::ToastText02)?;
    let fields = xml.GetElementsByTagName(&HSTRING::from("text"))?;
    for (index, text) in [title, body].into_iter().enumerate() {
        let node = xml.CreateTextNode(&HSTRING::from(text))?.cast::<IXmlNode>()?;
        fields.Item(index as u32)?.AppendChild(&node)?;
    }
    let toast = ToastNotification::CreateToastNotification(&xml)?;
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_ID))?.Show(&toast)
}

#[cfg(not(windows))]
fn prepare_system() {}

#[cfg(not(windows))]
fn show(title: &str, body: &str) -> std::io::Result<()> {
    std::process::Command::new("notify-send")
        .args(["--app-name=Litecord", "--", title, body])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn message(guild: Option<&str>, mentions: &[&str]) -> Value {
        let users: Vec<Value> = mentions.iter().map(|id| json!({ "id": id })).collect();
        json!({ "author": { "id": "2" }, "channel_id": "50", "guild_id": guild, "mentions": users, "mention_everyone": false })
    }

    fn settings(entry: Value) -> HashMap<String, GuildMutes> {
        HashMap::from([parse_entry(&entry, chrono::Utc::now())])
    }

    #[test]
    fn dms_and_mentions_notify_and_ordinary_server_messages_do_not() {
        let none = HashMap::new();
        assert!(should_notify(&message(None, &[]), "1", &none, false));
        assert!(should_notify(&message(Some("9"), &["1"]), "1", &none, false));
        assert!(!should_notify(&message(Some("9"), &["3"]), "1", &none, false));
    }

    #[test]
    fn nothing_for_your_own_messages_or_the_chat_you_are_reading() {
        let none = HashMap::new();
        assert!(!should_notify(&message(None, &[]), "2", &none, false));
        assert!(!should_notify(&message(None, &[]), "1", &none, true));
    }

    #[test]
    fn muted_servers_and_channels_stay_silent() {
        let server = settings(json!({ "guild_id": "9", "muted": true }));
        assert!(!should_notify(&message(Some("9"), &["1"]), "1", &server, false));
        let channel = settings(json!({ "guild_id": "9", "channel_overrides": [{ "channel_id": "50", "muted": true }] }));
        assert!(!should_notify(&message(Some("9"), &["1"]), "1", &channel, false));
        let dm = settings(json!({ "guild_id": null, "channel_overrides": [{ "channel_id": "50", "muted": true }] }));
        assert!(!should_notify(&message(None, &[]), "1", &dm, false));
    }

    #[test]
    fn an_expired_mute_no_longer_applies() {
        let expired = settings(json!({ "guild_id": "9", "muted": true, "mute_config": { "end_time": "2020-01-01T00:00:00+00:00" } }));
        assert!(should_notify(&message(Some("9"), &["1"]), "1", &expired, false));
    }

    #[test]
    fn reads_who_is_muted_and_notification_levels() {
        let now = chrono::Utc::now();
        let (_, entry) = parse_entry(&json!({ "guild_id": "9", "muted": true, "channel_overrides": [
            { "channel_id": "1", "muted": true },
            { "channel_id": "2", "muted": false, "message_notifications": 2 },
            { "channel_id": "3", "muted": true, "mute_config": { "end_time": "2020-01-01T00:00:00+00:00" } },
        ] }), now);
        assert!(entry.is_muted);
        assert!(entry.muted_channels.contains("1") && !entry.muted_channels.contains("2") && !entry.muted_channels.contains("3"));
        assert_eq!(entry.levels.get("2"), Some(&2));
    }

    #[test]
    fn a_muted_category_covers_its_channels() {
        let set = HashSet::from(["7".to_owned()]);
        assert!(holds(&set, "1", Some("7")));
        assert!(!holds(&set, "1", Some("8")));
    }

    #[test]
    fn everyone_follows_the_suppress_setting() {
        let mut ping = message(Some("9"), &[]);
        ping["mention_everyone"] = json!(true);
        assert!(should_notify(&ping, "1", &HashMap::new(), false));
        let suppressed = settings(json!({ "guild_id": "9", "suppress_everyone": true }));
        assert!(!should_notify(&ping, "1", &suppressed, false));
    }

    #[test]
    fn long_messages_are_shortened_on_one_line() {
        assert_eq!(short_body("oi\n  tudo   bem"), "oi tudo bem");
        assert_eq!(short_body(&"a".repeat(500)).chars().count(), MAX_BODY_CHARS + 1);
    }
}
