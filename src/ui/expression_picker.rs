//! The emoji and sticker tabs: server rail, Frequently Used, search, preview and locks.
//!
//! Lists are virtualized and images load only for the rows on screen, so a large account
//! costs little memory. Locked items are shown but cannot be sent.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use reqwest::Method;
use slint::{ComponentHandle, Image, Model, ModelRc, VecModel};

use crate::http::DiscordHttpClient;
use crate::ui::chat_actions::{next_field, read_field_varint};
use crate::utils::emoji_cache;
use crate::{AppWindow, EmojiPickerItem, EmojiRow, GifItem, GifRow, PickerRail};

const EMOJIS_PER_ROW: usize = 9;
const STICKERS_PER_ROW: usize = 3;
const MAX_CUSTOM: usize = 1500;
const MAX_FREQUENT: usize = 27;
const MAX_FREQUENT_STICKERS: usize = 12;
const MAX_STICKERS: usize = 120;
const SETTINGS_EMOJI_FRECENCY: u64 = 6;
const SETTINGS_STICKER_FRECENCY: u64 = 4;

static FREQUENT_EMOJIS: Mutex<Vec<String>> = Mutex::new(Vec::new());
static FREQUENT_STICKERS: Mutex<Vec<String>> = Mutex::new(Vec::new());
static STICKER_REQUEST: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq)]
struct Sticker {
    id: String,
    name: String,
    guild: String,
    extension: &'static str,
}

static SHOWN_STICKERS: Mutex<Vec<Sticker>> = Mutex::new(Vec::new());

thread_local! {
    static BASE_EMOJIS: RefCell<Option<Vec<EmojiPickerItem>>> = const { RefCell::new(None) };
}

/// Keys of a frecency map, most used first. String keys for emoji, fixed64 for stickers.
fn parse_frecency(settings: &[u8], section_field: u64) -> Vec<String> {
    let mut found: Vec<(String, u64, u64)> = Vec::new();
    let mut at = 0;
    while at < settings.len() {
        let Some((number, _, payload)) = next_field(settings, &mut at) else { break };
        let (true, Some(section)) = (number == section_field, payload) else { continue };
        let mut inner = 0;
        while inner < section.len() {
            let Some((field, _, entry)) = next_field(section, &mut inner) else { break };
            let (1, Some(entry)) = (field, entry) else { continue };
            let (mut key, mut score, mut uses, mut cursor) = (String::new(), 0, 0, 0);
            while cursor < entry.len() {
                let start = cursor;
                let Some((part, _, value)) = next_field(entry, &mut cursor) else { break };
                match (part, value) {
                    (1, Some(text)) => key = String::from_utf8_lossy(text).into_owned(),
                    (1, None) if cursor - start == 9 => {
                        if let Ok(bytes) = <[u8; 8]>::try_from(&entry[start + 1..cursor]) {
                            key = u64::from_le_bytes(bytes).to_string();
                        }
                    }
                    (2, Some(item)) => {
                        uses = read_field_varint(item, 1).unwrap_or(0);
                        score = read_field_varint(item, 4).unwrap_or(0);
                    }
                    _ => {}
                }
            }
            if !key.is_empty() {
                found.push((key, score, uses));
            }
        }
    }
    found.sort_by_key(|(_, score, uses)| std::cmp::Reverse((*score, *uses)));
    found.into_iter().map(|(key, _, _)| key).collect()
}

fn base_emojis(ui: &AppWindow) -> Vec<EmojiPickerItem> {
    BASE_EMOJIS.with_borrow_mut(|slot| {
        slot.get_or_insert_with(|| {
            ui.get_emoji_rows()
                .iter()
                .flat_map(|row| row.emojis.iter().collect::<Vec<_>>())
                .filter(|item| !item.is_custom)
                .map(|mut item| {
                    if let Some((_, _, _, id)) = crate::gateway::find_next_unicode_emoji(&item.code) {
                        item.emoji_id = id.into();
                    }
                    item
                })
                .collect()
        })
        .clone()
    })
}

fn custom_item(emoji: &emoji_cache::CustomEmojiEntry, active_guild: &str, has_plan: bool) -> EmojiPickerItem {
    let cache = emoji_cache::get_emoji_cache();
    EmojiPickerItem {
        code: format!("<{}:{}:{}>", if emoji.animated { "a" } else { "" }, emoji.name, emoji.id).into(),
        name: emoji.name.as_str().into(),
        category: "server".into(),
        is_custom: true,
        emoji_id: emoji.id.as_str().into(),
        img: cache.get(&emoji.id).unwrap_or_default(),
        locked: !emoji.available || (!has_plan && (emoji.guild_id != active_guild || emoji.animated)),
    }
}

fn guild_names(ui: &AppWindow) -> HashMap<String, (String, Image, bool)> {
    ui.get_guilds()
        .iter()
        .filter(|guild| !guild.is_folder)
        .map(|guild| (guild.id.to_string(), (guild.name.to_string(), guild.icon_image.clone(), guild.has_image)))
        .collect()
}

fn frequent_items(ui: &AppWindow, customs: &[emoji_cache::CustomEmojiEntry], active: &str, has_plan: bool) -> Vec<EmojiPickerItem> {
    let base = base_emojis(ui);
    let keys = FREQUENT_EMOJIS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    keys.iter()
        .filter_map(|key| {
            if key.chars().all(|c| c.is_ascii_digit()) {
                customs.iter().find(|emoji| emoji.id == *key).map(|emoji| custom_item(emoji, active, has_plan))
            } else {
                base.iter().find(|item| item.code == key.as_str() || item.name.eq_ignore_ascii_case(key)).cloned()
            }
        })
        .take(MAX_FREQUENT)
        .collect()
}

fn push_section(rows: &mut Vec<EmojiRow>, title: &str, items: &[EmojiPickerItem]) {
    rows.push(EmojiRow { header: title.into(), emojis: ModelRc::default() });
    for chunk in items.chunks(EMOJIS_PER_ROW) {
        rows.push(EmojiRow { header: "".into(), emojis: ModelRc::new(VecModel::from(chunk.to_vec())) });
    }
}

fn rail_item(id: &str, name: &str, icon: Image, has_icon: bool, is_recent: bool, row: usize) -> PickerRail {
    PickerRail {
        id: id.into(),
        initial: name.chars().next().unwrap_or('•').to_uppercase().to_string().into(),
        name: name.into(),
        icon,
        has_icon,
        is_recent,
        row: row as i32,
    }
}

/// Rebuilds the emoji tab for a search text.
pub fn build_emoji(ui: &AppWindow, query: &str) {
    let needle = query.trim().to_lowercase();
    let matches = |item: &EmojiPickerItem| needle.is_empty() || item.name.to_lowercase().contains(&needle);
    let active = ui.get_active_guild_id().to_string();
    let has_plan = crate::native_video::sender::premium_type() != 0;
    let mut customs = emoji_cache::get_all_custom_emojis();
    customs.truncate(MAX_CUSTOM);
    let names = guild_names(ui);
    let (mut rows, mut rail) = (Vec::new(), Vec::new());

    if needle.is_empty() {
        let frequent = frequent_items(ui, &customs, &active, has_plan);
        if !frequent.is_empty() {
            rail.push(rail_item("recent", "Frequently Used", Image::default(), false, true, rows.len()));
            push_section(&mut rows, "Frequently Used", &frequent);
        }
    }

    let mut order: Vec<String> = Vec::new();
    for emoji in &customs {
        if !order.contains(&emoji.guild_id) {
            order.push(emoji.guild_id.clone());
        }
    }
    order.sort_by_key(|guild| *guild != active);
    for guild in order {
        let items: Vec<EmojiPickerItem> = customs
            .iter()
            .filter(|emoji| emoji.guild_id == guild)
            .map(|emoji| custom_item(emoji, &active, has_plan))
            .filter(|item| matches(item))
            .collect();
        if items.is_empty() {
            continue;
        }
        let (name, icon, has_icon) = names.get(&guild).cloned().unwrap_or_else(|| ("Server".to_string(), Image::default(), false));
        rail.push(rail_item(&guild, &name, icon, has_icon, false, rows.len()));
        push_section(&mut rows, &name, &items);
    }

    let unicode: Vec<EmojiPickerItem> = base_emojis(ui).into_iter().filter(|item| matches(item)).collect();
    if !unicode.is_empty() {
        rail.push(rail_item("unicode", "Emoji", Image::default(), false, false, rows.len()));
        push_section(&mut rows, "Emoji", &unicode);
    }
    ui.set_emoji_rows(ModelRc::new(VecModel::from(rows)));
    ui.set_emoji_rail(ModelRc::new(VecModel::from(rail)));
}

pub fn open_emoji(ui: &AppWindow) {
    ui.set_emoji_preview_name("".into());
    build_emoji(ui, "");
}

/// A cell came on screen: its image is fetched now.
pub fn load_emoji(ui: &AppWindow, emoji_id: &str) {
    let cache = emoji_cache::get_emoji_cache();
    match cache.get(emoji_id) {
        Some(image) => {
            let (app, id) = (ui.as_weak(), emoji_id.to_owned());
            slint::Timer::single_shot(Duration::ZERO, move || {
                if let Some(ui) = app.upgrade() {
                    crate::ui::gif_picker::emoji_loaded(&ui, &id, &image);
                }
            });
        }
        None => cache.fetch_priority_async(emoji_id, "", ui.as_weak()),
    }
}

/// Reads the account's most used emoji and stickers.
pub fn refresh_frequent(ui: &AppWindow, http: Option<DiscordHttpClient>) {
    let Some(http) = http else { return };
    let app = ui.as_weak();
    tokio::spawn(async move {
        use base64::Engine;
        let Ok(body) = http.discord_call(Method::GET, "/users/@me/settings-proto/2", None).await else { return };
        let Some(settings) = body["settings"].as_str().and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok()) else { return };
        *FREQUENT_EMOJIS.lock().unwrap_or_else(|e| e.into_inner()) = parse_frecency(&settings, SETTINGS_EMOJI_FRECENCY);
        *FREQUENT_STICKERS.lock().unwrap_or_else(|e| e.into_inner()) = parse_frecency(&settings, SETTINGS_STICKER_FRECENCY);
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = app.upgrade() else { return };
            let active = ui.get_active_guild_id().to_string();
            let has_plan = crate::native_video::sender::premium_type() != 0;
            let customs = emoji_cache::get_all_custom_emojis();
            let quick: Vec<(String, String, String)> = frequent_items(&ui, &customs, &active, has_plan)
                .into_iter()
                .filter(|item| !item.locked)
                .take(3)
                .map(|item| {
                    if item.is_custom {
                        (format!("{}:{}", item.name, item.emoji_id), format!(":{}:", item.name), item.emoji_id.to_string())
                    } else {
                        (item.code.to_string(), item.code.to_string(), item.emoji_id.to_string())
                    }
                })
                .collect();
            crate::ui::chat_actions::set_quick_reactions(&ui, quick);
            if ui.get_show_emoji_picker() {
                build_emoji(&ui, "");
            }
        });
    });
}

fn parse_sticker(value: &serde_json::Value) -> Option<Sticker> {
    let format = value["format_type"].as_u64().unwrap_or(1);
    if format == 3 || !value["available"].as_bool().unwrap_or(true) {
        return None;
    }
    Some(Sticker {
        id: value["id"].as_str()?.to_owned(),
        name: value["name"].as_str().unwrap_or("").to_owned(),
        guild: value["guild_id"].as_str().unwrap_or("").to_owned(),
        extension: if format == 4 { "gif" } else { "png" },
    })
}

fn sticker_locked(sticker: &Sticker, active: &str, has_plan: bool) -> bool {
    !sticker.guild.is_empty() && sticker.guild != active && !has_plan
}

fn show_stickers(ui: &AppWindow, list: Vec<Sticker>) {
    let request = STICKER_REQUEST.fetch_add(1, Ordering::SeqCst) + 1;
    let active = ui.get_active_guild_id().to_string();
    let has_plan = crate::native_video::sender::premium_type() != 0;
    ui.set_sticker_empty(list.is_empty());
    let rows: Vec<GifRow> = list
        .chunks(STICKERS_PER_ROW)
        .map(|chunk| {
            let items: Vec<GifItem> = chunk
                .iter()
                .map(|sticker| GifItem {
                    url: sticker.id.as_str().into(),
                    title: sticker.name.as_str().into(),
                    locked: sticker_locked(sticker, &active, has_plan),
                    ..Default::default()
                })
                .collect();
            GifRow { items: ModelRc::new(VecModel::from(items)) }
        })
        .collect();
    ui.set_sticker_rows(ModelRc::new(VecModel::from(rows)));
    let app = ui.as_weak();
    tokio::spawn(async move {
        for sticker in list {
            if STICKER_REQUEST.load(Ordering::SeqCst) != request {
                return;
            }
            let url = format!("https://media.discordapp.net/stickers/{}.{}?size=160", sticker.id, sticker.extension);
            let Some(buffer) = crate::ui::gif_picker::fetch_preview(&url).await else { continue };
            let app = app.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if STICKER_REQUEST.load(Ordering::SeqCst) != request {
                    return;
                }
                let Some(ui) = app.upgrade() else { return };
                let image = Image::from_rgba8(buffer);
                for row in ui.get_sticker_rows().iter() {
                    for index in 0..row.items.row_count() {
                        if let Some(mut item) = row.items.row_data(index).filter(|i| i.url == sticker.id.as_str()) {
                            item.preview = image.clone();
                            item.has_preview = true;
                            row.items.set_row_data(index, item);
                        }
                    }
                }
            });
        }
    });
}

/// Opens stickers of one server, or the frequently used ones with `recent`.
pub fn open_stickers(ui: &AppWindow, http: Option<DiscordHttpClient>, source: Option<&str>) {
    let Some(http) = http else { return };
    let active = ui.get_active_guild_id().to_string();
    let names = guild_names(ui);
    let frequent = FREQUENT_STICKERS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut rail = Vec::new();
    if !frequent.is_empty() {
        rail.push(rail_item("recent", "Frequently Used", Image::default(), false, true, 0));
    }
    let mut guilds: Vec<(String, (String, Image, bool))> = names.into_iter().collect();
    guilds.sort_by_key(|(id, (name, _, _))| (*id != active, name.to_lowercase()));
    for (id, (name, icon, has_icon)) in guilds {
        rail.push(rail_item(&id, &name, icon, has_icon, false, 0));
    }
    ui.set_sticker_rail(ModelRc::new(VecModel::from(rail)));
    let default = if !frequent.is_empty() { "recent" } else { active.as_str() };
    let source = source.unwrap_or(default).to_owned();
    ui.set_sticker_source(source.as_str().into());
    ui.set_sticker_preview_name("".into());
    ui.set_sticker_rows(ModelRc::new(VecModel::<GifRow>::default()));
    ui.set_sticker_empty(false);
    if source == "@me" {
        ui.set_sticker_empty(true);
        return;
    }
    let app = ui.as_weak();
    tokio::spawn(async move {
        let mut list: Vec<Sticker> = Vec::new();
        if source == "recent" {
            for id in frequent.iter().take(MAX_FREQUENT_STICKERS) {
                if let Ok(body) = http.discord_call(Method::GET, &format!("/stickers/{id}"), None).await {
                    list.extend(parse_sticker(&body));
                }
            }
        } else if let Ok(body) = http.discord_call(Method::GET, &format!("/guilds/{source}/stickers"), None).await {
            list = body.as_array().into_iter().flatten().filter_map(parse_sticker).take(MAX_STICKERS).collect();
            for sticker in &mut list {
                if sticker.guild.is_empty() {
                    sticker.guild = source.clone();
                }
            }
        }
        *SHOWN_STICKERS.lock().unwrap_or_else(|e| e.into_inner()) = list.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = app.upgrade() {
                if ui.get_sticker_source() == source.as_str() {
                    show_stickers(&ui, list);
                }
            }
        });
    });
}

pub fn search_stickers(ui: &AppWindow, query: &str) {
    let needle = query.trim().to_lowercase();
    let list: Vec<Sticker> = SHOWN_STICKERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|sticker| needle.is_empty() || sticker.name.to_lowercase().contains(&needle))
        .cloned()
        .collect();
    show_stickers(ui, list);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varint(out: &mut Vec<u8>, mut value: u64) {
        while value >= 0x80 {
            out.push(value as u8 | 0x80);
            value >>= 7;
        }
        out.push(value as u8);
    }

    fn bytes(out: &mut Vec<u8>, field: u64, data: &[u8]) {
        varint(out, field << 3 | 2);
        varint(out, data.len() as u64);
        out.extend_from_slice(data);
    }

    fn item(uses: u64, score: u64) -> Vec<u8> {
        let mut out = Vec::new();
        varint(&mut out, 1 << 3);
        varint(&mut out, uses);
        varint(&mut out, 4 << 3);
        varint(&mut out, score);
        out
    }

    #[test]
    fn emoji_frecency_is_sorted_by_score() {
        let (mut section, mut settings) = (Vec::new(), Vec::new());
        for (key, score) in [("low", 1), ("high", 90), ("mid", 40)] {
            let mut entry = Vec::new();
            bytes(&mut entry, 1, key.as_bytes());
            bytes(&mut entry, 2, &item(1, score));
            bytes(&mut section, 1, &entry);
        }
        bytes(&mut settings, SETTINGS_EMOJI_FRECENCY, &section);
        assert_eq!(parse_frecency(&settings, SETTINGS_EMOJI_FRECENCY), ["high", "mid", "low"]);
    }

    #[test]
    fn sticker_frecency_reads_numeric_keys() {
        let (mut entry, mut section, mut settings) = (Vec::new(), Vec::new(), Vec::new());
        varint(&mut entry, 1 << 3 | 1);
        entry.extend_from_slice(&123456789u64.to_le_bytes());
        bytes(&mut entry, 2, &item(3, 7));
        bytes(&mut section, 1, &entry);
        bytes(&mut settings, SETTINGS_STICKER_FRECENCY, &section);
        assert_eq!(parse_frecency(&settings, SETTINGS_STICKER_FRECENCY), ["123456789"]);
    }

    #[test]
    fn other_server_stickers_need_a_plan() {
        let sticker = Sticker { id: "1".into(), name: "a".into(), guild: "2".into(), extension: "png" };
        assert!(sticker_locked(&sticker, "9", false));
        assert!(!sticker_locked(&sticker, "2", false));
        assert!(!sticker_locked(&sticker, "9", true));
    }

    #[test]
    fn animated_lottie_stickers_are_left_out() {
        assert!(parse_sticker(&serde_json::json!({ "id": "1", "format_type": 3 })).is_none());
        assert_eq!(parse_sticker(&serde_json::json!({ "id": "1", "format_type": 4 })).unwrap().extension, "gif");
    }
}
