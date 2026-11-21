//! The user menu's Roles, Move To, Note and Block. Discord stays the judge of each change.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use reqwest::Method;
use serde_json::{json, Value};
use slint::{Color, ComponentHandle, Model, ModelRc, VecModel};

use crate::http::DiscordHttpClient;
use crate::{AppWindow, UserMenuItem};

const BLOCKED_RELATIONSHIP: u64 = 2;
const MAX_NOTE: usize = 256;

static REQUEST: AtomicU64 = AtomicU64::new(0);
static BLOCKED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

fn on_ui(app: &slint::Weak<AppWindow>, job: impl FnOnce(&AppWindow) + Send + 'static) {
    let app = app.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = app.upgrade() {
            job(&ui);
        }
    });
}

fn blocked_ids(body: &Value) -> HashSet<String> {
    body.as_array()
        .into_iter()
        .flatten()
        .filter(|entry| entry["type"].as_u64() == Some(BLOCKED_RELATIONSHIP))
        .filter_map(|entry| entry["id"].as_str().map(str::to_owned))
        .collect()
}

/// Assignable roles, highest first: @everyone and bot-managed roles can't be toggled.
fn role_rows(roles: &Value, held: &HashSet<String>, guild: &str) -> Vec<(String, String, bool, u32)> {
    let mut list: Vec<&Value> = roles
        .as_array()
        .into_iter()
        .flatten()
        .filter(|role| role["id"].as_str() != Some(guild) && !role["managed"].as_bool().unwrap_or(false))
        .collect();
    list.sort_by_key(|role| std::cmp::Reverse(role["position"].as_i64().unwrap_or(0)));
    list.into_iter()
        .filter_map(|role| {
            let id = role["id"].as_str()?.to_owned();
            let checked = held.contains(&id);
            Some((id, role["name"].as_str().unwrap_or("Role").to_owned(), checked, role["color"].as_u64().unwrap_or(0) as u32))
        })
        .collect()
}

fn voice_rows(channels: &[Value], current: &str) -> Vec<(String, String)> {
    let mut list: Vec<&Value> = channels
        .iter()
        .filter(|c| matches!(c["type"].as_u64(), Some(2 | 13)) && c["id"].as_str() != Some(current))
        .collect();
    list.sort_by_key(|c| c["position"].as_i64().unwrap_or(0));
    list.into_iter()
        .filter_map(|c| Some((c["id"].as_str()?.to_owned(), c["name"].as_str().unwrap_or("Voice").to_owned())))
        .collect()
}

fn item(id: String, title: String, has_check: bool, checked: bool, color: u32) -> UserMenuItem {
    UserMenuItem {
        id: id.into(),
        title: crate::utils::styled_text::plain_display_name(&title).as_ref().into(),
        has_check,
        checked,
        has_color: color != 0,
        color: Color::from_rgb_u8((color >> 16) as u8, (color >> 8) as u8, color as u8),
    }
}

fn is_current(request: u64) -> bool {
    REQUEST.load(Ordering::SeqCst) == request
}

/// Sets the Block row for this user; the list is read from Discord once.
pub fn prepare(ui: &AppWindow, http: Option<DiscordHttpClient>, user: &str) {
    REQUEST.fetch_add(1, Ordering::SeqCst);
    ui.set_user_menu_sub("".into());
    ui.set_user_menu_items(ModelRc::default());
    ui.set_user_menu_note("".into());
    let known = BLOCKED.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|set| set.contains(user));
    ui.set_user_menu_blocked(known.unwrap_or(false));
    let (None, Some(http)) = (known, http) else { return };
    let (app, user) = (ui.as_weak(), user.to_owned());
    tokio::spawn(async move {
        let Ok(body) = http.discord_call(Method::GET, "/users/@me/relationships", None).await else { return };
        let ids = blocked_ids(&body);
        let blocked = ids.contains(&user);
        *BLOCKED.lock().unwrap_or_else(|e| e.into_inner()) = Some(ids);
        on_ui(&app, move |ui| {
            if ui.get_user_menu_user_id() == user.as_str() {
                ui.set_user_menu_blocked(blocked);
            }
        });
    });
}

fn open_roles(ui: &AppWindow, http: DiscordHttpClient, guild: String, user: String) {
    let request = REQUEST.fetch_add(1, Ordering::SeqCst) + 1;
    ui.set_user_menu_sub("roles".into());
    ui.set_user_menu_items(ModelRc::default());
    let app = ui.as_weak();
    tokio::spawn(async move {
        let roles = http.discord_call(Method::GET, &format!("/guilds/{guild}/roles"), None).await;
        let member = http.get_guild_member(&guild, &user).await;
        let rows = match (roles, member) {
            (Ok(roles), Ok(member)) => {
                let held = member["roles"].as_array().into_iter().flatten().filter_map(|r| r.as_str().map(str::to_owned)).collect();
                Ok(role_rows(&roles, &held, &guild))
            }
            (Err(error), _) | (_, Err(error)) => Err(error),
        };
        on_ui(&app, move |ui| {
            if !is_current(request) {
                return;
            }
            match rows {
                Ok(rows) => {
                    let items: Vec<UserMenuItem> = rows.into_iter().map(|(id, title, checked, color)| item(id, title, true, checked, color)).collect();
                    ui.set_user_menu_items(ModelRc::new(VecModel::from(items)));
                }
                Err(error) => ui.set_user_menu_error(format!("Couldn't load roles ({error})").into()),
            }
        });
    });
}

fn toggle_role(ui: &AppWindow, http: DiscordHttpClient, guild: String, user: String, role: String) {
    let items = ui.get_user_menu_items();
    let Some(index) = (0..items.row_count()).find(|i| items.row_data(*i).is_some_and(|row| row.id == role.as_str())) else { return };
    let Some(mut row) = items.row_data(index) else { return };
    let on = !row.checked;
    row.checked = on;
    items.set_row_data(index, row);
    let app = ui.as_weak();
    tokio::spawn(async move {
        let method = if on { Method::PUT } else { Method::DELETE };
        let path = format!("/guilds/{guild}/members/{user}/roles/{role}");
        if let Err(error) = http.discord_call(method, &path, None).await {
            on_ui(&app, move |ui| {
                let items = ui.get_user_menu_items();
                if let Some(mut row) = items.row_data(index).filter(|row| row.id == role.as_str()) {
                    row.checked = !on;
                    items.set_row_data(index, row);
                }
                ui.set_user_menu_error(format!("Couldn't change that role ({error})").into());
            });
        }
    });
}

fn open_move(ui: &AppWindow, http: DiscordHttpClient, guild: String, user: String) {
    let request = REQUEST.fetch_add(1, Ordering::SeqCst) + 1;
    ui.set_user_menu_sub("move".into());
    ui.set_user_menu_items(ModelRc::default());
    let current = user
        .parse::<u64>()
        .ok()
        .and_then(|id| crate::gateway::get_guild_voice_states_store().lock().ok()?.get(&id).cloned())
        .unwrap_or_default();
    let app = ui.as_weak();
    tokio::spawn(async move {
        let rows = http.get_guild_channels(&guild).await.map(|list| voice_rows(&list, &current));
        on_ui(&app, move |ui| {
            if !is_current(request) {
                return;
            }
            match rows {
                Ok(rows) => {
                    let items: Vec<UserMenuItem> = rows.into_iter().map(|(id, title)| item(id, title, false, false, 0)).collect();
                    ui.set_user_menu_items(ModelRc::new(VecModel::from(items)));
                }
                Err(error) => ui.set_user_menu_error(format!("Couldn't load channels ({error})").into()),
            }
        });
    });
}

fn move_to(ui: &AppWindow, http: DiscordHttpClient, guild: String, user: String, channel: String) {
    let app = ui.as_weak();
    tokio::spawn(async move {
        let result = http.update_member(&guild, &user, json!({ "channel_id": channel })).await;
        on_ui(&app, move |ui| match result {
            Ok(()) => ui.set_show_user_menu_popup(false),
            Err(error) => ui.set_user_menu_error(error.into()),
        });
    });
}

fn open_note(ui: &AppWindow, http: DiscordHttpClient, user: String) {
    let request = REQUEST.fetch_add(1, Ordering::SeqCst) + 1;
    ui.set_user_menu_sub("note".into());
    ui.set_user_menu_note("".into());
    let app = ui.as_weak();
    tokio::spawn(async move {
        // Discord answers 404 when there is no note yet.
        let Ok(body) = http.discord_call(Method::GET, &format!("/users/@me/notes/{user}"), None).await else { return };
        let note = body["note"].as_str().unwrap_or("").to_owned();
        on_ui(&app, move |ui| {
            if is_current(request) {
                ui.set_user_menu_note(note.into());
            }
        });
    });
}

fn save_note(ui: &AppWindow, http: DiscordHttpClient, user: String) {
    let note: String = ui.get_user_menu_note().chars().take(MAX_NOTE).collect();
    let app = ui.as_weak();
    tokio::spawn(async move {
        let result = http.discord_call(Method::PUT, &format!("/users/@me/notes/{user}"), Some(json!({ "note": note }))).await;
        on_ui(&app, move |ui| match result {
            Ok(_) => ui.set_user_menu_sub("".into()),
            Err(error) => ui.set_user_menu_error(format!("Couldn't save the note ({error})").into()),
        });
    });
}

fn set_blocked(ui: &AppWindow, http: DiscordHttpClient, user: String, block: bool) {
    ui.set_user_menu_confirm("".into());
    let app = ui.as_weak();
    tokio::spawn(async move {
        let path = format!("/users/@me/relationships/{user}");
        let result = if block {
            http.discord_call(Method::PUT, &path, Some(json!({ "type": BLOCKED_RELATIONSHIP }))).await
        } else {
            http.discord_call(Method::DELETE, &path, None).await
        };
        if result.is_ok() {
            if let Some(set) = BLOCKED.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                if block {
                    set.insert(user.clone());
                } else {
                    set.remove(&user);
                }
            }
        }
        on_ui(&app, move |ui| match result {
            Ok(_) => {
                ui.set_user_menu_blocked(block);
                ui.set_show_user_menu_popup(false);
            }
            Err(error) => ui.set_user_menu_error(format!("Couldn't update block ({error})").into()),
        });
    });
}

/// Runs a menu action of this module; `false` means it belongs to the main menu.
pub fn handle(ui: &AppWindow, http: Option<DiscordHttpClient>, guild: Option<String>, name: &str) -> bool {
    let user = ui.get_user_menu_user_id().to_string();
    if name == "back" {
        REQUEST.fetch_add(1, Ordering::SeqCst);
        ui.set_user_menu_sub("".into());
        ui.set_user_menu_error("".into());
        return true;
    }
    let known = matches!(name, "open_roles" | "open_move" | "open_note" | "save_note" | "block" | "unblock") || name.starts_with("pick:");
    if !known {
        return false;
    }
    ui.set_user_menu_error("".into());
    let Some(http) = http else { return true };
    match name {
        "open_note" => open_note(ui, http, user),
        "save_note" => save_note(ui, http, user),
        "block" => set_blocked(ui, http, user, true),
        "unblock" => set_blocked(ui, http, user, false),
        _ => {
            let Some(guild) = guild else { return true };
            match name {
                "open_roles" => open_roles(ui, http, guild, user),
                "open_move" => open_move(ui, http, guild, user),
                _ => {
                    let target = name.trim_start_matches("pick:").to_owned();
                    if ui.get_user_menu_sub() == "roles" {
                        toggle_role(ui, http, guild, user, target);
                    } else {
                        move_to(ui, http, guild, user, target);
                    }
                }
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_only_blocked_relationships() {
        let body = json!([{ "id": "1", "type": 1 }, { "id": "2", "type": 2 }]);
        assert_eq!(blocked_ids(&body), HashSet::from(["2".to_owned()]));
    }

    #[test]
    fn lists_assignable_roles_highest_first() {
        let roles = json!([
            { "id": "9", "name": "@everyone", "position": 0 },
            { "id": "1", "name": "low", "position": 1 },
            { "id": "2", "name": "high", "position": 5, "color": 255 },
            { "id": "3", "name": "bot", "position": 9, "managed": true },
        ]);
        let rows = role_rows(&roles, &HashSet::from(["1".to_owned()]), "9");
        assert_eq!(rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(), ["2", "1"]);
        assert!(!rows[0].2 && rows[1].2);
    }

    #[test]
    fn move_list_skips_the_current_channel_and_text() {
        let channels = vec![
            json!({ "id": "1", "type": 0, "name": "text", "position": 0 }),
            json!({ "id": "2", "type": 2, "name": "b", "position": 2 }),
            json!({ "id": "3", "type": 2, "name": "a", "position": 1 }),
            json!({ "id": "4", "type": 13, "name": "stage", "position": 3 }),
        ];
        let rows = voice_rows(&channels, "2");
        assert_eq!(rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(), ["3", "4"]);
    }
}
