//! Live status and activity of friends and DM contacts.
//!
//! Only presences without a server are kept: the server ones are most of the gateway
//! traffic, and the member list already receives them for the rows on screen.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use serde_json::Value;
use slint::{ComponentHandle, Model};

use crate::ui::member_list::{activity_line, status_color};
use crate::AppWindow;

const MAX_KEPT: usize = 5000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Presence {
    pub status: String,
    pub activity: String,
}

static STORE: Mutex<Option<HashMap<String, Presence>>> = Mutex::new(None);
static APP: Mutex<Option<slint::Weak<AppWindow>>> = Mutex::new(None);
static REFRESH_PENDING: AtomicBool = AtomicBool::new(false);

fn read(entry: &Value) -> Option<(String, Presence)> {
    let id = entry["user"]["id"].as_str().or_else(|| entry["user_id"].as_str())?;
    Some((
        id.to_owned(),
        Presence {
            status: entry["status"].as_str().unwrap_or("offline").to_owned(),
            activity: activity_line(entry),
        },
    ))
}

fn keep(entry: &Value) {
    let Some((id, presence)) = read(entry) else { return };
    let mut store = STORE.lock().unwrap_or_else(|e| e.into_inner());
    let map = store.get_or_insert_with(HashMap::new);
    if map.len() < MAX_KEPT || map.contains_key(&id) {
        map.insert(id, presence);
    }
}

pub fn get(user_id: &str) -> Option<Presence> {
    STORE.lock().unwrap_or_else(|e| e.into_inner()).as_ref()?.get(user_id).cloned()
}

pub fn status_label(status: &str) -> &'static str {
    match status {
        "online" => "Online",
        "idle" => "Idle",
        "dnd" => "Do Not Disturb",
        _ => "Offline",
    }
}

pub fn install(ui: &AppWindow) {
    *APP.lock().unwrap_or_else(|e| e.into_inner()) = Some(ui.as_weak());
}

pub(crate) fn schedule_refresh() {
    if REFRESH_PENDING.swap(true, Ordering::AcqRel) {
        return;
    }
    let Some(app) = APP.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
        REFRESH_PENDING.store(false, Ordering::Release);
        return;
    };
    let _ = slint::invoke_from_event_loop(move || {
        REFRESH_PENDING.store(false, Ordering::Release);
        if let Some(ui) = app.upgrade() {
            refresh_dms(&ui);
            crate::ui::channel_tools::apply_mutes(&ui);
            crate::ui::voice_flags::show_stream_viewers(&ui);
            crate::ui::guild_folders::refresh_mutes(&ui);
        }
    });
}

/// A friend's PRESENCE_UPDATE.
pub fn on_event(data: &Value) {
    keep(data);
    schedule_refresh();
}

/// The friends' presences of READY_SUPPLEMENTAL.
pub fn on_ready(data: &Value) {
    for entry in data["merged_presences"]["friends"].as_array().into_iter().flatten() {
        keep(entry);
    }
    schedule_refresh();
}

/// Updates the status dot and activity of the DM rows on screen.
pub fn refresh_dms(ui: &AppWindow) {
    let rows = ui.get_channels();
    for index in 0..rows.row_count() {
        let Some(mut row) = rows.row_data(index).filter(|row| !row.dm_user.is_empty()) else { continue };
        let presence = get(row.dm_user.as_str());
        let (has_status, color, activity) = match &presence {
            Some(p) => (true, status_color(&p.status), p.activity.as_str()),
            None => (false, row.status_color, ""),
        };
        if row.has_status != has_status || row.status_color != color || row.activity != activity {
            row.has_status = has_status;
            row.status_color = color;
            row.activity = activity.into();
            rows.set_row_data(index, row);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_both_presence_shapes() {
        let live = json!({ "user": { "id": "1" }, "status": "dnd", "activities": [{ "type": 0, "name": "Chess" }] });
        let (id, presence) = read(&live).unwrap();
        assert_eq!((id.as_str(), presence.status.as_str(), presence.activity.as_str()), ("1", "dnd", "Playing Chess"));
        let merged = json!({ "user_id": "2", "status": "idle", "activities": [{ "type": 2, "name": "Spotify" }] });
        assert_eq!(read(&merged).unwrap().1.activity, "Listening to Spotify");
    }

    #[test]
    fn labels_every_status() {
        assert_eq!(status_label("dnd"), "Do Not Disturb");
        assert_eq!(status_label("invisible"), "Offline");
    }
}
