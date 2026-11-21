//! The channel header panel: pinned messages, threads, notification level and search.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use reqwest::Method;
use serde_json::{json, Value};
use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::http::DiscordHttpClient;
use crate::ui::channel_tools::Context;
use crate::ui::data_loader::{load_messages_for_channel, remember_last_channel, show_messages_around};
use crate::{AppWindow, PanelRow};

const MANAGE_MESSAGES: u64 = 1 << 13;
const NOTIFICATION_LEVELS: [(u64, &str, &str); 4] = [
    (3, "Use Category Default", "Follow the server or category setting"),
    (0, "All Messages", "Notify for every message"),
    (1, "Only @mentions", "Notify only when you are mentioned"),
    (2, "Nothing", "Never notify for this channel"),
];

static REQUEST: AtomicU64 = AtomicU64::new(0);
static LEVELS: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

fn encode_query(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn short_time(stamp: &str) -> String {
    match (stamp.get(0..10), stamp.get(11..16)) {
        (Some(day), Some(time)) => format!("{day} {time}"),
        _ => String::new(),
    }
}

fn message_row(value: &Value, can_unpin: bool) -> Option<PanelRow> {
    let id = value["id"].as_str()?;
    let author = value["author"]["global_name"].as_str().or_else(|| value["author"]["username"].as_str()).unwrap_or("Unknown");
    let content = value["content"].as_str().unwrap_or("").replace('\n', " ");
    let text = if !content.is_empty() {
        content
    } else if value["attachments"].as_array().is_some_and(|list| !list.is_empty()) {
        "Attachment".to_owned()
    } else if value["embeds"].as_array().is_some_and(|list| !list.is_empty()) {
        "Embed".to_owned()
    } else {
        "Message".to_owned()
    };
    Some(PanelRow {
        id: id.into(),
        channel_id: value["channel_id"].as_str().unwrap_or("").into(),
        title: crate::utils::styled_text::plain_display_name(author).as_ref().into(),
        text: text.into(),
        time: short_time(value["timestamp"].as_str().unwrap_or("")).into(),
        can_jump: true,
        can_unpin,
        checked: false,
    })
}

/// Pins come as a list, or as `items` with a nested `message` on newer API versions.
fn pin_rows(body: &Value, can_unpin: bool) -> Vec<PanelRow> {
    let list = body.as_array().or_else(|| body["items"].as_array());
    list.into_iter()
        .flatten()
        .filter_map(|item| message_row(if item["message"].is_object() { &item["message"] } else { item }, can_unpin))
        .collect()
}

/// A search answer groups each hit with its context; the hit is the first entry.
fn search_rows(body: &Value) -> Vec<PanelRow> {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|group| message_row(group.as_array()?.first()?, false))
        .collect()
}

fn thread_rows(body: &Value) -> Vec<PanelRow> {
    body["threads"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|thread| {
            let count = thread["message_count"].as_u64().unwrap_or(0);
            Some(PanelRow {
                id: thread["id"].as_str()?.into(),
                channel_id: thread["id"].as_str()?.into(),
                title: thread["name"].as_str().unwrap_or("Thread").into(),
                text: format!("{count} messages").into(),
                time: "".into(),
                can_jump: false,
                can_unpin: false,
                checked: false,
            })
        })
        .collect()
}

fn notification_rows(level: u64) -> Vec<PanelRow> {
    NOTIFICATION_LEVELS
        .iter()
        .map(|(value, title, text)| PanelRow {
            id: value.to_string().into(),
            title: (*title).into(),
            text: (*text).into(),
            checked: *value == level,
            ..Default::default()
        })
        .collect()
}

fn level_of(channel: &str) -> u64 {
    LEVELS.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|map| map.get(channel).copied()).unwrap_or(3)
}

fn finish(ui: &AppWindow, request: u64, rows: Result<Vec<PanelRow>, String>, empty: &str) {
    if REQUEST.load(Ordering::SeqCst) != request {
        return;
    }
    match rows {
        Ok(rows) => {
            ui.set_channel_panel_status(if rows.is_empty() { empty.into() } else { "".into() });
            ui.set_channel_panel_rows(ModelRc::new(VecModel::from(rows)));
        }
        Err(error) => ui.set_channel_panel_status(format!("Couldn't load this ({error})").into()),
    }
}

fn deliver(app: &slint::Weak<AppWindow>, request: u64, rows: Result<Vec<PanelRow>, String>, empty: &'static str) {
    let app = app.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = app.upgrade() {
            finish(&ui, request, rows, empty);
        }
    });
}

fn open(ui: &AppWindow, ctx: &Context, kind: &str) {
    if ui.get_channel_panel() == kind {
        ui.set_channel_panel("".into());
        return;
    }
    let request = REQUEST.fetch_add(1, Ordering::SeqCst) + 1;
    ui.set_channel_panel(kind.into());
    ui.set_channel_panel_rows(ModelRc::default());
    ui.set_channel_panel_status("".into());
    let channel = ui.get_active_channel_id().to_string();
    let guild = ui.get_active_guild_id().to_string();
    match kind {
        "notifications" => ui.set_channel_panel_rows(ModelRc::new(VecModel::from(notification_rows(level_of(&channel))))),
        "search" => ui.set_channel_panel_status("Type something and press Enter.".into()),
        "pins" | "threads" => {
            let Some(http) = ctx.http.lock().unwrap().as_ref().cloned() else { return };
            ui.set_channel_panel_status("Loading...".into());
            let (app, kind) = (ui.as_weak(), kind.to_owned());
            tokio::spawn(async move {
                if kind == "pins" {
                    let can_unpin = guild == "@me" || http.get_channel_permissions(&guild).await.is_ok_and(|p| p.has_permission(MANAGE_MESSAGES));
                    let rows = http.discord_call(Method::GET, &format!("/channels/{channel}/pins"), None).await.map(|body| pin_rows(&body, can_unpin));
                    deliver(&app, request, rows, "This channel doesn't have any pinned messages yet.");
                } else {
                    let path = format!("/channels/{channel}/threads/search?archived=false&limit=25");
                    let rows = http.discord_call(Method::GET, &path, None).await.map(|body| thread_rows(&body));
                    deliver(&app, request, rows, "There are no active threads.");
                }
            });
        }
        _ => {}
    }
}

fn search(ui: &AppWindow, ctx: &Context, query: &str) {
    let query = query.trim();
    if query.is_empty() {
        return;
    }
    let Some(http) = ctx.http.lock().unwrap().as_ref().cloned() else { return };
    let request = REQUEST.fetch_add(1, Ordering::SeqCst) + 1;
    let guild = ui.get_active_guild_id().to_string();
    let channel = ui.get_active_channel_id().to_string();
    let scope = if guild == "@me" { format!("/channels/{channel}") } else { format!("/guilds/{guild}") };
    let path = format!("{scope}/messages/search?content={}", encode_query(query));
    ui.set_channel_panel_rows(ModelRc::default());
    ui.set_channel_panel_status("Searching...".into());
    let app = ui.as_weak();
    tokio::spawn(async move {
        let rows = http.discord_call(Method::GET, &path, None).await.map(|body| search_rows(&body));
        deliver(&app, request, rows, "No results.");
    });
}

fn set_level(ui: &AppWindow, ctx: &Context, level: u64) {
    let channel = ui.get_active_channel_id().to_string();
    let guild = ui.get_active_guild_id().to_string();
    if guild == "@me" {
        return;
    }
    let Some(http) = ctx.http.lock().unwrap().as_ref().cloned() else { return };
    let previous = level_of(&channel);
    LEVELS.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(channel.clone(), level);
    ui.set_channel_panel_rows(ModelRc::new(VecModel::from(notification_rows(level))));
    ui.set_channel_panel_status("".into());
    let app = ui.as_weak();
    tokio::spawn(async move {
        let body = json!({ "channel_overrides": { channel.clone(): { "message_notifications": level } } });
        if let Err(error) = http.discord_call(Method::PATCH, &format!("/users/@me/guilds/{guild}/settings"), Some(body)).await {
            LEVELS.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(channel, previous);
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = app.upgrade() {
                    if ui.get_channel_panel() == "notifications" {
                        ui.set_channel_panel_rows(ModelRc::new(VecModel::from(notification_rows(previous))));
                        ui.set_channel_panel_status(format!("Couldn't save ({error})").into());
                    }
                }
            });
        }
    });
}

fn switch_to(ui: &AppWindow, ctx: &Context, channel: &str, fallback_name: &str) {
    if ui.get_active_channel_id() == channel {
        return;
    }
    let guild = ctx.active_guild.lock().unwrap().clone();
    let name = ctx
        .guilds
        .lock()
        .unwrap()
        .get(&guild)
        .and_then(|data| data.channels.iter().find(|c| c.id == channel).map(|c| c.name.clone()))
        .unwrap_or_else(|| fallback_name.to_owned());
    *ctx.active_channel.lock().unwrap() = channel.to_owned();
    remember_last_channel(&guild, channel);
    ui.set_is_voice_focused(false);
    ui.set_active_channel_name(crate::utils::styled_text::plain_display_name(&name).as_ref().into());
    ui.set_active_channel_id(channel.into());
}

fn jump(ui: &AppWindow, ctx: &Context, message: &str, channel: &str) {
    let Some(http) = ctx.http.lock().unwrap().as_ref().cloned() else { return };
    let target_channel = if channel.is_empty() { ui.get_active_channel_id().to_string() } else { channel.to_owned() };
    switch_to(ui, ctx, &target_channel, "channel");
    ui.set_channel_panel("".into());
    let (app, message) = (ui.as_weak(), message.to_owned());
    tokio::spawn(async move {
        let path = format!("/channels/{target_channel}/messages?around={message}&limit=50");
        match http.discord_call(Method::GET, &path, None).await {
            Ok(body) => {
                let list = body.as_array().cloned().unwrap_or_default();
                show_messages_around(&list, &target_channel, &message, app);
            }
            Err(error) => {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = app.upgrade() {
                        ui.set_titlebar_error(format!("Couldn't open that message ({error})").into());
                    }
                });
            }
        }
    });
}

fn open_thread(ui: &AppWindow, ctx: &Context, thread: &str, name: &str) {
    switch_to(ui, ctx, thread, name);
    ui.set_channel_panel("".into());
    crate::ui::helpers::request_chat_scroll_to_bottom(ui.as_weak());
    if let Some(http) = ctx.http.lock().unwrap().as_ref().cloned() {
        let (app, thread) = (ui.as_weak(), thread.to_owned());
        tokio::spawn(async move {
            load_messages_for_channel(&http, app, &thread).await;
        });
    }
}

fn back_to_present(ui: &AppWindow, ctx: &Context) {
    let channel = ui.get_active_channel_id().to_string();
    ui.set_chat_jumped(false);
    ui.set_chat_highlight_id("".into());
    crate::ui::helpers::request_chat_scroll_to_bottom(ui.as_weak());
    if let Some(http) = ctx.http.lock().unwrap().as_ref().cloned() {
        let app = ui.as_weak();
        tokio::spawn(async move {
            load_messages_for_channel(&http, app, &channel).await;
        });
    }
}

/// Removes a pin after the confirmation dialog.
pub fn unpin(ui: &AppWindow, http: DiscordHttpClient, message: String) {
    let channel = ui.get_active_channel_id().to_string();
    let app = ui.as_weak();
    tokio::spawn(async move {
        let result = http.discord_call(Method::DELETE, &format!("/channels/{channel}/pins/{message}"), None).await;
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = app.upgrade() else { return };
            match result {
                Ok(_) => {
                    let kept: Vec<PanelRow> = ui.get_channel_panel_rows().iter().filter(|row| row.id != message.as_str()).collect();
                    if kept.is_empty() {
                        ui.set_channel_panel_status("This channel doesn't have any pinned messages yet.".into());
                    }
                    ui.set_channel_panel_rows(ModelRc::new(VecModel::from(kept)));
                }
                Err(error) => ui.set_channel_panel_status(format!("Couldn't unpin ({error})").into()),
            }
        });
    });
}

pub fn install(ui: &AppWindow, ctx: Context) {
    let app = ui.as_weak();
    ui.on_channel_panel_action(move |name, id, extra| {
        let Some(ui) = app.upgrade() else { return };
        match name.as_str() {
            "open" => open(&ui, &ctx, id.as_str()),
            "close" => ui.set_channel_panel("".into()),
            "search" => search(&ui, &ctx, id.as_str()),
            "row" => match ui.get_channel_panel().as_str() {
                "threads" => open_thread(&ui, &ctx, id.as_str(), extra.as_str()),
                "notifications" => set_level(&ui, &ctx, id.parse().unwrap_or(3)),
                _ => {}
            },
            "jump" => jump(&ui, &ctx, id.as_str(), extra.as_str()),
            "ask_unpin" => {
                ui.set_channel_confirm_action("unpin".into());
                ui.set_channel_confirm_id(id);
                ui.set_channel_confirm_text("Unpin this message?".into());
            }
            "present" => back_to_present(&ui, &ctx),
            _ => {}
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_text_is_encoded() {
        assert_eq!(encode_query("a b&c=é"), "a%20b%26c%3D%C3%A9");
    }

    #[test]
    fn reads_both_pin_shapes() {
        let plain = json!([{ "id": "1", "author": { "username": "a" }, "content": "hi", "timestamp": "2026-11-21T10:30:00+00:00" }]);
        let nested = json!({ "items": [{ "message": { "id": "2", "author": { "global_name": "B" }, "content": "yo" } }] });
        let first = pin_rows(&plain, true);
        assert_eq!((first[0].id.as_str(), first[0].time.as_str()), ("1", "2026-11-21 10:30"));
        assert!(first[0].can_unpin);
        assert_eq!(pin_rows(&nested, false)[0].title.as_str(), "B");
    }

    #[test]
    fn search_uses_the_hit_of_each_group() {
        let body = json!({ "messages": [[{ "id": "9", "channel_id": "4", "author": { "username": "a" }, "content": "x" }]] });
        let rows = search_rows(&body);
        assert_eq!((rows[0].id.as_str(), rows[0].channel_id.as_str()), ("9", "4"));
    }

    #[test]
    fn empty_messages_still_get_a_label() {
        let row = message_row(&json!({ "id": "1", "author": {}, "content": "", "attachments": [{}] }), false).unwrap();
        assert_eq!(row.text.as_str(), "Attachment");
    }

    #[test]
    fn marks_the_current_notification_level() {
        let rows = notification_rows(1);
        assert_eq!(rows.iter().filter(|r| r.checked).count(), 1);
        assert_eq!(rows.iter().find(|r| r.checked).unwrap().id.as_str(), "1");
    }
}
