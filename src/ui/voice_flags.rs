//! Mute and deafen follow Discord: deafening also mutes, and undeafening gives the
//! microphone back as it was. Each person's stream volume is kept apart from their voice.

use std::sync::atomic::{AtomicBool, Ordering};

use slint::Model;

use crate::AppWindow;

static MUTED_BEFORE_DEAFEN: AtomicBool = AtomicBool::new(false);

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
