//! The streaming side: one peer connection per viewer who asked to watch,
//! all fed from the same stream of encoded video frames and audio packets.

use crate::files;
use webrtc::data_channel::{DataChannelEvent, RTCDataChannelState};
use crate::h264::{insert_before_slices, normalize_start_codes, position_sei};
use crate::peer::{self, ConnectionState, PeerEvent, h264_codec, new_peer_connection, opus_codec, random_ssrc};
use crate::signaling::Signaling;
use crate::stats::{LinkStats, link_stats};
use anyhow::{Context, Result, bail};
use bytes::Bytes;
use mpvlink_proto::{PeerId, Signal};
use rtc::media::Sample;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::rtp_transceiver::rtp_sender::{RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc, watch};
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

    /// A video frame that's at `position` seconds in the streamed file
    /// (receivers can read it with [`crate::h264::read_position`]).
    pub fn video_at(annex_b: &[u8], keyframe: bool, duration: Duration, position: f64) -> Self {
        let frame = Self::new(annex_b, keyframe, duration);
        let data = insert_before_slices(&frame.data, &position_sei(position));
        Self { data, ..frame }
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
    name: String,
    task: JoinHandle<()>,
    answers: mpsc::UnboundedSender<String>,
    /// The connection's latest stats and send rate in kb/s (None until connected).
    stats: Arc<Mutex<Option<(LinkStats, f64)>>>,
}

/// How often each viewer's connection stats are read.
const STATS_EVERY: Duration = Duration::from_secs(1);

pub struct Publisher {
    signaling: Signaling,
    frames: broadcast::Sender<Frame>,
    /// Files for viewers (subtitle tracks and fonts), sent over a data
    /// channel; None: none.
    pack: watch::Sender<Option<Arc<files::Pack>>>,
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
        let (pack, _) = watch::channel(None);
        Self {
            signaling,
            frames,
            pack,
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

    /// Files every viewer should have (None: none). Sent to each viewer
    /// once connected, and again whenever they change.
    pub fn set_pack(&self, pack: Option<Arc<files::Pack>>) {
        self.pack.send_replace(pack);
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
        let stats = Arc::new(Mutex::new(None));
        let ctx = ViewerCtx {
            pack: self.pack.subscribe(),
            stats: stats.clone(),
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
        self.watchers.insert(id, Watcher { name: name.to_string(), task, answers, stats });
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

    /// Connection stats for each viewer that's connected: (id, name, stats,
    /// kb/s being sent to them).
    pub fn viewer_stats(&self) -> Vec<(PeerId, String, LinkStats, f64)> {
        let mut out: Vec<_> = self
            .watchers
            .iter()
            .filter_map(|(id, w)| {
                let (stats, kbps) = w.stats.lock().unwrap().clone()?;
                Some((*id, w.name.clone(), stats, kbps))
            })
            .collect();
        out.sort_by_key(|(id, ..)| *id);
        out
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

/// Stops a task when dropped (when the viewer's connection ends).
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct ViewerCtx {
    id: PeerId,
    pack: watch::Receiver<Option<Arc<files::Pack>>>,
    stats: Arc<Mutex<Option<(LinkStats, f64)>>>,
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
    // Created before the offer, so the offer includes it.
    let channel = pc.create_data_channel(files::LABEL, None).await?;

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

    // Files go on their own task: sending waits whenever the channel's
    // buffer is full, and video mustn't.
    let mut pack = ctx.pack.clone();
    let files_task = tokio::spawn(async move {
        // Wait until the viewer has the channel open.
        while channel.ready_state().await.ok() != Some(RTCDataChannelState::Open) {
            match channel.poll().await {
                Some(DataChannelEvent::OnOpen) => break,
                Some(DataChannelEvent::OnClose) | None => return,
                Some(_) => {}
            }
        }
        loop {
            let current = pack.borrow_and_update().clone();
            if let Err(e) = files::send(channel.as_ref(), current.as_deref()).await {
                eprintln!("[mpvlink] sending files to a viewer: {e}");
                return;
            }
            if pack.changed().await.is_err() {
                return;
            }
        }
    });
    let _abort_files = AbortOnDrop(files_task);

    // Start them on a keyframe; skip video frames until one comes. Audio
    // needs no such start, so it flows right away.
    let mut frames = ctx.frames.subscribe();
    ctx.keyframe_wanted.store(true, Ordering::Relaxed);
    let mut waiting_for_keyframe = true;
    let mut stats_timer = tokio::time::interval(STATS_EVERY);
    let mut last_sent: Option<(Instant, u64)> = None;
    loop {
        tokio::select! {
            _ = stats_timer.tick() => {
                let stats = link_stats(&pc).await;
                let now = Instant::now();
                let kbps = last_sent.map_or(0.0, |(then, bytes)| {
                    let secs = now.duration_since(then).as_secs_f64().max(0.001);
                    stats.bytes_sent.saturating_sub(bytes) as f64 * 8.0 / 1000.0 / secs
                });
                last_sent = Some((now, stats.bytes_sent));
                *ctx.stats.lock().unwrap() = Some((stats, kbps));
            }
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
