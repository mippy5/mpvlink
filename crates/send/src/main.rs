//! mpvlink-send: a test publisher.
//!
//! Joins a room, streaming a generated H.264 test pattern to everyone who
//! chooses to watch it (one peer connection each). The top-left of each frame
//! encodes the send time as 32 black/white blocks (milliseconds since the Unix
//! epoch, low 32 bits, most significant first) so glass-to-glass latency can
//! be measured from a screenshot of the viewer.
//!
//! Usage: mpvlink-send --room CODE [--server URL] [--name NAME]
//!                     [--size WxH] [--fps N] [--kbps N] [--stun URL | --no-stun]

mod pattern;

use anyhow::{Context, Result, bail};
use mpvlink_rtc::peer;
use mpvlink_rtc::proto::{PeerId, ServerMsg, normalize_name, normalize_room};
use mpvlink_rtc::publish::{Frame, Publisher};
use mpvlink_rtc::signaling::{JoinInfo, Signaling, SignalingEvent};
use openh264::OpenH264API;
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, Profile, RateControlMode,
    SpsPpsStrategy, UsageType,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

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

#[tokio::main]
async fn main() -> Result<()> {
    let args = Arc::new(parse_args()?);
    let join = JoinInfo { room: args.room.clone(), name: args.name.clone(), streaming: true };
    let (signaling, mut incoming) = Signaling::connect(&args.server, join).await?;
    let log = Arc::new(|line: String| println!("{line}"));
    let mut publisher = Publisher::new(signaling.clone(), args.ice_servers.clone(), log);

    // Encoder runs on its own thread; frames fan out to viewers.
    {
        let (args, frames, keyframe_wanted) = (args.clone(), publisher.frames(), publisher.keyframe_wanted());
        std::thread::Builder::new()
            .name("encoder".into())
            .spawn(move || {
                if let Err(e) = run_encoder(&args, &frames, &keyframe_wanted) {
                    eprintln!("encoder stopped: {e:#}");
                    std::process::exit(1);
                }
            })?;
    }

    // Names of everyone else in the room, for messages.
    let mut names: HashMap<PeerId, String> = HashMap::new();
    let mut welcomed = false;
    loop {
        let event = tokio::select! {
            event = incoming.recv() => event,
            _ = tokio::signal::ctrl_c() => break,
        };
        let msg = match event {
            Some(SignalingEvent::Message(msg)) => msg,
            Some(SignalingEvent::Disconnected { error, retry_in }) => {
                println!(
                    "Lost connection to the signaling server ({error}); retrying in {:.1}s. Streams keep going.",
                    retry_in.as_secs_f32()
                );
                continue;
            }
            None => bail!("disconnected from the signaling server"),
        };
        match msg {
            ServerMsg::Welcome { you, peers, resumed } => {
                if !welcomed {
                    println!("Streaming to room {} as {} (peer {you}). Waiting for viewers…", args.room, args.name);
                } else if resumed {
                    println!("Reconnected to the signaling server.");
                } else {
                    // The server lost its state (restarted): everyone has new
                    // ids. Viewers ask to watch again on their own.
                    println!("Reconnected to the signaling server (fresh session, peer {you}).");
                    publisher.clear();
                }
                welcomed = true;
                names = peers.into_iter().map(|p| (p.id, p.name)).collect();
            }
            ServerMsg::PeerJoined { peer } => {
                println!("{} ({}) joined", peer.name, peer.id);
                names.insert(peer.id, peer.name);
            }
            ServerMsg::PeerUpdated { peer } => {
                names.insert(peer.id, peer.name);
            }
            ServerMsg::PeerLeft { id } => {
                publisher.unwatch(id);
                if let Some(name) = names.remove(&id) {
                    println!("{name} ({id}) left");
                }
            }
            ServerMsg::Signal { from, signal } => {
                let name = names.get(&from).map_or("?", String::as_str).to_string();
                publisher.handle(from, &name, &signal);
            }
            ServerMsg::Error { message } => bail!("server: {message}"),
        }
    }
    // Ctrl+C: tell the server we're leaving, so viewers see the stream end
    // right away rather than after the server's 30 s reconnect window.
    println!("Leaving room {}…", args.room);
    drop(publisher);
    signaling.close().await;
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
            let _ = frames.send(Frame::new(&data, keyframe, interval));
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
