//! Outbound RTP video transmission for Discord Go Live and camera.
//!
//! Encodes/packetizes raw Annex-B H.264 video frames into RFC 6184 RTP packets,
//! encrypts them via the negotiated transport key (AES-256-GCM / XChaCha20),
//! and sends them to Discord's stream SFU UDP endpoint.

use std::borrow::Cow;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use davey::{Codec, DaveSession, MediaType};
use log::{info, warn};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use super::crypto::{protect_rtcp, protect_video_rtp, TransportMode};
use super::h264::H264Packetizer;
use super::rtp::is_rtcp;

static FRAME_TX: Mutex<Option<mpsc::Sender<Vec<u8>>>> = Mutex::new(None);
static IS_BROADCASTING: AtomicBool = AtomicBool::new(false);
static ACTIVE_STREAM_KEY: Mutex<Option<String>> = Mutex::new(None);
static PREMIUM_TYPE: AtomicU8 = AtomicU8::new(0);
static OUTBOUND_QUALITY: Mutex<(u32, u32, u32)> = Mutex::new((1280, 720, 30));

/// Keyframes asked for by viewers are honoured at most this often.
const KEYFRAME_REQUEST_GAP: Duration = Duration::from_millis(500);

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

/// RTCP feedback asking for a picture a decoder can start from: PLI or FIR.
/// Sender and receiver reports also arrive here and must not cost a keyframe.
fn is_keyframe_request(packet: &[u8]) -> bool {
    is_rtcp(packet) && packet[1] == 206 && matches!(packet[0] & 0x1F, 1 | 4)
}

/// The frame to send. While the call uses DAVE, nothing leaves unencrypted: frames
/// are dropped until the group is ready, because viewers would discard them anyway.
fn protect_frame<'a>(
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
        info!("🛑 [GO LIVE TX] Transmissor nativo de stream encerrado: {}", stream_key);
    }
}

/// Starts outbound native RTP H.264 transmission over the established stream UDP socket.
pub fn start_sender(
    socket: Arc<UdpSocket>,
    target_addr: SocketAddr,
    key: Vec<u8>,
    mode: TransportMode,
    video_ssrc: u32,
    stream_key: String,
    dave: Arc<Mutex<Option<DaveSession>>>,
    dave_required: Arc<AtomicBool>,
) {
    stop_sender("");

    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(5);
    *FRAME_TX.lock().unwrap() = Some(tx);
    *ACTIVE_STREAM_KEY.lock().unwrap() = Some(stream_key.clone());
    IS_BROADCASTING.store(true, Ordering::SeqCst);

    // Initial IDR keyframe to allow immediate playback by viewers
    crate::screen_capture::types::request_intra_keyframe();

    let stream_start = Instant::now();
    let nonce_counter = Arc::new(AtomicU32::new(1));
    let total_packets = Arc::new(AtomicU32::new(0));
    let total_octets = Arc::new(AtomicU32::new(0));

    let socket_send = Arc::clone(&socket);
    let socket_send_rtcp = Arc::clone(&socket);
    let socket_recv = Arc::clone(&socket);
    let stream_key_clone = stream_key.clone();

    let total_packets_rtp = Arc::clone(&total_packets);
    let total_octets_rtp = Arc::clone(&total_octets);
    let total_packets_rtcp = Arc::clone(&total_packets);
    let total_octets_rtcp = Arc::clone(&total_octets);
    let nonce_counter_rtcp = Arc::clone(&nonce_counter);
    let key_rtcp = key.clone();

    // 1. Outbound Video RTP Worker Task
    tokio::spawn(async move {
        info!(
            "🚀 [GO LIVE TX] Pumping de vídeo nativo H.264 iniciado para {} (SSRC={}, mode={:?})",
            target_addr, video_ssrc, mode
        );
        crate::gateway::set_stream_paused(false);
        let mut packetizer = H264Packetizer::new(video_ssrc, 103);
        let mut frame_count = 0u64;
        let mut waiting_for_dave = false;

        while let Some(frame) = rx.recv().await {
            if !IS_BROADCASTING.load(Ordering::Relaxed) {
                break;
            }
            let pts_ms = stream_start.elapsed().as_millis() as u64;
            let Some(protected) = protect_frame(&dave, dave_required.load(Ordering::Relaxed), &frame) else {
                if !waiting_for_dave {
                    waiting_for_dave = true;
                    info!("🔐 [GO LIVE TX] Aguardando o grupo DAVE para enviar vídeo");
                }
                continue;
            };
            if waiting_for_dave {
                waiting_for_dave = false;
                info!("🔐 [GO LIVE TX] Grupo DAVE pronto: enviando vídeo cifrado");
                crate::screen_capture::types::request_intra_keyframe();
            }
            let packets = packetizer.packetize(&protected, pts_ms);

            for (header, payload) in packets {
                let cnt = nonce_counter.fetch_add(1, Ordering::Relaxed);
                total_packets_rtp.fetch_add(1, Ordering::Relaxed);
                total_octets_rtp.fetch_add(payload.len() as u32, Ordering::Relaxed);
                if let Some(encrypted) = protect_video_rtp(&key, mode, &header, &payload, cnt) {
                    if let Err(e) = socket_send.send_to(&encrypted, target_addr).await {
                        warn!("⚠️ [GO LIVE TX] Falha ao enviar pacote RTP: {:?}", e);
                        break;
                    }
                }
            }

            frame_count += 1;
            if frame_count % 300 == 0 {
                info!(
                    "🎬 [GO LIVE TX] {} frames H.264 transmitidos com sucesso para Discord!",
                    frame_count
                );
            }
        }
        info!("🛑 [GO LIVE TX] Worker de envio de vídeo finalizado.");
    });

    // 2. Periodic RTCP Sender Report Task (RFC 3550 / Discord lip-sync & clock sync keepalive)
    tokio::spawn(async move {
        while IS_BROADCASTING.load(Ordering::Relaxed) {
            tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
            if !IS_BROADCASTING.load(Ordering::Relaxed) {
                break;
            }
            let now = std::time::SystemTime::now();
            let unix_dur = now.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
            let ntp_secs = (unix_dur.as_secs() + 2_208_988_800) as u32;
            let ntp_frac = ((unix_dur.subsec_nanos() as u64 * (1u64 << 32)) / 1_000_000_000) as u32;
            let pts_ms = stream_start.elapsed().as_millis() as u64;
            let rtp_ts = (pts_ms * 90) as u32;
            let pkt_cnt = total_packets_rtcp.load(Ordering::Relaxed);
            let oct_cnt = total_octets_rtcp.load(Ordering::Relaxed);

            let mut sr = [0u8; 28];
            sr[0] = 0x80; // V=2, P=0, RC=0
            sr[1] = 200;  // PT=200 (Sender Report)
            sr[2..4].copy_from_slice(&6u16.to_be_bytes()); // length in 32-bit words - 1 = 6
            sr[4..8].copy_from_slice(&video_ssrc.to_be_bytes());
            sr[8..12].copy_from_slice(&ntp_secs.to_be_bytes());
            sr[12..16].copy_from_slice(&ntp_frac.to_be_bytes());
            sr[16..20].copy_from_slice(&rtp_ts.to_be_bytes());
            sr[20..24].copy_from_slice(&pkt_cnt.to_be_bytes());
            sr[24..28].copy_from_slice(&oct_cnt.to_be_bytes());

            let cnt = nonce_counter_rtcp.fetch_add(1, Ordering::Relaxed);
            if let Some(encrypted_sr) = protect_rtcp(&key_rtcp, mode, &sr, cnt) {
                let _ = socket_send_rtcp.send_to(&encrypted_sr, target_addr).await;
            }
        }
    });

    // 2. Inbound RTCP Receiver Task (Handling PLI/FIR requests from Discord)
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        let mut last_keyframe: Option<Instant> = None;
        while IS_BROADCASTING.load(Ordering::Relaxed) {
            // Timed so the task (and its socket) ends soon after the stream stops.
            let Ok(received) = tokio::time::timeout(Duration::from_millis(500), socket_recv.recv_from(&mut buf)).await else {
                continue;
            };
            if let Ok((len, _from)) = received {
                if is_keyframe_request(&buf[..len])
                    && last_keyframe.map_or(true, |t| t.elapsed() >= KEYFRAME_REQUEST_GAP)
                {
                    last_keyframe = Some(Instant::now());
                    crate::screen_capture::types::request_intra_keyframe();
                }
            }
        }
    });

    info!("✅ [GO LIVE TX] Transmissão nativa ativa key={}", stream_key_clone);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU16;

    #[test]
    fn only_pli_and_fir_ask_for_a_keyframe() {
        let packet = |first: u8, kind: u8| [first, kind, 0, 2, 0, 0, 0, 1];
        assert!(is_keyframe_request(&packet(0x81, 206)));
        assert!(is_keyframe_request(&packet(0x84, 206)));
        assert!(!is_keyframe_request(&packet(0x80, 200)));
        assert!(!is_keyframe_request(&packet(0x81, 201)));
        assert!(!is_keyframe_request(&packet(0x8F, 206)));
    }

    #[test]
    fn video_packets_carry_the_playout_delay_extension() {
        use aes_gcm::aead::{Aead, KeyInit, Payload};
        use aes_gcm::{Aes256Gcm, Nonce};

        let key = [7u8; 32];
        let header = [0x80, 103, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3];
        let packet = protect_video_rtp(&key, TransportMode::Aes256Gcm, &header, &[0x65, 0x88], 9).unwrap();
        assert_eq!(packet[0], 0x90);
        assert_eq!(&packet[12..16], &[0xBE, 0xDE, 0, 1]);

        let (sealed, suffix) = packet.split_at(packet.len() - 4);
        let mut nonce = [0u8; 12];
        nonce[..4].copy_from_slice(suffix);
        let opened = Aes256Gcm::new_from_slice(&key)
            .unwrap()
            .decrypt(Nonce::from_slice(&nonce), Payload { msg: &sealed[16..], aad: &sealed[..16] })
            .unwrap();
        assert_eq!(opened, [0x52, 0, 0, 0x0A, 0x65, 0x88]);
    }

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
