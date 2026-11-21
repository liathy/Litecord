//! Channel actions: hover buttons, right-click menu, Create Channel, invites and settings.
//! Options are hidden by permission, and Discord stays the judge of every change.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use reqwest::Method;
use serde_json::{json, Map, Value};
use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::gateway::{self, arrange_channels, ChannelData, GuildData};
use crate::http::DiscordHttpClient;
use crate::ui::data_loader::{get_collapsed_categories, load_messages_for_channel, remember_last_channel};
use crate::ui::helpers::{build_ui_channels, clear_dm_unread, request_chat_scroll_to_bottom};
use crate::{AppWindow, ChannelInfoRow};

const CREATE_INVITE: u64 = 1 << 0;
const MANAGE_CHANNELS: u64 = 1 << 4;
const VIEW_CHANNEL: u64 = 1 << 10;
const INVITE_SECONDS: u64 = 604_800;
const MAX_SLOWMODE: u32 = 21_600;

static MUTED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
/// The channel as Discord last sent it, used to keep its other permission rules on save.
static OPEN_CHANNEL: Mutex<Option<Value>> = Mutex::new(None);

#[derive(Clone)]
pub struct Context {
    pub http: Arc<Mutex<Option<DiscordHttpClient>>>,
    pub guilds: Arc<Mutex<HashMap<String, GuildData>>>,
    pub active_guild: Arc<Mutex<String>>,
    pub active_channel: Arc<Mutex<String>>,
}

impl Context {
    fn client(&self) -> Option<DiscordHttpClient> {
        self.http.lock().unwrap().as_ref().cloned()
    }

    fn guild(&self) -> String {
        self.active_guild.lock().unwrap().clone()
    }

    fn channel(&self, id: &str) -> Option<ChannelData> {
        let guild = self.guild();
        self.guilds.lock().unwrap().get(&guild)?.channels.iter().find(|c| c.id == id).cloned()
    }
}

/// Discord's rule for names: text and forum channels are lowercase with dashes.
fn clean_name(name: &str, kind: u64) -> Option<String> {
    let trimmed = name.trim();
    let cleaned = if kind == 2 || kind == 4 {
        trimmed.to_owned()
    } else {
        trimmed.split_whitespace().collect::<Vec<_>>().join("-").to_lowercase()
    };
    (1..=100).contains(&cleaned.chars().count()).then_some(cleaned)
}

fn create_body(name: &str, kind: u64, parent: &str, private: bool, guild: &str, me: u64) -> Value {
    let mut body = json!({ "name": name, "type": kind });
    if !parent.is_empty() {
        body["parent_id"] = json!(parent);
    }
    if private {
        body["permission_overwrites"] = json!([
            { "id": guild, "type": 0, "allow": "0", "deny": VIEW_CHANNEL.to_string() },
            { "id": me.to_string(), "type": 1, "allow": VIEW_CHANNEL.to_string(), "deny": "0" },
        ]);
    }
    body
}

fn bits(value: &Value) -> u64 {
    value.as_u64().or_else(|| value.as_str()?.parse().ok()).unwrap_or(0)
}

fn overwrite<'a>(channel: &'a Value, id: &str) -> Option<&'a Value> {
    channel["permission_overwrites"].as_array()?.iter().find(|o| o["id"].as_str() == Some(id))
}

fn is_private(channel: &Value, guild: &str) -> bool {
    overwrite(channel, guild).is_some_and(|o| bits(&o["deny"]) & VIEW_CHANNEL != 0)
}

/// Only the View Channel bit changes; every other rule of the overwrite is kept.
fn with_view(allow: u64, deny: u64, private: bool) -> (u64, u64) {
    if private {
        (allow & !VIEW_CHANNEL, deny | VIEW_CHANNEL)
    } else {
        (allow, deny & !VIEW_CHANNEL)
    }
}

fn to_channel_data(value: &Value) -> Option<ChannelData> {
    let kind = value["type"].as_u64()?;
    Some(ChannelData {
        id: value["id"].as_str()?.to_owned(),
        name: value["name"].as_str()?.to_owned(),
        is_voice: kind == 2 || kind == 13,
        is_category: kind == 4,
        parent_id: value["parent_id"].as_str().map(str::to_owned),
        position: value["position"].as_i64().unwrap_or(0),
    })
}

fn copy_text(text: String) -> bool {
    copypasta::ClipboardContext::new()
        .and_then(|mut clipboard| copypasta::ClipboardProvider::set_contents(&mut clipboard, text))
        .is_ok()
}

fn on_ui(app: &slint::Weak<AppWindow>, job: impl FnOnce(&AppWindow) + Send + 'static) {
    let app = app.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = app.upgrade() {
            job(&ui);
        }
    });
}

/// Applies a change to the cached channels and redraws the list if that server is open.
fn change_list(ui: &AppWindow, ctx: &Context, guild: &str, change: impl FnOnce(&mut Vec<ChannelData>)) {
    let rows = {
        let mut guilds = ctx.guilds.lock().unwrap();
        let Some(data) = guilds.get_mut(guild) else { return };
        let mut list = std::mem::take(&mut data.channels);
        change(&mut list);
        data.channels = arrange_channels(list);
        let collapsed = get_collapsed_categories().lock().unwrap().clone();
        build_ui_channels(&data.channels, &collapsed)
    };
    if ctx.guild() == guild {
        ui.set_channels(ModelRc::new(VecModel::from(rows)));
    }
}

/// Which management options this account gets in the open server.
pub fn refresh_permissions(ui: &AppWindow, http: Option<DiscordHttpClient>) {
    let guild = ui.get_active_guild_id().to_string();
    ui.set_can_manage_channels(false);
    ui.set_can_create_invite(false);
    let Some(http) = http.filter(|_| !guild.is_empty() && guild != "@me") else { return };
    let app = ui.as_weak();
    tokio::spawn(async move {
        let Ok(permissions) = http.get_channel_permissions(&guild).await else { return };
        on_ui(&app, move |ui| {
            if ui.get_active_guild_id() == guild.as_str() {
                ui.set_can_manage_channels(permissions.has_permission(MANAGE_CHANNELS));
                ui.set_can_create_invite(permissions.has_permission(CREATE_INVITE));
            }
        });
    });
}

fn open_voice_chat(ui: &AppWindow, ctx: &Context, id: &str) {
    let Some(channel) = ctx.channel(id) else { return };
    *ctx.active_channel.lock().unwrap() = id.to_owned();
    remember_last_channel(&ctx.guild(), id);
    ui.set_is_voice_focused(false);
    ui.set_active_channel_name(crate::utils::styled_text::plain_display_name(&channel.name).as_ref().into());
    ui.set_active_channel_id(id.into());
    request_chat_scroll_to_bottom(ui.as_weak());
    if let Some(http) = ctx.client() {
        let (app, id) = (ui.as_weak(), id.to_owned());
        tokio::spawn(async move {
            load_messages_for_channel(&http, app, &id).await;
        });
    }
}

fn open_invite(ui: &AppWindow, ctx: &Context, id: &str) {
    let Some(http) = ctx.client() else { return };
    let name = ctx.channel(id).map(|c| c.name).unwrap_or_default();
    ui.set_invite_channel(crate::utils::styled_text::plain_display_name(&name).as_ref().into());
    ui.set_invite_link("".into());
    ui.set_invite_status("".into());
    ui.set_invite_open(true);
    let (app, id) = (ui.as_weak(), id.to_owned());
    tokio::spawn(async move {
        let body = json!({ "max_age": INVITE_SECONDS, "max_uses": 0, "unique": false });
        let result = http.discord_call(Method::POST, &format!("/channels/{id}/invites"), Some(body)).await;
        on_ui(&app, move |ui| match result.ok().and_then(|v| v["code"].as_str().map(str::to_owned)) {
            Some(code) => ui.set_invite_link(format!("https://discord.gg/{code}").into()),
            None => ui.set_invite_status("Couldn't create an invite for this channel.".into()),
        });
    });
}

fn mark_read(ui: &AppWindow, ctx: &Context, id: &str) {
    let remaining = clear_dm_unread(id);
    ui.set_total_dm_unread_count(remaining);
    let rows = ui.get_channels();
    for index in 0..rows.row_count() {
        if let Some(mut row) = rows.row_data(index).filter(|row| row.id == id) {
            row.unread_count = 0;
            rows.set_row_data(index, row);
        }
    }
    let Some(http) = ctx.client() else { return };
    let id = id.to_owned();
    tokio::spawn(async move {
        let Ok(channel) = http.discord_call(Method::GET, &format!("/channels/{id}"), None).await else { return };
        if let Some(last) = channel["last_message_id"].as_str() {
            let path = format!("/channels/{id}/messages/{last}/ack");
            let _ = http.discord_call(Method::POST, &path, Some(json!({ "token": null }))).await;
        }
    });
}

fn toggle_mute(ui: &AppWindow, ctx: &Context, id: &str) {
    let Some(http) = ctx.client() else { return };
    let guild = ctx.guild();
    let muted = {
        let mut store = MUTED.lock().unwrap_or_else(|e| e.into_inner());
        let set = store.get_or_insert_with(HashSet::new);
        if set.remove(id) {
            false
        } else {
            set.insert(id.to_owned());
            true
        }
    };
    ui.set_channel_menu_muted(muted);
    let (app, id) = (ui.as_weak(), id.to_owned());
    tokio::spawn(async move {
        let body = json!({ "channel_overrides": { id.clone(): { "muted": muted } } });
        let path = format!("/users/@me/guilds/{guild}/settings");
        if http.discord_call(Method::PATCH, &path, Some(body)).await.is_err() {
            if let Some(set) = MUTED.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                if muted {
                    set.remove(&id);
                } else {
                    set.insert(id);
                }
            }
            on_ui(&app, move |ui| ui.set_channel_menu_muted(!muted));
        }
    });
}

fn open_settings(ui: &AppWindow, ctx: &Context, id: &str) {
    let Some(http) = ctx.client() else { return };
    let Some(channel) = ctx.channel(id) else { return };
    ui.set_channel_settings_id(id.into());
    ui.set_channel_settings_title(crate::utils::styled_text::plain_display_name(&channel.name).as_ref().into());
    ui.set_channel_settings_is_voice(channel.is_voice);
    ui.set_channel_settings_is_category(channel.is_category);
    ui.set_channel_settings_tab("overview".into());
    ui.set_channel_settings_rows(ModelRc::default());
    ui.set_channel_settings_status("Loading...".into());
    ui.set_channel_settings_error("".into());
    ui.set_channel_settings_open(true);
    let guild = ctx.guild();
    let (app, id) = (ui.as_weak(), id.to_owned());
    tokio::spawn(async move {
        let result = http.discord_call(Method::GET, &format!("/channels/{id}"), None).await;
        on_ui(&app, move |ui| {
            if ui.get_channel_settings_id() != id.as_str() {
                return;
            }
            ui.set_channel_settings_status("".into());
            match result {
                Ok(value) => show_saved(ui, value, &guild),
                Err(error) => ui.set_channel_settings_error(format!("Couldn't load this channel ({error})").into()),
            }
        });
    });
}

fn show_saved(ui: &AppWindow, value: Value, guild: &str) {
    let name = value["name"].as_str().unwrap_or("").to_owned();
    let topic = value["topic"].as_str().unwrap_or("").to_owned();
    let slowmode = value["rate_limit_per_user"].as_u64().unwrap_or(0).to_string();
    let bitrate = (value["bitrate"].as_u64().unwrap_or(64_000) / 1000).to_string();
    let full_video = value["video_quality_mode"].as_u64() == Some(2);
    let private = is_private(&value, guild);
    ui.set_channel_settings_saved_name(name.as_str().into());
    ui.set_channel_settings_saved_topic(topic.as_str().into());
    ui.set_channel_settings_saved_slowmode(slowmode.as_str().into());
    ui.set_channel_settings_saved_bitrate(bitrate.as_str().into());
    ui.set_channel_settings_saved_full_video(full_video);
    ui.set_channel_settings_saved_private(private);
    *OPEN_CHANNEL.lock().unwrap_or_else(|e| e.into_inner()) = Some(value);
    reset_settings(ui);
}

fn reset_settings(ui: &AppWindow) {
    ui.set_channel_settings_name(ui.get_channel_settings_saved_name());
    ui.set_channel_settings_topic(ui.get_channel_settings_saved_topic());
    ui.set_channel_settings_slowmode(ui.get_channel_settings_saved_slowmode());
    ui.set_channel_settings_bitrate(ui.get_channel_settings_saved_bitrate());
    ui.set_channel_settings_full_video(ui.get_channel_settings_saved_full_video());
    ui.set_channel_settings_private(ui.get_channel_settings_saved_private());
    ui.set_channel_settings_error("".into());
}

fn changed_fields(ui: &AppWindow) -> Result<Map<String, Value>, String> {
    let is_category = ui.get_channel_settings_is_category();
    let is_voice = ui.get_channel_settings_is_voice();
    let kind = if is_category { 4 } else if is_voice { 2 } else { 0 };
    let mut body = Map::new();
    if ui.get_channel_settings_name() != ui.get_channel_settings_saved_name() {
        let name = clean_name(&ui.get_channel_settings_name(), kind).ok_or("Name must be 1 to 100 characters.")?;
        body.insert("name".into(), json!(name));
    }
    if ui.get_channel_settings_topic() != ui.get_channel_settings_saved_topic() {
        body.insert("topic".into(), json!(ui.get_channel_settings_topic().as_str()));
    }
    if ui.get_channel_settings_slowmode() != ui.get_channel_settings_saved_slowmode() {
        let seconds: u32 = ui.get_channel_settings_slowmode().trim().parse().map_err(|_| "Slowmode must be a number.")?;
        if seconds > MAX_SLOWMODE {
            return Err("Slowmode can be at most 21600 seconds.".into());
        }
        body.insert("rate_limit_per_user".into(), json!(seconds));
    }
    if ui.get_channel_settings_bitrate() != ui.get_channel_settings_saved_bitrate() {
        let kbps: u32 = ui.get_channel_settings_bitrate().trim().parse().map_err(|_| "Bitrate must be a number.")?;
        if kbps < 8 {
            return Err("Bitrate must be at least 8 kbps.".into());
        }
        body.insert("bitrate".into(), json!(kbps * 1000));
    }
    if ui.get_channel_settings_full_video() != ui.get_channel_settings_saved_full_video() {
        body.insert("video_quality_mode".into(), json!(if ui.get_channel_settings_full_video() { 2 } else { 1 }));
    }
    Ok(body)
}

fn save_settings(ui: &AppWindow, ctx: &Context) {
    let Some(http) = ctx.client() else { return };
    let body = match changed_fields(ui) {
        Ok(body) => body,
        Err(error) => {
            ui.set_channel_settings_error(error.into());
            return;
        }
    };
    let id = ui.get_channel_settings_id().to_string();
    let guild = ctx.guild();
    let private = ui.get_channel_settings_private();
    let private_changed = private != ui.get_channel_settings_saved_private();
    let before = OPEN_CHANNEL.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or(Value::Null);
    let me = gateway::get_my_user_id().to_string();
    ui.set_channel_settings_busy(true);
    ui.set_channel_settings_error("".into());
    let (app, ctx) = (ui.as_weak(), ctx.clone());
    tokio::spawn(async move {
        let result: Result<Value, String> = async {
            if private_changed {
                // The account's own access is added first, so hiding the channel cannot lock it out.
                if private {
                    let own = overwrite(&before, &me);
                    let allow = own.map_or(0, |o| bits(&o["allow"])) | VIEW_CHANNEL;
                    let deny = own.map_or(0, |o| bits(&o["deny"])) & !VIEW_CHANNEL;
                    let rule = json!({ "type": 1, "allow": allow.to_string(), "deny": deny.to_string() });
                    http.discord_call(Method::PUT, &format!("/channels/{id}/permissions/{me}"), Some(rule)).await?;
                }
                let current = overwrite(&before, &guild);
                let (allow, deny) = with_view(
                    current.map_or(0, |o| bits(&o["allow"])),
                    current.map_or(0, |o| bits(&o["deny"])),
                    private,
                );
                let path = format!("/channels/{id}/permissions/{guild}");
                if allow == 0 && deny == 0 {
                    http.discord_call(Method::DELETE, &path, None).await?;
                } else {
                    let rule = json!({ "type": 0, "allow": allow.to_string(), "deny": deny.to_string() });
                    http.discord_call(Method::PUT, &path, Some(rule)).await?;
                }
            }
            if !body.is_empty() {
                http.discord_call(Method::PATCH, &format!("/channels/{id}"), Some(Value::Object(body))).await?;
            }
            http.discord_call(Method::GET, &format!("/channels/{id}"), None).await
        }
        .await;
        let done = ctx.clone();
        on_ui(&app, move |ui| {
            ui.set_channel_settings_busy(false);
            match result {
                Ok(value) => {
                    if let Some(data) = to_channel_data(&value) {
                        ui.set_channel_settings_title(crate::utils::styled_text::plain_display_name(&data.name).as_ref().into());
                        change_list(ui, &done, &guild, |list| {
                            if let Some(slot) = list.iter_mut().find(|c| c.id == data.id) {
                                *slot = data;
                            }
                        });
                    }
                    show_saved(ui, value, &guild);
                }
                Err(error) => ui.set_channel_settings_error(format!("Couldn't save ({error})").into()),
            }
        });
    });
}

fn invite_rows(body: &Value) -> Vec<ChannelInfoRow> {
    body.as_array()
        .into_iter()
        .flatten()
        .filter_map(|invite| {
            let code = invite["code"].as_str()?;
            let by = invite["inviter"]["global_name"].as_str().or_else(|| invite["inviter"]["username"].as_str()).unwrap_or("Unknown");
            let uses = invite["uses"].as_u64().unwrap_or(0);
            Some(ChannelInfoRow {
                id: code.into(),
                title: code.into(),
                detail: format!("Created by {by}, used {uses} times").into(),
                can_remove: true,
            })
        })
        .collect()
}

fn webhook_rows(body: &Value) -> Vec<ChannelInfoRow> {
    body.as_array()
        .into_iter()
        .flatten()
        .filter_map(|hook| {
            Some(ChannelInfoRow {
                id: hook["id"].as_str()?.into(),
                title: hook["name"].as_str().unwrap_or("Webhook").into(),
                detail: "Webhook".into(),
                can_remove: false,
            })
        })
        .collect()
}

fn permission_rows(channel: &Value, roles: &Value, guild: &str) -> Vec<ChannelInfoRow> {
    let role_name = |id: &str| {
        if id == guild {
            return "@everyone".to_owned();
        }
        roles.as_array().into_iter().flatten().find(|r| r["id"].as_str() == Some(id)).and_then(|r| r["name"].as_str()).unwrap_or("Role").to_owned()
    };
    channel["permission_overwrites"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|rule| {
            let id = rule["id"].as_str()?;
            let title = if rule["type"].as_u64() == Some(1) {
                crate::utils::styled_text::plain_display_name(&gateway::get_user_name(id.parse().unwrap_or(0))).into_owned()
            } else {
                role_name(id)
            };
            let view = if bits(&rule["deny"]) & VIEW_CHANNEL != 0 {
                "Can't view this channel"
            } else if bits(&rule["allow"]) & VIEW_CHANNEL != 0 {
                "Can view this channel"
            } else {
                "Custom permissions"
            };
            Some(ChannelInfoRow { id: id.into(), title: title.into(), detail: view.into(), can_remove: false })
        })
        .collect()
}

fn pick_tab(ui: &AppWindow, ctx: &Context, tab: &str) {
    ui.set_channel_settings_tab(tab.into());
    ui.set_channel_settings_rows(ModelRc::default());
    ui.set_channel_settings_status("".into());
    if tab == "overview" {
        return;
    }
    let Some(http) = ctx.client() else { return };
    ui.set_channel_settings_status("Loading...".into());
    let id = ui.get_channel_settings_id().to_string();
    let guild = ctx.guild();
    let channel = OPEN_CHANNEL.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or(Value::Null);
    let (app, tab) = (ui.as_weak(), tab.to_owned());
    tokio::spawn(async move {
        let rows: Result<Vec<ChannelInfoRow>, String> = match tab.as_str() {
            "invites" => http.discord_call(Method::GET, &format!("/channels/{id}/invites"), None).await.map(|v| invite_rows(&v)),
            "integrations" => http.discord_call(Method::GET, &format!("/channels/{id}/webhooks"), None).await.map(|v| webhook_rows(&v)),
            _ => http.discord_call(Method::GET, &format!("/guilds/{guild}/roles"), None).await.map(|roles| permission_rows(&channel, &roles, &guild)),
        };
        send_rows(&app, tab, rows);
    });
}

fn send_rows(app: &slint::Weak<AppWindow>, tab: String, rows: Result<Vec<ChannelInfoRow>, String>) {
    let rows = rows.map(|list| list.into_iter().map(|row| (row.id.to_string(), row.title.to_string(), row.detail.to_string(), row.can_remove)).collect::<Vec<_>>());
    on_ui(app, move |ui| {
        if ui.get_channel_settings_tab() != tab.as_str() {
            return;
        }
        match rows {
            Ok(list) => {
                ui.set_channel_settings_status(if list.is_empty() { "Nothing here yet.".into() } else { "".into() });
                let rows: Vec<ChannelInfoRow> = list
                    .into_iter()
                    .map(|(id, title, detail, can_remove)| ChannelInfoRow { id: id.into(), title: title.into(), detail: detail.into(), can_remove })
                    .collect();
                ui.set_channel_settings_rows(ModelRc::new(VecModel::from(rows)));
            }
            Err(error) => {
                ui.set_channel_settings_status("".into());
                ui.set_channel_settings_error(format!("Couldn't load this page ({error})").into());
            }
        }
    });
}

fn create_channel(ui: &AppWindow, ctx: &Context) {
    let Some(http) = ctx.client() else { return };
    let kind = ui.get_create_channel_kind().max(0) as u64;
    let Some(name) = clean_name(&ui.get_create_channel_name(), kind) else {
        ui.set_create_channel_error("Name must be 1 to 100 characters.".into());
        return;
    };
    let guild = ctx.guild();
    let body = create_body(&name, kind, &ui.get_create_channel_parent(), ui.get_create_channel_private(), &guild, gateway::get_my_user_id());
    ui.set_create_channel_busy(true);
    ui.set_create_channel_error("".into());
    let (app, ctx) = (ui.as_weak(), ctx.clone());
    tokio::spawn(async move {
        let result = http.discord_call(Method::POST, &format!("/guilds/{guild}/channels"), Some(body)).await;
        on_ui(&app, move |ui| {
            ui.set_create_channel_busy(false);
            match result {
                Ok(value) => {
                    if let Some(data) = to_channel_data(&value) {
                        change_list(ui, &ctx, &guild, |list| list.push(data));
                    }
                    ui.set_create_channel_open(false);
                }
                Err(error) => ui.set_create_channel_error(format!("Couldn't create the channel ({error})").into()),
            }
        });
    });
}

fn run_confirmed(ui: &AppWindow, ctx: &Context) {
    let action = ui.get_channel_confirm_action().to_string();
    let target = ui.get_channel_confirm_id().to_string();
    ui.set_channel_confirm_text("".into());
    let Some(http) = ctx.client() else { return };
    let guild = ctx.guild();
    let (app, ctx) = (ui.as_weak(), ctx.clone());
    tokio::spawn(async move {
        if action == "revoke" {
            let result = http.discord_call(Method::DELETE, &format!("/invites/{target}"), None).await;
            on_ui(&app, move |ui| match result {
                Ok(_) => {
                    let kept: Vec<ChannelInfoRow> = ui.get_channel_settings_rows().iter().filter(|row| row.id != target.as_str()).collect();
                    ui.set_channel_settings_rows(ModelRc::new(VecModel::from(kept)));
                }
                Err(error) => ui.set_channel_settings_error(format!("Couldn't revoke the invite ({error})").into()),
            });
            return;
        }
        let result = http.discord_call(Method::DELETE, &format!("/channels/{target}"), None).await;
        on_ui(&app, move |ui| match result {
            Ok(_) => {
                change_list(ui, &ctx, &guild, |list| list.retain(|c| c.id != target));
                ui.set_channel_settings_open(false);
                if ui.get_active_channel_id() == target.as_str() {
                    ui.invoke_select_guild(guild.as_str().into());
                }
            }
            Err(error) => {
                if ui.get_channel_settings_open() {
                    ui.set_channel_settings_error(format!("Couldn't delete ({error})").into());
                } else {
                    ui.set_titlebar_error(format!("Couldn't delete the channel ({error})").into());
                }
            }
        });
    });
}

fn ask_delete(ui: &AppWindow, ctx: &Context, id: &str) {
    let Some(channel) = ctx.channel(id) else { return };
    let plain = crate::utils::styled_text::plain_display_name(&channel.name);
    let warning = if channel.is_category {
        format!("Delete the category {plain}? Its channels stay in the server. This can't be undone.")
    } else {
        format!("Delete #{plain}? Its messages will be gone. This can't be undone.")
    };
    ui.set_channel_confirm_action("delete".into());
    ui.set_channel_confirm_id(id.into());
    ui.set_channel_confirm_text(warning.into());
}

fn act(ui: &AppWindow, ctx: &Context, name: &str, id: &str, x: f32, y: f32) {
    if name != "menu" {
        ui.set_channel_menu_open(false);
    }
    match name {
        "menu" => {
            let Some(channel) = ctx.channel(id) else { return };
            ui.set_channel_menu_id(id.into());
            ui.set_channel_menu_is_voice(channel.is_voice);
            ui.set_channel_menu_is_category(channel.is_category);
            ui.set_channel_menu_in_server(ctx.guild() != "@me");
            ui.set_channel_menu_muted(MUTED.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|set| set.contains(id)));
            ui.set_channel_menu_x(x);
            ui.set_channel_menu_y(y);
            ui.set_channel_menu_open(true);
        }
        "open_chat" => open_voice_chat(ui, ctx, id),
        "invite" => open_invite(ui, ctx, id),
        "settings" => open_settings(ui, ctx, id),
        "mark_read" => mark_read(ui, ctx, id),
        "mute" => toggle_mute(ui, ctx, id),
        "copy_id" => {
            copy_text(id.to_owned());
        }
        "copy_link" => {
            copy_text(format!("https://discord.com/channels/{}/{id}", ctx.guild()));
        }
        "create" => {
            let category = ctx.channel(id).filter(|c| c.is_category);
            ui.set_create_channel_parent(category.as_ref().map_or("", |c| c.id.as_str()).into());
            ui.set_create_channel_category(category.as_ref().map(|c| crate::utils::styled_text::plain_display_name(&c.name).into_owned()).unwrap_or_default().into());
            ui.set_create_channel_kind(0);
            ui.set_create_channel_name("".into());
            ui.set_create_channel_private(false);
            ui.set_create_channel_error("".into());
            ui.set_create_channel_open(true);
        }
        "create_submit" => create_channel(ui, ctx),
        "copy_invite" => {
            let copied = copy_text(ui.get_invite_link().to_string());
            ui.set_invite_status(if copied { "Copied!".into() } else { "Clipboard unavailable.".into() });
        }
        "ask_delete" => ask_delete(ui, ctx, id),
        "ask_revoke" => {
            ui.set_channel_confirm_action("revoke".into());
            ui.set_channel_confirm_id(id.into());
            ui.set_channel_confirm_text(format!("Revoke the invite {id}? The link will stop working.").into());
        }
        "confirm" => run_confirmed(ui, ctx),
        "tab" => pick_tab(ui, ctx, id),
        "save" => save_settings(ui, ctx),
        "reset" => reset_settings(ui),
        _ => {}
    }
}

pub fn install(ui: &AppWindow, ctx: Context) {
    let app = ui.as_weak();
    ui.on_channel_action(move |name, id, x, y| {
        if let Some(ui) = app.upgrade() {
            act(&ui, &ctx, name.as_str(), id.as_str(), x, y);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_names_follow_discord() {
        assert_eq!(clean_name("  My New Channel ", 0).unwrap(), "my-new-channel");
        assert_eq!(clean_name("Lounge Two", 2).unwrap(), "Lounge Two");
        assert!(clean_name("   ", 0).is_none());
        assert!(clean_name(&"a".repeat(101), 0).is_none());
    }

    #[test]
    fn private_channels_keep_the_creator_in() {
        let body = create_body("x", 0, "5", true, "9", 7);
        assert_eq!(body["parent_id"], "5");
        let rules = body["permission_overwrites"].as_array().unwrap();
        assert_eq!(bits(&rules[0]["deny"]), VIEW_CHANNEL);
        assert_eq!(rules[1]["id"], "7");
        assert_eq!(bits(&rules[1]["allow"]), VIEW_CHANNEL);
    }

    #[test]
    fn privacy_changes_only_the_view_bit() {
        let other = 1 << 11;
        assert_eq!(with_view(other | VIEW_CHANNEL, other, true), (other, other | VIEW_CHANNEL));
        assert_eq!(with_view(other, other | VIEW_CHANNEL, false), (other, other));
    }

    #[test]
    fn detects_a_hidden_channel() {
        let hidden = json!({ "permission_overwrites": [{ "id": "9", "type": 0, "allow": "0", "deny": VIEW_CHANNEL.to_string() }] });
        assert!(is_private(&hidden, "9"));
        assert!(!is_private(&json!({ "permission_overwrites": [] }), "9"));
    }

    #[test]
    fn reads_voice_and_category_channels() {
        let voice = to_channel_data(&json!({ "id": "1", "name": "v", "type": 2, "parent_id": "3", "position": 4 })).unwrap();
        assert!(voice.is_voice && !voice.is_category);
        assert_eq!(voice.parent_id.as_deref(), Some("3"));
        assert!(to_channel_data(&json!({ "id": "2", "name": "c", "type": 4 })).unwrap().is_category);
    }
}
