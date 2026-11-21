//! What the chat can do to a message: reactions, reply, edit, forward, pin, delete and
//! favorite GIFs, plus `@` autofill. Destructive actions are confirmed in the interface.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use reqwest::Method;
use serde_json::{json, Value};
use slint::{ComponentHandle, Image, Model, ModelRc, VecModel};

use crate::http::DiscordHttpClient;
use crate::utils::emoji_cache;
use crate::{AppWindow, ForwardTarget, MentionItem, MessageReaction};

const MAX_MESSAGES: usize = 4000;
const QUICK_REACTIONS: [&str; 3] = ["😭", "❤️", "👍"];
const MAX_MENTIONS: usize = 8;
const MAX_FORWARD_TARGETS: usize = 80;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Reaction {
    key: String,
    label: String,
    emoji_id: String,
    count: i32,
    me: bool,
}

#[derive(Default)]
struct Store {
    reactions: HashMap<String, Vec<Reaction>>,
    raw: HashMap<String, (String, bool)>,
    gif_pages: HashMap<String, String>,
    favorites: HashSet<String>,
    mentions: HashMap<String, String>,
}

static STORE: Mutex<Option<Store>> = Mutex::new(None);
static APP: Mutex<Option<slint::Weak<AppWindow>>> = Mutex::new(None);

fn with_store<R>(f: impl FnOnce(&mut Store) -> R) -> R {
    let mut guard = STORE.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(Store::default))
}

fn parse_emoji(emoji: &Value) -> Option<(String, String, String)> {
    let name = emoji["name"].as_str()?;
    match emoji["id"].as_str() {
        Some(id) => Some((format!("{name}:{id}"), format!(":{name}:"), id.to_owned())),
        None => {
            let id = crate::gateway::find_next_unicode_emoji(name).map(|(_, _, _, id)| id).unwrap_or_default();
            Some((name.to_owned(), name.to_owned(), id))
        }
    }
}

fn parse_reactions(message: &Value) -> Vec<Reaction> {
    message["reactions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let (key, label, emoji_id) = parse_emoji(&item["emoji"])?;
            Some(Reaction {
                key,
                label,
                emoji_id,
                count: item["count"].as_i64().unwrap_or(1) as i32,
                me: item["me"].as_bool().unwrap_or(false),
            })
        })
        .collect()
}

/// Remembers what a message's actions need. Called wherever a message is parsed.
pub fn remember(message: &Value) {
    let Some(id) = message["id"].as_str() else { return };
    with_store(|store| {
        if store.raw.len() >= MAX_MESSAGES {
            store.raw.clear();
            store.reactions.clear();
        }
        if let Some(content) = message["content"].as_str() {
            store.raw.insert(id.to_owned(), (content.to_owned(), message["pinned"].as_bool().unwrap_or(false)));
        }
        if message.get("reactions").is_some() {
            store.reactions.insert(id.to_owned(), parse_reactions(message));
        }
    });
}

pub fn remember_gif_page(animation: &str, page: &str) {
    with_store(|store| {
        store.gif_pages.insert(animation.to_owned(), page.to_owned());
    });
}

fn favorite_key(store: &Store, url: &str) -> String {
    store.gif_pages.get(url).cloned().unwrap_or_else(|| url.to_owned())
}

pub fn is_favorite(url: &str) -> bool {
    with_store(|store| {
        let key = favorite_key(store, url);
        store.favorites.contains(&key)
    })
}

pub fn set_favorites(urls: impl IntoIterator<Item = String>) {
    with_store(|store| store.favorites = urls.into_iter().collect());
}

fn image_for(emoji_id: &str, app: &slint::Weak<AppWindow>) -> Option<Image> {
    if emoji_id.is_empty() {
        return None;
    }
    let cache = emoji_cache::get_emoji_cache();
    let image = cache.get(emoji_id);
    if image.is_none() {
        cache.fetch_priority_async(emoji_id, "", app.clone());
    }
    image
}

fn row(reaction: &Reaction, app: &slint::Weak<AppWindow>) -> MessageReaction {
    let image = image_for(&reaction.emoji_id, app);
    MessageReaction {
        key: reaction.key.as_str().into(),
        label: reaction.label.as_str().into(),
        emoji_id: reaction.emoji_id.as_str().into(),
        has_img: image.is_some(),
        img: image.unwrap_or_default(),
        count: reaction.count,
        me: reaction.me,
    }
}

/// The reaction pills of a message.
pub fn rows(message_id: &str) -> ModelRc<MessageReaction> {
    let list = with_store(|store| store.reactions.get(message_id).cloned().unwrap_or_default());
    if list.is_empty() {
        return ModelRc::default();
    }
    let app = APP.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_default();
    ModelRc::new(VecModel::from(list.iter().map(|r| row(r, &app)).collect::<Vec<_>>()))
}

fn refresh_message(ui: &AppWindow, message_id: &str) {
    let messages = ui.get_messages();
    for index in 0..messages.row_count() {
        if let Some(mut message) = messages.row_data(index).filter(|m| m.id == message_id) {
            message.reactions = rows(message_id);
            messages.set_row_data(index, message);
            break;
        }
    }
}

fn apply_event(list: &mut Vec<Reaction>, event: &str, data: &Value, my_id: &str) {
    match event {
        "MESSAGE_REACTION_REMOVE_ALL" => list.clear(),
        "MESSAGE_REACTION_REMOVE_EMOJI" => {
            if let Some((key, _, _)) = parse_emoji(&data["emoji"]) {
                list.retain(|r| r.key != key);
            }
        }
        "MESSAGE_REACTION_ADD" | "MESSAGE_REACTION_REMOVE" => {
            let Some((key, label, emoji_id)) = parse_emoji(&data["emoji"]) else { return };
            let mine = data["user_id"].as_str() == Some(my_id);
            if event == "MESSAGE_REACTION_ADD" {
                match list.iter_mut().find(|r| r.key == key) {
                    Some(reaction) => {
                        reaction.count += 1;
                        reaction.me |= mine;
                    }
                    None => list.push(Reaction { key, label, emoji_id, count: 1, me: mine }),
                }
            } else if let Some(reaction) = list.iter_mut().find(|r| r.key == key) {
                reaction.count -= 1;
                if mine {
                    reaction.me = false;
                }
                list.retain(|r| r.count > 0);
            }
        }
        _ => {}
    }
}

/// A reaction changed on Discord.
pub fn on_reaction_event(event: &str, data: &Value, my_id: &str) {
    let Some(id) = data["message_id"].as_str().map(str::to_owned) else { return };
    with_store(|store| apply_event(store.reactions.entry(id.clone()).or_default(), event, data, my_id));
    let app = APP.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(app) = app {
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = app.upgrade() {
                refresh_message(&ui, &id);
            }
        });
    }
}

fn quick_rows(app: &slint::Weak<AppWindow>) -> Vec<MessageReaction> {
    QUICK_REACTIONS
        .iter()
        .map(|emoji| {
            let id = crate::gateway::find_next_unicode_emoji(emoji).map(|(_, _, _, id)| id).unwrap_or_default();
            row(&Reaction { key: (*emoji).into(), label: (*emoji).into(), emoji_id: id, count: 0, me: false }, app)
        })
        .collect()
}

pub fn install(ui: &AppWindow) {
    *APP.lock().unwrap_or_else(|e| e.into_inner()) = Some(ui.as_weak());
    ui.set_quick_reactions(ModelRc::new(VecModel::from(quick_rows(&ui.as_weak()))));
}

/// An emoji image arrived: shows it on the toolbar and on the messages using it.
pub fn emoji_loaded(ui: &AppWindow, emoji_id: &str, image: &Image) {
    let quick = ui.get_quick_reactions();
    for index in 0..quick.row_count() {
        if let Some(mut item) = quick.row_data(index).filter(|item| item.emoji_id == emoji_id) {
            item.img = image.clone();
            item.has_img = true;
            quick.set_row_data(index, item);
        }
    }
    let ids: Vec<String> = with_store(|store| {
        store.reactions.iter().filter(|(_, list)| list.iter().any(|r| r.emoji_id == emoji_id)).map(|(id, _)| id.clone()).collect()
    });
    for id in ids {
        refresh_message(ui, &id);
    }
}

fn encode_path(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' | b':' => (byte as char).to_string(),
            other => format!("%{other:02X}"),
        })
        .collect()
}

fn show_error(app: &slint::Weak<AppWindow>, text: String) {
    let app = app.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = app.upgrade() {
            ui.set_titlebar_error(text.into());
        }
    });
}

fn call(http: DiscordHttpClient, app: slint::Weak<AppWindow>, method: Method, path: String, body: Option<Value>) {
    tokio::spawn(async move {
        if let Err(error) = http.discord_call(method, &path, body).await {
            log::warn!("Ação de mensagem falhou: {}", error);
            show_error(&app, format!("Discord refused that action ({error})"));
        }
    });
}

fn react(ui: &AppWindow, http: DiscordHttpClient, channel: &str, message: &str, key: &str) {
    let mine = with_store(|store| store.reactions.get(message).is_some_and(|list| list.iter().any(|r| r.key == key && r.me)));
    let method = if mine { Method::DELETE } else { Method::PUT };
    let path = format!("/channels/{channel}/messages/{message}/reactions/{}/@me", encode_path(key));
    call(http, ui.as_weak(), method, path, None);
}

fn picked_emoji_key(code: &str) -> String {
    let inner = code.trim().trim_start_matches('<').trim_end_matches('>');
    let inner = inner.strip_prefix("a:").or_else(|| inner.strip_prefix(':'));
    match inner {
        Some(custom) if code.trim().starts_with('<') => custom.to_owned(),
        _ => code.trim().to_owned(),
    }
}

fn copy(ui: &AppWindow, text: String) {
    let copied = copypasta::ClipboardContext::new().and_then(|mut clipboard| copypasta::ClipboardProvider::set_contents(&mut clipboard, text));
    if copied.is_err() {
        ui.set_titlebar_error("Clipboard unavailable".into());
    }
}

const MANAGE_MESSAGES: u64 = 1 << 13;

fn can_manage(guild: &str) -> bool {
    guild != "@me" && crate::channel_permissions::cached(guild).is_some_and(|p| p.has_permission(MANAGE_MESSAGES))
}

/// Everything the toolbar and its menu can do.
pub fn action(ui: &AppWindow, http: Option<DiscordHttpClient>, name: &str, message: &str, extra: &str, x: f32, y: f32) {
    let channel = ui.get_active_channel_id().to_string();
    let guild = ui.get_active_guild_id().to_string();
    if name == "favorite" {
        if let Some(http) = http {
            toggle_favorite(ui, http, message.to_owned());
        }
        return;
    }
    let raw = with_store(|store| store.raw.get(message).cloned()).unwrap_or_default();
    match name {
        "react" => {
            if let Some(http) = http {
                react(ui, http, &channel, message, extra);
            }
        }
        "add_reaction" => {
            ui.set_reaction_target_id(message.into());
            ui.invoke_open_expression("emoji".into());
        }
        "react_picked" => {
            ui.set_reaction_target_id("".into());
            if let Some(http) = http {
                react(ui, http, &channel, message, &picked_emoji_key(extra));
            }
        }
        "reply" => {
            let author = ui.get_messages().iter().find(|m| m.id == message).map(|m| m.author).unwrap_or_default();
            ui.set_editing_message_id("".into());
            ui.set_reply_to_id(message.into());
            ui.set_reply_to_name(author);
        }
        "edit" => {
            ui.set_reply_to_id("".into());
            ui.set_editing_message_id(message.into());
            ui.set_chat_input_text(raw.0.into());
        }
        "forward" => {
            let targets: Vec<ForwardTarget> = ui
                .get_channels()
                .iter()
                .filter(|c| !c.is_voice && !c.is_category && !c.is_voice_member)
                .take(MAX_FORWARD_TARGETS)
                .map(|c| ForwardTarget { name: format!("{}{}", if guild == "@me" { "@" } else { "#" }, c.name).into(), id: c.id })
                .collect();
            ui.set_forward_targets(ModelRc::new(VecModel::from(targets)));
            ui.set_forward_message_id(message.into());
        }
        "forward_to" => {
            let target = extra.to_owned();
            ui.set_forward_message_id("".into());
            if let Some(http) = http {
                let mut reference = json!({ "type": 1, "message_id": message, "channel_id": channel });
                if guild != "@me" {
                    reference["guild_id"] = json!(guild);
                }
                call(http, ui.as_weak(), Method::POST, format!("/channels/{target}/messages"), Some(json!({ "message_reference": reference })));
            }
        }
        "more" => {
            let own = ui.get_messages().iter().find(|m| m.id == message).is_some_and(|m| m.is_own);
            ui.set_message_menu_x(x);
            ui.set_message_menu_y(y);
            ui.set_message_menu_own(own);
            ui.set_message_menu_pinned(raw.1);
            ui.set_message_menu_can_manage(can_manage(&guild));
            ui.set_message_menu_id(message.into());
        }
        "copy_text" => {
            ui.set_message_menu_id("".into());
            copy(ui, raw.0);
        }
        "copy_link" => {
            ui.set_message_menu_id("".into());
            copy(ui, format!("https://discord.com/channels/{guild}/{channel}/{message}"));
        }
        "copy_id" => {
            ui.set_message_menu_id("".into());
            copy(ui, message.to_owned());
        }
        "pin" => {
            ui.set_message_menu_id("".into());
            with_store(|store| {
                if let Some(entry) = store.raw.get_mut(message) {
                    entry.1 = true;
                }
            });
            if let Some(http) = http {
                call(http, ui.as_weak(), Method::PUT, format!("/channels/{channel}/pins/{message}"), None);
            }
        }
        "ask_unpin" | "ask_delete" => {
            let delete = name == "ask_delete";
            ui.set_message_menu_id("".into());
            ui.set_confirm_action(if delete { "delete" } else { "unpin" }.into());
            ui.set_confirm_message_id(message.into());
            ui.set_confirm_text(if delete { "Delete this message? This can't be undone." } else { "Unpin this message?" }.into());
        }
        "confirm" => {
            let what = ui.get_confirm_action().to_string();
            let target = ui.get_confirm_message_id().to_string();
            ui.set_confirm_text("".into());
            if let Some(http) = http {
                if what == "delete" {
                    call(http, ui.as_weak(), Method::DELETE, format!("/channels/{channel}/messages/{target}"), None);
                } else if what == "unpin" {
                    with_store(|store| {
                        if let Some(entry) = store.raw.get_mut(&target) {
                            entry.1 = false;
                        }
                    });
                    call(http, ui.as_weak(), Method::DELETE, format!("/channels/{channel}/pins/{target}"), None);
                }
            }
        }
        _ => {}
    }
}

/// Sends an edit or reply. False when the text is an ordinary message.
pub fn send_special(ui: &AppWindow, http: Option<DiscordHttpClient>, channel: &str, content: &str) -> bool {
    let editing = ui.get_editing_message_id().to_string();
    let reply = ui.get_reply_to_id().to_string();
    if editing.is_empty() && reply.is_empty() {
        return false;
    }
    ui.set_editing_message_id("".into());
    ui.set_reply_to_id("".into());
    let Some(http) = http else { return true };
    if !editing.is_empty() {
        call(http, ui.as_weak(), Method::PATCH, format!("/channels/{channel}/messages/{editing}"), Some(json!({ "content": content })));
    } else {
        let body = json!({ "content": content, "message_reference": { "message_id": reply, "channel_id": channel } });
        call(http, ui.as_weak(), Method::POST, format!("/channels/{channel}/messages"), Some(body));
    }
    true
}

/// The text after an `@` being typed at the end of the message.
fn mention_query(text: &str) -> Option<&str> {
    let at = text.rfind('@')?;
    let query = &text[at + 1..];
    let starts_word = at == 0 || text[..at].ends_with(char::is_whitespace);
    (starts_word && !query.contains(char::is_whitespace)).then_some(query)
}

pub fn mention_input(ui: &AppWindow, text: &str) {
    let found = if ui.get_active_guild_id() == "@me" { None } else { mention_query(text) };
    let Some(query) = found else {
        ui.set_show_mentions(false);
        return;
    };
    let items: Vec<MentionItem> = crate::ui::member_list::search(query, MAX_MENTIONS)
        .into_iter()
        .map(|(id, name, subtitle)| MentionItem {
            user_id: id.into(),
            avatar_text: name.chars().next().unwrap_or('?').to_uppercase().to_string().into(),
            name: name.into(),
            subtitle: subtitle.into(),
        })
        .collect();
    ui.set_mention_index(0);
    ui.set_show_mentions(!items.is_empty());
    ui.set_mention_items(ModelRc::new(VecModel::from(items)));
}

pub fn pick_mention(ui: &AppWindow, index: i32) {
    let Some(item) = ui.get_mention_items().row_data(index.max(0) as usize) else { return };
    let text = ui.get_chat_input_text().to_string();
    let Some(at) = text.rfind('@') else { return };
    with_store(|store| {
        store.mentions.insert(item.name.to_string(), item.user_id.to_string());
    });
    ui.set_chat_input_text(format!("{}@{} ", &text[..at], item.name).into());
    ui.set_show_mentions(false);
}

fn replace_mentions(content: &str, names: &HashMap<String, String>) -> String {
    let mut ordered: Vec<(&String, &String)> = names.iter().collect();
    ordered.sort_by_key(|(name, _)| std::cmp::Reverse(name.len()));
    let mut out = content.to_owned();
    for (name, id) in ordered {
        out = out.replace(&format!("@{name}"), &format!("<@{id}>"));
    }
    out
}

/// `@Name` picked from the list becomes a real mention when sent.
pub fn encode_mentions(content: &str) -> String {
    with_store(|store| {
        let out = replace_mentions(content, &store.mentions);
        store.mentions.clear();
        out
    })
}

fn read_varint(buf: &[u8], at: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *buf.get(*at)?;
        *at += 1;
        value |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

fn write_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn write_bytes(out: &mut Vec<u8>, field: u64, bytes: &[u8]) {
    write_varint(out, field << 3 | 2);
    write_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

/// One field of a message: number, its raw bytes with the key, and its payload if it has one.
fn next_field<'a>(buf: &'a [u8], at: &mut usize) -> Option<(u64, &'a [u8], Option<&'a [u8]>)> {
    let start = *at;
    let key = read_varint(buf, at)?;
    let payload = match key & 7 {
        0 => {
            read_varint(buf, at)?;
            None
        }
        1 => {
            *at = at.checked_add(8)?;
            None
        }
        2 => {
            let len = usize::try_from(read_varint(buf, at)?).ok()?;
            let begin = *at;
            *at = begin.checked_add(len)?;
            Some(buf.get(begin..*at)?)
        }
        5 => {
            *at = at.checked_add(4)?;
            None
        }
        _ => return None,
    };
    Some((key >> 3, buf.get(start..*at)?, payload))
}

fn entry_info(entry: &[u8]) -> Option<(String, u64)> {
    let (mut at, mut url, mut order) = (0, String::new(), 0);
    while at < entry.len() {
        let (number, _, payload) = next_field(entry, &mut at)?;
        match (number, payload) {
            (1, Some(text)) => url = String::from_utf8_lossy(text).into_owned(),
            (2, Some(gif)) => {
                let mut inner = 0;
                while inner < gif.len() {
                    let start = inner;
                    let (field, _, _) = next_field(gif, &mut inner)?;
                    if field == 5 {
                        let mut value = start;
                        read_varint(gif, &mut value)?;
                        order = read_varint(gif, &mut value)?;
                    }
                }
            }
            _ => {}
        }
    }
    Some((url, order))
}

/// Adds or removes one favorite and keeps every other byte of the settings as it was.
/// `None` when the settings cannot be read, so nothing is written.
fn edit_favorites(settings: &[u8], url: &str, src: &str) -> Option<(Vec<u8>, bool)> {
    let (mut before, mut after, mut kept) = (Vec::new(), Vec::new(), Vec::new());
    let (mut section_other, mut seen_section, mut removed, mut max_order) = (Vec::new(), false, false, 0u64);
    let mut at = 0;
    while at < settings.len() {
        let (number, raw, payload) = next_field(settings, &mut at)?;
        if number == 2 {
            let section = payload?;
            seen_section = true;
            let mut inner = 0;
            while inner < section.len() {
                let (field, entry_raw, entry) = next_field(section, &mut inner)?;
                if field == 1 {
                    let (entry_url, order) = entry_info(entry?)?;
                    max_order = max_order.max(order);
                    if entry_url == url {
                        removed = true;
                    } else {
                        kept.extend_from_slice(entry_raw);
                    }
                } else {
                    section_other.extend_from_slice(entry_raw);
                }
            }
        } else if seen_section {
            after.extend_from_slice(raw);
        } else {
            before.extend_from_slice(raw);
        }
    }
    if !removed {
        let mut gif = Vec::new();
        write_varint(&mut gif, 1 << 3);
        write_varint(&mut gif, 1);
        write_bytes(&mut gif, 2, src.as_bytes());
        write_varint(&mut gif, 5 << 3);
        write_varint(&mut gif, max_order + 1);
        let mut entry = Vec::new();
        write_bytes(&mut entry, 1, url.as_bytes());
        write_bytes(&mut entry, 2, &gif);
        write_bytes(&mut kept, 1, &entry);
    }
    kept.extend_from_slice(&section_other);
    let mut out = before;
    write_bytes(&mut out, 2, &kept);
    out.extend_from_slice(&after);
    Some((out, !removed))
}

fn toggle_favorite(ui: &AppWindow, http: DiscordHttpClient, animation: String) {
    let app = ui.as_weak();
    let key = with_store(|store| favorite_key(store, &animation));
    tokio::spawn(async move {
        use base64::Engine;
        let engine = base64::engine::general_purpose::STANDARD;
        let result: Result<bool, String> = async {
            let body = http.discord_call(Method::GET, "/users/@me/settings-proto/2", None).await?;
            let current = engine.decode(body["settings"].as_str().ok_or("no settings")?).map_err(|e| e.to_string())?;
            let (updated, added) = edit_favorites(&current, &key, &animation).ok_or("settings could not be read")?;
            http.discord_call(Method::PATCH, "/users/@me/settings-proto/2", Some(json!({ "settings": engine.encode(updated) }))).await?;
            Ok(added)
        }
        .await;
        match result {
            Ok(added) => {
                with_store(|store| {
                    if added {
                        store.favorites.insert(key.clone());
                    } else {
                        store.favorites.remove(&key);
                    }
                });
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = app.upgrade() else { return };
                    let messages = ui.get_messages();
                    for index in 0..messages.row_count() {
                        let Some(message) = messages.row_data(index) else { continue };
                        for at in 0..message.attachments.row_count() {
                            if let Some(mut attachment) = message.attachments.row_data(at).filter(|a| a.url == animation.as_str()) {
                                attachment.is_favorite = added;
                                message.attachments.set_row_data(at, attachment);
                            }
                        }
                    }
                });
            }
            Err(error) => show_error(&app, format!("Couldn't update favorites ({error})")),
        }
    });
}

/// Loads the hovered stream's preview from Discord.
pub fn stream_hover(ui: &AppWindow, http: Option<DiscordHttpClient>, user_id: &str) {
    ui.set_stream_preview_has_image(false);
    let (Some(http), guild, channel) = (http, crate::gateway::get_my_voice_guild_id(), crate::gateway::get_guild_voice_states_store().lock().ok().and_then(|m| m.get(&user_id.parse::<u64>().unwrap_or(0)).cloned()).unwrap_or_default()) else { return };
    if channel.is_empty() || guild.is_empty() {
        return;
    }
    let key = format!("guild:{guild}:{channel}:{user_id}");
    let wanted = user_id.to_owned();
    let app = ui.as_weak();
    tokio::spawn(async move {
        let Ok(body) = http.discord_call(Method::GET, &format!("/streams/{}/preview", encode_path(&key)), None).await else { return };
        let Some(url) = body["url"].as_str().filter(|url| url.starts_with("https://")) else { return };
        let Some(buffer) = crate::ui::gif_picker::fetch_preview(url).await else { return };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = app.upgrade() {
                if ui.get_stream_preview_uid() == wanted.as_str() {
                    ui.set_stream_preview_image(Image::from_rgba8(buffer));
                    ui.set_stream_preview_has_image(true);
                }
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn favorite(url: &str, src: &str, order: u64) -> Vec<u8> {
        let mut gif = Vec::new();
        write_varint(&mut gif, 1 << 3);
        write_varint(&mut gif, 2);
        write_bytes(&mut gif, 2, src.as_bytes());
        write_varint(&mut gif, 5 << 3);
        write_varint(&mut gif, order);
        let mut entry = Vec::new();
        write_bytes(&mut entry, 1, url.as_bytes());
        write_bytes(&mut entry, 2, &gif);
        let mut out = Vec::new();
        write_bytes(&mut out, 1, &entry);
        out
    }

    fn settings(section: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        write_bytes(&mut out, 1, b"before");
        write_bytes(&mut out, 2, section);
        write_bytes(&mut out, 9, b"unknown after");
        out
    }

    #[test]
    fn adding_a_favorite_keeps_everything_else() {
        let current = settings(&favorite("https://tenor.com/a", "https://media.tenor.com/a.mp4", 7));
        let (updated, added) = edit_favorites(&current, "https://tenor.com/b", "https://media.tenor.com/b.gif").unwrap();
        assert!(added);
        let urls: Vec<String> = crate::ui::gif_picker::parse_favorite_gifs(&updated).into_iter().map(|g| g.url).collect();
        assert_eq!(urls, ["https://tenor.com/b", "https://tenor.com/a"]);
        assert!(updated.windows(6).any(|w| w == b"before"));
        assert!(updated.windows(13).any(|w| w == b"unknown after"));
    }

    #[test]
    fn removing_a_favorite_leaves_the_others_byte_for_byte() {
        let keep = favorite("https://tenor.com/a", "https://media.tenor.com/a.mp4", 1);
        let mut section = keep.clone();
        section.extend(favorite("https://tenor.com/b", "https://media.tenor.com/b.gif", 2));
        let (updated, added) = edit_favorites(&settings(&section), "https://tenor.com/b", "x").unwrap();
        assert!(!added);
        assert_eq!(updated, settings(&keep));
    }

    #[test]
    fn unreadable_settings_are_never_rewritten() {
        assert!(edit_favorites(&[0x12, 0xFF], "https://tenor.com/a", "x").is_none());
    }

    #[test]
    fn at_starts_a_mention_only_at_a_word() {
        assert_eq!(mention_query("hi @ma"), Some("ma"));
        assert_eq!(mention_query("@"), Some(""));
        assert_eq!(mention_query("me@mail"), None);
        assert_eq!(mention_query("@name done"), None);
    }

    #[test]
    fn picked_names_become_mentions() {
        let names = HashMap::from([("Ana".to_string(), "1".to_string()), ("Ana Clara".to_string(), "2".to_string())]);
        assert_eq!(replace_mentions("oi @Ana Clara e @Ana", &names), "oi <@2> e <@1>");
    }

    #[test]
    fn custom_emoji_codes_become_reaction_keys() {
        assert_eq!(picked_emoji_key("<:cat:123>"), "cat:123");
        assert_eq!(picked_emoji_key("<a:cat:123>"), "cat:123");
        assert_eq!(picked_emoji_key("😭"), "😭");
    }

    #[test]
    fn reaction_events_update_counts_and_my_state() {
        let mut list = Vec::new();
        let emoji = json!({ "name": "😭", "id": null });
        apply_event(&mut list, "MESSAGE_REACTION_ADD", &json!({ "emoji": emoji, "user_id": "me" }), "me");
        apply_event(&mut list, "MESSAGE_REACTION_ADD", &json!({ "emoji": emoji, "user_id": "you" }), "me");
        assert_eq!((list[0].count, list[0].me), (2, true));
        apply_event(&mut list, "MESSAGE_REACTION_REMOVE", &json!({ "emoji": emoji, "user_id": "me" }), "me");
        assert_eq!((list[0].count, list[0].me), (1, false));
        apply_event(&mut list, "MESSAGE_REACTION_REMOVE", &json!({ "emoji": emoji, "user_id": "you" }), "me");
        assert!(list.is_empty());
    }
}
