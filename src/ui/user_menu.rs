//! The right-click menu of a user. Discord stays the judge of every moderation action:
//! options are hidden by permission, and a refused request is shown in the menu.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::json;
use slint::ComponentHandle;

use crate::http::DiscordHttpClient;
use crate::AppWindow;

const KICK_MEMBERS: u64 = 1 << 1;
const BAN_MEMBERS: u64 = 1 << 2;
const MUTE_MEMBERS: u64 = 1 << 22;
const DEAFEN_MEMBERS: u64 = 1 << 23;
const MOVE_MEMBERS: u64 = 1 << 24;
const MANAGE_ROLES: u64 = 1 << 28;

static REQUEST: AtomicU64 = AtomicU64::new(0);

/// Rows pass a user id, or `voice-member:<channel>:<user>` from the channel list.
pub fn user_id_of(row_id: &str) -> Option<&str> {
    let id = row_id.rsplit(':').next()?;
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())).then_some(id)
}

fn current_guild(ui: &AppWindow) -> Option<String> {
    let guild = ui.get_active_guild_id().to_string();
    (!guild.is_empty() && guild != "@me").then_some(guild)
}

pub fn open(ui: &AppWindow, http: Option<DiscordHttpClient>, row_id: &str, x: f32, y: f32) {
    let Some(user_id) = user_id_of(row_id).map(str::to_owned) else { return };
    let numeric: u64 = user_id.parse().unwrap_or(0);
    let is_self = numeric == crate::gateway::get_my_user_id();
    let (muted, volume) = crate::gateway::get_user_mute_volume(&user_id);
    let in_voice = crate::gateway::get_guild_voice_states_store()
        .lock()
        .is_ok_and(|states| states.contains_key(&numeric));

    ui.set_user_menu_user_id(user_id.as_str().into());
    ui.set_user_menu_name(crate::utils::styled_text::plain_display_name(&crate::gateway::get_user_name(numeric)).as_ref().into());
    ui.set_user_menu_is_self(is_self);
    ui.set_user_menu_in_voice(in_voice);
    ui.set_user_menu_volume(volume);
    ui.set_user_menu_muted(muted);
    ui.set_user_menu_has_stream(!is_self && crate::screen_capture::peer_is_streaming(numeric));
    ui.set_user_menu_stream_volume(crate::screen_capture::audio_loopback::get_stream_volume(numeric).min(1.0));
    ui.set_user_menu_server_muted(false);
    ui.set_user_menu_server_deafened(false);
    ui.set_user_menu_can_mute(false);
    ui.set_user_menu_can_deafen(false);
    ui.set_user_menu_can_move(false);
    ui.set_user_menu_can_kick(false);
    ui.set_user_menu_can_ban(false);
    ui.set_user_menu_can_roles(false);
    crate::ui::user_menu_tools::prepare(ui, http.clone(), &user_id);
    ui.set_user_menu_confirm("".into());
    ui.set_user_menu_error("".into());
    ui.set_user_menu_x(x);
    ui.set_user_menu_y(y);
    ui.set_show_user_menu_popup(true);

    let request = REQUEST.fetch_add(1, Ordering::SeqCst) + 1;
    let (Some(http), Some(guild)) = (http, current_guild(ui)) else { return };
    let app = ui.as_weak();
    tokio::spawn(async move {
        let permissions = http.get_channel_permissions(&guild).await.ok();
        let member = http.get_guild_member(&guild, &user_id).await.ok();
        let _ = slint::invoke_from_event_loop(move || {
            if REQUEST.load(Ordering::SeqCst) != request {
                return;
            }
            let Some(ui) = app.upgrade() else { return };
            if let Some(member) = member {
                ui.set_user_menu_server_muted(member["mute"].as_bool().unwrap_or(false));
                ui.set_user_menu_server_deafened(member["deaf"].as_bool().unwrap_or(false));
                if let Some(nick) = member["nick"].as_str().filter(|n| !n.is_empty()) {
                    ui.set_user_menu_name(crate::utils::styled_text::plain_display_name(nick).as_ref().into());
                }
            }
            if let Some(p) = permissions.filter(|_| !is_self) {
                ui.set_user_menu_can_mute(p.has_permission(MUTE_MEMBERS));
                ui.set_user_menu_can_deafen(p.has_permission(DEAFEN_MEMBERS));
                ui.set_user_menu_can_move(p.has_permission(MOVE_MEMBERS));
                ui.set_user_menu_can_kick(p.has_permission(KICK_MEMBERS));
                ui.set_user_menu_can_ban(p.has_permission(BAN_MEMBERS));
                ui.set_user_menu_can_roles(p.has_permission(MANAGE_ROLES));
            }
        });
    });
}

fn finish(app: slint::Weak<AppWindow>, result: Result<(), String>, close: bool) {
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = app.upgrade() else { return };
        match result {
            Ok(()) if close => ui.set_show_user_menu_popup(false),
            Ok(()) => {}
            Err(e) => {
                log::warn!("Ação do menu de usuário recusada: {}", e);
                ui.set_user_menu_confirm("".into());
                ui.set_user_menu_error(e.into());
            }
        }
    });
}

pub fn action(ui: &AppWindow, http: Option<DiscordHttpClient>, active_channel: Arc<Mutex<String>>, name: &str) {
    let user_id = ui.get_user_menu_user_id().to_string();
    if crate::ui::user_menu_tools::handle(ui, http.clone(), current_guild(ui), name) {
        return;
    }
    match name {
        "profile" => {
            ui.set_show_user_menu_popup(false);
            ui.invoke_open_profile(user_id.into());
            return;
        }
        "mention" => {
            ui.set_chat_input_text(format!("{}<@{}> ", ui.get_chat_input_text(), user_id).into());
            ui.set_show_user_menu_popup(false);
            return;
        }
        "copy_id" => {
            let copied = copypasta::ClipboardContext::new()
                .and_then(|mut clipboard| copypasta::ClipboardProvider::set_contents(&mut clipboard, user_id));
            if copied.is_ok() {
                ui.set_show_user_menu_popup(false);
            } else {
                ui.set_user_menu_error("Clipboard unavailable".into());
            }
            return;
        }
        _ => {}
    }

    let Some(http) = http else { return };
    let app = ui.as_weak();
    let guild = current_guild(ui);
    let server_muted = ui.get_user_menu_server_muted();
    let server_deafened = ui.get_user_menu_server_deafened();
    let name = name.to_owned();
    tokio::spawn(async move {
        if name == "message" {
            match http.open_dm(&user_id).await {
                Ok(channel) => {
                    crate::ui::dm_close::take_closed(&channel);
                    *active_channel.lock().unwrap() = channel;
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = app.upgrade() {
                            ui.set_show_user_menu_popup(false);
                            ui.set_active_guild_id("@me".into());
                            ui.invoke_select_guild("@me".into());
                        }
                    });
                }
                Err(e) => finish(app, Err(e), false),
            }
            return;
        }
        let Some(guild) = guild else { return };
        let (result, close) = match name.as_str() {
            "server_mute" => (http.update_member(&guild, &user_id, json!({ "mute": !server_muted })).await, false),
            "server_deafen" => (http.update_member(&guild, &user_id, json!({ "deaf": !server_deafened })).await, false),
            "disconnect" => (http.update_member(&guild, &user_id, json!({ "channel_id": null })).await, true),
            "kick" => (http.kick_member(&guild, &user_id).await, true),
            "ban" => (http.ban_member(&guild, &user_id).await, true),
            _ => return,
        };
        if result.is_ok() && !close {
            let app_state = app.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = app_state.upgrade() {
                    if name == "server_mute" {
                        ui.set_user_menu_server_muted(!server_muted);
                    } else {
                        ui.set_user_menu_server_deafened(!server_deafened);
                    }
                }
            });
        }
        finish(app, result, close);
    });
}

#[cfg(test)]
mod tests {
    use super::user_id_of;

    #[test]
    fn reads_the_user_from_every_kind_of_row() {
        assert_eq!(user_id_of("123"), Some("123"));
        assert_eq!(user_id_of("voice-member:55:123"), Some("123"));
    }

    #[test]
    fn rejects_anything_that_is_not_an_id() {
        assert_eq!(user_id_of(""), None);
        assert_eq!(user_id_of("voice-member:55:abc"), None);
        assert_eq!(user_id_of("1/../2"), None);
    }
}
