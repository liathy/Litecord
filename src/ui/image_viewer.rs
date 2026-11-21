//! Click-to-expand images. The large copy lives only while the viewer is open.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use slint::{ComponentHandle, Global, Image};

use crate::{AppWindow, GifPlayback};

const MAX_SIDE: i32 = 1600;
const MEDIA_HOST: &str = "https://media.discordapp.net/";

static REQUEST: AtomicU64 = AtomicU64::new(0);

/// Discord's media proxy resizes on request, so a huge upload still opens quickly.
fn large_url(proxy: &str, original: &str, width: i32, height: i32) -> String {
    if !proxy.starts_with(MEDIA_HOST) {
        return original.to_owned();
    }
    let (mut w, mut h) = (width.max(1), height.max(1));
    if w.max(h) > MAX_SIDE {
        let scale = MAX_SIDE as f64 / w.max(h) as f64;
        w = ((w as f64 * scale).round() as i32).max(1);
        h = ((h as f64 * scale).round() as i32).max(1);
    }
    let separator = if proxy.contains('?') { '&' } else { '?' };
    format!("{proxy}{separator}width={w}&height={h}")
}

fn opened(ui: &AppWindow) {
    let request = REQUEST.fetch_add(1, Ordering::SeqCst) + 1;
    let (id, url) = (ui.get_viewer_id().to_string(), ui.get_viewer_url().to_string());
    if ui.get_viewer_gif() {
        // The row reports "mouse left" as the viewer covers it, so playback starts after that.
        let app = ui.as_weak();
        slint::Timer::single_shot(Duration::from_millis(80), move || {
            if let Some(ui) = app.upgrade() {
                if REQUEST.load(Ordering::SeqCst) == request && ui.get_viewer_open() {
                    GifPlayback::get(&ui).invoke_hover(id.into(), url.into(), true);
                }
            }
        });
        return;
    }
    let large = large_url(&ui.get_viewer_proxy(), &url, ui.get_viewer_width(), ui.get_viewer_height());
    let app = ui.as_weak();
    tokio::spawn(async move {
        let Some(buffer) = crate::ui::gif_picker::fetch_image(&large, MAX_SIDE as u32, MAX_SIDE as u32).await else { return };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = app.upgrade() {
                if REQUEST.load(Ordering::SeqCst) == request && ui.get_viewer_open() {
                    ui.set_viewer_img(Image::from_rgba8(buffer));
                }
            }
        });
    });
}

fn closed(ui: &AppWindow) {
    REQUEST.fetch_add(1, Ordering::SeqCst);
    if ui.get_viewer_gif() {
        GifPlayback::get(ui).invoke_hover(ui.get_viewer_id(), ui.get_viewer_url(), false);
    }
    ui.set_viewer_open(false);
    ui.set_viewer_img(Image::default());
}

pub fn install(ui: &AppWindow) {
    let app = ui.as_weak();
    ui.on_image_viewer(move |name| {
        let Some(ui) = app.upgrade() else { return };
        match name.as_str() {
            "open" => opened(&ui),
            "close" => closed(&ui),
            "browser" => {
                ui.invoke_open_link(ui.get_viewer_url());
                closed(&ui);
            }
            _ => {}
        }
    });
}

#[cfg(test)]
mod tests {
    use super::large_url;

    #[test]
    fn asks_the_proxy_for_a_bounded_size() {
        let proxy = "https://media.discordapp.net/attachments/1/2/a.png?ex=1";
        assert_eq!(large_url(proxy, "o", 4000, 2000), format!("{proxy}&width=1600&height=800"));
        assert_eq!(large_url("https://media.discordapp.net/a.png", "o", 800, 600), "https://media.discordapp.net/a.png?width=800&height=600");
    }

    #[test]
    fn other_hosts_use_the_original_link() {
        assert_eq!(large_url("https://example.com/a.png", "https://cdn.discordapp.com/a.png", 10, 10), "https://cdn.discordapp.com/a.png");
    }
}
