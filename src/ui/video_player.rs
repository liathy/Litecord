//! Playing videos sent in chat with Windows' own decoders, so nothing extra is bundled
//! and nothing runs until a video is opened. Formats Windows can't play open in the browser.

use slint::ComponentHandle;

use crate::AppWindow;

/// Media Foundation counts time in units of 100 nanoseconds.
const TICKS_PER_SECOND: f64 = 10_000_000.0;

fn clock(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    let (hours, minutes, rest) = (total / 3600, total % 3600 / 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{rest:02}")
    } else {
        format!("{minutes}:{rest:02}")
    }
}

fn time_label(position: i64, duration: i64) -> String {
    format!("{} / {}", clock(position as f64 / TICKS_PER_SECOND), clock(duration as f64 / TICKS_PER_SECOND))
}

fn seek_target(fraction: f32, duration: i64) -> i64 {
    (fraction.clamp(0.0, 1.0) as f64 * duration.max(0) as f64) as i64
}

#[cfg(windows)]
mod native {
    use std::cell::RefCell;
    use std::sync::Once;
    use std::time::{Duration, Instant};

    use i_slint_backend_winit::WinitWindowAccessor;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use slint::ComponentHandle;
    use windows::core::{GUID, HSTRING, PROPVARIANT};
    use windows::Win32::Foundation::{HWND, TRUE};
    use windows::Win32::Media::MediaFoundation::{
        IMFPMediaPlayer, IMFPMediaPlayerCallback, MFPCreateMediaPlayer, MFStartup, MFP_MEDIAPLAYER_STATE_PLAYING, MFP_OPTION_NONE, MFSTARTUP_FULL, MF_VERSION,
    };

    use super::{seek_target, time_label};
    use crate::AppWindow;

    const LOAD_TIMEOUT: Duration = Duration::from_secs(10);
    const REFRESH: Duration = Duration::from_millis(250);

    struct Player {
        window: windows_sys::Win32::Foundation::HWND,
        parent: windows_sys::Win32::Foundation::HWND,
        /// The video area inside the app, physical pixels.
        area: (i32, i32, i32, i32),
        /// Where the picture window is on the screen now.
        placed: (i32, i32, i32, i32),
        media: IMFPMediaPlayer,
        duration: i64,
        opened: Instant,
        _timer: slint::Timer,
    }

    thread_local! {
        static PLAYER: RefCell<Option<Player>> = const { RefCell::new(None) };
    }

    fn main_window(ui: &AppWindow) -> Option<windows_sys::Win32::Foundation::HWND> {
        ui.window()
            .with_winit_window(|w| match w.window_handle().ok()?.as_raw() {
                RawWindowHandle::Win32(h) => Some(h.hwnd.get() as windows_sys::Win32::Foundation::HWND),
                _ => None,
            })
            .flatten()
    }

    fn ticks(value: &PROPVARIANT) -> i64 {
        i64::try_from(value).ok().or_else(|| u64::try_from(value).ok().map(|v| v as i64)).unwrap_or(0)
    }

    /// Keeps the picture over its area, also while the app window is being moved.
    fn follow(player: &mut Player) {
        use windows_sys::Win32::Foundation::POINT;
        use windows_sys::Win32::Graphics::Gdi::ClientToScreen;
        use windows_sys::Win32::UI::WindowsAndMessaging::MoveWindow;
        let (x, y, w, h) = player.area;
        let mut corner = POINT { x, y };
        unsafe {
            if ClientToScreen(player.parent, &mut corner) == 0 {
                return;
            }
            let wanted = (corner.x, corner.y, w, h);
            if wanted != player.placed {
                player.placed = wanted;
                MoveWindow(player.window, corner.x, corner.y, w.max(1), h.max(1), 1);
                let _ = player.media.UpdateVideo();
            }
        }
    }

    fn tick(ui: &AppWindow) {
        PLAYER.with_borrow_mut(|slot| {
            let Some(player) = slot.as_mut() else { return };
            follow(player);
            let time = GUID::zeroed();
            unsafe {
                if let Ok(value) = player.media.GetDuration(&time) {
                    player.duration = ticks(&value);
                }
                let position = player.media.GetPosition(&time).map(|v| ticks(&v)).unwrap_or(0);
                ui.set_video_playing(player.media.GetState().is_ok_and(|s| s == MFP_MEDIAPLAYER_STATE_PLAYING));
                if player.duration > 0 {
                    ui.set_video_status("".into());
                    ui.set_video_position((position as f64 / player.duration as f64) as f32);
                    ui.set_video_time(time_label(position, player.duration).into());
                } else if player.opened.elapsed() > LOAD_TIMEOUT {
                    ui.set_video_status("This video can't play here. Use Open in Browser.".into());
                }
            }
        });
    }

    pub fn open(ui: &AppWindow, url: &str) -> bool {
        close();
        static STARTUP: Once = Once::new();
        STARTUP.call_once(|| unsafe {
            let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
        });
        let Some(parent) = main_window(ui) else { return false };
        use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP, WS_VISIBLE};
        let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
        let window = unsafe {
            // Owned, not a child: the app paints its whole client area, which would cover a child.
            CreateWindowExW(WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW, class.as_ptr(), std::ptr::null(), WS_POPUP | WS_VISIBLE, 0, 0, 1, 1, parent, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null())
        };
        if window.is_null() {
            return false;
        }
        let mut media: Option<IMFPMediaPlayer> = None;
        let created = unsafe { MFPCreateMediaPlayer(&HSTRING::from(url), TRUE, MFP_OPTION_NONE, None::<&IMFPMediaPlayerCallback>, HWND(window as _), Some(&mut media)) };
        let Some(media) = media.filter(|_| created.is_ok()) else {
            log::warn!("Vídeo não abriu: {:?}", created.err());
            unsafe { DestroyWindow(window) };
            return false;
        };
        let timer = slint::Timer::default();
        let app = ui.as_weak();
        timer.start(slint::TimerMode::Repeated, REFRESH, move || {
            if let Some(ui) = app.upgrade() {
                tick(&ui);
            }
        });
        PLAYER.with_borrow_mut(|slot| *slot = Some(Player { window, parent, area: (0, 0, 1, 1), placed: (0, 0, 0, 0), media, duration: 0, opened: Instant::now(), _timer: timer }));
        true
    }

    pub fn close() {
        let Some(player) = PLAYER.with_borrow_mut(Option::take) else { return };
        unsafe {
            let _ = player.media.Shutdown();
            windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(player.window);
        }
    }

    pub fn place(ui: &AppWindow, x: f32, y: f32, w: f32, h: f32) {
        let k = ui.window().scale_factor();
        PLAYER.with_borrow_mut(|slot| {
            let Some(player) = slot.as_mut() else { return };
            player.area = ((x * k).round() as i32, (y * k).round() as i32, (w * k).round() as i32, (h * k).round() as i32);
            follow(player);
        });
    }

    pub fn toggle() {
        PLAYER.with_borrow(|slot| {
            let Some(player) = slot.as_ref() else { return };
            unsafe {
                if player.media.GetState().is_ok_and(|s| s == MFP_MEDIAPLAYER_STATE_PLAYING) {
                    let _ = player.media.Pause();
                } else {
                    let _ = player.media.Play();
                }
            }
        });
    }

    pub fn seek(fraction: f32) {
        PLAYER.with_borrow(|slot| {
            let Some(player) = slot.as_ref().filter(|p| p.duration > 0) else { return };
            let value = PROPVARIANT::from(seek_target(fraction, player.duration));
            unsafe {
                let _ = player.media.SetPosition(&GUID::zeroed(), &value);
            }
        });
    }

    pub fn set_volume(volume: f32) {
        PLAYER.with_borrow(|slot| {
            if let Some(player) = slot.as_ref() {
                unsafe {
                    let _ = player.media.SetVolume(volume.clamp(0.0, 1.0));
                }
            }
        });
    }
}

#[cfg(not(windows))]
mod native {
    use crate::AppWindow;
    pub fn open(_ui: &AppWindow, _url: &str) -> bool { false }
    pub fn close() {}
    pub fn place(_ui: &AppWindow, _x: f32, _y: f32, _w: f32, _h: f32) {}
    pub fn toggle() {}
    pub fn seek(_fraction: f32) {}
    pub fn set_volume(_volume: f32) {}
}

fn close(ui: &AppWindow) {
    native::close();
    if ui.get_video_fullscreen() {
        ui.set_video_fullscreen(false);
        crate::ui::fullscreen::set(ui.window(), false);
    }
    ui.set_video_open(false);
}

pub fn install(ui: &AppWindow) {
    let app = ui.as_weak();
    ui.on_open_video(move |url, title| {
        let Some(ui) = app.upgrade() else { return };
        ui.set_video_url(url.clone());
        ui.set_video_title(title);
        ui.set_video_time("0:00 / 0:00".into());
        ui.set_video_position(0.0);
        ui.set_video_status("".into());
        ui.set_video_volume(1.0);
        ui.set_video_saved_volume(1.0);
        if native::open(&ui, url.as_str()) {
            ui.set_video_open(true);
            ui.invoke_video_refit();
        } else {
            ui.invoke_open_link(url);
        }
    });
    let app = ui.as_weak();
    ui.on_video_area_changed(move |x, y, w, h| {
        if let Some(ui) = app.upgrade() {
            ui.set_video_area_x(x);
            ui.set_video_area_y(y);
            ui.set_video_area_w(w);
            ui.set_video_area_h(h);
            native::place(&ui, x, y, w, h);
        }
    });
    let app = ui.as_weak();
    ui.on_video_refit(move || {
        if let Some(ui) = app.upgrade() {
            native::place(&ui, ui.get_video_area_x(), ui.get_video_area_y(), ui.get_video_area_w(), ui.get_video_area_h());
        }
    });
    let app = ui.as_weak();
    ui.on_video_action(move |name, value| {
        let Some(ui) = app.upgrade() else { return };
        match name.as_str() {
            "toggle" => native::toggle(),
            "seek" => {
                ui.set_video_position(value);
                native::seek(value);
            }
            "volume" => {
                ui.set_video_volume(value);
                if value > 0.01 {
                    ui.set_video_saved_volume(value);
                }
                native::set_volume(value);
            }
            "mute" => {
                let next = if ui.get_video_volume() > 0.01 { 0.0 } else { ui.get_video_saved_volume().max(0.05) };
                ui.set_video_volume(next);
                native::set_volume(next);
            }
            "fullscreen" => {
                let next = !ui.get_video_fullscreen();
                ui.set_video_fullscreen(next);
                crate::ui::fullscreen::set(ui.window(), next);
            }
            "browser" => {
                let url = ui.get_video_url();
                close(&ui);
                ui.invoke_open_link(url);
            }
            "close" => close(&ui),
            _ => {}
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_read_like_a_player() {
        assert_eq!(clock(5.9), "0:05");
        assert_eq!(clock(125.0), "2:05");
        assert_eq!(clock(3725.0), "1:02:05");
        assert_eq!(time_label(10 * 10_000_000, 60 * 10_000_000), "0:10 / 1:00");
    }

    #[test]
    fn seeking_stays_inside_the_video() {
        assert_eq!(seek_target(0.5, 100), 50);
        assert_eq!(seek_target(2.0, 100), 100);
        assert_eq!(seek_target(-1.0, 100), 0);
    }
}
