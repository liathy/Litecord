//! True fullscreen for streams. The resizable frame keeps Windows from treating the app
//! as fullscreen, so the taskbar stayed on top: the frame is removed while it lasts.

use i_slint_backend_winit::WinitWindowAccessor;

#[cfg(windows)]
mod native {
    use std::cell::RefCell;

    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Foundation::{HWND, RECT};
    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, GetWindowPlacement, SetForegroundWindow, SetWindowLongPtrW, SetWindowPlacement, SetWindowPos,
        GWL_STYLE, HWND_TOP, SWP_FRAMECHANGED, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW,
        WINDOWPLACEMENT, WS_OVERLAPPEDWINDOW, WS_POPUP,
    };

    thread_local! {
        static SAVED: RefCell<Option<(isize, WINDOWPLACEMENT)>> = const { RefCell::new(None) };
    }

    pub(super) fn borderless_style(style: isize) -> isize {
        (style & !(WS_OVERLAPPEDWINDOW as isize)) | WS_POPUP as isize
    }

    fn hwnd(window: &winit::window::Window) -> Option<HWND> {
        match window.window_handle().ok()?.as_raw() {
            RawWindowHandle::Win32(handle) => Some(handle.hwnd.get() as HWND),
            _ => None,
        }
    }

    fn enter(hwnd: HWND) {
        if SAVED.with_borrow(Option::is_some) {
            return;
        }
        unsafe {
            let mut placement: WINDOWPLACEMENT = std::mem::zeroed();
            placement.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
            let mut info: MONITORINFO = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
            if GetWindowPlacement(hwnd, &mut placement) == 0 || GetMonitorInfoW(monitor, &mut info) == 0 {
                return;
            }
            let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
            SAVED.with_borrow_mut(|saved| *saved = Some((style, placement)));
            SetWindowLongPtrW(hwnd, GWL_STYLE, borderless_style(style));
            let RECT { left, top, right, bottom } = info.rcMonitor;
            SetWindowPos(hwnd, HWND_TOP, left, top, right - left, bottom - top, SWP_NOOWNERZORDER | SWP_FRAMECHANGED | SWP_SHOWWINDOW);
            SetForegroundWindow(hwnd);
        }
    }

    fn leave(hwnd: HWND) {
        let Some((style, placement)) = SAVED.with_borrow_mut(Option::take) else { return };
        unsafe {
            SetWindowLongPtrW(hwnd, GWL_STYLE, style);
            SetWindowPlacement(hwnd, &placement);
            SetWindowPos(hwnd, std::ptr::null_mut(), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOOWNERZORDER | SWP_FRAMECHANGED);
        }
    }

    pub(super) fn set(window: &winit::window::Window, on: bool) {
        let Some(hwnd) = hwnd(window) else { return };
        if on { enter(hwnd) } else { leave(hwnd) }
    }
}

#[cfg(not(windows))]
mod native {
    pub(super) fn set(window: &winit::window::Window, on: bool) {
        window.set_fullscreen(on.then_some(winit::window::Fullscreen::Borderless(None)));
    }
}

pub fn set(window: &slint::Window, on: bool) {
    let _ = window.with_winit_window(|w| native::set(w, on));
}

#[cfg(all(test, windows))]
mod tests {
    use super::native::borderless_style;
    use windows_sys::Win32::UI::WindowsAndMessaging::{WS_CAPTION, WS_POPUP, WS_THICKFRAME, WS_VISIBLE};

    #[test]
    fn removes_the_frame_and_keeps_the_window_visible() {
        let style = borderless_style((WS_VISIBLE | WS_CAPTION | WS_THICKFRAME) as isize) as u32;
        assert_eq!(style & (WS_CAPTION | WS_THICKFRAME), 0);
        assert_ne!(style & WS_POPUP, 0);
        assert_ne!(style & WS_VISIBLE, 0);
    }
}
