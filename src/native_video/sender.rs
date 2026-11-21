//! State of the outgoing Go Live stream: frames from the encoder, the plan's quality, and
//! the rule that video never leaves unencrypted while the call uses DAVE. The connection
//! itself is in `webrtc_tx`.

use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Mutex;

use davey::{Codec, DaveSession, MediaType};
use log::info;
use tokio::sync::mpsc;

static FRAME_TX: Mutex<Option<mpsc::Sender<Vec<u8>>>> = Mutex::new(None);
static AUDIO_TX: Mutex<Option<mpsc::Sender<Vec<u8>>>> = Mutex::new(None);
static IS_BROADCASTING: AtomicBool = AtomicBool::new(false);
static ACTIVE_STREAM_KEY: Mutex<Option<String>> = Mutex::new(None);
static PREMIUM_TYPE: AtomicU8 = AtomicU8::new(0);
static OUTBOUND_QUALITY: Mutex<(u32, u32, u32)> = Mutex::new((1280, 720, 30));

/// Discord's payload types for H.264 (103/104 belong to H.265).
pub const H264_PAYLOAD_TYPE: u8 = 101;
pub const H264_RTX_PAYLOAD_TYPE: u8 = 102;

/// The account's Nitro plan, from the gateway READY payload.
pub fn set_premium_type(premium_type: u8) {
    PREMIUM_TYPE.store(premium_type, Ordering::Relaxed);
}

pub fn premium_type() -> u8 {
    PREMIUM_TYPE.load(Ordering::Relaxed)
}

/// The Go Live quality the account's plan allows for the requested one.
/// 2 = Nitro, 1 = Nitro Classic; Nitro Basic and free accounts stream at 720p30.
pub fn go_live_limit(premium_type: u8, height: i32, fps: i32) -> (i32, i32) {
    match premium_type {
        2 => (height, fps),
        1 => (height.min(1080), fps),
        _ => (height.min(720), fps.min(30)),
    }
}

/// What the encoder really produces, announced to Discord as the stream's quality.
pub fn set_outbound_quality(height: u32, fps: u32) {
    *OUTBOUND_QUALITY.lock().unwrap() = (height * 16 / 9, height, fps);
}

pub fn outbound_quality() -> (u32, u32, u32) {
    *OUTBOUND_QUALITY.lock().unwrap()
}

/// The frame to send. While the call uses DAVE, nothing leaves unencrypted: frames
/// are dropped until the group is ready, because viewers would discard them anyway.
pub(super) fn protect_frame<'a>(
    dave: &Mutex<Option<DaveSession>>,
    dave_required: bool,
    frame: &'a [u8],
) -> Option<Cow<'a, [u8]>> {
    if !dave_required {
        return Some(Cow::Borrowed(frame));
    }
    let mut guard = dave.lock().unwrap();
    let session = guard.as_mut()?;
    if !session.is_ready() {
        return None;
    }
    session.encrypt(MediaType::VIDEO, Codec::H264, frame).ok()
}

/// The same rule for the stream's sound.
pub(super) fn protect_audio<'a>(
    dave: &Mutex<Option<DaveSession>>,
    dave_required: bool,
    packet: &'a [u8],
) -> Option<Cow<'a, [u8]>> {
    if !dave_required {
        return Some(Cow::Borrowed(packet));
    }
    let mut guard = dave.lock().unwrap();
    let session = guard.as_mut()?;
    if !session.is_ready() {
        return None;
    }
    session.encrypt(MediaType::AUDIO, Codec::OPUS, packet).ok()
}

/// Hands one 20 ms Opus packet to the stream. Non-blocking: late audio is dropped.
pub fn offer_outbound_audio(packet: Vec<u8>) {
    if let Some(tx) = AUDIO_TX.lock().unwrap().as_ref() {
        let _ = tx.try_send(packet);
    }
}

/// Opens the queue of Opus packets, after `begin_outbound`.
pub(super) fn begin_outbound_audio() -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel::<Vec<u8>>(8);
    *AUDIO_TX.lock().unwrap() = Some(tx);
    rx
}

/// Returns true if an outbound Discord stream is actively broadcasting.
pub fn is_broadcasting() -> bool {
    IS_BROADCASTING.load(Ordering::Relaxed)
}

/// Hands an Annex-B encoded H.264 frame to the outbound transmitter.
/// Non-blocking: drops frames if the queue is full to avoid adding latency.
pub fn offer_outbound_frame(annex_b: &[u8]) {
    if !is_broadcasting() || annex_b.is_empty() {
        return;
    }
    let guard = FRAME_TX.lock().unwrap();
    if let Some(tx) = guard.as_ref() {
        let _ = tx.try_send(annex_b.to_vec());
    }
}

/// Stops any active outbound stream sender.
pub fn stop_sender(stream_key: &str) {
    let mut key_guard = ACTIVE_STREAM_KEY.lock().unwrap();
    if key_guard.as_deref() == Some(stream_key) || stream_key.is_empty() {
        *key_guard = None;
        IS_BROADCASTING.store(false, Ordering::SeqCst);
        let mut tx_guard = FRAME_TX.lock().unwrap();
        *tx_guard = None;
        *AUDIO_TX.lock().unwrap() = None;
        info!("🛑 [GO LIVE TX] Transmissor nativo de stream encerrado: {}", stream_key);
    }
}

/// Opens the queue of encoded frames for a stream that just connected.
pub fn begin_outbound(stream_key: &str) -> mpsc::Receiver<Vec<u8>> {
    stop_sender("");
    let (tx, rx) = mpsc::channel::<Vec<u8>>(5);
    *FRAME_TX.lock().unwrap() = Some(tx);
    *ACTIVE_STREAM_KEY.lock().unwrap() = Some(stream_key.to_owned());
    IS_BROADCASTING.store(true, Ordering::SeqCst);
    crate::screen_capture::types::request_intra_keyframe();
    info!("✅ [GO LIVE TX] Transmissão nativa ativa key={}", stream_key);
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU16;

    #[test]
    fn go_live_quality_follows_the_plan() {
        assert_eq!(go_live_limit(0, 1440, 60), (720, 30));
        assert_eq!(go_live_limit(3, 1080, 60), (720, 30));
        assert_eq!(go_live_limit(1, 1440, 60), (1080, 60));
        assert_eq!(go_live_limit(2, 1440, 60), (1440, 60));
        assert_eq!(go_live_limit(0, 480, 15), (480, 15));
    }

    #[test]
    fn video_never_leaves_unencrypted_while_dave_is_required() {
        let frame = [0, 0, 0, 1, 0x65, 0x88];
        let none = Mutex::new(None);
        assert!(protect_frame(&none, true, &frame).is_none());
        let pending = Mutex::new(DaveSession::new(NonZeroU16::MIN, 1, 1, None).ok());
        assert!(protect_frame(&pending, true, &frame).is_none());
        assert_eq!(protect_frame(&none, false, &frame).as_deref(), Some(&frame[..]));
    }
}
