//! A room session: who's in the room, whose stream we're watching (fed into
//! the room's FrameQueue for mpv), and our own stream when we're streaming.
//!
//! Runs on its own Tokio runtime so the mpv plugin thread never blocks on
//! network I/O. The plugin sends commands in; progress comes back through a
//! callback.
//!
//! Whom to watch (the "target", drawn as an arrow in the member list): until
//! the user picks someone, it follows whoever is streaming, else points at
//! ourselves. While we're streaming it points at ourselves.

use crate::ffmpeg::{Pipeline, PipelineEvent, StreamSource};
use crate::stream::{self, AudioQueue, FrameQueue};
use anyhow::{Context, Result, bail};
use mpvlink_rtc::h264::{FrameAssembler, is_keyframe};
use mpvlink_rtc::peer::{self, ConnectionState, PeerEvent, new_peer_connection};
use mpvlink_rtc::proto::{PeerId, PeerInfo, ServerMsg, Signal};
use mpvlink_rtc::publish::Publisher;
use mpvlink_rtc::rtc::peer_connection::sdp::RTCSessionDescription;
use mpvlink_rtc::rtc::rtp_transceiver::rtp_sender::RtpCodecKind;
use mpvlink_rtc::rtc::rtp_transceiver::{RTCRtpTransceiverDirection, RTCRtpTransceiverInit};
use mpvlink_rtc::signaling::{JoinInfo, Signaling, SignalingEvent};
use mpvlink_rtc::webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use mpvlink_rtc::webrtc::peer_connection::PeerConnection;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Don't ask for keyframes more often than this; one is usually in flight.
const KEYFRAME_REQUEST_INTERVAL: Duration = Duration::from_millis(500);

pub struct SessionConfig {
    pub server: String,
    pub room: String,
    pub name: String,
    pub ice_servers: Vec<String>,
}

/// Session progress, for the UI.
#[derive(Debug)]
pub enum SessionEvent {
    /// Something to tell the user.
    Status(String),
    /// Video from `peer` arrived: play `mpvlink://ROOM/PEER`, plus its audio
    /// track if `audio`.
    WatchStarted { peer: PeerId, name: String, audio: bool },
    /// Audio from `peer` started after its video did: add the audio track.
    AudioStarted { peer: PeerId },
    /// The stream we were watching ended.
    StreamEnded(String),
    /// The session is over (e.g. rejected by the server).
    Failed(String),
    /// [`Session::room_state`] changed.
    RoomChanged,
    /// The subtitle line of the stream we're watching ("" = none).
    Subtitle(String),
    /// Our own stream stopped because of an error.
    StreamingFailed(String),
}

/// Someone in the room, as far as the signaling server has told us.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub id: PeerId,
    pub name: String,
    pub streaming: bool,
    /// This is us.
    pub you: bool,
}

/// Who's in the room, for the UI.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RoomState {
    /// In order of joining. Empty until the server welcomes us.
    pub members: Vec<Member>,
    /// Cut off from the signaling server and trying to reconnect, so the list
    /// may be out of date.
    pub reconnecting: bool,
    /// Whose stream we're watching; our own id means our own playback (and
    /// that a video opened now gets streamed).
    pub target: Option<PeerId>,
}

impl RoomState {
    pub fn me(&self) -> Option<&Member> {
        self.members.iter().find(|m| m.you)
    }

    pub fn member(&self, id: PeerId) -> Option<&Member> {
        self.members.iter().find(|m| m.id == id)
    }

    /// The arrow points at ourselves.
    pub fn targets_me(&self) -> bool {
        self.target.is_some() && self.target == self.me().map(|m| m.id)
    }

    /// Replace the list with what the server's welcome says.
    fn welcome(&mut self, you: PeerId, name: &str, streaming: bool, peers: Vec<PeerInfo>) {
        self.members = peers.into_iter().map(|p| member(p, false)).collect();
        self.members.push(Member { id: you, name: name.to_string(), streaming, you: true });
        self.reconnecting = false;
        self.sort();
    }

    /// Someone joined, or started/stopped streaming.
    fn upsert(&mut self, peer: PeerInfo) {
        match self.members.iter_mut().find(|m| m.id == peer.id) {
            Some(m) => *m = member(peer, m.you),
            None => {
                self.members.push(member(peer, false));
                self.sort();
            }
        }
    }

    fn left(&mut self, id: PeerId) {
        self.members.retain(|m| m.id != id);
    }

    fn set_my_streaming(&mut self, streaming: bool) {
        if let Some(me) = self.members.iter_mut().find(|m| m.you) {
            me.streaming = streaming;
        }
    }

    /// Server ids count up as people connect, so sorting by id is join order.
    /// (A stable order: rows don't jump around under the mouse.)
    fn sort(&mut self) {
        self.members.sort_by_key(|m| m.id);
    }
}

fn member(p: PeerInfo, you: bool) -> Member {
    Member { id: p.id, name: p.name, streaming: p.streaming, you }
}

/// Requests from the plugin thread.
enum Command {
    Select(PeerId),
    StartStreaming(StreamSource),
    StopStreaming,
    Subtitle(String),
}

pub type Notify = Arc<dyn Fn(SessionEvent) + Send + Sync>;

pub struct Session {
    room: String,
    state: Arc<Mutex<RoomState>>,
    commands: mpsc::UnboundedSender<Command>,
    /// The signaling connection, once made; used to leave cleanly.
    signaling: Arc<Mutex<Option<Signaling>>>,
    /// Set when the session starts shutting down: from then on nothing is
    /// reported, so a late "disconnected" can't reach a newer session.
    closing: Arc<AtomicBool>,
    queue: Arc<FrameQueue>,
    audio: Arc<AudioQueue>,
    runtime: Option<tokio::runtime::Runtime>,
}

impl Session {
    pub fn start(config: SessionConfig, notify: Notify) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("mpvlink-rtc")
            .enable_all()
            .build()?;
        let queue = FrameQueue::new();
        let audio = AudioQueue::new();
        stream::publish(&config.room, queue.clone(), audio.clone());

        let room = config.room.clone();
        let state = Arc::new(Mutex::new(RoomState::default()));
        let signaling = Arc::new(Mutex::new(None));
        let closing = Arc::new(AtomicBool::new(false));
        let notify: Notify = {
            let closing = closing.clone();
            Arc::new(move |event| {
                if !closing.load(Ordering::SeqCst) {
                    notify(event);
                }
            })
        };
        let (commands, command_rx) = mpsc::unbounded_channel();
        let media = Media { video: queue.clone(), audio: audio.clone() };
        let (st, sig) = (state.clone(), signaling.clone());
        runtime.spawn(async move {
            if let Err(e) = run(config, media, st, sig, command_rx, notify.clone()).await {
                notify(SessionEvent::Failed(format!("{e:#}")));
            }
        });
        Ok(Self { room, state, commands, signaling, closing, queue, audio, runtime: Some(runtime) })
    }

    pub fn room(&self) -> &str {
        &self.room
    }

    pub fn room_state(&self) -> RoomState {
        self.state.lock().unwrap().clone()
    }

    /// Point the arrow at `id`: watch their stream (or, for ourselves, our
    /// own playback). Choosing someone else stops our own stream.
    pub fn select(&self, id: PeerId) {
        let _ = self.commands.send(Command::Select(id));
    }

    /// Stream `source` (replacing anything we're streaming already). Only
    /// does anything while the arrow points at ourselves.
    pub fn start_streaming(&self, source: StreamSource) {
        let _ = self.commands.send(Command::StartStreaming(source));
    }

    pub fn stop_streaming(&self) {
        let _ = self.commands.send(Command::StopStreaming);
    }

    /// The subtitle line our own playback shows now, for our viewers.
    pub fn subtitle(&self, text: String) {
        let _ = self.commands.send(Command::Subtitle(text));
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.closing.store(true, Ordering::SeqCst);
        stream::unpublish(&self.room);
        self.queue.end();
        self.audio.end();
        let Some(rt) = self.runtime.take() else { return };
        // Tell the server we're leaving, so others see it right away rather
        // than after its reconnect window. Takes at most a second.
        let signaling = self.signaling.lock().unwrap().take();
        if let Some(signaling) = signaling {
            rt.block_on(signaling.close());
        }
        // Wait (briefly) for tasks to stop: they may hold an mpv handle that
        // becomes invalid once the plugin returns. (Dropping the room task
        // also stops any ffmpeg pipeline, which reads mpv's clock.)
        rt.shutdown_timeout(Duration::from_secs(1));
    }
}

/// Where received video and audio go (mpv reads them via `stream`).
#[derive(Clone)]
struct Media {
    video: Arc<FrameQueue>,
    audio: Arc<AudioQueue>,
}

impl Media {
    /// A new stream is coming: drop what's queued (mpv freezes on the last frame).
    fn reset(&self) {
        self.video.reset();
        self.audio.reset();
    }

    /// No stream any more: mpv reaches the end.
    fn end(&self) {
        self.video.end();
        self.audio.end();
    }
}

async fn run(
    config: SessionConfig,
    media: Media,
    state: Arc<Mutex<RoomState>>,
    signaling_slot: Arc<Mutex<Option<Signaling>>>,
    mut commands: mpsc::UnboundedReceiver<Command>,
    notify: Notify,
) -> Result<()> {
    let join = JoinInfo { room: config.room.clone(), name: config.name.clone(), streaming: false };
    let (signaling, mut incoming) = Signaling::connect(&config.server, join).await?;
    *signaling_slot.lock().unwrap() = Some(signaling.clone());

    let (pipeline_events, mut pipeline_rx) = mpsc::unbounded_channel();
    let mut room = Room {
        config,
        media,
        state,
        notify,
        signaling,
        pipeline_events,
        me: None,
        chosen: false,
        target_name: None,
        watching: None,
        receiver: None,
        publisher: None,
        pipeline: None,
        subtitle: String::new(),
        joined: false,
        reconnecting: false,
    };

    loop {
        tokio::select! {
            event = incoming.recv() => match event {
                Some(SignalingEvent::Message(msg)) => room.on_message(msg)?,
                Some(SignalingEvent::Disconnected { retry_in, .. }) => room.disconnected(retry_in),
                None => break,
            },
            Some(command) = commands.recv() => room.on_command(command),
            Some(event) = pipeline_rx.recv() => room.on_pipeline(event),
        }
    }
    room.stop_watching();
    bail!("Disconnected from the signaling server.")
}

struct Room {
    config: SessionConfig,
    media: Media,
    state: Arc<Mutex<RoomState>>,
    notify: Notify,
    signaling: Signaling,
    pipeline_events: mpsc::UnboundedSender<PipelineEvent>,
    me: Option<PeerId>,
    /// The user picked the target (otherwise it's chosen automatically).
    chosen: bool,
    /// The target's name, to find them again after a server restart.
    target_name: Option<String>,
    /// Whom we've asked for their stream (sent `watch`).
    watching: Option<PeerId>,
    /// The connection receiving it.
    receiver: Option<JoinHandle<()>>,
    /// Our own stream: viewers' connections, and the encoder feeding them.
    publisher: Option<Publisher>,
    pipeline: Option<Pipeline>,
    /// The subtitle line our playback shows now (for new viewers too).
    subtitle: String,
    joined: bool,
    reconnecting: bool,
}

impl Room {
    fn update(&self, f: impl FnOnce(&mut RoomState)) {
        f(&mut self.state.lock().unwrap());
        (self.notify)(SessionEvent::RoomChanged);
    }

    fn status(&self, text: String) {
        (self.notify)(SessionEvent::Status(text));
    }

    fn name_of(&self, id: PeerId) -> String {
        self.state.lock().unwrap().member(id).map_or_else(|| "?".into(), |m| m.name.clone())
    }

    fn disconnected(&mut self, retry_in: Duration) {
        self.reconnecting = true;
        self.update(|s| s.reconnecting = true);
        let video = if self.receiver.is_some() || self.pipeline.is_some() { " Video keeps going." } else { "" };
        self.status(format!(
            "Lost connection to the server. Reconnecting in {}s…{video}",
            retry_in.as_secs_f32().ceil()
        ));
    }

    fn on_message(&mut self, msg: ServerMsg) -> Result<()> {
        match msg {
            ServerMsg::Welcome { you, peers, resumed } => self.welcome(you, peers, resumed),
            ServerMsg::PeerJoined { peer } => {
                self.update(|s| s.upsert(peer));
                self.reconcile();
            }
            ServerMsg::PeerUpdated { peer } => {
                if self.watching == Some(peer.id) && !peer.streaming {
                    (self.notify)(SessionEvent::StreamEnded(format!("{} stopped streaming.", peer.name)));
                }
                self.update(|s| s.upsert(peer));
                self.reconcile();
            }
            ServerMsg::PeerLeft { id } => {
                if let Some(p) = self.publisher.as_mut() {
                    p.unwatch(id);
                }
                if self.watching == Some(id) {
                    (self.notify)(SessionEvent::StreamEnded(format!("{} left.", self.name_of(id))));
                    self.stop_watching();
                }
                self.update(|s| s.left(id));
                self.reconcile();
            }
            ServerMsg::Signal { from, signal } => self.on_signal(from, signal),
            ServerMsg::Error { message } => bail!(message),
        }
        Ok(())
    }

    fn welcome(&mut self, you: PeerId, peers: Vec<PeerInfo>, resumed: bool) {
        if std::mem::replace(&mut self.reconnecting, false) {
            self.status("Reconnected to the server.".into());
        }
        let restarted = self.joined && !resumed;
        if restarted {
            // The server lost its state: everyone has new ids, so connections
            // tied to the old ones are re-established (viewers ask again; we
            // ask our target again). Video freezes briefly meanwhile.
            if let Some(p) = self.publisher.as_mut() {
                p.clear();
            }
            self.stop_watching();
            self.media.reset();
        }
        let streaming = self.pipeline.is_some();
        let name = self.config.name.clone();
        let target_was_me = self.state.lock().unwrap().targets_me();
        self.me = Some(you);
        self.update(|s| {
            s.welcome(you, &name, streaming, peers);
            if restarted {
                s.target = None;
            }
        });
        if restarted {
            // Find whoever we were watching again, by name.
            let state = self.state.lock().unwrap().clone();
            let again = if target_was_me {
                Some(you)
            } else {
                self.target_name.as_ref().and_then(|n| state.members.iter().find(|m| !m.you && &m.name == n)).map(|m| m.id)
            };
            self.chosen &= again.is_some();
            self.update(|s| s.target = again);
        }
        if !self.joined {
            self.joined = true;
            let streamers: Vec<String> =
                self.state.lock().unwrap().members.iter().filter(|m| m.streaming).map(|m| m.name.clone()).collect();
            let room = &self.config.room;
            self.status(match streamers.as_slice() {
                [] => format!("Joined room {room}. Drop a video here to stream it."),
                [one] => format!("Joined room {room}. Connecting to {one}'s stream…"),
                _ => format!("Joined room {room}. Click a name in the list to watch their stream."),
            });
        }
        self.reconcile();
    }

    fn on_signal(&mut self, from: PeerId, signal: Signal) {
        match signal {
            Signal::Offer { sdp } if self.watching == Some(from) => self.receive(from, sdp),
            Signal::Subtitle { text } if self.watching == Some(from) => (self.notify)(SessionEvent::Subtitle(text)),
            Signal::Watch | Signal::Unwatch | Signal::Answer { .. } | Signal::KeyframeRequest => {
                let name = self.name_of(from);
                let Some(publisher) = self.publisher.as_mut() else { return };
                let watch = matches!(signal, Signal::Watch);
                publisher.handle(from, &name, &signal);
                if watch && !self.subtitle.is_empty() {
                    self.signaling.signal(from, Signal::Subtitle { text: self.subtitle.clone() });
                }
            }
            _ => {}
        }
    }

    fn on_command(&mut self, command: Command) {
        match command {
            Command::Select(id) => {
                let state = self.state.lock().unwrap().clone();
                if state.member(id).is_none() {
                    return;
                }
                if Some(id) != self.me && self.pipeline.is_some() {
                    self.stop_streaming(None);
                }
                self.chosen = true;
                self.update(|s| s.target = Some(id));
                self.reconcile();
                if Some(id) == self.me && self.pipeline.is_none() {
                    self.status("Drop a video here to stream it to the room.".into());
                } else if let Some(m) = state.member(id).filter(|m| !m.you && !m.streaming) {
                    self.status(format!("{} isn't streaming. You'll see their stream when they start.", m.name));
                }
            }
            Command::StartStreaming(source) => {
                if !self.state.lock().unwrap().targets_me() {
                    return;
                }
                let title = source.title.clone();
                let restart = self.pipeline.take().is_some(); // stop any previous one first
                let publisher = self.publisher.get_or_insert_with(|| {
                    let log = Arc::new(|line: String| eprintln!("[mpvlink] {line}"));
                    Publisher::new(self.signaling.clone(), self.config.ice_servers.clone(), true, log)
                });
                let frames = publisher.frames();
                self.pipeline = Some(Pipeline::start(source, frames, self.pipeline_events.clone()));
                self.signaling.set_streaming(true);
                self.chosen = true;
                self.update(|s| s.set_my_streaming(true));
                if !restart {
                    self.status(format!("Streaming {title} to room {}.", self.config.room));
                }
            }
            Command::StopStreaming => self.stop_streaming(None),
            Command::Subtitle(text) => {
                if text == self.subtitle {
                    return;
                }
                self.subtitle = text;
                if let Some(p) = &self.publisher {
                    for viewer in p.watchers() {
                        self.signaling.signal(viewer, Signal::Subtitle { text: self.subtitle.clone() });
                    }
                }
            }
        }
    }

    fn on_pipeline(&mut self, event: PipelineEvent) {
        match event {
            PipelineEvent::Started { encoder } => eprintln!("[mpvlink] streaming with {encoder}"),
            PipelineEvent::Failed(message) => self.stop_streaming(Some(message)),
        }
    }

    fn stop_streaming(&mut self, error: Option<String>) {
        if self.pipeline.is_none() && self.publisher.is_none() {
            return;
        }
        self.pipeline = None;
        self.publisher = None; // ends every viewer's connection
        self.subtitle.clear();
        self.signaling.set_streaming(false);
        self.update(|s| s.set_my_streaming(false));
        if let Some(message) = error {
            (self.notify)(SessionEvent::StreamingFailed(message));
        }
    }

    /// Decide where the arrow points, then make sure we're watching exactly
    /// that (if they're streaming).
    fn reconcile(&mut self) {
        let Some(me) = self.me else { return };
        let state = self.state.lock().unwrap().clone();
        let mut target = state.target.filter(|t| state.member(*t).is_some());
        if target.is_none() {
            self.chosen = false; // they left: back to automatic
        }
        let streams = |id: PeerId| state.member(id).is_some_and(|m| m.streaming && !m.you);
        if self.pipeline.is_some() {
            target = Some(me);
        } else if !self.chosen && !target.is_some_and(streams) {
            // Automatic: whoever streams (first to join), else ourselves.
            target = Some(state.members.iter().find(|m| m.streaming && !m.you).map_or(me, |m| m.id));
        }
        if target != state.target {
            self.update(|s| s.target = target);
        }
        self.target_name = target.and_then(|t| state.member(t)).map(|m| m.name.clone());

        let want = target.filter(|t| streams(*t));
        if want == self.watching {
            return;
        }
        if let Some(old) = self.watching {
            if state.member(old).is_some() {
                self.signaling.signal(old, Signal::Unwatch);
            }
            self.stop_watching();
        }
        match want {
            Some(id) => {
                // Freeze on the last frame until the new stream starts.
                self.media.reset();
                self.signaling.signal(id, Signal::Watch);
                self.watching = Some(id);
                if self.joined {
                    self.status(format!("Connecting to {}'s stream…", self.name_of(id)));
                }
            }
            // Nothing to watch: end the stream so mpv stops showing it.
            None => self.media.end(),
        }
    }

    fn stop_watching(&mut self) {
        if let Some(task) = self.receiver.take() {
            task.abort();
        }
        if self.watching.take().is_some() {
            (self.notify)(SessionEvent::Subtitle(String::new()));
        }
    }

    /// An offer from the streamer we're watching: (re)connect to it.
    fn receive(&mut self, from: PeerId, sdp: String) {
        if let Some(task) = self.receiver.take() {
            task.abort();
        }
        self.media.reset();
        let ctx = Receiver {
            publisher: from,
            name: self.name_of(from),
            signaling: self.signaling.clone(),
            media: self.media.clone(),
            notify: self.notify.clone(),
            ice_servers: self.config.ice_servers.clone(),
        };
        let notify = self.notify.clone();
        self.receiver = Some(tokio::spawn(async move {
            if let Err(e) = ctx.run(sdp).await {
                notify(SessionEvent::StreamEnded(format!("Stream connection failed: {e:#}")));
            }
        }));
    }
}

/// One peer connection to the streamer we're watching.
struct Receiver {
    publisher: PeerId,
    name: String,
    signaling: Signaling,
    media: Media,
    notify: Notify,
    ice_servers: Vec<String>,
}

impl Receiver {
    async fn run(self, offer: String) -> Result<()> {
        let (pc, mut events) = new_peer_connection(&self.ice_servers).await?;
        let recvonly = || RTCRtpTransceiverInit { direction: RTCRtpTransceiverDirection::Recvonly, ..Default::default() };
        pc.add_transceiver_from_kind(RtpCodecKind::Video, Some(recvonly())).await?;
        // Streams from mpv carry audio; the test sender's don't.
        if offer.contains("m=audio") {
            pc.add_transceiver_from_kind(RtpCodecKind::Audio, Some(recvonly())).await?;
        }
        pc.set_remote_description(RTCSessionDescription::offer(offer)?).await?;
        let answer = pc.create_answer(None).await?;
        pc.set_local_description(answer).await?;
        let mut deferred = peer::wait_for_gathering(&mut events).await;
        deferred.reverse(); // popped from the back below
        let local = pc.local_description().await.context("no local description")?;
        self.signaling.signal(self.publisher, Signal::Answer { sdp: local.sdp });

        let mut readers: Vec<JoinHandle<()>> = Vec::new();
        let started = Arc::new(Started::default());
        let result = loop {
            let event = match deferred.pop() {
                Some(e) => e,
                None => match events.recv().await {
                    Some(e) => e,
                    None => break Ok(()),
                },
            };
            match event {
                PeerEvent::State(ConnectionState::Connected) => {
                    self.signaling.signal(self.publisher, Signal::KeyframeRequest);
                }
                PeerEvent::State(s @ (ConnectionState::Failed | ConnectionState::Closed)) => {
                    break Err(anyhow::anyhow!("connection {s}"));
                }
                PeerEvent::Track(track) => {
                    let task = if track.kind().await == RtpCodecKind::Audio {
                        tokio::spawn(read_audio(
                            track,
                            self.publisher,
                            self.media.audio.clone(),
                            started.clone(),
                            self.notify.clone(),
                        ))
                    } else {
                        tokio::spawn(read_video(
                            track,
                            (self.publisher, self.name.clone()),
                            self.signaling.clone(),
                            self.media.clone(),
                            started.clone(),
                            self.notify.clone(),
                        ))
                    };
                    readers.push(task);
                }
                _ => {}
            }
        };
        for task in readers {
            task.abort();
        }
        let _ = pc.close().await;
        result
    }
}

/// What the plugin has been told about one connection's media, shared by
/// its video and audio readers.
#[derive(Default)]
struct Started {
    video: AtomicBool,
    audio: AtomicBool,
}

/// Reassemble RTP into frames and queue them for mpv.
async fn read_video(
    track: Arc<dyn TrackRemote>,
    (publisher, name): (PeerId, String),
    signaling: Signaling,
    media: Media,
    started: Arc<Started>,
    notify: Notify,
) {
    let mut assembler = FrameAssembler::new();
    let mut last_request: Option<Instant> = None;

    while let Some(event) = track.poll().await {
        let TrackRemoteEvent::OnRtpPacket(packet) = event else { continue };
        let pushed = assembler.push(packet.header.sequence_number, packet.header.marker, &packet.payload);
        let mut want_keyframe = pushed.lost || assembler.waiting_for_keyframe();
        if let Some(frame) = pushed.frame {
            let keyframe = is_keyframe(&frame);
            want_keyframe |= media.video.push(frame, keyframe);
            if keyframe && !started.video.swap(true, Ordering::SeqCst) {
                // Audio arrives continuously, so by the first keyframe it's
                // flowing if the stream has any.
                let audio = media.audio.active() && !started.audio.swap(true, Ordering::SeqCst);
                notify(SessionEvent::WatchStarted { peer: publisher, name: name.clone(), audio });
            }
        }
        if want_keyframe && last_request.is_none_or(|t| t.elapsed() >= KEYFRAME_REQUEST_INTERVAL) {
            signaling.signal(publisher, Signal::KeyframeRequest);
            last_request = Some(Instant::now());
        }
    }
}

/// Opus packets go to mpv as they come (one per RTP packet).
async fn read_audio(
    track: Arc<dyn TrackRemote>,
    publisher: PeerId,
    audio: Arc<AudioQueue>,
    started: Arc<Started>,
    notify: Notify,
) {
    while let Some(event) = track.poll().await {
        let TrackRemoteEvent::OnRtpPacket(packet) = event else { continue };
        if packet.payload.is_empty() {
            continue;
        }
        audio.push(packet.payload);
        // Sound that starts after the picture (e.g. the streamer switched to
        // a file with audio): mpv needs to add the track.
        if started.video.load(Ordering::SeqCst) && !started.audio.swap(true, Ordering::SeqCst) {
            notify(SessionEvent::AudioStarted { peer: publisher });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(id: PeerId, name: &str, streaming: bool) -> PeerInfo {
        PeerInfo { id, name: name.into(), streaming }
    }

    fn names(s: &RoomState) -> Vec<&str> {
        s.members.iter().map(|m| m.name.as_str()).collect()
    }

    #[test]
    fn room_state_tracks_members_in_join_order() {
        let mut s = RoomState { reconnecting: true, ..Default::default() };
        // Welcome lists others in any order; we're added; join order kept.
        s.welcome(5, "Me", false, vec![peer(3, "Cy", false), peer(4, "Pub", true), peer(1, "Al", false)]);
        assert_eq!(names(&s), ["Al", "Cy", "Pub", "Me"]);
        assert!(s.members[3].you && !s.reconnecting);

        s.upsert(peer(9, "Late", false));
        s.left(1);
        assert_eq!(names(&s), ["Cy", "Pub", "Me", "Late"]);
        // Streaming changes update in place.
        s.upsert(peer(3, "Cy", true));
        assert!(s.member(3).unwrap().streaming);
        assert_eq!(s.members.len(), 4);

        s.target = Some(5);
        assert!(s.targets_me());
        s.set_my_streaming(true);
        assert!(s.me().unwrap().streaming);

        // A later welcome (e.g. after a server restart) replaces everything.
        s.welcome(2, "Me", true, vec![peer(1, "Pub", true)]);
        assert_eq!(names(&s), ["Pub", "Me"]);
        assert!(s.me().unwrap().streaming);
    }
}
