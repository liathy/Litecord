//! The server's member list and user profiles.
//!
//! The list is the one the official client shows: the gateway sends the rows of the
//! ranges this client subscribed to, so a huge server costs only the rows on screen.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use serde_json::Value;
use slint::{Color, ComponentHandle, Model, ModelRc, VecModel};
use tokio::sync::mpsc;

use crate::gateway::GatewayCommand;
use crate::http::DiscordHttpClient;
use crate::utils::avatar_cache;
use crate::{AppWindow, MemberRow, ProfileRole};

const RANGE_SIZE: usize = 100;
const MAX_ROWS: usize = 2000;
const MAX_PROFILE_ROLES: usize = 12;
const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;
const DEFAULT_ACCENT: u32 = 0x5865F2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Role {
    pub id: String,
    pub name: String,
    pub color: u32,
    pub position: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub id: String,
    pub name: String,
    pub avatar_url: String,
    pub status: String,
    pub role_ids: Vec<String>,
    pub is_bot: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    Group { id: String, count: u64 },
    Member(Member),
}

fn display_name(user: &Value, nick: Option<&str>) -> String {
    nick.filter(|n| !n.is_empty())
        .or_else(|| user["global_name"].as_str().filter(|n| !n.is_empty()))
        .or_else(|| user["username"].as_str())
        .unwrap_or("?")
        .to_owned()
}

fn avatar_url(user_id: &str, hash: &str) -> String {
    if user_id.is_empty() || !crate::ui::message_grouping::valid_avatar_hash(hash) {
        return String::new();
    }
    format!("https://cdn.discordapp.com/avatars/{user_id}/{hash}.webp?size=64")
}

fn parse_item(item: &Value) -> Option<Entry> {
    if let Some(group) = item.get("group") {
        return Some(Entry::Group {
            id: group["id"].as_str()?.to_owned(),
            count: group["count"].as_u64().unwrap_or(0),
        });
    }
    let member = item.get("member")?;
    let user = &member["user"];
    let id = user["id"].as_str()?.to_owned();
    Some(Entry::Member(Member {
        name: display_name(user, member["nick"].as_str()),
        avatar_url: avatar_url(&id, user["avatar"].as_str().unwrap_or("")),
        status: member["presence"]["status"].as_str().unwrap_or("offline").to_owned(),
        role_ids: member["roles"]
            .as_array()
            .map(|roles| roles.iter().filter_map(|r| r.as_str().map(str::to_owned)).collect())
            .unwrap_or_default(),
        is_bot: user["bot"].as_bool().unwrap_or(false),
        id,
    }))
}

/// The rows of one channel's list; rows not received yet are `None`.
#[derive(Default)]
pub struct MemberRows {
    entries: Vec<Option<Entry>>,
    /// Discord's id of the list on screen. Channels with the same permissions share one.
    list_id: String,
}

impl MemberRows {
    fn set(&mut self, index: usize, entry: Option<Entry>) {
        if index >= MAX_ROWS {
            return;
        }
        if index >= self.entries.len() {
            self.entries.resize(index + 1, None);
        }
        self.entries[index] = entry;
    }

    /// Applies a GUILD_MEMBER_LIST_UPDATE.
    pub fn apply(&mut self, update: &Value) {
        let list_id = update["id"].as_str().unwrap_or("");
        if list_id != self.list_id {
            // A full list replaces this one; anything else is the previous channel's list.
            let has_rows = update["ops"].as_array().into_iter().flatten().any(|op| op["op"] == "SYNC");
            if !has_rows {
                return;
            }
            self.entries.clear();
            self.list_id = list_id.to_owned();
        }
        for op in update["ops"].as_array().into_iter().flatten() {
            let index = op["index"].as_u64().map(|i| i as usize);
            let start = op["range"][0].as_u64().unwrap_or(0) as usize;
            let end = op["range"][1].as_u64().unwrap_or(0) as usize;
            match op["op"].as_str().unwrap_or("") {
                "SYNC" => {
                    for (offset, item) in op["items"].as_array().into_iter().flatten().enumerate() {
                        self.set(start + offset, parse_item(item));
                    }
                }
                "INVALIDATE" => {
                    for index in start..=end.min(self.entries.len().saturating_sub(1)) {
                        if let Some(row) = self.entries.get_mut(index) {
                            *row = None;
                        }
                    }
                }
                "INSERT" => {
                    if let Some(index) = index.filter(|i| *i < MAX_ROWS) {
                        if index > self.entries.len() {
                            self.entries.resize(index, None);
                        }
                        self.entries.insert(index, parse_item(&op["item"]));
                        self.entries.truncate(MAX_ROWS);
                    }
                }
                "UPDATE" => {
                    if let Some(index) = index {
                        self.set(index, parse_item(&op["item"]));
                    }
                }
                "DELETE" => {
                    if let Some(index) = index.filter(|i| *i < self.entries.len()) {
                        self.entries.remove(index);
                    }
                }
                _ => {}
            }
        }
        if let Some(groups) = update["groups"].as_array() {
            let total: u64 = groups
                .iter()
                .map(|g| g["count"].as_u64().unwrap_or(0))
                .filter(|count| *count > 0)
                .map(|count| count + 1)
                .sum();
            self.entries.resize((total as usize).min(MAX_ROWS), None);
        }
    }
}

/// The color of a member's name: their highest role that has one.
fn name_color(role_ids: &[String], roles: &HashMap<String, Role>) -> Option<u32> {
    role_ids
        .iter()
        .filter_map(|id| roles.get(id))
        .filter(|role| role.color != 0)
        .max_by_key(|role| role.position)
        .map(|role| role.color)
}

fn rgb(color: u32) -> Color {
    Color::from_rgb_u8((color >> 16) as u8, (color >> 8) as u8, color as u8)
}

fn status_color(status: &str) -> Color {
    rgb(match status {
        "online" => 0x23A55A,
        "idle" => 0xF0B232,
        "dnd" => 0xF23F43,
        _ => 0x80848E,
    })
}

fn initials(name: &str) -> String {
    name.chars().next().unwrap_or('?').to_uppercase().collect()
}

/// When an account or server was created, from its id.
fn snowflake_date(id: &str) -> String {
    id.parse::<u64>()
        .ok()
        .and_then(|id| chrono::DateTime::from_timestamp((((id >> 22) + DISCORD_EPOCH_MS) / 1000) as i64, 0))
        .map(|date| date.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Profile {
    pub id: String,
    pub display_name: String,
    pub username: String,
    pub pronouns: String,
    pub bio: String,
    pub member_since: String,
    pub avatar_url: String,
    pub accent: Option<u32>,
    pub role_ids: Vec<String>,
    pub mutual: String,
}

/// Reads `/users/{id}/profile`, or a plain user object. The server's profile wins.
pub fn parse_profile(body: &Value) -> Option<Profile> {
    let user = if body["user"].is_object() { &body["user"] } else { body };
    let id = user["id"].as_str()?.to_owned();
    let member = &body["guild_member"];
    let server = &body["guild_member_profile"];
    let global = &body["user_profile"];
    let text = |key: &str| {
        server[key]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| global[key].as_str().filter(|s| !s.is_empty()))
            .or_else(|| user[key].as_str())
            .unwrap_or("")
            .to_owned()
    };
    Some(Profile {
        display_name: display_name(user, member["nick"].as_str()),
        username: format!("@{}", user["username"].as_str().unwrap_or("")),
        pronouns: text("pronouns"),
        bio: text("bio"),
        member_since: snowflake_date(&id),
        avatar_url: avatar_url(&id, user["avatar"].as_str().unwrap_or("")),
        accent: global["accent_color"]
            .as_u64()
            .or_else(|| user["accent_color"].as_u64())
            .map(|c| c as u32),
        role_ids: member["roles"]
            .as_array()
            .map(|roles| roles.iter().filter_map(|r| r.as_str().map(str::to_owned)).collect())
            .unwrap_or_default(),
        mutual: mutual_text(body),
        id,
    })
}

fn mutual_text(body: &Value) -> String {
    let friends = body["mutual_friends"].as_array().map(|list| list.len() as u64).or_else(|| body["mutual_friends_count"].as_u64()).unwrap_or(0);
    let servers = body["mutual_guilds"].as_array().map_or(0, |list| list.len());
    let plural = |count: u64, name: &str| format!("{count} Mutual {name}{}", if count == 1 { "" } else { "s" });
    let mut parts = Vec::new();
    if friends > 0 {
        parts.push(plural(friends, "Friend"));
    }
    if servers > 0 {
        parts.push(plural(servers as u64, "Server"));
    }
    parts.join(" · ")
}

/// Server members whose name matches, for `@` autofill: the loaded list, then known names.
pub fn search(query: &str, limit: usize) -> Vec<(String, String, String)> {
    let needle = query.to_lowercase();
    let mut found: Vec<(String, String, String)> = with_state(|state| {
        state.rows.entries.iter().flatten().filter_map(|entry| match entry {
            Entry::Member(member) if member.name.to_lowercase().contains(&needle) => {
                Some((member.id.clone(), crate::utils::styled_text::plain_display_name(&member.name).to_string(), String::new()))
            }
            _ => None,
        }).take(limit).collect()
    });
    if found.len() < limit {
        if let Ok(names) = crate::gateway::get_user_names_store().lock() {
            for (id, name) in names.iter() {
                if found.len() >= limit {
                    break;
                }
                let id = id.to_string();
                if name.to_lowercase().contains(&needle) && !found.iter().any(|(known, _, _)| *known == id) {
                    found.push((id, crate::utils::styled_text::plain_display_name(name).to_string(), String::new()));
                }
            }
        }
    }
    found.sort_by_key(|(_, name, _)| !name.to_lowercase().starts_with(&needle));
    found
}

#[derive(Default)]
struct State {
    guild: String,
    channel: String,
    window: usize,
    rows: MemberRows,
    roles: HashMap<String, Role>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
static RENDER_PENDING: AtomicBool = AtomicBool::new(false);
static PROFILE_REQUEST: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static MODEL: RefCell<Option<Rc<VecModel<MemberRow>>>> = const { RefCell::new(None) };
}

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> R {
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(State::default))
}

fn ranges(window: usize) -> Vec<[usize; 2]> {
    let mut ranges = vec![[0, RANGE_SIZE - 1]];
    if window > 0 {
        ranges.push([window, window + RANGE_SIZE - 1]);
    }
    ranges
}

fn row_for(entry: &Option<Entry>, roles: &HashMap<String, Role>) -> MemberRow {
    let text = crate::utils::i18n_ui::ui_text();
    match entry {
        None => MemberRow::default(),
        Some(Entry::Group { id, count }) => {
            let name = match id.as_str() {
                "online" => text.members_online.to_string(),
                "offline" => text.members_offline.to_string(),
                _ => roles.get(id).map(|r| r.name.clone()).unwrap_or_else(|| text.members_group.to_string()),
            };
            MemberRow {
                is_group: true,
                is_loaded: true,
                label: format!("{} — {}", name.to_uppercase(), count).into(),
                ..Default::default()
            }
        }
        Some(Entry::Member(member)) => {
            let color = name_color(&member.role_ids, roles);
            MemberRow {
                is_loaded: true,
                label: crate::utils::styled_text::plain_display_name(&member.name).as_ref().into(),
                user_id: member.id.as_str().into(),
                avatar_text: initials(&member.name).into(),
                avatar_slot: avatar_cache::get_avatar_cache().slot_for(&member.avatar_url),
                avatar_url: member.avatar_url.as_str().into(),
                status_color: status_color(&member.status),
                name_color: color.map(rgb).unwrap_or_default(),
                has_name_color: color.is_some(),
                is_bot: member.is_bot,
                ..Default::default()
            }
        }
    }
}

/// Only the rows that changed are touched, so a busy server does not redraw the list.
fn render(ui: &AppWindow) {
    let rows: Vec<MemberRow> = with_state(|state| {
        state.rows.entries.iter().map(|entry| row_for(entry, &state.roles)).collect()
    });
    let model = MODEL.with_borrow_mut(|slot| {
        slot.get_or_insert_with(|| {
            let model = Rc::new(VecModel::default());
            ui.set_member_rows(ModelRc::from(model.clone()));
            model
        })
        .clone()
    });
    for (index, row) in rows.iter().enumerate() {
        if index >= model.row_count() {
            model.push(row.clone());
        } else if model.row_data(index).as_ref() != Some(row) {
            model.set_row_data(index, row.clone());
        }
    }
    while model.row_count() > rows.len() {
        model.remove(model.row_count() - 1);
    }
}

fn schedule_render(app: slint::Weak<AppWindow>) {
    if RENDER_PENDING.swap(true, Ordering::AcqRel) {
        return;
    }
    let _ = slint::invoke_from_event_loop(move || {
        RENDER_PENDING.store(false, Ordering::Release);
        if let Some(ui) = app.upgrade() {
            render(&ui);
        }
    });
}

fn subscribe(tx: &Option<mpsc::Sender<GatewayCommand>>, guild_id: &str, channel_id: &str, window: usize) {
    if let Some(tx) = tx {
        let _ = tx.try_send(GatewayCommand::SubscribeMembers {
            guild_id: guild_id.to_owned(),
            channel_id: channel_id.to_owned(),
            ranges: ranges(window),
        });
    }
}

/// The open channel or the panel changed: follows it, and stops listening otherwise.
pub fn context_changed(
    ui: &AppWindow,
    http: Option<DiscordHttpClient>,
    tx: Option<mpsc::Sender<GatewayCommand>>,
) {
    let guild = ui.get_active_guild_id().to_string();
    let channel = ui.get_active_channel_id().to_string();
    let wanted = ui.get_show_member_list() && guild != "@me" && !guild.is_empty() && !channel.is_empty();

    let (previous, same) = with_state(|state| (state.guild.clone(), wanted && state.guild == guild && state.channel == channel));
    if same {
        return;
    }
    if !previous.is_empty() && (!wanted || previous != guild) {
        subscribe(&tx, &previous, "", 0);
    }
    let keep_roles = wanted && previous == guild;
    with_state(|state| {
        let roles = std::mem::take(&mut state.roles);
        let rows = std::mem::take(&mut state.rows);
        *state = State::default();
        if keep_roles {
            state.roles = roles;
            // Discord sends no new rows when the next channel shares this list.
            state.rows = rows;
        }
        if wanted {
            state.guild = guild.clone();
            state.channel = channel.clone();
        }
    });
    render(ui);
    if !wanted {
        return;
    }
    subscribe(&tx, &guild, &channel, 0);
    if !keep_roles {
        if let Some(http) = http {
            let app = ui.as_weak();
            tokio::spawn(async move {
                if let Ok(roles) = http.get_guild_roles(&guild).await {
                    let applied = with_state(|state| {
                        let current = state.guild == guild;
                        if current {
                            state.roles = roles.into_iter().map(|r| (r.id.clone(), r)).collect();
                        }
                        current
                    });
                    if applied {
                        schedule_render(app);
                    }
                }
            });
        }
    }
}

/// After a new gateway session the subscription has to be sent again.
pub fn forget_subscription() {
    with_state(|state| {
        state.guild.clear();
        state.channel.clear();
    });
}

pub fn on_update(app: slint::Weak<AppWindow>, guild_id: &str, update: &Value) {
    let applied = with_state(|state| {
        let current = state.guild == guild_id;
        if current {
            state.rows.apply(update);
        }
        current
    });
    if applied {
        schedule_render(app);
    }
}

/// Asks for the rows around the first one on screen.
pub fn scrolled(first_row: usize, tx: Option<mpsc::Sender<GatewayCommand>>) {
    let window = first_row / RANGE_SIZE * RANGE_SIZE;
    let target = with_state(|state| {
        if state.guild.is_empty() || state.window == window {
            return None;
        }
        state.window = window;
        Some((state.guild.clone(), state.channel.clone()))
    });
    if let Some((guild, channel)) = target {
        subscribe(&tx, &guild, &channel, window);
    }
}

fn show_profile(ui: &AppWindow, profile: &Profile, roles: &HashMap<String, Role>) {
    ui.set_profile_name(crate::utils::styled_text::plain_display_name(&profile.display_name).as_ref().into());
    ui.set_profile_username(profile.username.as_str().into());
    ui.set_profile_pronouns(profile.pronouns.as_str().into());
    ui.set_profile_bio(profile.bio.as_str().into());
    ui.set_profile_member_since(profile.member_since.as_str().into());
    ui.set_profile_avatar_text(initials(&profile.display_name).into());
    let image = avatar_cache::get_avatar_cache().image_for(&profile.avatar_url);
    ui.set_profile_has_avatar(image.size().width > 0);
    ui.set_profile_avatar(image);
    ui.set_profile_accent(rgb(profile.accent.filter(|c| *c != 0).unwrap_or(DEFAULT_ACCENT)));
    ui.set_profile_mutual(profile.mutual.as_str().into());

    let mut held: Vec<&Role> = profile.role_ids.iter().filter_map(|id| roles.get(id)).collect();
    held.sort_by_key(|role| std::cmp::Reverse(role.position));
    let items: Vec<ProfileRole> = held
        .into_iter()
        .take(MAX_PROFILE_ROLES)
        .map(|role| ProfileRole {
            name: role.name.as_str().into(),
            color: rgb(if role.color == 0 { 0x80848E } else { role.color }),
        })
        .collect();
    ui.set_profile_roles(ModelRc::new(VecModel::from(items)));
}

pub fn open_profile(ui: &AppWindow, http: Option<DiscordHttpClient>, user_id: String) {
    let Some(http) = http else { return };
    let request = PROFILE_REQUEST.fetch_add(1, Ordering::SeqCst) + 1;
    let known = crate::gateway::get_user_name(user_id.parse().unwrap_or(0));
    show_profile(ui, &Profile { display_name: known, ..Default::default() }, &HashMap::new());
    ui.set_profile_user_id(user_id.as_str().into());
    ui.set_profile_loading(true);
    ui.set_show_profile(true);

    let guild = ui.get_active_guild_id().to_string();
    let app = ui.as_weak();
    tokio::spawn(async move {
        let guild = (guild != "@me" && !guild.is_empty()).then_some(guild);
        let body = http.get_full_profile(&user_id, guild.as_deref()).await;
        let profile = body.ok().as_ref().and_then(parse_profile);
        let roles = with_state(|state| state.roles.clone());
        let _ = slint::invoke_from_event_loop(move || {
            if PROFILE_REQUEST.load(Ordering::SeqCst) != request {
                return;
            }
            if let Some(ui) = app.upgrade() {
                ui.set_profile_loading(false);
                if let Some(profile) = profile {
                    show_profile(&ui, &profile, &roles);
                }
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn member(id: &str) -> Value {
        json!({ "member": { "user": { "id": id, "username": id }, "roles": [], "presence": { "status": "online" } } })
    }

    fn ids(rows: &MemberRows) -> Vec<String> {
        rows.entries
            .iter()
            .map(|e| match e {
                Some(Entry::Member(m)) => m.id.clone(),
                Some(Entry::Group { id, .. }) => format!("#{id}"),
                None => "-".to_string(),
            })
            .collect()
    }

    #[test]
    fn sync_places_rows_and_leaves_room_for_the_rest() {
        let mut rows = MemberRows::default();
        rows.apply(&json!({
            "groups": [{ "id": "online", "count": 3 }, { "id": "offline", "count": 0 }],
            "ops": [{ "op": "SYNC", "range": [0, 99], "items": [
                { "group": { "id": "online", "count": 3 } }, member("a"), member("b")
            ]}]
        }));
        assert_eq!(ids(&rows), ["#online", "a", "b", "-"]);
    }

    #[test]
    fn insert_update_delete_and_invalidate_follow_the_server() {
        let mut rows = MemberRows::default();
        rows.apply(&json!({ "ops": [{ "op": "SYNC", "range": [0, 99], "items": [member("a"), member("b")] }] }));
        rows.apply(&json!({ "ops": [{ "op": "INSERT", "index": 1, "item": member("c") }] }));
        assert_eq!(ids(&rows), ["a", "c", "b"]);
        rows.apply(&json!({ "ops": [{ "op": "UPDATE", "index": 0, "item": member("z") }] }));
        rows.apply(&json!({ "ops": [{ "op": "DELETE", "index": 1 }] }));
        assert_eq!(ids(&rows), ["z", "b"]);
        rows.apply(&json!({ "ops": [{ "op": "INVALIDATE", "range": [0, 99] }] }));
        assert_eq!(ids(&rows), ["-", "-"]);
    }

    #[test]
    fn a_new_list_replaces_the_rows_and_the_old_list_is_ignored() {
        let mut rows = MemberRows::default();
        rows.apply(&json!({ "id": "one", "ops": [{ "op": "SYNC", "range": [0, 99], "items": [member("a"), member("b")] }] }));
        rows.apply(&json!({ "id": "two", "ops": [{ "op": "SYNC", "range": [0, 99], "items": [member("c")] }] }));
        assert_eq!(ids(&rows), ["c"]);
        rows.apply(&json!({ "id": "one", "ops": [{ "op": "INVALIDATE", "range": [0, 99] }] }));
        assert_eq!(ids(&rows), ["c"]);
    }

    #[test]
    fn updates_of_the_list_on_screen_still_apply() {
        let mut rows = MemberRows::default();
        rows.apply(&json!({ "id": "one", "ops": [{ "op": "SYNC", "range": [0, 99], "items": [member("a")] }] }));
        rows.apply(&json!({ "id": "one", "ops": [{ "op": "UPDATE", "index": 0, "item": member("z") }] }));
        assert_eq!(ids(&rows), ["z"]);
    }

    #[test]
    fn a_huge_server_is_capped() {
        let mut rows = MemberRows::default();
        rows.apply(&json!({ "groups": [{ "id": "online", "count": 500000 }], "ops": [] }));
        assert_eq!(rows.entries.len(), MAX_ROWS);
        rows.apply(&json!({ "ops": [{ "op": "UPDATE", "index": 900000, "item": member("x") }] }));
        assert_eq!(rows.entries.len(), MAX_ROWS);
    }

    #[test]
    fn the_highest_colored_role_names_the_color() {
        let role = |id: &str, color, position| (id.to_string(), Role { id: id.into(), name: id.into(), color, position });
        let roles = HashMap::from([role("low", 0x111111, 1), role("high", 0x222222, 5), role("plain", 0, 9)]);
        let held = ["low".to_string(), "high".to_string(), "plain".to_string()];
        assert_eq!(name_color(&held, &roles), Some(0x222222));
        assert_eq!(name_color(&["plain".to_string()], &roles), None);
    }

    #[test]
    fn the_server_profile_wins_over_the_global_one() {
        let profile = parse_profile(&json!({
            "user": { "id": "175928847299117063", "username": "ana", "global_name": "Ana", "bio": "global", "accent_color": 255 },
            "user_profile": { "bio": "global", "pronouns": "they/them" },
            "guild_member": { "nick": "Nick", "roles": ["1"] },
            "guild_member_profile": { "bio": "server" }
        })).unwrap();
        assert_eq!(profile.display_name, "Nick");
        assert_eq!(profile.username, "@ana");
        assert_eq!(profile.bio, "server");
        assert_eq!(profile.pronouns, "they/them");
        assert_eq!(profile.accent, Some(255));
        assert_eq!(profile.member_since, "2016-04-30");
    }

    #[test]
    fn scrolling_keeps_the_top_range_subscribed() {
        assert_eq!(ranges(0), vec![[0, 99]]);
        assert_eq!(ranges(300), vec![[0, 99], [300, 399]]);
    }
}
