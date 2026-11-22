//! Sending files from the chat input.
//!
//! Files go straight to Discord's storage through a signed upload link, then the message
//! points at them. A file is read only after Discord accepted its size, and the account
//! token is never sent to the storage host.

use std::path::{Path, PathBuf};

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::http::DiscordHttpClient;
use crate::{AppWindow, PendingFile};

const MAX_FILES: usize = 10;

fn size_label(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let value = bytes as f64;
    if value < KB {
        format!("{bytes} B")
    } else if value < KB * KB {
        format!("{:.1} KB", value / KB)
    } else if value < KB * KB * KB {
        format!("{:.1} MB", value / (KB * KB))
    } else {
        format!("{:.2} GB", value / (KB * KB * KB))
    }
}

fn pending(ui: &AppWindow) -> Vec<PendingFile> {
    ui.get_pending_files().iter().collect()
}

fn set_pending(ui: &AppWindow, files: Vec<PendingFile>) {
    ui.set_pending_files(ModelRc::new(VecModel::from(files)));
}

/// Adds what can be sent: real, non-empty files, once each, up to Discord's limit.
fn merge(mut files: Vec<PendingFile>, picked: Vec<PathBuf>) -> Vec<PendingFile> {
    for path in picked {
        if files.len() >= MAX_FILES {
            break;
        }
        let text = path.to_string_lossy();
        if files.iter().any(|file| file.path == text.as_ref()) {
            continue;
        }
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else { continue };
        let Ok(metadata) = std::fs::metadata(&path) else { continue };
        if !metadata.is_file() || metadata.len() == 0 {
            continue;
        }
        files.push(PendingFile {
            path: text.as_ref().into(),
            name: name.into(),
            size: size_label(metadata.len()).into(),
        });
    }
    files
}

/// Opens the system's file picker without blocking the window.
pub fn pick(ui: &AppWindow) {
    let app = ui.as_weak();
    let _ = std::thread::Builder::new().name("file-picker".into()).spawn(move || {
        let picked = open_dialog();
        if picked.is_empty() {
            return;
        }
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = app.upgrade() {
                ui.set_file_upload_error("".into());
                set_pending(&ui, merge(pending(&ui), picked));
            }
        });
    });
}

/// Attaches files that came from somewhere else, such as the clipboard.
pub fn add_paths(ui: &AppWindow, paths: Vec<PathBuf>) {
    ui.set_file_upload_error("".into());
    set_pending(ui, merge(pending(ui), paths));
}

pub fn remove(ui: &AppWindow, index: usize) {
    let mut files = pending(ui);
    if index < files.len() {
        files.remove(index);
        set_pending(ui, files);
    }
}

async fn upload(http: &DiscordHttpClient, channel: &str, content: &str, files: &[(PathBuf, String)]) -> Result<(), String> {
    let mut sizes = Vec::with_capacity(files.len());
    for (path, name) in files {
        let size = tokio::fs::metadata(path).await.map_err(|e| format!("{name}: {e}"))?.len();
        sizes.push((name.clone(), size));
    }
    let targets = http.request_upload_urls(channel, &sizes).await?;
    if targets.len() != files.len() {
        return Err("Discord did not accept every file".to_string());
    }
    for ((path, name), target) in files.iter().zip(&targets) {
        let bytes = tokio::fs::read(Path::new(path)).await.map_err(|e| format!("{name}: {e}"))?;
        crate::http::put_upload(&target.upload_url, bytes).await.map_err(|e| format!("{name}: {e}"))?;
    }
    http.send_message_with_files(channel, content, &targets).await
}

/// Sends the chosen files with the typed text. On failure both stay, ready to retry.
pub fn send(ui: &AppWindow, http: Option<DiscordHttpClient>, channel: String, content: String) {
    let Some(http) = http else { return };
    if ui.get_file_uploading() || channel.is_empty() {
        return;
    }
    let files: Vec<(PathBuf, String)> = pending(ui)
        .into_iter()
        .map(|file| (PathBuf::from(file.path.as_str()), file.name.to_string()))
        .collect();
    ui.set_file_uploading(true);
    ui.set_file_upload_error("".into());
    let app = ui.as_weak();
    tokio::spawn(async move {
        let result = upload(&http, &channel, &content, &files).await;
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = app.upgrade() else { return };
            ui.set_file_uploading(false);
            match result {
                Ok(()) => set_pending(&ui, Vec::new()),
                Err(e) => {
                    log::warn!("Envio de arquivo falhou: {}", e);
                    ui.set_file_upload_error(e.into());
                    if ui.get_chat_input_text().is_empty() {
                        ui.set_chat_input_text(content.into());
                    }
                }
            }
        });
    });
}

/// The picker's buffer: one full path, or a folder followed by file names.
#[cfg(any(windows, test))]
fn parse_selection(buffer: &[u16]) -> Vec<PathBuf> {
    let end = buffer.windows(2).position(|pair| pair == [0, 0]).unwrap_or(buffer.len());
    let parts: Vec<String> = buffer[..end]
        .split(|unit| *unit == 0)
        .filter(|part| !part.is_empty())
        .map(String::from_utf16_lossy)
        .collect();
    match parts.as_slice() {
        [] => Vec::new(),
        [single] => vec![PathBuf::from(single)],
        [folder, names @ ..] => names.iter().map(|name| Path::new(folder).join(name)).collect(),
    }
}

#[cfg(windows)]
fn open_dialog() -> Vec<PathBuf> {
    use windows_sys::Win32::UI::Controls::Dialogs::{
        GetOpenFileNameW, OFN_ALLOWMULTISELECT, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_HIDEREADONLY, OFN_NOCHANGEDIR,
        OFN_PATHMUSTEXIST, OPENFILENAMEW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

    let mut buffer = vec![0u16; 64 * 1024];
    let filter: Vec<u16> = "All files\0*.*\0Images and video\0*.png;*.jpg;*.jpeg;*.gif;*.webp;*.mp4;*.webm;*.mov\0\0"
        .encode_utf16()
        .collect();
    unsafe {
        let mut dialog: OPENFILENAMEW = std::mem::zeroed();
        dialog.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
        dialog.hwndOwner = GetForegroundWindow();
        dialog.lpstrFilter = filter.as_ptr();
        dialog.lpstrFile = buffer.as_mut_ptr();
        dialog.nMaxFile = buffer.len() as u32;
        dialog.Flags = OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_ALLOWMULTISELECT | OFN_NOCHANGEDIR | OFN_HIDEREADONLY;
        if GetOpenFileNameW(&mut dialog) == 0 {
            return Vec::new();
        }
    }
    parse_selection(&buffer)
}

#[cfg(not(windows))]
fn open_dialog() -> Vec<PathBuf> {
    let attempts: [(&str, &[&str]); 2] = [
        ("zenity", &["--file-selection", "--multiple", "--separator=\n"]),
        ("kdialog", &["--getopenfilename", "--multiple", "--separate-output"]),
    ];
    for (program, args) in attempts {
        if let Ok(output) = std::process::Command::new(program).args(args).output() {
            return String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter(|line| !line.is_empty())
                .map(PathBuf::from)
                .collect();
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    #[test]
    fn one_selected_file_is_a_full_path() {
        assert_eq!(parse_selection(&wide("C:\\a\\b.png\0\0")), [PathBuf::from("C:\\a\\b.png")]);
    }

    #[test]
    fn several_files_share_their_folder() {
        let picked = parse_selection(&wide("C:\\a\0b.png\0c.txt\0\0"));
        assert_eq!(picked, [Path::new("C:\\a").join("b.png"), Path::new("C:\\a").join("c.txt")]);
    }

    #[test]
    fn a_cancelled_picker_selects_nothing() {
        assert!(parse_selection(&[0, 0]).is_empty());
    }

    #[test]
    fn sizes_are_readable() {
        assert_eq!(size_label(512), "512 B");
        assert_eq!(size_label(2048), "2.0 KB");
        assert_eq!(size_label(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn only_real_non_empty_files_are_added_once_and_up_to_the_limit() {
        let dir = std::env::temp_dir().join(format!("litecord-upload-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let full = dir.join("full.txt");
        let empty = dir.join("empty.txt");
        std::fs::write(&full, b"data").unwrap();
        std::fs::write(&empty, b"").unwrap();

        let files = merge(Vec::new(), vec![full.clone(), full.clone(), empty, dir.clone(), dir.join("missing")]);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "full.txt");
        assert_eq!(files[0].size, "4 B");

        let many: Vec<PathBuf> = (0..15).map(|i| {
            let path = dir.join(format!("{i}.txt"));
            std::fs::write(&path, b"x").unwrap();
            path
        }).collect();
        assert_eq!(merge(Vec::new(), many).len(), MAX_FILES);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
