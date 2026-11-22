//! Connection details of the call: voice ping and received packet loss.
//!
//! Counters are plain atomics on the audio path, and the popup reads them only while open.

use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use slint::ComponentHandle;

use crate::AppWindow;

const REFRESH: Duration = Duration::from_secs(2);
/// Weight of the newest ping in the average, in percent.
const AVERAGE_WEIGHT: u64 = 20;

static HEARTBEAT_SENT_MS: AtomicU64 = AtomicU64::new(0);
static LAST_PING_MS: AtomicU64 = AtomicU64::new(0);
static AVERAGE_PING_MS: AtomicU64 = AtomicU64::new(0);
static PACKETS_RECEIVED: AtomicU64 = AtomicU64::new(0);
static PACKETS_LOST: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static TIMER: RefCell<Option<slint::Timer>> = const { RefCell::new(None) };
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

/// A new voice connection: figures of the last one no longer apply.
pub fn reset() {
    for counter in [&HEARTBEAT_SENT_MS, &LAST_PING_MS, &AVERAGE_PING_MS, &PACKETS_RECEIVED, &PACKETS_LOST] {
        counter.store(0, Ordering::Relaxed);
    }
}

pub fn heartbeat_sent(at_ms: u64) {
    HEARTBEAT_SENT_MS.store(at_ms, Ordering::Relaxed);
}

fn next_average(average: u64, ping: u64) -> u64 {
    if average == 0 {
        return ping;
    }
    (average * (100 - AVERAGE_WEIGHT) + ping * AVERAGE_WEIGHT + 50) / 100
}

pub fn heartbeat_acked() {
    let sent = HEARTBEAT_SENT_MS.swap(0, Ordering::Relaxed);
    if sent == 0 {
        return;
    }
    let ping = now_ms().saturating_sub(sent).max(1);
    LAST_PING_MS.store(ping, Ordering::Relaxed);
    AVERAGE_PING_MS.store(next_average(AVERAGE_PING_MS.load(Ordering::Relaxed), ping), Ordering::Relaxed);
}

#[inline]
pub fn packet_received() {
    PACKETS_RECEIVED.fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub fn packets_lost(count: u64) {
    PACKETS_LOST.fetch_add(count, Ordering::Relaxed);
}

fn loss_text(received: u64, lost: u64) -> String {
    let expected = received + lost;
    if expected == 0 {
        return "No audio received yet".to_owned();
    }
    format!("{:.1}%", lost as f64 * 100.0 / expected as f64)
}

fn ping_text(ping: u64) -> String {
    if ping == 0 { "Measuring...".to_owned() } else { format!("{ping} ms") }
}

/// Discord's bar color: green under 150 ms, yellow under 300 ms, then red.
fn quality(ping: u64) -> i32 {
    match ping {
        0 => 0,
        1..=149 => 1,
        150..=299 => 2,
        _ => 3,
    }
}

fn show(ui: &AppWindow) {
    let (last, average) = (LAST_PING_MS.load(Ordering::Relaxed), AVERAGE_PING_MS.load(Ordering::Relaxed));
    ui.set_voice_stats_ping(ping_text(last).into());
    ui.set_voice_stats_average(ping_text(average).into());
    ui.set_voice_stats_loss(loss_text(PACKETS_RECEIVED.load(Ordering::Relaxed), PACKETS_LOST.load(Ordering::Relaxed)).into());
    ui.set_voice_stats_quality(quality(average));
}

fn close(ui: &AppWindow) {
    ui.set_voice_stats_open(false);
    TIMER.with_borrow_mut(|timer| *timer = None);
}

pub fn install(ui: &AppWindow) {
    let app_tick = ui.as_weak();
    ui.on_voice_stats_tick(move || {
        if let Some(ui) = app_tick.upgrade() {
            ui.set_voice_stats_quality(quality(AVERAGE_PING_MS.load(Ordering::Relaxed)));
        }
    });
    let app = ui.as_weak();
    ui.on_voice_stats_toggle(move || {
        let Some(ui) = app.upgrade() else { return };
        if ui.get_voice_stats_open() {
            close(&ui);
            return;
        }
        show(&ui);
        ui.set_voice_stats_open(true);
        let weak = ui.as_weak();
        let timer = slint::Timer::default();
        timer.start(slint::TimerMode::Repeated, REFRESH, move || {
            if let Some(ui) = weak.upgrade() {
                if ui.get_is_in_voice() {
                    show(&ui);
                } else {
                    close(&ui);
                }
            }
        });
        TIMER.with_borrow_mut(|slot| *slot = Some(timer));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn average_starts_at_the_first_ping_and_smooths() {
        assert_eq!(next_average(0, 80), 80);
        assert_eq!(next_average(100, 200), 120);
    }

    #[test]
    fn loss_is_a_share_of_expected_packets() {
        assert_eq!(loss_text(0, 0), "No audio received yet");
        assert_eq!(loss_text(95, 5), "5.0%");
        assert_eq!(loss_text(1000, 0), "0.0%");
    }

    #[test]
    fn quality_follows_the_ping() {
        assert_eq!((quality(0), quality(40), quality(200), quality(400)), (0, 1, 2, 3));
    }
}
