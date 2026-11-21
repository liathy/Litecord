//! The GIF picker, fed by Discord's own GIF search, and Twemoji in the emoji picker.
//!
//! A GIF is sent as its link, which Discord embeds for everyone. The picker shows one
//! small still frame per result, so browsing costs little memory.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde_json::Value;
use slint::{ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};
use tokio::sync::Semaphore;

use crate::http::DiscordHttpClient;
use crate::utils::emoji_cache;
use crate::{AppWindow, EmojiPickerItem, EmojiRow, GifItem, GifRow};

const MAX_GIFS: usize = 40;
const GIFS_PER_ROW: usize = 2;
const PARALLEL_PREVIEWS: usize = 4;
const MAX_PREVIEW_BYTES: usize = 6 * 1024 * 1024;
const MAX_PREVIEW_SIDE: u32 = 2048;
const PREVIEW_WIDTH: u32 = 192;
const PREVIEW_HEIGHT: u32 = 144;
const FREQUENCY_FAVORITE_GIFS: u64 = 2;
const EMOJIS_PER_ROW: usize = 12;
const MAX_SERVER_EMOJIS: usize = 120;

static REQUEST: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gif {
    pub url: String,
    pub title: String,
    pub preview: String,
}

fn https(value: &Value) -> Option<&str> {
    value.as_str().filter(|url| url.starts_with("https://"))
}

/// Results of Discord's GIF endpoints: a list, or an object holding one in `gifs`.
pub fn parse_gifs(body: &Value) -> Vec<Gif> {
    let list = body.as_array().or_else(|| body["gifs"].as_array());
    list.into_iter()
        .flatten()
        .filter_map(|gif| {
            Some(Gif {
                url: https(&gif["url"])?.to_owned(),
                title: gif["title"].as_str().unwrap_or("").to_owned(),
                preview: https(&gif["preview"])
                    .or_else(|| https(&gif["gif_src"]))
                    .or_else(|| https(&gif["src"]))?
                    .to_owned(),
            })
        })
        .take(MAX_GIFS)
        .collect()
}

/// A still image for a favorite. Tenor stores favorites as video; its small GIF rendition
/// has the same id with another suffix.
fn still_url(src: &str) -> String {
    if src.starts_with("https://media.tenor.com/") && src.ends_with(".mp4") {
        return src.replace("AAAPo/", "AAAAM/").replacen(".mp4", ".gif", 1);
    }
    src.to_owned()
}

/// Favorites in `FrecencyUserSettings`, most recently added first.
pub fn parse_favorite_gifs(settings: &[u8]) -> Vec<Gif> {
    use crate::ui::guild_folders::{read_fields, Field};
    let mut found: Vec<(u64, Gif)> = Vec::new();
    for (number, field) in read_fields(settings).unwrap_or_default() {
        let (FREQUENCY_FAVORITE_GIFS, Field::Bytes(section)) = (number, field) else { continue };
        for (inner, entry) in read_fields(section).unwrap_or_default() {
            let (1, Field::Bytes(entry)) = (inner, entry) else { continue };
            let (mut url, mut src, mut order) = (String::new(), String::new(), 0u64);
            for (key, value) in read_fields(entry).unwrap_or_default() {
                match (key, value) {
                    (1, Field::Bytes(text)) => url = String::from_utf8_lossy(text).into_owned(),
                    (2, Field::Bytes(gif)) => {
                        for (gif_key, gif_value) in read_fields(gif).unwrap_or_default() {
                            match (gif_key, gif_value) {
                                (2, Field::Bytes(text)) => src = String::from_utf8_lossy(text).into_owned(),
                                (5, Field::Varint(n)) => order = n,
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            if url.starts_with("https://") && src.starts_with("https://") {
                found.push((order, Gif { url, title: String::new(), preview: still_url(&src) }));
            }
        }
    }
    found.sort_by_key(|(order, _)| std::cmp::Reverse(*order));
    found.into_iter().map(|(_, gif)| gif).take(MAX_GIFS).collect()
}

/// Custom emojis the account can send here: the open server's, or any server's with a
/// plan that includes them. Animated ones need a plan too.
fn sendable_emojis(
    emojis: Vec<emoji_cache::CustomEmojiEntry>,
    guild_id: &str,
    premium_type: u8,
) -> Vec<emoji_cache::CustomEmojiEntry> {
    let has_plan = premium_type != 0;
    let mut usable: Vec<_> = emojis
        .into_iter()
        .filter(|e| e.available && (has_plan || (e.guild_id == guild_id && !e.animated)))
        .collect();
    usable.sort_by_key(|e| e.guild_id != guild_id);
    usable.truncate(MAX_SERVER_EMOJIS);
    usable
}

fn emoji_code(emoji: &emoji_cache::CustomEmojiEntry) -> String {
    format!("<{}:{}:{}>", if emoji.animated { "a" } else { "" }, emoji.name, emoji.id)
}

fn rows_for(gifs: &[Gif]) -> Vec<GifRow> {
    gifs.chunks(GIFS_PER_ROW)
        .map(|chunk| {
            let items: Vec<GifItem> = chunk
                .iter()
                .map(|gif| GifItem {
                    url: gif.url.as_str().into(),
                    title: gif.title.as_str().into(),
                    ..Default::default()
                })
                .collect();
            GifRow { items: ModelRc::new(VecModel::from(items)) }
        })
        .collect()
}

fn show_preview(ui: &AppWindow, url: &str, preview: Image) {
    let rows = ui.get_gif_rows();
    for row in rows.iter() {
        for index in 0..row.items.row_count() {
            if let Some(mut item) = row.items.row_data(index).filter(|item| item.url == url) {
                item.preview = preview;
                item.has_preview = true;
                row.items.set_row_data(index, item);
                return;
            }
        }
    }
}

pub(crate) async fn fetch_preview(url: &str) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    fetch_image(url, PREVIEW_WIDTH, PREVIEW_HEIGHT).await
}

pub(crate) async fn fetch_image(url: &str, max_width: u32, max_height: u32) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    // Not the Discord client: this request goes to the GIF host and must not carry the token.
    let response = crate::utils::network_settings::shared_http_client().get(url).send().await.ok()?;
    if !response.status().is_success() || response.content_length().unwrap_or(0) > MAX_PREVIEW_BYTES as u64 {
        return None;
    }
    let bytes = response.bytes().await.ok()?;
    if bytes.len() > MAX_PREVIEW_BYTES {
        return None;
    }
    tokio::task::spawn_blocking(move || {
        let decoded = image::load_from_memory(&bytes).ok()?;
        if decoded.width() > MAX_PREVIEW_SIDE || decoded.height() > MAX_PREVIEW_SIDE {
            return None;
        }
        let small = decoded.thumbnail(max_width, max_height).to_rgba8();
        let (width, height) = small.dimensions();
        Some(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(small.as_raw(), width, height))
    })
    .await
    .ok()
    .flatten()
}

fn load_previews(app: slint::Weak<AppWindow>, gifs: Vec<Gif>, request: u64) {
    let permits = Arc::new(Semaphore::new(PARALLEL_PREVIEWS));
    for gif in gifs {
        let (app, permits) = (app.clone(), Arc::clone(&permits));
        tokio::spawn(async move {
            let Ok(_permit) = permits.acquire().await else { return };
            if REQUEST.load(Ordering::SeqCst) != request {
                return;
            }
            let Some(buffer) = fetch_preview(&gif.preview).await else { return };
            let _ = slint::invoke_from_event_loop(move || {
                if REQUEST.load(Ordering::SeqCst) == request {
                    if let Some(ui) = app.upgrade() {
                        show_preview(&ui, &gif.url, Image::from_rgba8(buffer));
                    }
                }
            });
        });
    }
}

/// Opens the selected tab: the account's favorites, or trending GIFs.
pub fn open(ui: &AppWindow, http: Option<DiscordHttpClient>) {
    let favorites = ui.get_gif_tab() == "favorites";
    start(ui, http, "", favorites);
}

/// Searches Discord's GIFs; an empty query shows the trending ones.
pub fn search(ui: &AppWindow, http: Option<DiscordHttpClient>, query: &str) {
    ui.set_gif_tab(if query.trim().is_empty() { "trending" } else { "search" }.into());
    start(ui, http, query, false);
}

fn start(ui: &AppWindow, http: Option<DiscordHttpClient>, query: &str, favorites: bool) {
    let Some(http) = http else { return };
    let request = REQUEST.fetch_add(1, Ordering::SeqCst) + 1;
    ui.set_gif_loading(true);
    ui.set_gif_empty(false);
    ui.set_gif_rows(ModelRc::new(VecModel::<GifRow>::default()));

    let app = ui.as_weak();
    let query = query.trim().to_owned();
    tokio::spawn(async move {
        let result = if favorites {
            http.get_favorite_gifs().await
        } else {
            http.get_gifs(&query).await.map(|body| parse_gifs(&body))
        };
        let gifs = result.unwrap_or_else(|e| {
            log::warn!("GIFs indisponíveis: {}", e);
            Vec::new()
        });
        if favorites {
            crate::ui::chat_actions::set_favorites(gifs.iter().map(|gif| gif.url.clone()));
        }
        let app_ui = app.clone();
        let shown = gifs.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if REQUEST.load(Ordering::SeqCst) != request {
                return;
            }
            if let Some(ui) = app_ui.upgrade() {
                ui.set_gif_loading(false);
                ui.set_gif_empty(shown.is_empty());
                ui.set_gif_rows(ModelRc::new(VecModel::from(rows_for(&shown))));
            }
        });
        load_previews(app, gifs, request);
    });
}

/// The open server's stickers. Animated Lottie ones cannot be drawn and are left out.
pub fn open_stickers(ui: &AppWindow, http: Option<DiscordHttpClient>) {
    let Some(http) = http else { return };
    let guild = ui.get_active_guild_id().to_string();
    ui.set_sticker_rows(ModelRc::new(VecModel::<GifRow>::default()));
    ui.set_sticker_empty(false);
    if guild == "@me" {
        ui.set_sticker_empty(true);
        return;
    }
    let request = REQUEST.fetch_add(1, Ordering::SeqCst) + 1;
    let app = ui.as_weak();
    tokio::spawn(async move {
        let body = http.discord_call(reqwest::Method::GET, &format!("/guilds/{guild}/stickers"), None).await.unwrap_or_default();
        let stickers: Vec<Gif> = body
            .as_array()
            .into_iter()
            .flatten()
            .filter(|s| s["available"].as_bool().unwrap_or(true) && s["format_type"].as_u64() != Some(3))
            .filter_map(|s| {
                let id = s["id"].as_str()?;
                let extension = if s["format_type"].as_u64() == Some(4) { "gif" } else { "png" };
                Some(Gif {
                    url: id.to_owned(),
                    title: s["name"].as_str().unwrap_or("").to_owned(),
                    preview: format!("https://media.discordapp.net/stickers/{id}.{extension}?size=160"),
                })
            })
            .take(MAX_GIFS)
            .collect();
        let shown = stickers.clone();
        let app_ui = app.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = app_ui.upgrade() {
                ui.set_sticker_empty(shown.is_empty());
                let rows: Vec<GifRow> = shown.chunks(4).map(|chunk| {
                    let items: Vec<GifItem> = chunk.iter().map(|s| GifItem { url: s.url.as_str().into(), title: s.title.as_str().into(), ..Default::default() }).collect();
                    GifRow { items: ModelRc::new(VecModel::from(items)) }
                }).collect();
                ui.set_sticker_rows(ModelRc::new(VecModel::from(rows)));
            }
        });
        for sticker in stickers {
            if REQUEST.load(Ordering::SeqCst) != request {
                return;
            }
            let Some(buffer) = fetch_preview(&sticker.preview).await else { continue };
            let app_item = app.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = app_item.upgrade() else { return };
                let image = Image::from_rgba8(buffer);
                for row in ui.get_sticker_rows().iter() {
                    for index in 0..row.items.row_count() {
                        if let Some(mut item) = row.items.row_data(index).filter(|i| i.url == sticker.url.as_str()) {
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

/// Puts Twemoji images in the emoji picker; the ones still downloading follow.
pub fn hydrate_emoji_picker(ui: &AppWindow) {
    let cache = emoji_cache::get_emoji_cache();
    let mut rows: Vec<EmojiRow> = ui
        .get_emoji_rows()
        .iter()
        .filter(|row| row.emojis.iter().any(|item| !item.is_custom))
        .map(|row| {
            let items: Vec<EmojiPickerItem> = row
                .emojis
                .iter()
                .map(|mut item| {
                    if let Some((_, _, _, id)) = crate::gateway::find_next_unicode_emoji(&item.code) {
                        match cache.get(&id) {
                            Some(image) => item.img = image,
                            None => cache.fetch_priority_async(&id, "", ui.as_weak()),
                        }
                        item.emoji_id = id.into();
                    }
                    item
                })
                .collect();
            EmojiRow { header: "".into(), emojis: ModelRc::new(VecModel::from(items)) }
        })
        .collect();

    let guild_id = ui.get_active_guild_id();
    let server_emojis = sendable_emojis(
        emoji_cache::get_all_custom_emojis(),
        &guild_id,
        crate::native_video::sender::premium_type(),
    );
    let custom: Vec<EmojiRow> = server_emojis
        .chunks(EMOJIS_PER_ROW)
        .map(|chunk| {
            let items: Vec<EmojiPickerItem> = chunk
                .iter()
                .map(|emoji| {
                    let img = cache.get(&emoji.id).unwrap_or_else(|| {
                        cache.fetch_priority_async(&emoji.id, "", ui.as_weak());
                        Image::default()
                    });
                    EmojiPickerItem {
                        code: emoji_code(emoji).into(),
                        name: emoji.name.as_str().into(),
                        category: "server".into(),
                        is_custom: true,
                        emoji_id: emoji.id.as_str().into(),
                        img, locked: false,
                    }
                })
                .collect();
            EmojiRow { header: "".into(), emojis: ModelRc::new(VecModel::from(items)) }
        })
        .collect();
    // The open server's emojis come first, as they are the ones used most here.
    rows.splice(0..0, custom);
    ui.set_emoji_rows(ModelRc::new(VecModel::from(rows)));
}

pub fn emoji_loaded(ui: &AppWindow, emoji_id: &str, image: &Image) {
    crate::ui::chat_actions::emoji_loaded(ui, emoji_id, image);
    crate::ui::expression_picker::rail_emoji_loaded(ui, emoji_id, image);
    if !ui.get_show_emoji_picker() {
        return;
    }
    for row in ui.get_emoji_rows().iter() {
        for index in 0..row.emojis.row_count() {
            if let Some(mut item) = row.emojis.row_data(index).filter(|item| item.emoji_id == emoji_id) {
                item.img = image.clone();
                row.emojis.set_row_data(index, item);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_both_result_shapes() {
        let gif = json!({ "url": "https://tenor.com/a", "title": "cat", "preview": "https://media.tenor.com/a.png" });
        let expected = Gif { url: "https://tenor.com/a".into(), title: "cat".into(), preview: "https://media.tenor.com/a.png".into() };
        assert_eq!(parse_gifs(&json!([gif.clone()])), vec![expected.clone()]);
        assert_eq!(parse_gifs(&json!({ "gifs": [gif] })), vec![expected]);
    }

    #[test]
    fn falls_back_to_the_other_image_fields() {
        let gifs = parse_gifs(&json!([{ "url": "https://tenor.com/a", "gif_src": "https://media.tenor.com/a.gif" }]));
        assert_eq!(gifs[0].preview, "https://media.tenor.com/a.gif");
    }

    fn emoji(id: &str, guild: &str, animated: bool) -> emoji_cache::CustomEmojiEntry {
        emoji_cache::CustomEmojiEntry { id: id.into(), name: id.into(), animated, guild_id: guild.into(), available: true }
    }

    #[test]
    fn without_a_plan_only_this_servers_static_emojis_are_offered() {
        let all = vec![emoji("here", "1", false), emoji("gif", "1", true), emoji("other", "2", false)];
        let ids: Vec<String> = sendable_emojis(all, "1", 0).into_iter().map(|e| e.id).collect();
        assert_eq!(ids, ["here"]);
    }

    #[test]
    fn with_a_plan_this_server_comes_first() {
        let all = vec![emoji("other", "2", false), emoji("here", "1", true)];
        let ids: Vec<String> = sendable_emojis(all, "1", 2).into_iter().map(|e| e.id).collect();
        assert_eq!(ids, ["here", "other"]);
    }

    #[test]
    fn unavailable_emojis_are_not_offered() {
        let mut locked = emoji("locked", "1", false);
        locked.available = false;
        assert!(sendable_emojis(vec![locked], "1", 2).is_empty());
    }

    #[test]
    fn emoji_codes_follow_discords_format() {
        assert_eq!(emoji_code(&emoji("5", "1", false)), "<:5:5>");
        assert_eq!(emoji_code(&emoji("5", "1", true)), "<a:5:5>");
    }

    #[test]
    fn reads_favorites_newest_first() {
        fn text(tag: u8, value: &str) -> Vec<u8> {
            let mut out = vec![tag, value.len() as u8];
            out.extend_from_slice(value.as_bytes());
            out
        }
        fn favorite(url: &str, src: &str, order: u8) -> Vec<u8> {
            let mut gif = text(0x12, src);
            gif.extend_from_slice(&[0x28, order]);
            let mut entry = text(0x0A, url);
            entry.extend_from_slice(&[0x12, gif.len() as u8]);
            entry.extend(gif);
            let mut item = vec![0x0A, entry.len() as u8];
            item.extend(entry);
            item
        }
        let mut section = favorite("https://tenor.com/old", "https://media.tenor.com/xAAAPo/o.mp4", 1);
        section.extend(favorite("https://tenor.com/new", "https://media.tenor.com/yAAAPo/n.mp4", 2));
        // The section is over 127 bytes, so its protobuf length takes two bytes.
        let mut settings = vec![0x12, (section.len() as u8 & 0x7F) | 0x80, (section.len() >> 7) as u8];
        settings.extend(section);

        let gifs = parse_favorite_gifs(&settings);
        assert_eq!(gifs[0].url, "https://tenor.com/new");
        assert_eq!(gifs[0].preview, "https://media.tenor.com/yAAAAM/n.gif");
        assert_eq!(gifs[1].url, "https://tenor.com/old");
    }

    #[test]
    fn links_that_are_not_https_are_ignored() {
        assert!(parse_gifs(&json!([{ "url": "http://x/a", "preview": "https://x/a.png" }])).is_empty());
        assert!(parse_gifs(&json!([{ "url": "https://x/a", "preview": "file:///c:/a.png" }])).is_empty());
    }

    #[test]
    fn results_are_capped() {
        let many: Vec<Value> = (0..100).map(|i| json!({ "url": format!("https://x/{i}"), "preview": "https://x/p.png" })).collect();
        assert_eq!(parse_gifs(&Value::Array(many)).len(), MAX_GIFS);
    }
}
