//! Plays one GIF at a time, only while the mouse is over it.
//!
//! The chat keeps a still picture of every GIF. Frames exist only for the GIF being
//! hovered and are released when the mouse leaves, so GIF-heavy channels stay light.

use std::cell::RefCell;
use std::io::Cursor;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use image::codecs::gif::GifDecoder;
use image::codecs::webp::WebPDecoder;
use image::AnimationDecoder;
use slint::{ComponentHandle, Global, Image, Rgba8Pixel, SharedPixelBuffer};

use crate::{AppWindow, GifPlayback};

const MAX_DOWNLOAD_BYTES: usize = 20 * 1024 * 1024;
const MAX_DECODED_BYTES: usize = 48 * 1024 * 1024;
const MAX_SIDE: u32 = 400;
const MAX_SOURCE_SIDE: u32 = 2048;
/// The row under the mouse can be rebuilt without a "mouse left" event.
const MAX_PLAY_TIME: Duration = Duration::from_secs(120);

static GENERATION: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static CURRENT: RefCell<String> = const { RefCell::new(String::new()) };
}

struct DecodedFrame {
    pixels: SharedPixelBuffer<Rgba8Pixel>,
    delay: Duration,
}

pub fn install(ui: &AppWindow) {
    let app = ui.as_weak();
    GifPlayback::get(ui).on_hover(move |id, url, active| {
        if let Some(ui) = app.upgrade() {
            hover(&ui, id.as_str(), url.as_str(), active);
        }
    });
}

fn stop(ui: &AppWindow) {
    GENERATION.fetch_add(1, Ordering::SeqCst);
    CURRENT.with(|current| current.borrow_mut().clear());
    let playback = GifPlayback::get(ui);
    playback.set_id("".into());
    playback.set_frame(Image::default());
}

fn hover(ui: &AppWindow, id: &str, url: &str, active: bool) {
    if !active {
        // Moving straight onto another GIF can report its "enter" first.
        if CURRENT.with(|current| current.borrow().as_str() == id) {
            stop(ui);
        }
        return;
    }
    stop(ui);
    if !is_allowed_source(url) {
        return;
    }
    CURRENT.with(|current| *current.borrow_mut() = id.to_string());
    let generation = GENERATION.load(Ordering::SeqCst);
    let (app, id, url) = (ui.as_weak(), id.to_string(), url.to_string());
    tokio::spawn(async move {
        let Some(bytes) = load(&id, &url).await else { return };
        let Ok(Some(frames)) = tokio::task::spawn_blocking(move || decode(&bytes)).await else { return };
        let _ = slint::invoke_from_event_loop(move || {
            if GENERATION.load(Ordering::SeqCst) != generation {
                return;
            }
            let Some(ui) = app.upgrade() else { return };
            let frames: Rc<Vec<(Image, Duration)>> = Rc::new(
                frames.into_iter().map(|frame| (Image::from_rgba8(frame.pixels), frame.delay)).collect(),
            );
            GifPlayback::get(&ui).set_id(id.into());
            show_frame(app_weak(&ui), frames, 0, generation, Instant::now());
        });
    });
}

fn app_weak(ui: &AppWindow) -> slint::Weak<AppWindow> {
    ui.as_weak()
}

fn show_frame(app: slint::Weak<AppWindow>, frames: Rc<Vec<(Image, Duration)>>, index: usize, generation: u64, started: Instant) {
    if GENERATION.load(Ordering::SeqCst) != generation {
        return;
    }
    let Some(ui) = app.upgrade() else { return };
    if started.elapsed() > MAX_PLAY_TIME {
        stop(&ui);
        return;
    }
    let (frame, delay) = &frames[index];
    GifPlayback::get(&ui).set_frame(frame.clone());
    let delay = *delay;
    let next = (index + 1) % frames.len();
    slint::Timer::single_shot(delay, move || show_frame(app, frames, next, generation, started));
}

async fn load(id: &str, url: &str) -> Option<Vec<u8>> {
    let path = std::env::temp_dir()
        .join("Litecord")
        .join("temp_images")
        .join(format!("{}.anim", crate::utils::attachment_cache::sanitize_filename(id)));
    if let Ok(bytes) = tokio::fs::read(&path).await {
        return Some(bytes);
    }
    let mut response = crate::utils::network_settings::shared_http_client()
        .get(url)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() || response.content_length().is_some_and(|len| len as usize > MAX_DOWNLOAD_BYTES) {
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if bytes.len() + chunk.len() > MAX_DOWNLOAD_BYTES {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    let _ = tokio::fs::write(&path, &bytes).await;
    Some(bytes)
}

/// GIFs are fetched from these hosts only, and never with the account token.
fn is_allowed_source(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else { return false };
    let Some(host) = parsed.host_str() else { return false };
    parsed.scheme() == "https"
        && ["tenor.com", "giphy.com", "discordapp.net", "discordapp.com"]
            .iter()
            .any(|allowed| host.ends_with(&format!(".{allowed}")))
}

fn decode(bytes: &[u8]) -> Option<Vec<DecodedFrame>> {
    if bytes.starts_with(b"GIF8") {
        collect(GifDecoder::new(Cursor::new(bytes)).ok()?.into_frames())
    } else if bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        let decoder = WebPDecoder::new(Cursor::new(bytes)).ok()?;
        if !decoder.has_animation() {
            return None;
        }
        collect(decoder.into_frames())
    } else {
        None
    }
}

fn collect(source: image::Frames<'_>) -> Option<Vec<DecodedFrame>> {
    let mut frames = Vec::new();
    let mut total = 0usize;
    for frame in source {
        let Ok(frame) = frame else { break };
        let (numerator, denominator) = frame.delay().numer_denom_ms();
        let ms = numerator.checked_div(denominator).unwrap_or(0);
        // Browsers treat very short delays as 100 ms too.
        let delay = Duration::from_millis(if ms < 20 { 100 } else { ms as u64 });
        let buffer = frame.into_buffer();
        let (width, height) = buffer.dimensions();
        if width == 0 || height == 0 || width > MAX_SOURCE_SIDE || height > MAX_SOURCE_SIDE {
            return None;
        }
        let scale = (MAX_SIDE as f32 / width.max(height) as f32).min(1.0);
        let fitted = if scale < 1.0 {
            image::imageops::thumbnail(&buffer, ((width as f32 * scale) as u32).max(1), ((height as f32 * scale) as u32).max(1))
        } else {
            buffer
        };
        let (width, height) = fitted.dimensions();
        total += width as usize * height as usize * 4;
        if total > MAX_DECODED_BYTES {
            break;
        }
        frames.push(DecodedFrame {
            pixels: SharedPixelBuffer::clone_from_slice(fitted.as_raw(), width, height),
            delay,
        });
    }
    (!frames.is_empty()).then_some(frames)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Delay, Frame, RgbaImage};

    fn gif(frames: u32, side: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut bytes);
            for index in 0..frames {
                let picture = RgbaImage::from_pixel(side, side, image::Rgba([index as u8 * 40, 0, 0, 255]));
                encoder.encode_frame(Frame::from_parts(picture, 0, 0, Delay::from_numer_denom_ms(50, 1))).unwrap();
            }
        }
        bytes
    }

    #[test]
    fn every_frame_is_decoded_with_its_delay() {
        let frames = decode(&gif(3, 8)).unwrap();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].delay, Duration::from_millis(50));
    }

    #[test]
    fn large_gifs_are_scaled_down() {
        let frames = decode(&gif(1, 800)).unwrap();
        assert_eq!(frames[0].pixels.width(), MAX_SIDE);
    }

    #[test]
    fn data_that_is_not_an_animation_is_refused() {
        assert!(decode(b"not an image").is_none());
    }

    #[test]
    fn only_known_https_hosts_are_contacted() {
        assert!(is_allowed_source("https://media.tenor.com/a/b.gif"));
        assert!(is_allowed_source("https://images-ext-1.discordapp.net/external/a"));
        assert!(!is_allowed_source("http://media.tenor.com/a/b.gif"));
        assert!(!is_allowed_source("https://tenor.com.evil.example/a.gif"));
        assert!(!is_allowed_source("https://example.com/a.gif"));
    }
}
