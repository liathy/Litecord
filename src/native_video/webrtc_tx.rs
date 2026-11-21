//! Go Live transmission over WebRTC, on voice gateway v8.
//!
//! The media connection is str0m in RTP mode: Discord assigns the stream's SSRCs, frames
//! are DAVE-encrypted and packetized here, and str0m does ICE, DTLS, SRTP, sender reports,
//! keyframe requests and retransmissions. No extra dependency: browser sharing uses it too.

use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroU16;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use davey::DaveSession;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use str0m::config::Fingerprint;
use str0m::format::{Codec, FormatParams};
use str0m::media::{Frequency, MediaKind, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::rtp::{Extension, RtpWrite, Ssrc};
use str0m::{Candidate, Event, IceConnectionState, IceCreds, Input, Output, Rtc, RtcConfig};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex as AsyncMutex};
use tokio_tungstenite::tungstenite::Message;

use super::h264::H264Packetizer;
use super::sender::{H264_PAYLOAD_TYPE, H264_RTX_PAYLOAD_TYPE};
use super::stream::{begin_generation, end_generation, handle_dave_binary, is_current, rejoin_dave, send_text, VoiceSink};

const OPUS_PAYLOAD_TYPE: u8 = 120;
const AUDIO_MID: &str = "0";
const VIDEO_MID: &str = "1";
/// One Opus packet: 20 ms at 48 kHz.
const OPUS_FRAME_TICKS: u32 = 960;
const SOCKET_BUFFER: usize = 4 << 20;
const IDLE_WAKE: Duration = Duration::from_millis(250);

/// `wss://host/?v=8` from what the gateway calls the endpoint.
fn gateway_url(endpoint: &str) -> String {
    let host = endpoint.trim().trim_start_matches("wss://").trim_start_matches("ws://");
    let host = host.split(['/', '?']).next().unwrap_or(host);
    format!("wss://{host}/?v=8")
}

fn build_offer(creds: &IceCreds, fingerprint: &Fingerprint, local: SocketAddr, video: u32, rtx: u32) -> String {
    let (ip, port) = (local.ip(), local.port());
    let h264 = H264_PAYLOAD_TYPE;
    let resend = H264_RTX_PAYLOAD_TYPE;
    let opus = OPUS_PAYLOAD_TYPE;
    let session = [
        "v=0".to_string(),
        format!("o=litecord {} 0 IN IP4 {ip}", rand::random::<u32>()),
        "s=-".to_string(),
        "t=0 0".to_string(),
        "a=group:BUNDLE 0 1".to_string(),
        "a=msid-semantic:WMS *".to_string(),
        "a=setup:actpass".to_string(),
        format!("a=ice-ufrag:{}", creds.ufrag),
        format!("a=ice-pwd:{}", creds.pass),
        format!("a=fingerprint:{fingerprint}"),
    ];
    let candidate = format!("a=candidate:1 1 udp 2122260223 {ip} {port} typ host");
    let audio = [
        format!("m=audio {port} UDP/TLS/RTP/SAVPF {opus}"),
        format!("c=IN IP4 {ip}"),
        "a=mid:0".to_string(),
        "a=sendrecv".to_string(),
        "a=rtcp-mux".to_string(),
        format!("a=rtpmap:{opus} opus/48000/2"),
        format!("a=fmtp:{opus} minptime=10;useinbandfec=1;usedtx=1"),
        candidate.clone(),
    ];
    let media = [
        format!("m=video {port} UDP/TLS/RTP/SAVPF {h264} {resend}"),
        format!("c=IN IP4 {ip}"),
        format!("a=mid:{VIDEO_MID}"),
        "a=sendrecv".to_string(),
        "a=rtcp-mux".to_string(),
        "a=extmap:2 http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time".to_string(),
        "a=extmap:3 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01".to_string(),
        format!("a=rtpmap:{h264} H264/90000"),
        format!("a=fmtp:{h264} profile-level-id=42e01f;packetization-mode=1;level-asymmetry-allowed=1"),
        format!("a=rtcp-fb:{h264} ccm fir"),
        format!("a=rtcp-fb:{h264} nack"),
        format!("a=rtcp-fb:{h264} nack pli"),
        format!("a=rtcp-fb:{h264} goog-remb"),
        format!("a=rtcp-fb:{h264} transport-cc"),
        format!("a=rtpmap:{resend} rtx/90000"),
        format!("a=fmtp:{resend} apt={h264}"),
        format!("a=ssrc-group:FID {video} {rtx}"),
        format!("a=ssrc:{video} cname:litecord"),
        format!("a=ssrc:{rtx} cname:litecord"),
        candidate,
    ];
    let mut lines: Vec<String> = session.into_iter().collect();
    lines.extend(audio);
    lines.extend(media);
    lines.join("\r\n") + "\r\n"
}

#[derive(Debug)]
struct RemoteDescription {
    creds: IceCreds,
    fingerprint: Fingerprint,
    address: SocketAddr,
}

/// Discord's answer holds only the transport lines; its media sections are not a valid SDP.
fn parse_answer(sdp: &str) -> Result<RemoteDescription, String> {
    let (mut ufrag, mut pass, mut fingerprint, mut candidate) = (None, None, None, None);
    let (mut connection_ip, mut rtcp_port) = (None, None);
    for line in sdp.lines().map(str::trim) {
        if let Some(v) = line.strip_prefix("a=ice-ufrag:") {
            ufrag = Some(v.to_owned());
        } else if let Some(v) = line.strip_prefix("a=ice-pwd:") {
            pass = Some(v.to_owned());
        } else if let Some(v) = line.strip_prefix("a=fingerprint:") {
            fingerprint = v.parse::<Fingerprint>().ok();
        } else if let Some(v) = line.strip_prefix("a=candidate:") {
            let fields: Vec<&str> = v.split_whitespace().collect();
            if candidate.is_none() && fields.len() >= 6 {
                let ip = fields[4].parse::<IpAddr>().ok();
                let port = fields[5].parse::<u16>().ok();
                candidate = ip.zip(port).map(|(ip, port)| SocketAddr::new(ip, port));
            }
        } else if let Some(v) = line.strip_prefix("c=IN IP4 ") {
            connection_ip = v.split('/').next().and_then(|ip| ip.parse::<IpAddr>().ok());
        } else if let Some(v) = line.strip_prefix("a=rtcp:") {
            rtcp_port = v.split_whitespace().next().and_then(|port| port.parse::<u16>().ok());
        }
    }
    let address = candidate
        .or_else(|| connection_ip.zip(rtcp_port).map(|(ip, port)| SocketAddr::new(ip, port)))
        .ok_or("answer without an address")?;
    Ok(RemoteDescription {
        creds: IceCreds {
            ufrag: ufrag.ok_or("answer without ice-ufrag")?,
            pass: pass.ok_or("answer without ice-pwd")?,
        },
        fingerprint: fingerprint.ok_or("answer without fingerprint")?,
        address,
    })
}

/// The sans-IO media connection, so a test can run it against a real peer in memory.
struct GoLiveRtc {
    rtc: Rtc,
    local: SocketAddr,
    mid: Mid,
    ssrc: Ssrc,
    rtx: u32,
    pt: Pt,
    seq: u64,
    packetizer: H264Packetizer,
    started: Instant,
    audio_ssrc: Option<Ssrc>,
    audio_seq: u64,
    audio_time: u32,
    audio_last: Option<Instant>,
}

impl GoLiveRtc {
    fn new(now: Instant, local: SocketAddr, video_ssrc: u32, rtx_ssrc: u32) -> Result<Self, String> {
        crate::screen_capture::web_share::install_crypto_provider();
        let mut config = RtcConfig::new()
            .clear_codecs()
            .set_rtp_mode(true)
            .clear_extension_map()
            .set_extension(2, Extension::AbsoluteSendTime)
            .set_extension(3, Extension::TransportSequenceNumber);
        config
            .codec_config()
            .add_h264(H264_PAYLOAD_TYPE.into(), Some(H264_RTX_PAYLOAD_TYPE.into()), true, 0x42e01f);
        config.codec_config().add_config(
            OPUS_PAYLOAD_TYPE.into(),
            None,
            Codec::Opus,
            Frequency::FORTY_EIGHT_KHZ,
            Some(2),
            FormatParams::default(),
        );
        let mut rtc = config.build(now);
        let candidate = Candidate::host(local, "udp").map_err(|e| format!("candidate: {e}"))?;
        rtc.add_local_candidate(candidate);
        let mid: Mid = VIDEO_MID.into();
        let ssrc: Ssrc = video_ssrc.into();
        {
            let mut api = rtc.direct_api();
            api.set_ice_controlling(true);
            api.declare_media(mid, MediaKind::Video);
            api.declare_stream_tx(ssrc, Some(rtx_ssrc.into()), mid, None);
        }
        Ok(Self {
            rtc,
            local,
            mid,
            ssrc,
            rtx: rtx_ssrc,
            pt: H264_PAYLOAD_TYPE.into(),
            seq: rand::random::<u16>() as u64,
            packetizer: H264Packetizer::new(video_ssrc, H264_PAYLOAD_TYPE),
            started: now,
            audio_ssrc: None,
            audio_seq: rand::random::<u16>() as u64,
            audio_time: rand::random::<u32>(),
            audio_last: None,
        })
    }

    /// Opens the stream's sound on the SSRC Discord assigned.
    fn enable_audio(&mut self, ssrc: u32) {
        let mid: Mid = AUDIO_MID.into();
        let ssrc: Ssrc = ssrc.into();
        let mut api = self.rtc.direct_api();
        api.declare_media(mid, MediaKind::Audio);
        api.declare_stream_tx(ssrc, None, mid, None);
        self.audio_ssrc = Some(ssrc);
    }

    fn write_audio(&mut self, packet: &[u8], now: Instant) {
        let Some(ssrc) = self.audio_ssrc else { return };
        // Nothing is sent during silence, but the audio clock keeps running.
        if let Some(last) = self.audio_last {
            let gap = now.saturating_duration_since(last);
            if gap > Duration::from_millis(100) {
                self.audio_time = self.audio_time.wrapping_add((gap.as_millis() as u32).wrapping_mul(48));
            }
        }
        self.audio_last = Some(now);
        let mut api = self.rtc.direct_api();
        let Some(stream) = api.stream_tx(&ssrc) else { return };
        stream.write_rtp(RtpWrite::new(OPUS_PAYLOAD_TYPE.into(), self.audio_seq.into(), self.audio_time, now, packet.to_vec()).nackable(false));
        self.audio_seq += 1;
        self.audio_time = self.audio_time.wrapping_add(OPUS_FRAME_TICKS);
    }

    fn offer(&mut self) -> String {
        let mut api = self.rtc.direct_api();
        let creds = api.local_ice_credentials();
        let fingerprint = api.local_dtls_fingerprint().clone();
        build_offer(&creds, &fingerprint, self.local, *self.ssrc, self.rtx)
    }

    fn apply_answer(&mut self, sdp: &str) -> Result<(), String> {
        let remote = parse_answer(sdp)?;
        let candidate = Candidate::host(remote.address, "udp").map_err(|e| format!("remote candidate: {e}"))?;
        self.rtc.add_remote_candidate(candidate);
        let mut api = self.rtc.direct_api();
        api.set_remote_ice_credentials(remote.creds);
        api.set_remote_fingerprint(remote.fingerprint);
        api.start_dtls(true).map_err(|e| format!("DTLS: {e}"))
    }

    fn write_frame(&mut self, frame: &[u8], now: Instant) {
        let pts = now.saturating_duration_since(self.started).as_millis() as u64;
        let packets = self.packetizer.packetize(frame, pts);
        let mut api = self.rtc.direct_api();
        let Some(stream) = api.stream_tx(&self.ssrc) else { return };
        for (header, payload) in packets {
            let marker = header[1] & 0x80 != 0;
            let time = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
            stream.write_rtp(RtpWrite::new(self.pt, self.seq.into(), time, now, payload).marker(marker).nackable(true));
            self.seq += 1;
        }
    }
}

#[derive(Default)]
struct Drive {
    connected: bool,
    keyframe: bool,
    lost: bool,
}

/// Sends what str0m has ready; returns when it wants to run again.
fn drive(media: &mut GoLiveRtc, socket: &UdpSocket) -> Result<(Instant, Drive), String> {
    let mut seen = Drive::default();
    loop {
        match media.rtc.poll_output().map_err(|e| format!("WebRTC: {e}"))? {
            Output::Transmit(packet) => {
                let _ = socket.try_send_to(&packet.contents, packet.destination);
            }
            Output::Timeout(at) => return Ok((at, seen)),
            Output::Event(Event::Connected) => seen.connected = true,
            Output::Event(Event::KeyframeRequest(_)) => seen.keyframe = true,
            Output::Event(Event::IceConnectionStateChange(IceConnectionState::Disconnected)) => seen.lost = true,
            Output::Event(_) => {}
        }
    }
}

fn local_address(port: u16) -> SocketAddr {
    let ip = crate::screen_capture::get_local_lan_addresses(0)
        .into_iter()
        .map(|a| a.ip())
        .find(|ip| ip.is_ipv4() && !ip.is_loopback())
        .unwrap_or(IpAddr::from([127, 0, 0, 1]));
    SocketAddr::new(ip, port)
}

fn connection_id() -> String {
    let id = rand::random::<u128>();
    format!("{:08x}-{:04x}-4{:03x}-a{:03x}-{:012x}", (id >> 96) as u32, (id >> 80) as u16, (id >> 68) as u16 & 0xfff, (id >> 56) as u16 & 0xfff, id as u64 & 0xffff_ffff_ffff)
}

async fn recv_packet(socket: &Option<Arc<UdpSocket>>, buffer: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
    match socket {
        Some(socket) => socket.recv_from(buffer).await,
        None => std::future::pending().await,
    }
}

async fn next_frame(frames: &mut Option<mpsc::Receiver<Vec<u8>>>) -> Option<Vec<u8>> {
    match frames {
        Some(frames) => frames.recv().await,
        None => std::future::pending().await,
    }
}

/// Connects to a Go Live stream server as its broadcaster.
pub async fn connect_stream_sender(
    endpoint: String,
    token: String,
    server_id: String,
    channel_id: String,
    session_id: String,
    user_id: u64,
    stream_key: String,
) {
    let _ = channel_id;
    let generation = begin_generation(&stream_key);
    if let Err(e) = run(&endpoint, &token, &server_id, &session_id, user_id, &stream_key, generation).await {
        log::warn!("🎬 [GO LIVE TX] Conexão WebRTC falhou: {}", e);
    }
    super::sender::stop_sender(&stream_key);
    end_generation(&stream_key, generation);
    log::info!("🛑 [GO LIVE TX] Conexão emissora encerrada key={}", stream_key);
}

async fn announce_video(write: &Arc<AsyncMutex<VoiceSink>>, audio: u32, video: u32, rtx: u32) -> Result<(), String> {
    let (width, height, fps) = super::sender::outbound_quality();
    send_text(write, json!({ "op": 12, "d": {
        "audio_ssrc": audio,
        "video_ssrc": video,
        "rtx_ssrc": rtx,
        "streams": [{
            "type": "video", "rid": "100", "ssrc": video, "active": true, "quality": 100,
            "rtx_ssrc": rtx, "max_bitrate": 8_000_000, "max_framerate": fps,
            "max_resolution": { "type": "fixed", "width": width, "height": height }
        }]
    }})).await?;
    send_text(write, json!({ "op": 5, "d": { "speaking": 2, "delay": 0, "ssrc": audio } })).await
}

#[allow(clippy::too_many_arguments)]
async fn run(
    endpoint: &str,
    token: &str,
    server_id: &str,
    session_id: &str,
    user_id: u64,
    stream_key: &str,
    generation: u64,
) -> Result<(), String> {
    let url = gateway_url(endpoint);
    log::info!("🎬 [GO LIVE TX] Conectando por WebRTC a {} key={}", url, stream_key);
    let (ws, _) = crate::gateway::voice::connect_voice_ws(&url).await.map_err(|e| format!("WebSocket: {e:?}"))?;
    let (write, mut read) = ws.split();
    let write = Arc::new(AsyncMutex::new(write));

    let seq_ack = Arc::new(AtomicI64::new(-1));
    let dave: Arc<Mutex<Option<DaveSession>>> = Arc::new(Mutex::new(None));
    let external_sender: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let dave_required = Arc::new(AtomicBool::new(true));
    // Discord's rule for Go Live: the DAVE group is the stream server's id minus one.
    let dave_group = server_id.parse::<u64>().unwrap_or(2).saturating_sub(1).max(1);
    let mut pending_version: Option<u64> = None;

    let mut media: Option<GoLiveRtc> = None;
    let mut socket: Option<Arc<UdpSocket>> = None;
    let mut frames: Option<mpsc::Receiver<Vec<u8>>> = None;
    let mut audio_packets: Option<mpsc::Receiver<Vec<u8>>> = None;
    let (mut audio_ssrc, mut video_ssrc, mut rtx_ssrc) = (0u32, 0u32, 0u32);
    let mut buffer = vec![0u8; 2048];
    let mut waiting_for_dave = false;

    while is_current(stream_key, generation) {
        let mut wake = Instant::now() + IDLE_WAKE;
        if let (Some(rtc), Some(udp)) = (media.as_mut(), socket.as_ref()) {
            let (at, seen) = drive(rtc, udp)?;
            wake = wake.min(at);
            if seen.keyframe {
                crate::screen_capture::types::request_intra_keyframe();
            }
            if seen.lost {
                log::warn!("🎬 [GO LIVE TX] WebRTC perdeu a conexão");
            }
            if seen.connected {
                log::info!("✅ [GO LIVE TX] WebRTC conectado (DTLS/SRTP)");
                announce_video(&write, audio_ssrc, video_ssrc, rtx_ssrc).await?;
                crate::gateway::set_stream_paused(false);
                frames = Some(super::sender::begin_outbound(stream_key));
                audio_packets = Some(super::sender::begin_outbound_audio());
            }
        }

        tokio::select! {
            message = read.next() => {
                let message = match message {
                    Some(Ok(message)) => message,
                    Some(Err(e)) => return Err(format!("WebSocket: {e}")),
                    None => return Ok(()),
                };
                match message {
                    Message::Text(text) => {
                        let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
                        if let Some(seq) = value["seq"].as_i64() {
                            seq_ack.store(seq, Ordering::Relaxed);
                        }
                        match value["op"].as_u64().unwrap_or(u64::MAX) {
                            8 => {
                                let interval = value["d"]["heartbeat_interval"].as_f64().unwrap_or(41250.0).max(1000.0) as u64;
                                send_text(&write, json!({ "op": 0, "d": {
                                    "server_id": server_id,
                                    "user_id": user_id.to_string(),
                                    "session_id": session_id,
                                    "token": token,
                                    "video": true,
                                    "streams": [{ "type": "screen", "rid": "100", "quality": 100 }],
                                    "max_dave_protocol_version": 1
                                }})).await?;
                                let (beat_write, beat_seq, key) = (Arc::clone(&write), Arc::clone(&seq_ack), stream_key.to_owned());
                                tokio::spawn(async move {
                                    loop {
                                        tokio::time::sleep(Duration::from_millis(interval)).await;
                                        if !is_current(&key, generation) {
                                            break;
                                        }
                                        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
                                        let beat = json!({ "op": 3, "d": { "t": t, "seq_ack": beat_seq.load(Ordering::Relaxed) } });
                                        if send_text(&beat_write, beat).await.is_err() {
                                            break;
                                        }
                                    }
                                });
                            }
                            2 => {
                                audio_ssrc = value["d"]["ssrc"].as_u64().unwrap_or(0) as u32;
                                let stream = &value["d"]["streams"][0];
                                video_ssrc = stream["ssrc"].as_u64().ok_or("Ready without a video SSRC")? as u32;
                                rtx_ssrc = stream["rtx_ssrc"].as_u64().ok_or("Ready without an RTX SSRC")? as u32;

                                let udp = UdpSocket::bind("0.0.0.0:0").await.map_err(|e| format!("UDP: {e}"))?;
                                let buffers = socket2::SockRef::from(&udp);
                                let _ = buffers.set_send_buffer_size(SOCKET_BUFFER);
                                let _ = buffers.set_recv_buffer_size(SOCKET_BUFFER / 4);
                                let local = local_address(udp.local_addr().map_err(|e| format!("UDP: {e}"))?.port());
                                let mut rtc = GoLiveRtc::new(Instant::now(), local, video_ssrc, rtx_ssrc)?;
                                if audio_ssrc != 0 {
                                    rtc.enable_audio(audio_ssrc);
                                }
                                let sdp = rtc.offer();
                                socket = Some(Arc::new(udp));
                                media = Some(rtc);
                                send_text(&write, json!({ "op": 1, "d": {
                                    "protocol": "webrtc",
                                    "data": sdp,
                                    "sdp": sdp,
                                    "rtc_connection_id": connection_id(),
                                    "codecs": [
                                        { "name": "opus", "type": "audio", "priority": 1000, "payload_type": OPUS_PAYLOAD_TYPE },
                                        { "name": "H264", "type": "video", "priority": 1000,
                                          "payload_type": H264_PAYLOAD_TYPE, "rtx_payload_type": H264_RTX_PAYLOAD_TYPE,
                                          "encode": true, "decode": true }
                                    ]
                                }})).await?;
                                send_text(&write, json!({ "op": 12, "d": {
                                    "audio_ssrc": audio_ssrc, "video_ssrc": 0, "rtx_ssrc": 0, "streams": []
                                }})).await?;
                            }
                            4 => {
                                let answer = value["d"]["sdp"].as_str().ok_or("Session description without SDP")?;
                                media.as_mut().ok_or("Session description before Ready")?.apply_answer(answer)?;
                                let version = value["d"]["dave_protocol_version"].as_u64().unwrap_or(1);
                                log::info!("🎬 [GO LIVE TX] Sessão WebRTC: codec={:?} DAVE={}", value["d"]["video_codec"].as_str(), version);
                                dave_required.store(version > 0, Ordering::Relaxed);
                                if version > 0 && dave.lock().unwrap().is_none() {
                                    rejoin_dave(&dave, &external_sender, user_id, dave_group, version as u16, &write, None).await?;
                                }
                            }
                            21 => {
                                let transition = value["d"]["transition_id"].as_u64().unwrap_or(0);
                                let version = value["d"]["protocol_version"].as_u64().unwrap_or(1);
                                if transition == 0 {
                                    dave_required.store(version > 0, Ordering::Relaxed);
                                } else {
                                    pending_version = Some(version);
                                    send_text(&write, json!({ "op": 23, "d": { "transition_id": transition } })).await?;
                                }
                            }
                            22 => {
                                if let Some(version) = pending_version.take() {
                                    dave_required.store(version > 0, Ordering::Relaxed);
                                }
                                crate::screen_capture::types::request_intra_keyframe();
                            }
                            24 => {
                                if value["d"]["epoch"].as_u64() == Some(1) {
                                    let version = value["d"]["protocol_version"].as_u64().unwrap_or(1).max(1) as u16;
                                    dave_required.store(true, Ordering::Relaxed);
                                    rejoin_dave(&dave, &external_sender, user_id, dave_group, version, &write, None).await?;
                                }
                            }
                            _ => {}
                        }
                    }
                    Message::Binary(data) if data.len() > 2 => {
                        // v8: two bytes of sequence number, then the DAVE opcode and its body.
                        seq_ack.store(u16::from_be_bytes([data[0], data[1]]) as i64, Ordering::Relaxed);
                        let body = &data[2..];
                        handle_dave_binary(body, &dave, &external_sender, user_id, dave_group, &write).await?;
                        if matches!(body[0], 29 | 30) {
                            crate::screen_capture::types::request_intra_keyframe();
                            log::info!("🔐 [GO LIVE TX] Grupo DAVE mudou: {:?} membros", dave.lock().unwrap().as_ref().and_then(|s| s.get_user_ids()).map(|u| u.len()));
                        }
                    }
                    Message::Close(frame) => {
                        log::info!("🎬 [GO LIVE TX] Gateway fechou: {:?}", frame);
                        return Ok(());
                    }
                    _ => {}
                }
            }
            packet = recv_packet(&socket, &mut buffer) => {
                if let (Ok((length, source)), Some(rtc)) = (packet, media.as_mut()) {
                    if let Ok(contents) = buffer[..length].try_into() {
                        let input = Input::Receive(Instant::now(), Receive { proto: Protocol::Udp, source, destination: rtc.local, contents });
                        if rtc.rtc.accepts(&input) {
                            rtc.rtc.handle_input(input).map_err(|e| format!("WebRTC: {e}"))?;
                        }
                    }
                }
            }
            frame = next_frame(&mut frames) => {
                let Some(frame) = frame else { frames = None; continue };
                let frame = crate::encoder::h264_vui::without_reordering(&frame).unwrap_or(frame);
                match super::sender::protect_frame(&dave, dave_required.load(Ordering::Relaxed), &frame) {
                    Some(protected) => {
                        if waiting_for_dave {
                            waiting_for_dave = false;
                            log::info!("🔐 [GO LIVE TX] Grupo DAVE pronto: enviando vídeo cifrado");
                            crate::screen_capture::types::request_intra_keyframe();
                        }
                        if let Some(rtc) = media.as_mut() {
                            rtc.write_frame(&protected, Instant::now());
                        }
                    }
                    None if !waiting_for_dave => {
                        waiting_for_dave = true;
                        log::info!("🔐 [GO LIVE TX] Aguardando o grupo DAVE para enviar vídeo");
                    }
                    None => {}
                }
            }
            packet = next_frame(&mut audio_packets) => {
                let Some(packet) = packet else { audio_packets = None; continue };
                if let Some(protected) = super::sender::protect_audio(&dave, dave_required.load(Ordering::Relaxed), &packet) {
                    if let Some(rtc) = media.as_mut() {
                        rtc.write_audio(&protected, Instant::now());
                    }
                }
            }
            _ = tokio::time::sleep_until(wake.into()) => {
                if let Some(rtc) = media.as_mut() {
                    rtc.rtc.handle_input(Input::Timeout(Instant::now())).map_err(|e| format!("WebRTC: {e}"))?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_video::h264::H264Depacketizer;

    const VIDEO: u32 = 4242;
    const RTX: u32 = 4243;
    const AUDIO: u32 = 4244;

    fn deliver(to: &mut Rtc, t: &str0m::net::Transmit, now: Instant) {
        let input = Input::Receive(now, Receive {
            proto: t.proto,
            source: t.source,
            destination: t.destination,
            contents: (&*t.contents).try_into().unwrap(),
        });
        to.handle_input(input).unwrap();
    }

    /// In RTP mode str0m holds one received packet until polled, so each side is drained
    /// after every delivery, as the real socket loop does.
    fn pass_packets(from: &mut Rtc, to: &mut Rtc, now: Instant, from_seen: &mut Vec<Event>, to_seen: &mut Vec<Event>) {
        loop {
            match from.poll_output().unwrap() {
                Output::Transmit(t) => {
                    deliver(to, &t, now);
                    loop {
                        match to.poll_output().unwrap() {
                            Output::Transmit(back) => deliver(from, &back, now),
                            Output::Timeout(_) => break,
                            Output::Event(event) => to_seen.push(event),
                        }
                    }
                }
                Output::Timeout(_) => break,
                Output::Event(event) => from_seen.push(event),
            }
        }
    }

    fn peer(now: Instant, address: SocketAddr, sender: SocketAddr, offer: &str) -> (Rtc, String) {
        let mut config = RtcConfig::new().clear_codecs().set_rtp_mode(true).clear_extension_map()
            .set_extension(2, Extension::AbsoluteSendTime)
            .set_extension(3, Extension::TransportSequenceNumber);
        config.codec_config().add_h264(H264_PAYLOAD_TYPE.into(), Some(H264_RTX_PAYLOAD_TYPE.into()), true, 0x42e01f);
        config.codec_config().add_config(OPUS_PAYLOAD_TYPE.into(), None, Codec::Opus, Frequency::FORTY_EIGHT_KHZ, Some(2), FormatParams::default());
        let mut rtc = config.build(now);
        rtc.add_local_candidate(Candidate::host(address, "udp").unwrap());
        rtc.add_remote_candidate(Candidate::host(sender, "udp").unwrap());
        let offered = parse_answer(offer).unwrap();
        let answer = {
            let mut api = rtc.direct_api();
            let mid: Mid = VIDEO_MID.into();
            api.declare_media(mid, MediaKind::Video);
            api.expect_stream_rx(VIDEO.into(), Some(RTX.into()), mid, None);
            let audio_mid: Mid = AUDIO_MID.into();
            api.declare_media(audio_mid, MediaKind::Audio);
            api.expect_stream_rx(AUDIO.into(), None, audio_mid, None);
            let creds = api.local_ice_credentials();
            let fingerprint = api.local_dtls_fingerprint().clone();
            api.set_ice_controlling(false);
            api.set_remote_ice_credentials(offered.creds);
            api.set_remote_fingerprint(offered.fingerprint);
            api.start_dtls(false).unwrap();
            // The shape Discord answers with: transport lines only.
            format!(
                "c=IN IP4 {ip}\na=rtcp:{port}\na=ice-ufrag:{}\na=ice-pwd:{}\na=fingerprint:{fingerprint}\na=candidate:1 1 udp 2122260223 {ip} {port} typ host\n",
                creds.ufrag, creds.pass, ip = address.ip(), port = address.port()
            )
        };
        (rtc, answer)
    }

    #[test]
    fn a_large_frame_crosses_a_real_webrtc_connection_intact() {
        let mut now = Instant::now();
        let sender_at: SocketAddr = "10.0.0.1:5000".parse().unwrap();
        let viewer_at: SocketAddr = "10.0.0.2:6000".parse().unwrap();
        let mut sender = GoLiveRtc::new(now, sender_at, VIDEO, RTX).unwrap();
        sender.enable_audio(AUDIO);
        let offer = sender.offer();
        let (mut viewer, answer) = peer(now, viewer_at, sender_at, &offer);
        sender.apply_answer(&answer).unwrap();

        let (mut sender_events, mut viewer_events) = (Vec::new(), Vec::new());
        let mut connected = false;
        for _ in 0..600 {
            pass_packets(&mut sender.rtc, &mut viewer, now, &mut sender_events, &mut viewer_events);
            pass_packets(&mut viewer, &mut sender.rtc, now, &mut viewer_events, &mut sender_events);
            connected = sender_events.iter().any(|e| matches!(e, Event::Connected));
            if connected && viewer_events.iter().any(|e| matches!(e, Event::Connected)) {
                break;
            }
            now += Duration::from_millis(10);
            sender.rtc.handle_input(Input::Timeout(now)).unwrap();
            viewer.handle_input(Input::Timeout(now)).unwrap();
        }
        assert!(connected, "ICE and DTLS should connect");

        let mut frame = vec![0, 0, 0, 1, 0x67, 0x4D, 0x00, 0x1F, 0, 0, 0, 1, 0x68, 0xEE, 0, 0, 0, 1, 0x65];
        frame.extend((0..6000u32).map(|i| (i % 251 + 2) as u8));
        sender.write_frame(&frame, now);
        let sound = [0xFC, 1, 2, 3, 4, 5];
        sender.write_audio(&sound, now);
        viewer_events.clear();
        for _ in 0..200 {
            now += Duration::from_millis(5);
            sender.rtc.handle_input(Input::Timeout(now)).unwrap();
            viewer.handle_input(Input::Timeout(now)).unwrap();
            pass_packets(&mut sender.rtc, &mut viewer, now, &mut sender_events, &mut viewer_events);
            pass_packets(&mut viewer, &mut sender.rtc, now, &mut viewer_events, &mut sender_events);
        }

        let mut depacketizer = H264Depacketizer::new();
        let mut received = None;
        let mut received_sound = None;
        for event in &viewer_events {
            if let Event::RtpPacket(packet) = event {
                if *packet.header.ssrc == AUDIO {
                    assert_eq!(*packet.header.payload_type, OPUS_PAYLOAD_TYPE);
                    received_sound = Some(packet.payload.clone());
                    continue;
                }
                assert_eq!(*packet.header.ssrc, VIDEO);
                assert_eq!(*packet.header.payload_type, H264_PAYLOAD_TYPE);
                if let Some(done) = depacketizer.push(&packet.payload, packet.header.marker) {
                    received = Some(done);
                }
            }
        }
        assert_eq!(received.as_deref(), Some(&frame[..]));
        assert_eq!(received_sound.as_deref(), Some(&sound[..]));
    }

    #[test]
    fn the_offer_names_discords_h264_and_stream_ids() {
        let mut rtc = GoLiveRtc::new(Instant::now(), "10.0.0.1:5000".parse().unwrap(), VIDEO, RTX).unwrap();
        let offer = rtc.offer();
        assert!(offer.contains("a=rtpmap:101 H264/90000"));
        assert!(offer.contains("a=fmtp:102 apt=101"));
        assert!(offer.contains("a=ssrc-group:FID 4242 4243"));
        assert!(offer.contains("a=candidate:1 1 udp 2122260223 10.0.0.1 5000 typ host"));
        assert!(offer.ends_with("\r\n"));
    }

    #[test]
    fn the_gateway_url_asks_for_version_8() {
        assert_eq!(gateway_url("c-gru21.discord.media:2053"), "wss://c-gru21.discord.media:2053/?v=8");
        assert_eq!(gateway_url("wss://host:443/?v=4"), "wss://host:443/?v=8");
    }

    #[test]
    fn an_answer_without_transport_details_is_refused() {
        assert!(parse_answer("a=ice-ufrag:x\n").is_err());
    }
}
