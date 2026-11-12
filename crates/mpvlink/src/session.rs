//! A viewer session: joins a room on the signaling server, accepts the
//! publisher's offer, and feeds received video into the room's FrameQueue.
//!
//! Runs on its own Tokio runtime so the mpv plugin thread never blocks on
//! network I/O. Progress is reported through a callback.

use crate::stream::{self, FrameQueue};
use anyhow::{Context, Result, bail};
use mpvlink_rtc::h264::{FrameAssembler, is_keyframe};
use mpvlink_rtc::peer::{self, ConnectionState, PeerEvent, new_peer_connection};
use mpvlink_rtc::proto::{ClientMsg, PeerId, Role, ServerMsg, Signal};
use mpvlink_rtc::rtc::peer_connection::sdp::RTCSessionDescription;
use mpvlink_rtc::rtc::rtp_transceiver::rtp_sender::RtpCodecKind;
use mpvlink_rtc::rtc::rtp_transceiver::{RTCRtpTransceiverDirection, RTCRtpTransceiverInit};
use mpvlink_rtc::signaling::Signaling;
use mpvlink_rtc::webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use mpvlink_rtc::webrtc::peer_connection::PeerConnection;
use std::sync::Arc;
use std::time::{Duration, Instant};
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
    /// The first keyframe arrived: start playing `mpvlink://ROOM`.
    StreamStarted,
    /// The publisher left or the connection dropped.
    StreamEnded(String),
    /// The session is over (e.g. rejected by the server).
    Failed(String),
}

pub type Notify = Arc<dyn Fn(SessionEvent) + Send + Sync>;

pub struct Session {
    room: String,
    queue: Arc<FrameQueue>,
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
        stream::publish(&config.room, queue.clone());

        let room = config.room.clone();
        let q = queue.clone();
        runtime.spawn(async move {
            if let Err(e) = run(config, q, notify.clone()).await {
                notify(SessionEvent::Failed(format!("{e:#}")));
            }
        });
        Ok(Self { room, queue, runtime: Some(runtime) })
    }

    pub fn room(&self) -> &str {
        &self.room
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        stream::unpublish(&self.room);
        self.queue.end();
        // Wait (briefly) for tasks to stop: they may hold an mpv handle that
        // becomes invalid once the plugin returns.
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_timeout(Duration::from_secs(1));
        }
    }
}

async fn run(config: SessionConfig, queue: Arc<FrameQueue>, notify: Notify) -> Result<()> {
    let (signaling, mut incoming) = Signaling::connect(&config.server).await?;
    signaling.send(ClientMsg::Join { room: config.room.clone(), name: config.name.clone(), role: Role::Viewer });

    let mut publisher: Option<(PeerId, String)> = None;
    let mut receiver: Option<JoinHandle<()>> = None;

    while let Some(msg) = incoming.recv().await {
        match msg {
            ServerMsg::Welcome { peers, .. } => {
                publisher = peers.into_iter().find(|p| p.role == Role::Publisher).map(|p| (p.id, p.name));
                notify(SessionEvent::Status(match &publisher {
                    Some((_, name)) => format!("Joined room {}. Connecting to {name}'s stream…", config.room),
                    None => format!("Joined room {}. Waiting for someone to start streaming…", config.room),
                }));
            }
            ServerMsg::PeerJoined { peer } if peer.role == Role::Publisher => {
                notify(SessionEvent::Status(format!("{} started streaming. Connecting…", peer.name)));
                publisher = Some((peer.id, peer.name));
            }
            ServerMsg::PeerJoined { .. } => {}
            ServerMsg::PeerLeft { id } => {
                if let Some((_, name)) = publisher.take_if(|(pid, _)| *pid == id) {
                    if let Some(task) = receiver.take() {
                        task.abort();
                    }
                    queue.end();
                    notify(SessionEvent::StreamEnded(format!("{name} stopped streaming.")));
                }
            }
            ServerMsg::Signal { from, signal: Signal::Offer { sdp } } => {
                if publisher.as_ref().is_none_or(|(pid, _)| *pid != from) {
                    continue; // only the room's publisher may send us video
                }
                // A new offer replaces any previous connection.
                if let Some(task) = receiver.take() {
                    task.abort();
                }
                queue.reset();
                let ctx = Receiver {
                    publisher: from,
                    signaling: signaling.clone(),
                    queue: queue.clone(),
                    notify: notify.clone(),
                    ice_servers: config.ice_servers.clone(),
                };
                let notify = notify.clone();
                receiver = Some(tokio::spawn(async move {
                    if let Err(e) = ctx.run(sdp).await {
                        notify(SessionEvent::StreamEnded(format!("Stream connection failed: {e:#}")));
                    }
                }));
            }
            ServerMsg::Signal { .. } => {}
            ServerMsg::Error { message } => bail!(message),
        }
    }
    if let Some(task) = receiver.take() {
        task.abort();
    }
    bail!("Lost connection to the signaling server.")
}

/// One peer connection to the publisher.
struct Receiver {
    publisher: PeerId,
    signaling: Signaling,
    queue: Arc<FrameQueue>,
    notify: Notify,
    ice_servers: Vec<String>,
}

impl Receiver {
    async fn run(self, offer: String) -> Result<()> {
        let (pc, mut events) = new_peer_connection(&self.ice_servers).await?;
        pc.add_transceiver_from_kind(
            RtpCodecKind::Video,
            Some(RTCRtpTransceiverInit { direction: RTCRtpTransceiverDirection::Recvonly, ..Default::default() }),
        )
        .await?;
        pc.set_remote_description(RTCSessionDescription::offer(offer)?).await?;
        let answer = pc.create_answer(None).await?;
        pc.set_local_description(answer).await?;
        let mut deferred = peer::wait_for_gathering(&mut events).await;
        deferred.reverse(); // popped from the back below
        let local = pc.local_description().await.context("no local description")?;
        self.signaling.signal(self.publisher, Signal::Answer { sdp: local.sdp });

        let mut reader: Option<JoinHandle<()>> = None;
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
                    if let Some(old) = reader.replace(tokio::spawn(read_track(
                        track,
                        self.publisher,
                        self.signaling.clone(),
                        self.queue.clone(),
                        self.notify.clone(),
                    ))) {
                        old.abort();
                    }
                }
                _ => {}
            }
        };
        if let Some(task) = reader {
            task.abort();
        }
        let _ = pc.close().await;
        result
    }
}

/// Reassemble RTP into frames and queue them for mpv.
async fn read_track(
    track: Arc<dyn TrackRemote>,
    publisher: PeerId,
    signaling: Signaling,
    queue: Arc<FrameQueue>,
    notify: Notify,
) {
    let mut assembler = FrameAssembler::new();
    let mut last_request: Option<Instant> = None;
    let mut started = false;

    while let Some(event) = track.poll().await {
        let TrackRemoteEvent::OnRtpPacket(packet) = event else { continue };
        let pushed = assembler.push(packet.header.sequence_number, packet.header.marker, &packet.payload);
        let mut want_keyframe = pushed.lost || assembler.waiting_for_keyframe();
        if let Some(frame) = pushed.frame {
            let keyframe = is_keyframe(&frame);
            want_keyframe |= queue.push(frame, keyframe);
            if keyframe && !started {
                started = true;
                notify(SessionEvent::StreamStarted);
            }
        }
        if want_keyframe && last_request.is_none_or(|t| t.elapsed() >= KEYFRAME_REQUEST_INTERVAL) {
            signaling.signal(publisher, Signal::KeyframeRequest);
            last_request = Some(Instant::now());
        }
    }
}
