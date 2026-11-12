//! Peer connection setup shared by publisher and viewer.

use anyhow::{Context, Result};
use rtc::interceptor::Registry;
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{MIME_TYPE_H264, MediaEngine};
use rtc::peer_connection::transport::RTCIceServer;
use rtc::rtp_transceiver::PayloadType;
use rtc::rtp_transceiver::rtp_sender::{RTCRtpCodec, RTCRtpCodecParameters, RtpCodecKind};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use webrtc::media_stream::track_remote::TrackRemote;
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState,
    RTCPeerConnectionState,
};

pub use webrtc::peer_connection::RTCPeerConnectionState as ConnectionState;

/// Public STUN server used by default so peers behind NAT can find their
/// public address. Configurable; an empty list means host candidates only
/// (fine on a LAN or the same machine).
pub const DEFAULT_STUN_SERVER: &str = "stun:stun.l.google.com:19302";

/// We only offer one codec, so both sides agree on a fixed payload type.
pub const H264_PAYLOAD_TYPE: PayloadType = 102;

/// How long to wait for ICE gathering before sending whatever candidates we
/// have. Host candidates are instant; this bounds the wait on slow STUN.
const GATHER_TIMEOUT: Duration = Duration::from_secs(3);

/// H.264 Constrained Baseline, packetization-mode 1: no B-frames (no
/// reordering delay) and what browsers expect, should one ever join.
pub fn h264_codec() -> RTCRtpCodec {
    RTCRtpCodec {
        mime_type: MIME_TYPE_H264.to_owned(),
        clock_rate: 90_000,
        channels: 0,
        sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f".to_owned(),
        rtcp_feedback: vec![],
    }
}

/// Peer connection callbacks, delivered as a stream of events.
pub enum PeerEvent {
    GatheringComplete,
    State(RTCPeerConnectionState),
    Track(Arc<dyn TrackRemote>),
}

struct Forwarder(mpsc::UnboundedSender<PeerEvent>);

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Forwarder {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.0.send(PeerEvent::GatheringComplete);
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        let _ = self.0.send(PeerEvent::State(state));
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let _ = self.0.send(PeerEvent::Track(track));
    }
}

/// Create a peer connection that can send or receive H.264 video. Must be
/// called inside a Tokio runtime.
pub async fn new_peer_connection(
    ice_servers: &[String],
) -> Result<(impl PeerConnection, mpsc::UnboundedReceiver<PeerEvent>)> {
    let mut media_engine = MediaEngine::default();
    media_engine.register_codec(
        RTCRtpCodecParameters { rtp_codec: h264_codec(), payload_type: H264_PAYLOAD_TYPE },
        RtpCodecKind::Video,
    )?;
    let registry = register_default_interceptors(Registry::new(), &mut media_engine)?;

    let mut config = RTCConfigurationBuilder::new();
    if !ice_servers.is_empty() {
        config = config.with_ice_servers(vec![RTCIceServer { urls: ice_servers.to_vec(), ..Default::default() }]);
    }

    let (tx, rx) = mpsc::unbounded_channel();
    let runtime = webrtc::runtime::default_runtime().context("no WebRTC runtime compiled in")?;
    let pc = PeerConnectionBuilder::new()
        .with_configuration(config.build())
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_handler(Arc::new(Forwarder(tx)))
        .with_runtime(runtime)
        // Wildcard = one socket per non-loopback interface.
        .with_udp_addrs(vec!["0.0.0.0:0".to_string()])
        .build()
        .await?;
    Ok((pc, rx))
}

/// Wait for ICE gathering to finish (or time out). Other events that arrive
/// meanwhile are returned so the caller can still handle them.
pub async fn wait_for_gathering(events: &mut mpsc::UnboundedReceiver<PeerEvent>) -> Vec<PeerEvent> {
    let mut deferred = Vec::new();
    let _ = tokio::time::timeout(GATHER_TIMEOUT, async {
        while let Some(event) = events.recv().await {
            match event {
                PeerEvent::GatheringComplete => return,
                other => deferred.push(other),
            }
        }
    })
    .await;
    deferred
}

/// Random SSRC for an outgoing stream, without pulling in a RNG crate.
pub fn random_ssrc() -> u32 {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new().build_hasher().finish() as u32
}
