//! Pasting screenshots and copied files into the chat box. Plain text stays with the
//! text field; this only takes over when the clipboard holds a picture or files.

use std::path::PathBuf;

use slint::ComponentHandle;

use crate::AppWindow;

const MAX_SIDE: i32 = 16_384;
const MAX_KEPT_PASTES: usize = 10;

/// RGBA pixels from a clipboard bitmap (DIB): 24 or 32 bits, top-down or bottom-up.
pub(crate) fn dib_to_rgba(dib: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let read_u32 = |at: usize| Some(u32::from_le_bytes(dib.get(at..at + 4)?.try_into().ok()?));
    let header = read_u32(0)? as usize;
    let width = read_u32(4)? as i32;
    let height = read_u32(8)? as i32;
    let bits = u16::from_le_bytes(dib.get(14..16)?.try_into().ok()?);
    let compression = read_u32(16)?;
    if header < 40 || width <= 0 || height == 0 || width > MAX_SIDE || height.abs() > MAX_SIDE {
        return None;
    }
    if !matches!(bits, 24 | 32) || !matches!(compression, 0 | 3) {
        return None;
    }
    let masks = if compression == 3 && header == 40 { 12 } else { 0 };
    let start = header + masks;
    let (w, h) = (width as usize, height.unsigned_abs() as usize);
    let bytes = bits as usize / 8;
    let stride = (w * bytes + 3) & !3;
    if dib.len() < start + stride.checked_mul(h)? {
        return None;
    }
    let has_alpha = bits == 32 && (0..h).any(|row| dib[start + row * stride..start + row * stride + w * 4].chunks_exact(4).any(|p| p[3] != 0));
    let mut out = Vec::with_capacity(w * h * 4);
    for row in 0..h {
        let source = if height > 0 { h - 1 - row } else { row };
        let line = &dib[start + source * stride..start + source * stride + w * bytes];
        for pixel in line.chunks_exact(bytes) {
            out.extend_from_slice(&[pixel[2], pixel[1], pixel[0], if has_alpha { pixel[3] } else { 255 }]);
        }
    }
    Some((w as u32, h as u32, out))
}

fn paste_folder() -> PathBuf {
    std::env::temp_dir().join("litecord-paste")
}

/// Keeps only the newest pasted pictures on disk.
fn trim_folder(folder: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(folder) else { return };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .collect();
    files.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
    for (_, path) in files.into_iter().skip(MAX_KEPT_PASTES) {
        let _ = std::fs::remove_file(path);
    }
}

fn save_png(dib: &[u8]) -> Option<PathBuf> {
    let (width, height, pixels) = dib_to_rgba(dib)?;
    let picture = image::RgbaImage::from_raw(width, height, pixels)?;
    let folder = paste_folder();
    std::fs::create_dir_all(&folder).ok()?;
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_millis();
    let path = folder.join(format!("image-{stamp}.png"));
    picture.save_with_format(&path, image::ImageFormat::Png).ok()?;
    trim_folder(&folder);
    Some(path)
}

fn add_dib(ui: &AppWindow, dib: Vec<u8>) {
    let app = ui.as_weak();
    let _ = std::thread::Builder::new().name("clipboard-image".into()).spawn(move || {
        let saved = save_png(&dib);
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = app.upgrade() else { return };
            match saved {
                Some(path) => crate::ui::file_upload::add_paths(&ui, vec![path]),
                None => ui.set_file_upload_error("Couldn't read that image.".into()),
            }
        });
    });
}

#[cfg(windows)]
enum Clip {
    Files(Vec<PathBuf>),
    Picture(Vec<u8>),
}

#[cfg(windows)]
fn read_clipboard() -> Option<Clip> {
    use windows_sys::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard};
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    use windows_sys::Win32::UI::Shell::{DragQueryFileW, HDROP};

    const CF_DIB: u32 = 8;
    const CF_HDROP: u32 = 15;
    unsafe {
        let has_files = IsClipboardFormatAvailable(CF_HDROP) != 0;
        if !has_files && IsClipboardFormatAvailable(CF_DIB) == 0 {
            return None;
        }
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return None;
        }
        let result = if has_files {
            let drop = GetClipboardData(CF_HDROP) as HDROP;
            let count = if drop.is_null() { 0 } else { DragQueryFileW(drop, u32::MAX, std::ptr::null_mut(), 0) };
            let files = (0..count)
                .map(|index| {
                    let length = DragQueryFileW(drop, index, std::ptr::null_mut(), 0) as usize;
                    let mut buffer = vec![0u16; length + 1];
                    DragQueryFileW(drop, index, buffer.as_mut_ptr(), buffer.len() as u32);
                    PathBuf::from(String::from_utf16_lossy(&buffer[..length]))
                })
                .collect::<Vec<_>>();
            (!files.is_empty()).then_some(Clip::Files(files))
        } else {
            let handle = GetClipboardData(CF_DIB);
            let data = GlobalLock(handle);
            if data.is_null() {
                None
            } else {
                let copied = std::slice::from_raw_parts(data as *const u8, GlobalSize(handle)).to_vec();
                GlobalUnlock(handle);
                Some(Clip::Picture(copied))
            }
        };
        CloseClipboard();
        result
    }
}

/// True when the clipboard held a picture or files and they were attached.
#[cfg(windows)]
pub fn paste(ui: &AppWindow) -> bool {
    match read_clipboard() {
        Some(Clip::Files(files)) => {
            crate::ui::file_upload::add_paths(ui, files);
            true
        }
        Some(Clip::Picture(dib)) => {
            add_dib(ui, dib);
            true
        }
        None => false,
    }
}

#[cfg(not(windows))]
pub fn paste(_ui: &AppWindow) -> bool {
    let _ = add_dib;
    false
}

#[cfg(test)]
mod tests {
    use super::dib_to_rgba;

    fn dib(width: i32, height: i32, bits: u16, pixels: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&40u32.to_le_bytes());
        out.extend_from_slice(&width.to_le_bytes());
        out.extend_from_slice(&height.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&bits.to_le_bytes());
        out.extend_from_slice(&[0u8; 24]);
        out.extend_from_slice(pixels);
        out
    }

    #[test]
    fn bottom_up_rows_are_flipped_and_blue_red_swapped() {
        let data = dib(1, 2, 32, &[1, 2, 3, 0, 4, 5, 6, 0]);
        let (w, h, rgba) = dib_to_rgba(&data).unwrap();
        assert_eq!((w, h), (1, 2));
        assert_eq!(rgba, [6, 5, 4, 255, 3, 2, 1, 255]);
    }

    #[test]
    fn real_alpha_is_kept() {
        let (_, _, rgba) = dib_to_rgba(&dib(1, -1, 32, &[1, 2, 3, 128])).unwrap();
        assert_eq!(rgba, [3, 2, 1, 128]);
    }

    #[test]
    fn rows_of_24_bit_images_are_padded() {
        let (_, _, rgba) = dib_to_rgba(&dib(1, -2, 24, &[1, 2, 3, 0, 4, 5, 6, 0])).unwrap();
        assert_eq!(rgba, [3, 2, 1, 255, 6, 5, 4, 255]);
    }

    #[test]
    fn truncated_or_unknown_data_is_refused() {
        assert!(dib_to_rgba(&dib(2, 2, 32, &[0; 4])).is_none());
        assert!(dib_to_rgba(&dib(1, 1, 8, &[0; 4])).is_none());
        assert!(dib_to_rgba(&[0; 8]).is_none());
    }
}
