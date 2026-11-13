//! Streaming a file: ffmpeg encodes it to H.264, and frames are sent as mpv's
//! own playback reaches them.
//!
//! mpv is the only clock. ffmpeg runs as fast as it can (its output pipe
//! fills up and makes it wait), each frame carries its position in the file,
//! and a frame goes out when mpv's `time-pos` gets there. So pausing, speed
//! changes and encoder delay are all handled by the same rule. Seeks restart
//! ffmpeg at the new position.

use crate::mpegts::{Kind, TsDemuxer, TsFrame, opus_samples};
use mpvlink_rtc::h264::is_keyframe;
use mpvlink_rtc::publish::Frame;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::sync::broadcast;

/// mpv's playback position in seconds, or None while there isn't one.
pub type Clock = Arc<dyn Fn() -> Option<f64> + Send + Sync>;

/// Encoders to try, best first. Hardware ones are nearly free; any that this
/// ffmpeg lacks, or that fail to start (no such GPU), are skipped.
const ENCODERS: &[&str] = &["h264_videotoolbox", "h264_nvenc", "h264_qsv", "libx264", "libopenh264"];

/// Encoded frames ffmpeg can get ahead of playback by: in the pipe to us,
/// and waiting here to be sent (video and audio together).
const LOOKAHEAD_FRAMES: usize = 48;
const MAX_PENDING: usize = 96;
/// A frame due this far after mpv's position means playback jumped back
/// (start over there); this far before it, ffmpeg is behind (skip ahead to
/// the next keyframe rather than sending a backlog).
const MAX_DRIFT: f64 = 1.5;
/// After a seek, restart unless the next frame is already this close.
const SEEK_TOLERANCE: f64 = 0.15;
/// How often to look at the clock while waiting.
const POLL: Duration = Duration::from_millis(10);
/// Audio goes out this much ahead of its place in playback: about what the
/// viewer buffers before playing it (video is shown as soon as it arrives),
/// so the two come out together.
const AUDIO_LEAD: f64 = 0.1;
/// Audio this late is dropped: sending it would only add delay on the other side.
const AUDIO_MAX_LATE: f64 = 0.25;

/// Which audio to stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioChoice {
    /// None (audio is turned off in mpv).
    Off,
    /// This stream of the file (ffmpeg's index).
    Stream(i64),
    /// The file's first audio stream, if it has one.
    First,
}

pub struct StreamSource {
    /// Absolute file path, or an http(s) URL.
    pub path: String,
    pub title: String,
    /// The file's video stream to send (ffmpeg's index), else the first.
    pub video_stream: Option<i64>,
    pub audio: AudioChoice,
    /// The ffmpeg executable.
    pub ffmpeg: String,
    pub kbps: u32,
    pub clock: Clock,
    /// Bumped by the plugin each time mpv finishes a seek.
    pub seeks: Arc<AtomicU64>,
}

#[derive(Debug)]
pub enum PipelineEvent {
    /// Encoding works; `encoder` is the one in use.
    Started { encoder: String },
    /// Streaming can't continue.
    Failed(String),
}

pub struct Pipeline {
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    thread: Option<JoinHandle<()>>,
}

impl Pipeline {
    pub fn start(
        source: StreamSource,
        frames: broadcast::Sender<Frame>,
        events: tokio::sync::mpsc::UnboundedSender<PipelineEvent>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let child = Arc::new(Mutex::new(None));
        let (s, c) = (stop.clone(), child.clone());
        let thread = std::thread::Builder::new()
            .name("mpvlink-ffmpeg".into())
            .spawn(move || Runner { source, frames, events, stop: s, child: c }.run())
            .ok();
        Self { stop, child, thread }
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(child) = self.child.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A frame waiting until it's due.
struct Pending {
    data: Vec<u8>,
    audio: bool,
    keyframe: bool,
    /// Its position in the file, and when (on mpv's clock) to send it.
    file_time: f64,
    at: f64,
    duration: Duration,
}

/// Why one ffmpeg run ended.
enum Ended {
    Stopped,
    /// Start over at this position (a seek, or too far out of step).
    Restart(f64),
    /// ffmpeg exited before producing anything; its error output.
    NoOutput(String),
}

struct Runner {
    source: StreamSource,
    frames: broadcast::Sender<Frame>,
    events: tokio::sync::mpsc::UnboundedSender<PipelineEvent>,
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
}

impl Runner {
    fn run(mut self) {
        let encoders = match find_ffmpeg(&self.source.ffmpeg) {
            Ok((path, found)) if !found.video.is_empty() => {
                self.source.ffmpeg = path;
                if !found.opus {
                    self.source.audio = AudioChoice::Off;
                    eprintln!("[mpvlink] this ffmpeg has no libopus: streaming video only");
                }
                found.video
            }
            Ok(_) => return self.fail("This ffmpeg has no H.264 encoder.".into()),
            Err(e) => {
                return self.fail(format!(
                    "couldn't run ffmpeg ({e}).\nInstall it, or set script-opts mpvlink-ffmpeg=/path/to/ffmpeg."
                ));
            }
        };
        let mut encoder = 0;
        let mut start = (self.source.clock)().unwrap_or(0.0);
        let mut announced = false;
        loop {
            match self.encode_from(start, encoders[encoder], &mut announced) {
                Ended::Stopped => return,
                Ended::Restart(at) => start = at,
                // Nothing from this position (e.g. a seek to the very end):
                // wait for the next seek.
                Ended::NoOutput(_) if announced => match self.wait_for_seek() {
                    Some(at) => start = at,
                    None => return,
                },
                Ended::NoOutput(error) => {
                    encoder += 1;
                    if encoder == encoders.len() {
                        let error = error.trim();
                        let detail = if error.is_empty() { "no output" } else { error };
                        return self.fail(format!("ffmpeg couldn't encode {}: {detail}", self.source.title));
                    }
                }
            }
        }
    }

    /// Block until mpv seeks; its new position, or None if stopped.
    fn wait_for_seek(&self) -> Option<f64> {
        let seeks = self.source.seeks.load(Ordering::SeqCst);
        while !self.stopped() {
            if self.source.seeks.load(Ordering::SeqCst) != seeks {
                return Some((self.source.clock)().unwrap_or(0.0));
            }
            std::thread::sleep(POLL * 5);
        }
        None
    }

    fn fail(&self, message: String) {
        let _ = self.events.send(PipelineEvent::Failed(message));
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// Run ffmpeg from `start` seconds and pace its frames to mpv's clock.
    fn encode_from(&self, start: f64, encoder: &str, announced: &mut bool) -> Ended {
        let mut command = ffmpeg_command(&self.source, start, encoder);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => return Ended::NoOutput(e.to_string()),
        };
        let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
        *self.child.lock().unwrap() = Some(child);
        if self.stopped() {
            self.kill_child();
            return Ended::Stopped;
        }

        let errors = Arc::new(Mutex::new(String::new()));
        let stderr_thread = stderr.map(|mut stderr| {
            let errors = errors.clone();
            std::thread::spawn(move || {
                let mut text = String::new();
                let _ = stderr.read_to_string(&mut text);
                *errors.lock().unwrap() = text;
            })
        });
        let (tx, rx) = sync_channel(LOOKAHEAD_FRAMES);
        let reader = stdout.map(|stdout| std::thread::spawn(move || read_frames(stdout, tx)));

        let ended = self.pace(start, &rx, encoder, announced);

        self.kill_child();
        drop(rx); // unblocks the reader if it's waiting to hand over a frame
        if let Some(reader) = reader {
            let _ = reader.join();
        }
        if let Some(t) = stderr_thread {
            let _ = t.join();
        }
        match ended {
            Ended::NoOutput(_) => Ended::NoOutput(errors.lock().unwrap().clone()),
            other => other,
        }
    }

    fn kill_child(&self) {
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn pace(&self, start: f64, rx: &Receiver<TsFrame>, encoder: &str, announced: &mut bool) -> Ended {
        let clock = &self.source.clock;
        let mut seeks = self.source.seeks.load(Ordering::SeqCst);
        // Timestamp of the first frame, which is at `start` in the file.
        let mut base: Option<i64> = None;
        let mut previous_video: Option<i64> = None;
        let mut position = start; // file position of the last frame sent
        let mut finished = false;
        // Behind schedule: dropping video until a keyframe to resume from.
        let mut skipping = false;
        // Frames read from ffmpeg, per kind. ffmpeg's output has audio ahead
        // of video (the video encoder holds frames back), so each kind waits
        // in its own queue and whichever is due first goes first.
        let mut queues: [VecDeque<Pending>; 2] = Default::default();

        loop {
            if self.stopped() {
                return Ended::Stopped;
            }
            // Take what ffmpeg has produced, up to a limit (beyond it, ffmpeg
            // waits: that's what keeps it from running far ahead).
            let room = |queues: &[VecDeque<Pending>; 2]| queues.iter().map(VecDeque::len).sum::<usize>() < MAX_PENDING;
            while !finished && room(&queues) {
                match rx.try_recv() {
                    Ok(frame) => self.queue_frame(frame, start, &mut base, &mut previous_video, &mut queues, encoder, announced),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => finished = true,
                }
            }
            let next = queues
                .iter()
                .enumerate()
                .filter_map(|(i, q)| q.front().map(|f| (i, f.at)))
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(i, _)| i);
            let Some(now) = clock() else {
                std::thread::sleep(POLL);
                continue;
            };

            let Some(kind) = next else {
                if finished && base.is_none() {
                    return Ended::NoOutput(String::new());
                }
                // Waiting on ffmpeg (or at the end): still follow seeks.
                if let Some(restart) = self.check_seek(&mut seeks, position) {
                    return restart;
                }
                if finished {
                    std::thread::sleep(POLL * 5);
                } else if let Ok(frame) = rx.recv_timeout(POLL) {
                    self.queue_frame(frame, start, &mut base, &mut previous_video, &mut queues, encoder, announced);
                }
                continue;
            };
            let frame = queues[kind].front().expect("picked a non-empty queue");

            let seeked = self.source.seeks.load(Ordering::SeqCst);
            if seeked != seeks {
                seeks = seeked;
                if (frame.file_time - now).abs() > SEEK_TOLERANCE {
                    return Ended::Restart(now);
                }
            }
            if frame.file_time - now > MAX_DRIFT {
                return Ended::Restart(now); // playback went back
            }
            // Video behind: drop frames until a keyframe to resume from
            // (frames in between can't be decoded without the ones dropped).
            if !frame.audio {
                if frame.keyframe {
                    skipping = false;
                } else if now - frame.at > MAX_DRIFT {
                    skipping = true;
                }
            }
            let due = frame.at <= now + 0.004 || (!frame.audio && skipping);
            if !due {
                // Wait for it, but keep reading: a frame of the other kind
                // that's due sooner may arrive meanwhile.
                let wait = Duration::from_secs_f64(frame.at - now).min(POLL * 2);
                if !finished && room(&queues) {
                    if let Ok(frame) = rx.recv_timeout(wait) {
                        self.queue_frame(frame, start, &mut base, &mut previous_video, &mut queues, encoder, announced);
                    }
                } else {
                    std::thread::sleep(wait);
                }
                continue;
            }
            let frame = queues[kind].pop_front().expect("non-empty");
            position = frame.file_time;
            if frame.audio {
                if now - frame.at <= AUDIO_MAX_LATE {
                    let _ = self.frames.send(Frame::audio(&frame.data, frame.duration));
                }
            } else if !skipping {
                let _ = self.frames.send(Frame::new(&frame.data, frame.keyframe, frame.duration));
            }
        }
    }

    /// Work out when a frame from ffmpeg is due and queue it by kind.
    #[allow(clippy::too_many_arguments)]
    fn queue_frame(
        &self,
        frame: TsFrame,
        start: f64,
        base: &mut Option<i64>,
        previous_video: &mut Option<i64>,
        queues: &mut [VecDeque<Pending>; 2],
        encoder: &str,
        announced: &mut bool,
    ) {
        if !*announced {
            *announced = true;
            let _ = self.events.send(PipelineEvent::Started { encoder: encoder.to_string() });
        }
        let base = *base.get_or_insert(frame.pts);
        let file_time = start + (frame.pts - base) as f64 / 90_000.0;
        let audio = frame.kind == Kind::Audio;
        let (at, duration) = if audio {
            let samples = opus_samples(&frame.data).unwrap_or(960);
            (file_time - AUDIO_LEAD, Duration::from_secs_f64(f64::from(samples) / 48_000.0))
        } else {
            let d = previous_video.map(|p| ((frame.pts - p) as f64 / 90_000.0).clamp(0.001, 1.0)).unwrap_or(0.033);
            *previous_video = Some(frame.pts);
            (file_time, Duration::from_secs_f64(d))
        };
        let keyframe = !audio && is_keyframe(&frame.data);
        queues[usize::from(audio)].push_back(Pending { data: frame.data, audio, keyframe, file_time, at, duration });
    }

    /// While no frame is due: restart if mpv seeked away from `position`, or
    /// jumped back (e.g. looping).
    fn check_seek(&self, seeks: &mut u64, position: f64) -> Option<Ended> {
        let now = (self.source.clock)()?;
        let seeked = self.source.seeks.load(Ordering::SeqCst);
        let moved = seeked != *seeks && (now - position).abs() > SEEK_TOLERANCE;
        *seeks = seeked;
        (moved || now < position - MAX_DRIFT).then_some(Ended::Restart(now))
    }
}

/// Read ffmpeg's MPEG-TS output and hand over whole frames.
fn read_frames(mut stdout: impl Read, tx: SyncSender<TsFrame>) {
    let mut demux = TsDemuxer::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut out = Vec::new();
    loop {
        let n = match stdout.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        demux.push(&buf[..n], &mut out);
        for frame in out.drain(..) {
            if tx.send(frame).is_err() {
                return;
            }
        }
    }
    demux.finish(&mut out);
    for frame in out {
        let _ = tx.send(frame);
    }
}

fn ffmpeg_command(source: &StreamSource, start: f64, encoder: &str) -> Command {
    let mut c = Command::new(&source.ffmpeg);
    c.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
    if start > 0.0 {
        // Before -i: a fast seek that still starts exactly at `start`.
        c.args(["-ss", &format!("{start:.3}")]);
    }
    let video = source.video_stream.map_or_else(|| "0:v:0".to_string(), |i| format!("0:{i}"));
    c.args(["-i", &source.path, "-map", &video]);
    let audio = match source.audio {
        AudioChoice::Off => None,
        AudioChoice::Stream(i) => Some(format!("0:{i}")),
        AudioChoice::First => Some("0:a:0?".to_string()), // "?": fine if there's none
    };
    match audio {
        // Opus, 48 kHz stereo, 20 ms packets (what WebRTC expects).
        Some(map) => c.args(["-map", &map, "-c:a", "libopus", "-b:a", "128k", "-ac", "2", "-ar", "48000"])
            .args(["-frame_duration", "20", "-application", "audio"]),
        None => c.arg("-an"),
    };
    c.args(["-sn", "-dn"]);
    // At most 1080p, 8-bit 4:2:0 (what every H.264 decoder handles).
    c.args([
        "-vf",
        "scale=w='min(iw,1920)':h='min(ih,1080)':force_original_aspect_ratio=decrease:force_divisible_by=2,format=yuv420p",
    ]);
    let kbps = source.kbps;
    let (rate, buffer) = (format!("{kbps}k"), format!("{}k", kbps / 2));
    c.args(["-c:v", encoder]);
    match encoder {
        "h264_videotoolbox" => c.args(["-realtime", "1", "-b:v", &rate]),
        "h264_nvenc" => c.args(["-preset", "p4", "-tune", "ll", "-forced-idr", "1", "-b:v", &rate, "-maxrate", &rate, "-bufsize", &buffer]),
        "h264_qsv" => c.args(["-preset", "veryfast", "-forced_idr", "1", "-b:v", &rate, "-maxrate", &rate]),
        "libx264" => c.args(["-preset", "veryfast", "-tune", "zerolatency", "-b:v", &rate, "-maxrate", &rate, "-bufsize", &buffer]),
        _ => c.args(["-b:v", &rate]),
    };
    // No B-frames (no reordering delay); a keyframe every second (where new
    // viewers can start, and where a viewer recovers after packet loss), and
    // only those.
    c.args(["-bf", "0", "-g", "600", "-force_key_frames", "expr:gte(t,n_forced*1)"]);
    // One PES per audio packet, so audio arrives evenly rather than in bursts.
    c.args(["-f", "mpegts", "-muxdelay", "0", "-muxpreload", "0", "-pes_payload_size", "0", "pipe:1"]);
    c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    hide_console_window(&mut c);
    c
}

/// The ffmpeg that streaming would use for the `configured` setting, or a
/// short explanation of why there isn't a usable one. Runs ffmpeg once.
pub fn locate(configured: &str) -> Result<String, String> {
    match find_ffmpeg(configured) {
        Ok((path, found)) if !found.video.is_empty() => Ok(on_path(&path).unwrap_or(path)),
        Ok((path, _)) => Err(format!("{path} has no H.264 encoder.")),
        Err(_) if configured == "ffmpeg" => Err("No ffmpeg found.".into()),
        Err(e) => Err(format!("Couldn't run it ({e}).")),
    }
}

/// Where a bare program name like "ffmpeg" is found on PATH (to show it).
fn on_path(name: &str) -> Option<String> {
    if name.contains(['/', '\\']) {
        return None;
    }
    let exe = if cfg!(windows) && !name.to_ascii_lowercase().ends_with(".exe") { format!("{name}.exe") } else { name.to_string() };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(&exe))
        .find(|p| p.is_file())
        .map(|p| p.to_string_lossy().into_owned())
}

/// Find a working ffmpeg: `configured`, or when that's the plain default
/// "ffmpeg", also the usual install locations (mpv started from the macOS
/// Dock or Finder doesn't get Homebrew's directory on its PATH). Returns its
/// path and H.264 encoders.
fn find_ffmpeg(configured: &str) -> std::io::Result<(String, Encoders)> {
    let fallbacks: &[&str] = if configured == "ffmpeg" { &["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg"] } else { &[] };
    let mut first_error = None;
    for path in std::iter::once(configured).chain(fallbacks.iter().copied()) {
        match available_encoders(path) {
            Ok(list) => return Ok((path.to_string(), list)),
            Err(e) => {
                first_error.get_or_insert(e);
            }
        }
    }
    Err(first_error.expect("tried at least one"))
}

struct Encoders {
    /// H.264 encoders from [`ENCODERS`], best first.
    video: Vec<&'static str>,
    /// libopus, for audio.
    opus: bool,
}

/// The encoders this ffmpeg has.
fn available_encoders(ffmpeg: &str) -> std::io::Result<Encoders> {
    let mut c = Command::new(ffmpeg);
    c.args(["-hide_banner", "-encoders"]).stdin(Stdio::null()).stderr(Stdio::null());
    hide_console_window(&mut c);
    let listing = String::from_utf8_lossy(&c.output()?.stdout).into_owned();
    let has = |name: &str| listing.split_whitespace().any(|word| word == name);
    Ok(Encoders { video: ENCODERS.iter().copied().filter(|e| has(e)).collect(), opus: has("libopus") })
}

/// ffmpeg is a console program; on Windows, don't flash a console window.
fn hide_console_window(c: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    let _ = c;
}
