//! The Friends page: online, all, pending, blocked and Add Friend.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use reqwest::Method;
use serde_json::{json, Value};
use slint::{ComponentHandle, ModelRc, VecModel};

use crate::http::DiscordHttpClient;
use crate::ui::member_list::{public_avatar_url, status_color};
use crate::ui::presence;
use crate::utils::avatar_cache;
use crate::{AppWindow, FriendRow};

const FRIEND: u64 = 1;
const BLOCKED: u64 = 2;
const INCOMING: u64 = 3;
const OUTGOING: u64 = 4;

static REQUEST: AtomicU64 = AtomicU64::new(0);
static ENTRIES: Mutex<Vec<Relationship>> = Mutex::new(Vec::new());

#[derive(Clone, Debug, PartialEq, Eq)]
struct Relationship {
    id: String,
    kind: u64,
    name: String,
    username: String,
    avatar_url: String,
}

fn parse(body: &Value) -> Vec<Relationship> {
    let mut list: Vec<Relationship> = body
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let user = &entry["user"];
            let id = entry["id"].as_str().or_else(|| user["id"].as_str())?.to_owned();
            let username = user["username"].as_str().unwrap_or("").to_owned();
            let name = entry["nickname"]
                .as_str()
                .or_else(|| user["global_name"].as_str())
                .filter(|n| !n.is_empty())
                .unwrap_or(&username)
                .to_owned();
            Some(Relationship {
                avatar_url: public_avatar_url(&id, user["avatar"].as_str().unwrap_or("")),
                id,
                kind: entry["type"].as_u64()?,
                name,
                username,
            })
        })
        .collect();
    list.sort_by_key(|entry| entry.name.to_lowercase());
    list
}

fn is_online(id: &str) -> bool {
    presence::get(id).is_some_and(|p| matches!(p.status.as_str(), "online" | "idle" | "dnd"))
}

fn belongs(entry: &Relationship, tab: &str, online: bool) -> bool {
    match tab {
        "online" => entry.kind == FRIEND && online,
        "all" => entry.kind == FRIEND,
        "pending" => entry.kind == INCOMING || entry.kind == OUTGOING,
        "blocked" => entry.kind == BLOCKED,
        _ => false,
    }
}

fn detail(entry: &Relationship) -> String {
    match entry.kind {
        INCOMING => return "Incoming Friend Request".to_owned(),
        OUTGOING => return "Outgoing Friend Request".to_owned(),
        BLOCKED => return "Blocked".to_owned(),
        _ => {}
    }
    match presence::get(&entry.id) {
        Some(p) if !p.activity.is_empty() => p.activity,
        Some(p) => presence::status_label(&p.status).to_owned(),
        None => "Offline".to_owned(),
    }
}

fn render(ui: &AppWindow) {
    let tab = ui.get_friends_tab().to_string();
    let needle = ui.get_friends_query().to_lowercase();
    let entries = ENTRIES.lock().unwrap_or_else(|e| e.into_inner()).clone();
    ui.set_friends_pending_count(entries.iter().filter(|e| e.kind == INCOMING).count() as i32);
    let rows: Vec<FriendRow> = entries
        .iter()
        .filter(|entry| belongs(entry, &tab, is_online(&entry.id)))
        .filter(|entry| needle.is_empty() || entry.name.to_lowercase().contains(&needle) || entry.username.to_lowercase().contains(&needle))
        .map(|entry| {
            let name = crate::utils::styled_text::plain_display_name(&entry.name).into_owned();
            FriendRow {
                id: entry.id.as_str().into(),
                avatar_text: name.chars().next().unwrap_or('?').to_uppercase().to_string().into(),
                name: name.into(),
                username: entry.username.as_str().into(),
                detail: detail(entry).into(),
                avatar_slot: avatar_cache::get_avatar_cache().slot_for(&entry.avatar_url),
                avatar_url: entry.avatar_url.as_str().into(),
                status_color: status_color(&presence::get(&entry.id).map_or("offline".to_owned(), |p| p.status)),
                kind: entry.kind as i32,
            }
        })
        .collect();
    ui.set_friends_empty(rows.is_empty());
    ui.set_friends_rows(ModelRc::new(VecModel::from(rows)));
    if ui.get_invite_open() {
        crate::ui::channel_tools::refresh_invite_friends(ui);
    }
}

/// Friends matching a search, for the invite dialog.
pub(crate) fn invite_rows(query: &str) -> Vec<FriendRow> {
    let needle = query.trim().to_lowercase();
    ENTRIES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|entry| entry.kind == FRIEND)
        .filter(|entry| needle.is_empty() || entry.name.to_lowercase().contains(&needle) || entry.username.to_lowercase().contains(&needle))
        .map(|entry| {
            let name = crate::utils::styled_text::plain_display_name(&entry.name).into_owned();
            FriendRow {
                id: entry.id.as_str().into(),
                avatar_text: name.chars().next().unwrap_or('?').to_uppercase().to_string().into(),
                name: name.into(),
                username: entry.username.as_str().into(),
                avatar_slot: avatar_cache::get_avatar_cache().slot_for(&entry.avatar_url),
                avatar_url: entry.avatar_url.as_str().into(),
                kind: FRIEND as i32,
                ..Default::default()
            }
        })
        .collect()
}

pub(crate) fn reload(ui: &AppWindow, http: DiscordHttpClient) {
    let request = REQUEST.fetch_add(1, Ordering::SeqCst) + 1;
    let app = ui.as_weak();
    tokio::spawn(async move {
        let result = http.discord_call(Method::GET, "/users/@me/relationships", None).await;
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = app.upgrade() else { return };
            if REQUEST.load(Ordering::SeqCst) != request {
                return;
            }
            match result {
                Ok(body) => {
                    *ENTRIES.lock().unwrap_or_else(|e| e.into_inner()) = parse(&body);
                    ui.set_friends_status("".into());
                    render(&ui);
                }
                Err(error) => ui.set_friends_status(format!("Couldn't load friends ({error})").into()),
            }
        });
    });
}

fn change(ui: &AppWindow, http: DiscordHttpClient, method: Method, id: String, body: Option<Value>) {
    let app = ui.as_weak();
    tokio::spawn(async move {
        let result = http.discord_call(method, &format!("/users/@me/relationships/{id}"), body).await;
        let client = http.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = app.upgrade() else { return };
            match result {
                Ok(_) => reload(&ui, client),
                Err(error) => ui.set_friends_status(format!("Discord refused that ({error})").into()),
            }
        });
    });
}

/// Discord's unique usernames have no `#` part.
fn clean_username(text: &str) -> Option<String> {
    let name = text.trim().trim_start_matches('@').to_lowercase();
    let valid = (2..=32).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.');
    valid.then_some(name)
}

fn add_friend(ui: &AppWindow, http: DiscordHttpClient) {
    let Some(username) = clean_username(&ui.get_friends_add_name()) else {
        ui.set_friends_status("Enter a valid username.".into());
        return;
    };
    ui.set_friends_status("Sending...".into());
    let app = ui.as_weak();
    tokio::spawn(async move {
        let body = json!({ "username": username.clone(), "discriminator": null });
        let result = http.discord_call(Method::POST, "/users/@me/relationships", Some(body)).await;
        let client = http.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = app.upgrade() else { return };
            match result {
                Ok(_) => {
                    ui.set_friends_add_name("".into());
                    ui.set_friends_status(format!("Friend request sent to {username}.").into());
                    reload(&ui, client);
                }
                Err(error) => ui.set_friends_status(format!("Couldn't send the request ({error})").into()),
            }
        });
    });
}

fn message(ui: &AppWindow, http: DiscordHttpClient, active: Arc<Mutex<String>>, id: String) {
    let app = ui.as_weak();
    tokio::spawn(async move {
        let result = http.open_dm(&id).await;
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = app.upgrade() else { return };
            match result {
                Ok(channel) => {
                    crate::ui::dm_close::take_closed(&channel);
                    *active.lock().unwrap() = channel;
                    ui.set_friends_open(false);
                    ui.set_active_guild_id("@me".into());
                    ui.invoke_select_guild("@me".into());
                }
                Err(error) => ui.set_friends_status(format!("Couldn't open that conversation ({error})").into()),
            }
        });
    });
}

pub fn install(ui: &AppWindow, http: Arc<Mutex<Option<DiscordHttpClient>>>, active: Arc<Mutex<String>>) {
    let app = ui.as_weak();
    ui.on_friends_action(move |name, id| {
        let Some(ui) = app.upgrade() else { return };
        let client = http.lock().unwrap().as_ref().cloned();
        let id = id.to_string();
        match name.as_str() {
            "open" => {
                ui.set_friends_open(true);
                ui.set_friends_status("".into());
                render(&ui);
                if let Some(client) = client {
                    reload(&ui, client);
                }
            }
            "tab" => {
                ui.set_friends_tab(id.as_str().into());
                ui.set_friends_status("".into());
                render(&ui);
            }
            "filter" => render(&ui),
            "ask_remove" => {
                ui.set_friends_confirm_id(id.as_str().into());
                ui.set_friends_confirm_text("Remove this friend?".into());
            }
            "ask_unblock" => {
                ui.set_friends_confirm_id(id.as_str().into());
                ui.set_friends_confirm_text("Unblock this user?".into());
            }
            _ => {
                let Some(client) = client else { return };
                match name.as_str() {
                    "message" => message(&ui, client, Arc::clone(&active), id),
                    "accept" => change(&ui, client, Method::PUT, id, Some(json!({}))),
                    "decline" => change(&ui, client, Method::DELETE, id, None),
                    "add" => add_friend(&ui, client),
                    "confirm" => {
                        let target = ui.get_friends_confirm_id().to_string();
                        ui.set_friends_confirm_text("".into());
                        change(&ui, client, Method::DELETE, target, None);
                    }
                    _ => {}
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: u64) -> Relationship {
        Relationship { id: "1".into(), kind, name: "a".into(), username: "a".into(), avatar_url: String::new() }
    }

    #[test]
    fn sorts_by_name_and_prefers_nicknames() {
        let body = json!([
            { "id": "2", "type": 1, "user": { "username": "zed" } },
            { "id": "1", "type": 1, "nickname": "Amy", "user": { "username": "yy", "global_name": "Why" } },
        ]);
        let list = parse(&body);
        assert_eq!((list[0].name.as_str(), list[1].name.as_str()), ("Amy", "zed"));
    }

    #[test]
    fn each_tab_shows_the_right_people() {
        assert!(belongs(&entry(FRIEND), "online", true));
        assert!(!belongs(&entry(FRIEND), "online", false));
        assert!(belongs(&entry(FRIEND), "all", false));
        assert!(belongs(&entry(INCOMING), "pending", false));
        assert!(belongs(&entry(OUTGOING), "pending", false));
        assert!(belongs(&entry(BLOCKED), "blocked", false));
        assert!(!belongs(&entry(BLOCKED), "all", false));
    }

    #[test]
    fn validates_usernames() {
        assert_eq!(clean_username(" @Some.User_1 ").as_deref(), Some("some.user_1"));
        assert!(clean_username("a").is_none());
        assert!(clean_username("bad name").is_none());
        assert!(clean_username("user#1234").is_none());
    }
}
