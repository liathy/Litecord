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
const GIFS_PER_ROW: usize = 4;
const PARALLEL_PREVIEWS: usize = 4;
const MAX_PREVIEW_BYTES: usize = 6 * 1024 * 1024;
const MAX_PREVIEW_SIDE: u32 = 2048;
const PREVIEW_WIDTH: u32 = 192;
const PREVIEW_HEIGHT: u32 = 144;

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

async fn fetch_preview(url: &str) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
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
        let small = decoded.thumbnail(PREVIEW_WIDTH, PREVIEW_HEIGHT).to_rgba8();
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

/// Shows trending GIFs for an empty query, search results otherwise.
pub fn load(ui: &AppWindow, http: Option<DiscordHttpClient>, query: &str) {
    let Some(http) = http else { return };
    let request = REQUEST.fetch_add(1, Ordering::SeqCst) + 1;
    ui.set_gif_loading(true);
    ui.set_gif_empty(false);
    ui.set_gif_rows(ModelRc::new(VecModel::<GifRow>::default()));

    let app = ui.as_weak();
    let query = query.trim().to_owned();
    tokio::spawn(async move {
        let gifs = match http.get_gifs(&query).await {
            Ok(body) => parse_gifs(&body),
            Err(e) => {
                log::warn!("GIFs indisponíveis: {}", e);
                Vec::new()
            }
        };
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

/// Puts Twemoji images in the emoji picker; the ones still downloading follow.
pub fn hydrate_emoji_picker(ui: &AppWindow) {
    let cache = emoji_cache::get_emoji_cache();
    let rows: Vec<EmojiRow> = ui
        .get_emoji_rows()
        .iter()
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
            EmojiRow { emojis: ModelRc::new(VecModel::from(items)) }
        })
        .collect();
    ui.set_emoji_rows(ModelRc::new(VecModel::from(rows)));
}

pub fn emoji_loaded(ui: &AppWindow, emoji_id: &str, image: &Image) {
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
