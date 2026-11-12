//! mpvlink-send: a test publisher.
//!
//! Joins a room as the publisher and streams a generated H.264 test pattern
//! to every viewer (one peer connection each). The top-left of each frame
//! encodes the send time as 32 black/white blocks (milliseconds since the Unix
//! epoch, low 32 bits, most significant first) so glass-to-glass latency can
//! be measured from a screenshot of the viewer.
//!
//! Usage: mpvlink-send --room CODE [--server URL] [--name NAME]
//!                     [--size WxH] [--fps N] [--kbps N] [--stun URL | --no-stun]

mod pattern;

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use mpvlink_rtc::h264::normalize_start_codes;
use mpvlink_rtc::peer::{self, ConnectionState, PeerEvent, h264_codec, new_peer_connection, random_ssrc};
use mpvlink_rtc::proto::{ClientMsg, PeerId, Role, ServerMsg, Signal, normalize_name, normalize_room};
use mpvlink_rtc::rtc::media::Sample;
use mpvlink_rtc::rtc::media_stream::MediaStreamTrack;
use mpvlink_rtc::rtc::peer_connection::sdp::RTCSessionDescription;
use mpvlink_rtc::rtc::rtp_transceiver::rtp_sender::{RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind};
use mpvlink_rtc::signaling::Signaling;
use mpvlink_rtc::webrtc::media_stream::track_local::TrackLocal;
use mpvlink_rtc::webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use mpvlink_rtc::webrtc::peer_connection::PeerConnection;
use openh264::OpenH264API;
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, Profile, RateControlMode,
    SpsPpsStrategy, UsageType,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};

struct Args {
    server: String,
    room: String,
    name: String,
    width: usize,
    height: usize,
    fps: u32,
    kbps: u32,
    ice_servers: Vec<String>,
}

/// One encoded frame, shared by all viewers.
#[derive(Clone)]
struct Frame {
    data: Bytes,
    keyframe: bool,
    duration: Duration,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Arc::new(parse_args()?);
    let (signaling, mut incoming) = Signaling::connect(&args.server).await?;
    signaling.send(ClientMsg::Join { room: args.room.clone(), name: args.name.clone(), role: Role::Publisher });

    // Encoder runs on its own thread; frames fan out to viewers.
    let (frames_tx, _) = broadcast::channel::<Frame>(8);
    let keyframe_wanted = Arc::new(AtomicBool::new(true));
    {
        let (args, frames_tx, keyframe_wanted) = (args.clone(), frames_tx.clone(), keyframe_wanted.clone());
        std::thread::Builder::new()
            .name("encoder".into())
            .spawn(move || {
                if let Err(e) = run_encoder(&args, &frames_tx, &keyframe_wanted) {
                    eprintln!("encoder stopped: {e:#}");
                    std::process::exit(1);
                }
            })?;
    }

    // Per-viewer tasks, plus a channel to forward each one its answer.
    let mut viewers: HashMap<PeerId, (tokio::task::JoinHandle<()>, mpsc::UnboundedSender<String>)> =
        HashMap::new();
    let spawn_viewer = |id: PeerId, name: String| {
        let (answer_tx, answer_rx) = mpsc::unbounded_channel();
        let ctx = ViewerCtx {
            id,
            name,
            signaling: signaling.clone(),
            frames: frames_tx.clone(),
            keyframe_wanted: keyframe_wanted.clone(),
            ice_servers: args.ice_servers.clone(),
        };
        let task = tokio::spawn(async move {
            let (id, name) = (ctx.id, ctx.name.clone());
            match serve_viewer(ctx, answer_rx).await {
                Ok(()) => println!("{name} ({id}) disconnected"),
                Err(e) => println!("{name} ({id}): {e:#}"),
            }
        });
        (task, answer_tx)
    };

    loop {
        let msg = tokio::select! {
            msg = incoming.recv() => msg,
            _ = tokio::signal::ctrl_c() => break,
        };
        let Some(msg) = msg else { bail!("lost connection to the signaling server") };
        match msg {
            ServerMsg::Welcome { you, peers } => {
                println!("Streaming to room {} as {} (peer {you}). Waiting for viewers…", args.room, args.name);
                for p in peers.into_iter().filter(|p| p.role == Role::Viewer) {
                    viewers.insert(p.id, spawn_viewer(p.id, p.name));
                }
            }
            ServerMsg::PeerJoined { peer } if peer.role == Role::Viewer => {
                println!("{} ({}) joined", peer.name, peer.id);
                viewers.insert(peer.id, spawn_viewer(peer.id, peer.name));
            }
            ServerMsg::PeerJoined { .. } => {}
            ServerMsg::PeerLeft { id } => {
                if let Some((task, _)) = viewers.remove(&id) {
                    task.abort();
                }
            }
            ServerMsg::Signal { from, signal: Signal::Answer { sdp } } => {
                if let Some((_, answer_tx)) = viewers.get(&from) {
                    let _ = answer_tx.send(sdp);
                }
            }
            ServerMsg::Signal { signal: Signal::KeyframeRequest, .. } => {
                keyframe_wanted.store(true, Ordering::Relaxed);
            }
            ServerMsg::Signal { .. } => {}
            ServerMsg::Error { message } => bail!("server: {message}"),
        }
    }
    Ok(())
}

struct ViewerCtx {
    id: PeerId,
    name: String,
    signaling: Signaling,
    frames: broadcast::Sender<Frame>,
    keyframe_wanted: Arc<AtomicBool>,
    ice_servers: Vec<String>,
}

/// Offer our video to one viewer, then forward encoded frames to them until
/// they leave or the connection fails.
async fn serve_viewer(ctx: ViewerCtx, mut answers: mpsc::UnboundedReceiver<String>) -> Result<()> {
    let (pc, mut events) = new_peer_connection(&ctx.ice_servers).await?;

    let ssrc = random_ssrc();
    let track = Arc::new(TrackLocalStaticSample::new(
        Instant::now(),
        MediaStreamTrack::new(
            "mpvlink".to_string(),
            "mpvlink-video".to_string(),
            "mpvlink video".to_string(),
            RtpCodecKind::Video,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters { ssrc: Some(ssrc), ..Default::default() },
                codec: h264_codec(),
                ..Default::default()
            }],
        ),
    )?);
    let sender = pc.add_track(Arc::clone(&track) as Arc<dyn TrackLocal>).await?;

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
    println!("{} ({}) connected", ctx.name, ctx.id);

    let payload_type = sender
        .get_parameters()
        .await?
        .rtp_parameters
        .codecs
        .first()
        .map(|c| c.payload_type)
        .context("no negotiated codec")?;

    // Start them on a keyframe; skip frames until one comes.
    let mut frames = ctx.frames.subscribe();
    ctx.keyframe_wanted.store(true, Ordering::Relaxed);
    let mut waiting_for_keyframe = true;
    loop {
        tokio::select! {
            frame = frames.recv() => match frame {
                Ok(frame) => {
                    if waiting_for_keyframe && !frame.keyframe {
                        continue;
                    }
                    waiting_for_keyframe = false;
                    track
                        .sample_writer(ssrc, payload_type)
                        .write_sample(&Sample { data: frame.data, duration: frame.duration, ..Sample::new(Instant::now()) })
                        .await?;
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
                    println!("{} ({}) connection {s}", ctx.name, ctx.id);
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

/// Generate and encode frames at a steady rate until the process exits.
fn run_encoder(args: &Args, frames: &broadcast::Sender<Frame>, keyframe_wanted: &AtomicBool) -> Result<()> {
    let config = EncoderConfig::new()
        .usage_type(UsageType::CameraVideoRealTime)
        .profile(Profile::Baseline)
        .rate_control_mode(RateControlMode::Bitrate)
        .bitrate(BitRate::from_bps(args.kbps * 1000))
        .max_frame_rate(FrameRate::from_hz(args.fps as f32))
        // Keyframes on request (joins, loss); periodic ones are a safety net.
        .intra_frame_period(IntraFramePeriod::from_num_frames(args.fps * 4))
        .sps_pps_strategy(SpsPpsStrategy::ConstantId)
        // Dropping frames to hit the bitrate would look like stutter.
        .skip_frames(false);
    let mut encoder = Encoder::with_api_config(OpenH264API::from_source(), config)?;
    let mut pattern = pattern::TestPattern::new(args.width, args.height);

    let interval = Duration::from_secs_f64(1.0 / args.fps as f64);
    let mut next = Instant::now();
    loop {
        if keyframe_wanted.swap(false, Ordering::Relaxed) {
            encoder.force_intra_frame();
        }
        let yuv = pattern.next_frame();
        let bitstream = encoder.encode(&yuv)?;
        let keyframe = matches!(bitstream.frame_type(), FrameType::IDR | FrameType::I);
        let data = bitstream.to_vec();
        if !data.is_empty() {
            // No receivers is fine (nobody watching yet).
            let _ = frames.send(Frame { data: normalize_start_codes(&data), keyframe, duration: interval });
        }

        next += interval;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now; // fell behind; don't try to catch up with a burst
        }
    }
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        server: "ws://127.0.0.1:8787".into(),
        room: String::new(),
        name: "Test pattern".into(),
        width: 1280,
        height: 720,
        fps: 30,
        kbps: 4000,
        ice_servers: vec![peer::DEFAULT_STUN_SERVER.into()],
    };
    let mut custom_stun = false;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().with_context(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--server" => args.server = value()?,
            "--room" => args.room = normalize_room(&value()?).context("room codes are 4-16 letters/digits")?,
            "--name" => args.name = normalize_name(&value()?).context("empty name")?,
            "--size" => {
                let v = value()?;
                let (w, h) = v.split_once('x').context("--size is WIDTHxHEIGHT")?;
                (args.width, args.height) = (w.parse()?, h.parse()?);
                if !args.width.is_multiple_of(2) || !args.height.is_multiple_of(2) {
                    bail!("--size dimensions must be even");
                }
            }
            "--fps" => args.fps = value()?.parse()?,
            "--kbps" => args.kbps = value()?.parse()?,
            "--stun" => {
                if !custom_stun {
                    args.ice_servers.clear();
                    custom_stun = true;
                }
                args.ice_servers.push(value()?);
            }
            "--no-stun" => args.ice_servers.clear(),
            "-h" | "--help" => {
                println!(
                    "usage: mpvlink-send --room CODE [--server ws://127.0.0.1:8787] [--name NAME]\n\
                     \x20                   [--size 1280x720] [--fps 30] [--kbps 4000] [--stun URL]... [--no-stun]"
                );
                std::process::exit(0);
            }
            other => bail!("unknown argument {other:?} (try --help)"),
        }
    }
    if args.room.is_empty() {
        bail!("--room is required (try --help)");
    }
    if args.fps == 0 {
        bail!("--fps must be positive");
    }
    Ok(args)
}
