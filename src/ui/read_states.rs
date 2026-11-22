//! What the account has read: unread channels, mention counts and the NEW divider.
//!
//! Discord's read positions arrive at login and change through messages and acks. Only
//! a change between read and unread touches the interface, so busy servers cost nothing.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::Method;
use serde_json::{json, Value};

use crate::http::DiscordHttpClient;

const ACK_DELAY: Duration = Duration::from_secs(3);

#[derive(Default)]
struct ReadStates {
    /// Channel to (last read message, mentions).
    read: HashMap<String, (u64, i32)>,
    latest: HashMap<String, u64>,
    guild_of: HashMap<String, String>,
    channels_of: HashMap<String, HashSet<String>>,
    divider: HashMap<String, u64>,
    unsent_acks: HashMap<String, u64>,
}

fn id_of(value: &Value) -> u64 {
    value.as_str().and_then(|s| s.parse().ok()).or_else(|| value.as_u64()).unwrap_or(0)
}

impl ReadStates {
    fn load(&mut self, value: &Value) {
        let list = value.as_array().or_else(|| value["entries"].as_array());
        for entry in list.into_iter().flatten() {
            if entry["read_state_type"].as_u64().unwrap_or(0) != 0 {
                continue;
            }
            let Some(channel) = entry["id"].as_str().or_else(|| entry["channel_id"].as_str()) else { continue };
            self.read.insert(channel.to_owned(), (id_of(&entry["last_message_id"]), entry["mention_count"].as_i64().unwrap_or(0) as i32));
        }
    }

    fn register(&mut self, guild: &str, channel: &str, last_message: u64) {
        self.guild_of.insert(channel.to_owned(), guild.to_owned());
        self.channels_of.entry(guild.to_owned()).or_default().insert(channel.to_owned());
        if last_message > 0 {
            let latest = self.latest.entry(channel.to_owned()).or_insert(0);
            *latest = (*latest).max(last_message);
        }
    }

    fn has_unread(&self, channel: &str) -> bool {
        match (self.read.get(channel), self.latest.get(channel)) {
            (Some((read, _)), Some(latest)) => latest > read,
            _ => false,
        }
    }

    fn mentions(&self, channel: &str) -> i32 {
        self.read.get(channel).map_or(0, |(_, count)| *count)
    }

    /// Returns true when what the interface shows for this channel changed.
    fn message(&mut self, guild: &str, channel: &str, id: u64, mentioned: bool, reading: bool) -> bool {
        let before = (self.has_unread(channel), self.mentions(channel));
        let previous = self.latest.get(channel).copied().unwrap_or(id.saturating_sub(1));
        self.register(guild, channel, id);
        let entry = self.read.entry(channel.to_owned()).or_insert((previous, 0));
        if reading {
            *entry = (id, 0);
            self.unsent_acks.insert(channel.to_owned(), id);
        } else if mentioned {
            entry.1 += 1;
        }
        before != (self.has_unread(channel), self.mentions(channel))
    }

    fn ack(&mut self, channel: &str, id: u64, mentions: i32) -> bool {
        let before = (self.has_unread(channel), self.mentions(channel));
        let entry = self.read.entry(channel.to_owned()).or_insert((0, 0));
        entry.0 = id;
        entry.1 = mentions;
        before != (self.has_unread(channel), self.mentions(channel))
    }

    /// Opening a channel: remembers where NEW goes and returns the message to ack.
    fn open(&mut self, channel: &str) -> Option<u64> {
        let (read, mentions) = self.read.get(channel).copied()?;
        let latest = self.latest.get(channel).copied().unwrap_or(0);
        if latest <= read && mentions == 0 {
            self.divider.remove(channel);
            return None;
        }
        if latest > read {
            self.divider.insert(channel.to_owned(), read);
        }
        let newest = latest.max(read);
        self.read.insert(channel.to_owned(), (newest, 0));
        Some(newest)
    }
}

static STATE: Mutex<Option<ReadStates>> = Mutex::new(None);
static CLIENT: Mutex<Option<Arc<Mutex<Option<DiscordHttpClient>>>>> = Mutex::new(None);
static FLUSHING: AtomicBool = AtomicBool::new(false);

fn with<R>(job: impl FnOnce(&mut ReadStates) -> R) -> R {
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    job(guard.get_or_insert_with(ReadStates::default))
}

pub fn install(http: Arc<Mutex<Option<DiscordHttpClient>>>) {
    *CLIENT.lock().unwrap_or_else(|e| e.into_inner()) = Some(http);
}

pub fn load(value: &Value) {
    with(|state| state.load(value));
    crate::ui::presence::schedule_refresh();
}

pub fn register_channel(guild: &str, channel: &str, last_message: Option<&str>) {
    let id = last_message.and_then(|s| s.parse().ok()).unwrap_or(0);
    with(|state| state.register(guild, channel, id));
}

pub fn has_unread(channel: &str) -> bool {
    with(|state| state.has_unread(channel))
}

pub fn mentions(channel: &str) -> i32 {
    with(|state| state.mentions(channel))
}

pub fn guild_of(channel: &str) -> Option<String> {
    with(|state| state.guild_of.get(channel).cloned())
}

pub fn channels_of(guild: &str) -> Vec<String> {
    with(|state| state.channels_of.get(guild).map(|set| set.iter().cloned().collect()).unwrap_or_default())
}

/// A server's dot and badge: unread only counts channels that aren't muted.
pub fn guild_summary(guild: &str) -> (bool, i32) {
    let channels = channels_of(guild);
    let server_muted = crate::ui::notifications::is_guild_muted(guild);
    with(|state| {
        let mentions = channels.iter().map(|c| state.mentions(c)).sum();
        let unread = !server_muted
            && channels.iter().any(|c| state.has_unread(c) && !crate::ui::notifications::is_channel_muted(guild, c));
        (unread, mentions)
    })
}

/// Read positions to send for "Mark As Read" on a server.
pub fn mark_guild_read(guild: &str) -> Vec<(String, u64)> {
    let channels = channels_of(guild);
    let marked = with(|state| {
        let targets: Vec<(String, u64)> = channels
            .iter()
            .filter(|c| state.has_unread(c) || state.mentions(c) > 0)
            .filter_map(|c| Some((c.clone(), state.latest.get(c).copied()?)))
            .collect();
        for (channel, newest) in &targets {
            state.read.insert(channel.clone(), (*newest, 0));
        }
        targets
    });
    crate::ui::presence::schedule_refresh();
    marked
}

fn flush_acks() {
    if FLUSHING.swap(true, Ordering::SeqCst) {
        return;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        FLUSHING.store(false, Ordering::SeqCst);
        return;
    };
    handle.spawn(async {
        tokio::time::sleep(ACK_DELAY).await;
        FLUSHING.store(false, Ordering::SeqCst);
        let pending: Vec<(String, u64)> = with(|state| state.unsent_acks.drain().collect());
        let client = CLIENT.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|c| c.lock().ok()?.as_ref().cloned());
        let Some(http) = client else { return };
        for (channel, id) in pending {
            let _ = http.discord_call(Method::POST, &format!("/channels/{channel}/messages/{id}/ack"), Some(json!({ "token": null }))).await;
        }
    });
}

/// A new message from the gateway.
pub fn on_message(message: &Value, my_id: &str) {
    let Some(channel) = message["channel_id"].as_str() else { return };
    let id = id_of(&message["id"]);
    if id == 0 {
        return;
    }
    let guild = message["guild_id"].as_str().unwrap_or("@me");
    let mine = message["author"]["id"].as_str() == Some(my_id);
    let is_dm = guild == "@me";
    let mentioned = !mine
        && !is_dm
        && (message["mention_everyone"].as_bool() == Some(true)
            || message["mentions"].as_array().is_some_and(|users| users.iter().any(|u| u["id"].as_str() == Some(my_id))));
    let reading = mine
        || (crate::ui::notifications::open_channel() == channel
            && crate::ui::helpers::APP_IS_VISIBLE.load(Ordering::Relaxed));
    let changed = with(|state| state.message(guild, channel, id, mentioned, reading));
    if reading && !mine {
        flush_acks();
    } else if mine {
        with(|state| state.unsent_acks.remove(channel));
    }
    if changed {
        crate::ui::presence::schedule_refresh();
    }
}

/// MESSAGE_ACK: this account read a channel, here or on another device.
pub fn on_ack(data: &Value) {
    let Some(channel) = data["channel_id"].as_str() else { return };
    let mentions = data["mention_count"].as_i64().unwrap_or(0) as i32;
    if with(|state| state.ack(channel, id_of(&data["message_id"]), mentions)) {
        crate::ui::presence::schedule_refresh();
    }
}

/// Opening a channel marks it read on Discord.
pub fn opened(http: &DiscordHttpClient, channel: &str) {
    let Some(id) = with(|state| state.open(channel)) else { return };
    crate::ui::presence::schedule_refresh();
    let (http, channel) = (http.clone(), channel.to_owned());
    tokio::spawn(async move {
        let _ = http.discord_call(Method::POST, &format!("/channels/{channel}/messages/{id}/ack"), Some(json!({ "token": null }))).await;
    });
}

/// The first message after what was read when this channel was opened.
pub fn new_divider_index(channel: &str, ids_oldest_first: &[u64]) -> Option<usize> {
    let read = with(|state| state.divider.get(channel).copied())?;
    ids_oldest_first.iter().position(|id| *id > read)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> ReadStates {
        let mut s = ReadStates::default();
        s.load(&json!([{ "id": "1", "last_message_id": "100", "mention_count": 0 }]));
        s.register("9", "1", 100);
        s
    }

    #[test]
    fn a_new_message_makes_a_channel_unread_once() {
        let mut s = state();
        assert!(!s.has_unread("1"));
        assert!(s.message("9", "1", 101, false, false));
        assert!(s.has_unread("1"));
        assert!(!s.message("9", "1", 102, false, false), "already unread");
    }

    #[test]
    fn mentions_add_up_and_reading_clears_them() {
        let mut s = state();
        s.message("9", "1", 101, true, false);
        s.message("9", "1", 102, true, false);
        assert_eq!(s.mentions("1"), 2);
        assert_eq!(s.open("1"), Some(102));
        assert_eq!((s.has_unread("1"), s.mentions("1")), (false, 0));
    }

    #[test]
    fn opening_remembers_where_new_starts() {
        let mut s = state();
        s.message("9", "1", 105, false, false);
        s.open("1");
        assert_eq!(s.divider.get("1"), Some(&100));
        assert_eq!(s.open("1"), None);
        assert!(!s.divider.contains_key("1"));
    }

    #[test]
    fn the_open_channel_stays_read() {
        let mut s = state();
        assert!(!s.message("9", "1", 101, true, true));
        assert_eq!((s.has_unread("1"), s.mentions("1")), (false, 0));
        assert_eq!(s.unsent_acks.get("1"), Some(&101));
    }

    #[test]
    fn an_ack_from_another_device_clears_the_channel() {
        let mut s = state();
        s.message("9", "1", 101, true, false);
        assert!(s.ack("1", 101, 0));
        assert!(!s.has_unread("1"));
    }

    #[test]
    fn reads_the_wrapped_login_format() {
        let mut s = ReadStates::default();
        s.load(&json!({ "entries": [
            { "id": "1", "last_message_id": "5", "mention_count": 2 },
            { "id": "2", "read_state_type": 1, "last_message_id": "5" },
        ] }));
        assert_eq!(s.mentions("1"), 2);
        assert!(!s.read.contains_key("2"));
    }
}
