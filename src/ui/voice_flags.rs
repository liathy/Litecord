//! Mute and deafen follow Discord: deafening also mutes, and undeafening gives the
//! microphone back as it was. Each person's stream volume is kept apart from their voice.

use std::sync::atomic::{AtomicBool, Ordering};

use slint::Model;

use crate::AppWindow;

static MUTED_BEFORE_DEAFEN: AtomicBool = AtomicBool::new(false);
static STREAM_VIEWERS: std::sync::Mutex<Option<std::collections::HashMap<u64, i32>>> = std::sync::Mutex::new(None);

/// The streamer of `guild:<id>:<channel>:<user>` or `call:<channel>:<user>`.
fn stream_owner(key: &str) -> Option<u64> {
    key.rsplit(':').next()?.parse().ok()
}

/// STREAM_CREATE and STREAM_UPDATE carry who is watching.
pub fn on_stream_event(data: &serde_json::Value) {
    let (Some(owner), Some(viewers)) = (data["stream_key"].as_str().and_then(stream_owner), data["viewer_ids"].as_array()) else { return };
    STREAM_VIEWERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(std::collections::HashMap::new)
        .insert(owner, viewers.len() as i32);
    crate::ui::presence::schedule_refresh();
}

pub fn show_stream_viewers(ui: &AppWindow) {
    let owner = ui.get_active_stream_user_id().parse::<u64>().unwrap_or(0);
    let count = STREAM_VIEWERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|map| map.get(&owner).copied())
        .unwrap_or(0);
    if ui.get_active_stream_viewers() != count {
        ui.set_active_stream_viewers(count);
    }
}

fn next_mute(muted: bool, deafened: bool) -> (bool, bool) {
    if deafened {
        (false, false)
    } else {
        (!muted, false)
    }
}

/// Returns (muted, deafened, the microphone state to restore).
fn next_deafen(muted: bool, deafened: bool, saved: bool) -> (bool, bool, bool) {
    if deafened {
        (saved, false, saved)
    } else {
        (true, true, muted)
    }
}

/// The new (muted, deafened) after pressing the microphone button.
pub fn toggle_mute(muted: bool, deafened: bool) -> (bool, bool) {
    next_mute(muted, deafened)
}

/// The new (muted, deafened) after pressing the headphones button.
pub fn toggle_deafen(muted: bool, deafened: bool) -> (bool, bool) {
    let saved = MUTED_BEFORE_DEAFEN.load(Ordering::Relaxed);
    let (new_muted, new_deafened, new_saved) = next_deafen(muted, deafened, saved);
    MUTED_BEFORE_DEAFEN.store(new_saved, Ordering::Relaxed);
    (new_muted, new_deafened)
}

/// Shows a stream's volume on that person's tile only.
pub fn show_stream_volume(ui: &AppWindow, user_id: &str, volume: f32) {
    if ui.get_active_stream_user_id() == user_id {
        ui.set_active_stream_volume(volume.clamp(0.0, 1.0));
    }
    let model = ui.get_voice_participants();
    for row in 0..model.row_count() {
        if let Some(mut participant) = model.row_data(row) {
            if participant.user_id == user_id {
                participant.stream_volume = volume;
                model.set_row_data(row, participant);
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_streamer_from_a_stream_key() {
        assert_eq!(stream_owner("guild:1:2:33"), Some(33));
        assert_eq!(stream_owner("call:2:44"), Some(44));
        assert_eq!(stream_owner("bad"), None);
    }

    #[test]
    fn deafening_also_mutes_and_undeafening_restores_an_open_mic() {
        let (muted, deafened, saved) = next_deafen(false, false, false);
        assert_eq!((muted, deafened), (true, true));
        let (muted, deafened, _) = next_deafen(muted, deafened, saved);
        assert_eq!((muted, deafened), (false, false));
    }

    #[test]
    fn a_mic_muted_before_deafening_stays_muted_after() {
        let (muted, deafened, saved) = next_deafen(true, false, false);
        assert_eq!((muted, deafened), (true, true));
        let (muted, deafened, _) = next_deafen(muted, deafened, saved);
        assert_eq!((muted, deafened), (true, false));
    }

    #[test]
    fn the_mic_button_while_deafened_turns_both_off() {
        assert_eq!(next_mute(true, true), (false, false));
    }

    #[test]
    fn the_mic_button_otherwise_only_toggles_the_mic() {
        assert_eq!(next_mute(false, false), (true, false));
        assert_eq!(next_mute(true, false), (false, false));
    }
}
