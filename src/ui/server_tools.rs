//! The server icon's right-click menu and hover card.

use std::sync::atomic::{AtomicU64, Ordering};

use reqwest::Method;
use serde_json::{json, Value};
use slint::{ComponentHandle, ModelRc, VecModel};

use crate::ui::channel_tools::Context;
use crate::ui::{notifications, read_states};
use crate::utils::avatar_cache;
use crate::{AppWindow, ServerHoverUser};

const CREATE_INVITE: u64 = 1 << 0;
const MANAGE_CHANNELS: u64 = 1 << 4;
const HOVER_FACES: usize = 5;
const TEXT_CHANNEL_TYPES_NOTE: &str = "Couldn't do that";

static MENU_REQUEST: AtomicU64 = AtomicU64::new(0);

fn on_ui(app: &slint::Weak<AppWindow>, job: impl FnOnce(&AppWindow) + Send + 'static) {
    let app = app.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = app.upgrade() {
            job(&ui);
        }
    });
}

fn report(app: &slint::Weak<AppWindow>, result: Result<Value, String>) {
    if let Err(error) = result {
        on_ui(app, move |ui| ui.set_titlebar_error(format!("{TEXT_CHANNEL_TYPES_NOTE} ({error})").into()));
    }
}

/// The new `restricted_guilds` after turning DMs from a server on or off.
fn toggled_restricted(current: &Value, guild: &str, allow: bool) -> Vec<String> {
    let mut list: Vec<String> = current.as_array().into_iter().flatten().filter_map(|id| id.as_str().map(str::to_owned)).filter(|id| id != guild).collect();
    if !allow {
        list.push(guild.to_owned());
    }
    list
}

fn open(ui: &AppWindow, ctx: &Context, guild: &str, x: f32, y: f32) {
    let request = MENU_REQUEST.fetch_add(1, Ordering::SeqCst) + 1;
    ui.set_server_menu_id(guild.into());
    ui.set_server_menu_x(x);
    ui.set_server_menu_y(y);
    ui.set_server_menu_levels(false);
    ui.set_server_menu_muted(notifications::is_guild_muted(guild));
    ui.set_server_menu_level(notifications::guild_level(guild) as i32);
    ui.set_server_menu_hide_muted(notifications::hides_muted(guild));
    ui.set_server_menu_allow_dms(true);
    ui.set_server_menu_owner(false);
    ui.set_server_menu_can_manage(false);
    ui.set_server_menu_can_invite(false);
    ui.set_server_menu_open(true);
    let Some(http) = ctx.http.lock().unwrap().as_ref().cloned() else { return };
    let (app, guild) = (ui.as_weak(), guild.to_owned());
    tokio::spawn(async move {
        let permissions = http.get_channel_permissions(&guild).await.ok();
        let info = http.discord_call(Method::GET, &format!("/guilds/{guild}"), None).await.ok();
        let settings = http.discord_call(Method::GET, "/users/@me/settings", None).await.ok();
        on_ui(&app, move |ui| {
            if MENU_REQUEST.load(Ordering::SeqCst) != request {
                return;
            }
            if let Some(p) = permissions {
                ui.set_server_menu_can_manage(p.has_permission(MANAGE_CHANNELS));
                ui.set_server_menu_can_invite(p.has_permission(CREATE_INVITE));
            }
            let me = crate::gateway::get_my_user_id().to_string();
            ui.set_server_menu_owner(info.is_some_and(|g| g["owner_id"].as_str() == Some(me.as_str())));
            if let Some(s) = settings {
                let blocked = s["restricted_guilds"].as_array().is_some_and(|list| list.iter().any(|id| id.as_str() == Some(guild.as_str())));
                ui.set_server_menu_allow_dms(!blocked);
            }
        });
    });
}

fn patch_settings(ui: &AppWindow, ctx: &Context, guild: &str, body: Value) {
    let Some(http) = ctx.http.lock().unwrap().as_ref().cloned() else { return };
    let (app, path) = (ui.as_weak(), format!("/users/@me/guilds/{guild}/settings"));
    tokio::spawn(async move {
        let result = http.discord_call(Method::PATCH, &path, Some(body)).await;
        report(&app, result);
    });
}

fn first_text_channel(ctx: &Context, guild: &str) -> Option<String> {
    let guilds = ctx.guilds.lock().unwrap();
    guilds.get(guild)?.channels.iter().find(|c| !c.is_voice && !c.is_category).map(|c| c.id.clone())
}

fn make_active(ui: &AppWindow, guild: &str) {
    if ui.get_active_guild_id() != guild {
        ui.set_active_guild_id(guild.into());
        ui.invoke_select_guild(guild.into());
    }
}

fn act(ui: &AppWindow, ctx: &Context, name: &str) {
    let guild = ui.get_server_menu_id().to_string();
    if let Some(level) = name.strip_prefix("level:").and_then(|l| l.parse::<u64>().ok()) {
        notifications::set_local_guild(&guild, None, Some(level), None);
        ui.set_server_menu_level(level as i32);
        ui.set_server_menu_levels(false);
        patch_settings(ui, ctx, &guild, json!({ "message_notifications": level }));
        return;
    }
    let http = ctx.http.lock().unwrap().as_ref().cloned();
    match name {
        "mute" => {
            let muted = !ui.get_server_menu_muted();
            notifications::set_local_guild(&guild, Some(muted), None, None);
            ui.set_server_menu_muted(muted);
            crate::ui::presence::schedule_refresh();
            patch_settings(ui, ctx, &guild, json!({ "muted": muted }));
        }
        "hide_muted" => {
            let hide = !ui.get_server_menu_hide_muted();
            notifications::set_local_guild(&guild, None, None, Some(hide));
            ui.set_server_menu_hide_muted(hide);
            crate::ui::channel_tools::rebuild_soon();
            patch_settings(ui, ctx, &guild, json!({ "hide_muted_channels": hide }));
        }
        "privacy" => {
            let allow = !ui.get_server_menu_allow_dms();
            ui.set_server_menu_allow_dms(allow);
            let Some(http) = http else { return };
            let app = ui.as_weak();
            tokio::spawn(async move {
                let result = match http.discord_call(Method::GET, "/users/@me/settings", None).await {
                    Ok(settings) => {
                        let list = toggled_restricted(&settings["restricted_guilds"], &guild, allow);
                        http.discord_call(Method::PATCH, "/users/@me/settings", Some(json!({ "restricted_guilds": list }))).await
                    }
                    Err(error) => Err(error),
                };
                report(&app, result);
            });
        }
        "mark_read" => {
            ui.set_server_menu_open(false);
            let marks = read_states::mark_guild_read(&guild);
            let Some(http) = http.filter(|_| !marks.is_empty()) else { return };
            let body: Vec<Value> = marks.iter().map(|(channel, id)| json!({ "channel_id": channel, "message_id": id.to_string() })).collect();
            let app = ui.as_weak();
            tokio::spawn(async move {
                let result = http.discord_call(Method::POST, "/read-states/ack-bulk", Some(json!({ "read_states": body }))).await;
                report(&app, result);
            });
        }
        "invite" => {
            ui.set_server_menu_open(false);
            if let Some(channel) = first_text_channel(ctx, &guild) {
                make_active(ui, &guild);
                ui.invoke_channel_action("invite".into(), channel.as_str().into(), 0.0, 0.0);
            }
        }
        "create_channel" | "create_category" => {
            ui.set_server_menu_open(false);
            make_active(ui, &guild);
            ui.invoke_channel_action("create".into(), "".into(), 0.0, 0.0);
            if name == "create_category" {
                ui.set_create_channel_kind(4);
            }
        }
        "server_settings" | "create_event" => {
            ui.set_server_menu_open(false);
            ui.invoke_open_link(format!("https://discord.com/channels/{guild}").into());
        }
        "nick" => {
            ui.set_server_menu_open(false);
            ui.set_server_nick_text("".into());
            ui.set_server_nick_error("".into());
            ui.set_server_nick_guild(guild.as_str().into());
            ui.set_server_nick_open(true);
            let Some(http) = http else { return };
            let app = ui.as_weak();
            let me = crate::gateway::get_my_user_id().to_string();
            tokio::spawn(async move {
                if let Ok(member) = http.get_guild_member(&guild, &me).await {
                    let nick = member["nick"].as_str().unwrap_or("").to_owned();
                    on_ui(&app, move |ui| {
                        if ui.get_server_nick_guild() == guild.as_str() {
                            ui.set_server_nick_text(nick.into());
                        }
                    });
                }
            });
        }
        "nick_save" => {
            let Some(http) = http else { return };
            let (guild, nick) = (ui.get_server_nick_guild().to_string(), ui.get_server_nick_text().trim().to_owned());
            let app = ui.as_weak();
            tokio::spawn(async move {
                let body = json!({ "nick": if nick.is_empty() { Value::Null } else { json!(nick) } });
                let result = http.discord_call(Method::PATCH, &format!("/guilds/{guild}/members/@me"), Some(body)).await;
                on_ui(&app, move |ui| match result {
                    Ok(_) => ui.set_server_nick_open(false),
                    Err(error) => ui.set_server_nick_error(format!("Couldn't change your nickname ({error})").into()),
                });
            });
        }
        "ask_leave" => {
            ui.set_server_menu_open(false);
            ui.set_channel_confirm_action("leave_guild".into());
            ui.set_channel_confirm_id(guild.as_str().into());
            ui.set_channel_confirm_text("Leave this server? You'll need a new invite to come back.".into());
        }
        "copy_id" => {
            ui.set_server_menu_open(false);
            let _ = copypasta::ClipboardContext::new().and_then(|mut c| copypasta::ClipboardProvider::set_contents(&mut c, guild));
        }
        _ => {}
    }
}

/// After the confirmation dialog.
pub fn leave(ui: &AppWindow, ctx: &Context, guild: String) {
    let Some(http) = ctx.http.lock().unwrap().as_ref().cloned() else { return };
    let app = ui.as_weak();
    tokio::spawn(async move {
        let result = http.discord_call(Method::DELETE, &format!("/users/@me/guilds/{guild}"), Some(json!({ "lurking": false }))).await;
        on_ui(&app, move |ui| match result {
            Ok(_) => {
                crate::ui::guild_folders::remove_guild(ui, &guild);
                if ui.get_active_guild_id() == guild.as_str() {
                    ui.set_active_guild_id("@me".into());
                    ui.invoke_select_guild("@me".into());
                }
            }
            Err(error) => ui.set_titlebar_error(format!("Couldn't leave the server ({error})").into()),
        });
    });
}

fn faces(ids: &[u64]) -> (Vec<ServerHoverUser>, i32) {
    let shown: Vec<ServerHoverUser> = ids
        .iter()
        .take(HOVER_FACES)
        .map(|id| {
            let url = crate::gateway::get_user_avatar_url(*id);
            let name = crate::gateway::get_user_name(*id);
            ServerHoverUser {
                text: name.chars().next().unwrap_or('?').to_uppercase().to_string().into(),
                slot: avatar_cache::get_avatar_cache().slot_for(&url),
                url: url.into(),
            }
        })
        .collect();
    (shown, ids.len().saturating_sub(HOVER_FACES) as i32)
}

fn hover(ui: &AppWindow, guild: &str, name: &str, y: f32) {
    let channels = read_states::channels_of(guild);
    let mut voice: Vec<u64> = crate::gateway::get_guild_voice_states_store()
        .lock()
        .map(|states| states.iter().filter(|(_, channel)| channels.contains(channel)).map(|(id, _)| *id).collect())
        .unwrap_or_default();
    voice.sort_unstable();
    let streams: Vec<u64> = voice.iter().copied().filter(|id| crate::screen_capture::types::get_user_stream_flags(*id).0).collect();
    let (voice_faces, voice_extra) = faces(&voice);
    let (stream_faces, stream_extra) = faces(&streams);
    ui.set_server_hover_name(crate::utils::styled_text::plain_display_name(name).as_ref().into());
    ui.set_server_hover_muted(notifications::is_guild_muted(guild));
    ui.set_server_hover_y(y);
    ui.set_server_hover_voice(ModelRc::new(VecModel::from(voice_faces)));
    ui.set_server_hover_voice_extra(voice_extra);
    ui.set_server_hover_streams(ModelRc::new(VecModel::from(stream_faces)));
    ui.set_server_hover_streams_extra(stream_extra);
    ui.set_server_hover_id(guild.into());
}

pub fn install(ui: &AppWindow, ctx: Context) {
    let (app, menu_ctx) = (ui.as_weak(), ctx.clone());
    ui.on_server_menu_action(move |name| {
        if let Some(ui) = app.upgrade() {
            act(&ui, &menu_ctx, name.as_str());
        }
    });
    let app = ui.as_weak();
    ui.on_open_server_menu(move |guild, x, y| {
        if let Some(ui) = app.upgrade() {
            ui.set_server_hover_id("".into());
            open(&ui, &ctx, guild.as_str(), x, y);
        }
    });
    let app = ui.as_weak();
    ui.on_server_hovered(move |guild, name, y, active| {
        let Some(ui) = app.upgrade() else { return };
        if active {
            hover(&ui, guild.as_str(), name.as_str(), y);
        } else if ui.get_server_hover_id() == guild {
            ui.set_server_hover_id("".into());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dm_privacy_only_changes_this_server() {
        let current = json!(["1", "2"]);
        assert_eq!(toggled_restricted(&current, "2", true), ["1"]);
        assert_eq!(toggled_restricted(&current, "3", false), ["1", "2", "3"]);
        assert_eq!(toggled_restricted(&current, "2", false), ["1", "2"]);
    }
}
