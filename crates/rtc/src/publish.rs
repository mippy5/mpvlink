//! The streaming side: one peer connection per viewer who asked to watch,
//! all fed from the same stream of encoded video frames and audio packets.

use crate::h264::normalize_start_codes;
use crate::peer::{self, ConnectionState, PeerEvent, h264_codec, new_peer_connection, opus_codec, random_ssrc};
use crate::signaling::Signaling;
use anyhow::{Context, Result, bail};
use bytes::Bytes;
use mpvlink_proto::{PeerId, Signal};
use rtc::media::Sample;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::rtp_transceiver::rtp_sender::{RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::peer_connection::PeerConnection;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Media {
    Video,
    Audio,
}

/// One encoded video frame (Annex B) or Opus packet, shared by all viewers.
#[derive(Clone, Debug)]
pub struct Frame {
    pub media: Media,
    pub data: Bytes,
    /// For video: decoding can start here.
    pub keyframe: bool,
    /// How long it lasts; advances the RTP timestamp.
    pub duration: Duration,
}

impl Frame {
    /// A video frame from an Annex B access unit. Access unit delimiters are
    /// dropped: the receiver adds its own after each frame.
    pub fn new(annex_b: &[u8], keyframe: bool, duration: Duration) -> Self {
        let data = normalize_start_codes(annex_b, &[NAL_ACCESS_UNIT_DELIMITER]);
        Self { media: Media::Video, data, keyframe, duration }
    }

    /// An Opus packet.
    pub fn audio(packet: &[u8], duration: Duration) -> Self {
        Self { media: Media::Audio, data: Bytes::copy_from_slice(packet), keyframe: true, duration }
    }
}

const NAL_ACCESS_UNIT_DELIMITER: u8 = 9;

/// Progress messages, for whoever is running the publisher to show or log.
pub type Log = Arc<dyn Fn(String) + Send + Sync>;

/// Frames (video and audio) that a viewer's sender can fall behind by before
/// it skips ahead to the next keyframe: about half a second.
const FRAME_BUFFER: usize = 48;

struct Watcher {
    task: JoinHandle<()>,
    answers: mpsc::UnboundedSender<String>,
}

pub struct Publisher {
    signaling: Signaling,
    frames: broadcast::Sender<Frame>,
    keyframe_wanted: Arc<AtomicBool>,
    ice_servers: Vec<String>,
    /// Offer an audio track too.
    audio: bool,
    log: Log,
    watchers: HashMap<PeerId, Watcher>,
}

impl Publisher {
    /// `audio`: whether this stream can carry audio (viewers get an audio
    /// track; it's fine if no packets ever arrive on it).
    pub fn new(signaling: Signaling, ice_servers: Vec<String>, audio: bool, log: Log) -> Self {
        let (frames, _) = broadcast::channel(FRAME_BUFFER);
        Self {
            signaling,
            frames,
            keyframe_wanted: Arc::new(AtomicBool::new(true)),
            ice_servers,
            audio,
            log,
            watchers: HashMap::new(),
        }
    }

    /// Where to send encoded frames. Sending with nobody watching is fine.
    pub fn frames(&self) -> broadcast::Sender<Frame> {
        self.frames.clone()
    }

    /// Set when a viewer needs a keyframe; an encoder that can produce one
    /// on demand should check (and clear) it before each frame.
    pub fn keyframe_wanted(&self) -> Arc<AtomicBool> {
        self.keyframe_wanted.clone()
    }

    /// Handle a signal from `from` that's meant for a streamer (`watch`,
    /// `unwatch`, `answer`, `keyframe_request`). Returns false for others.
    pub fn handle(&mut self, from: PeerId, name: &str, signal: &Signal) -> bool {
        match signal {
            Signal::Watch => self.watch(from, name),
            Signal::Unwatch => self.unwatch(from),
            Signal::Answer { sdp } => {
                if let Some(w) = self.watchers.get(&from) {
                    let _ = w.answers.send(sdp.clone());
                }
            }
            Signal::KeyframeRequest => self.keyframe_wanted.store(true, Ordering::Relaxed),
            _ => return false,
        }
        true
    }

    /// Start (or restart) sending to `id`.
    pub fn watch(&mut self, id: PeerId, name: &str) {
        self.unwatch(id);
        let (answers, answer_rx) = mpsc::unbounded_channel();
        let ctx = ViewerCtx {
            id,
            name: name.to_string(),
            signaling: self.signaling.clone(),
            frames: self.frames.clone(),
            keyframe_wanted: self.keyframe_wanted.clone(),
            ice_servers: self.ice_servers.clone(),
            audio: self.audio,
            log: self.log.clone(),
        };
        let log = self.log.clone();
        let task = tokio::spawn(async move {
            let (id, name) = (ctx.id, ctx.name.clone());
            match serve_viewer(ctx, answer_rx).await {
                Ok(()) => log(format!("{name} ({id}) stopped watching")),
                Err(e) => log(format!("{name} ({id}): {e:#}")),
            }
        });
        self.watchers.insert(id, Watcher { task, answers });
    }

    /// Stop sending to `id` (they switched away or left).
    pub fn unwatch(&mut self, id: PeerId) {
        if let Some(w) = self.watchers.remove(&id) {
            w.task.abort();
        }
    }

    /// Everyone currently watching (or connecting).
    pub fn watchers(&self) -> Vec<PeerId> {
        self.watchers.keys().copied().collect()
    }

    /// Stop sending to everyone.
    pub fn clear(&mut self) {
        for (_, w) in self.watchers.drain() {
            w.task.abort();
        }
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        self.clear();
    }
}

struct ViewerCtx {
    id: PeerId,
    name: String,
    signaling: Signaling,
    frames: broadcast::Sender<Frame>,
    keyframe_wanted: Arc<AtomicBool>,
    ice_servers: Vec<String>,
    audio: bool,
    log: Log,
}

/// An outgoing track and what's needed to write to it.
struct Outgoing {
    track: Arc<TrackLocalStaticSample>,
    ssrc: u32,
    payload_type: u8,
}

impl Outgoing {
    async fn write(&self, frame: Frame) -> Result<()> {
        let sample = Sample { data: frame.data, duration: frame.duration, ..Sample::new(Instant::now()) };
        self.track.sample_writer(self.ssrc, self.payload_type).write_sample(&sample).await?;
        Ok(())
    }
}

fn local_track(kind: RtpCodecKind, id: &str, codec: rtc::rtp_transceiver::rtp_sender::RTCRtpCodec) -> Result<(Arc<TrackLocalStaticSample>, u32)> {
    let ssrc = random_ssrc();
    let track = TrackLocalStaticSample::new(
        Instant::now(),
        MediaStreamTrack::new(
            "mpvlink".to_string(),
            format!("mpvlink-{id}"),
            format!("mpvlink {id}"),
            kind,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters { ssrc: Some(ssrc), ..Default::default() },
                codec,
                ..Default::default()
            }],
        ),
    )?;
    Ok((Arc::new(track), ssrc))
}

/// Offer our video to one viewer, then forward encoded frames to them until
/// they leave or the connection fails.
async fn serve_viewer(ctx: ViewerCtx, mut answers: mpsc::UnboundedReceiver<String>) -> Result<()> {
    let (pc, mut events) = new_peer_connection(&ctx.ice_servers).await?;

    let (video_track, video_ssrc) = local_track(RtpCodecKind::Video, "video", h264_codec())?;
    let video_sender = pc.add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal>).await?;
    let audio = if ctx.audio {
        let (track, ssrc) = local_track(RtpCodecKind::Audio, "audio", opus_codec())?;
        let sender = pc.add_track(Arc::clone(&track) as Arc<dyn TrackLocal>).await?;
        Some((track, ssrc, sender))
    } else {
        None
    };

    let offer = pc.create_offer(None).await?;
    pc.set_local_description(offer).await?;
    let mut deferred = peer::wait_for_gathering(&mut events).await;
    let local = pc.local_description().await.context("no local description")?;
    ctx.signaling.signal(ctx.id, Signal::Offer { sdp: local.sdp });

    let answer = answers.recv().await.context("viewer left before answering")?;
    pc.set_remote_description(RTCSessionDescription::answer(answer)?).await?;

    // Wait until connected.
    loop {
        let event = match deferred.pop() {
            Some(e) => e,
            None => events.recv().await.context("peer connection closed")?,
        };
        match event {
            PeerEvent::State(ConnectionState::Connected) => break,
            PeerEvent::State(s @ (ConnectionState::Failed | ConnectionState::Closed)) => bail!("connection {s}"),
            _ => {}
        }
    }
    (ctx.log)(format!("{} ({}) is watching", ctx.name, ctx.id));

    let negotiated = |params: rtc::rtp_transceiver::rtp_sender::RTCRtpSendParameters| {
        params.rtp_parameters.codecs.first().map(|c| c.payload_type).context("no negotiated codec")
    };
    let video = Outgoing {
        payload_type: negotiated(video_sender.get_parameters().await?)?,
        track: video_track,
        ssrc: video_ssrc,
    };
    let audio = match audio {
        Some((track, ssrc, sender)) => Some(Outgoing { payload_type: negotiated(sender.get_parameters().await?)?, track, ssrc }),
        None => None,
    };

    // Start them on a keyframe; skip video frames until one comes. Audio
    // needs no such start, so it flows right away.
    let mut frames = ctx.frames.subscribe();
    ctx.keyframe_wanted.store(true, Ordering::Relaxed);
    let mut waiting_for_keyframe = true;
    loop {
        tokio::select! {
            frame = frames.recv() => match frame {
                Ok(frame) if frame.media == Media::Audio => {
                    if let Some(audio) = &audio {
                        audio.write(frame).await?;
                    }
                }
                Ok(frame) => {
                    if waiting_for_keyframe && !frame.keyframe {
                        continue;
                    }
                    waiting_for_keyframe = false;
                    video.write(frame).await?;
                }
                // We fell behind and frames were dropped: resync on a keyframe.
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    waiting_for_keyframe = true;
                    ctx.keyframe_wanted.store(true, Ordering::Relaxed);
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            event = events.recv() => match event {
                Some(PeerEvent::State(s @ (ConnectionState::Failed | ConnectionState::Closed))) => {
                    (ctx.log)(format!("{} ({}) connection {s}", ctx.name, ctx.id));
                    break;
                }
                Some(_) => {}
                None => break,
            },
        }
    }
    let _ = pc.close().await;
    Ok(())
}
