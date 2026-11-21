//! Streaming a file: ffmpeg encodes it to H.264, and frames are sent as mpv's
//! own playback reaches them.
//!
//! mpv is the only clock. ffmpeg runs as fast as it can (its output pipe
//! fills up and makes it wait), each frame carries its position in the file,
//! and a frame goes out when mpv's `time-pos` gets there. So pausing, speed
//! changes and encoder delay are all handled by the same rule. Seeks restart
//! ffmpeg at the new position.

use crate::bitrate::{self, Bitrate};
use crate::jitter::{SendMeter, SmoothClock};
use crate::mpegts::{Kind, TsDemuxer, TsFrame, opus_samples};
use crate::substyle::{AssHeader, SubStyle};
use mpvlink_rtc::h264::is_keyframe;
use mpvlink_rtc::publish::Frame;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
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
/// A frame this close to its time goes now, rather than after another
/// sleep (which may overshoot by more).
const DUE_EARLY: f64 = 0.001;
/// How often to read mpv's playback position.
const CLOCK_POLL: Duration = Duration::from_millis(4);
/// Audio goes out this much ahead of its place in playback: about what the
/// viewer buffers before playing it (video is shown as soon as it arrives),
/// so the two come out together.
const AUDIO_LEAD: f64 = 0.1;
/// Audio this late is dropped: sending it would only add delay on the other side.
const AUDIO_MAX_LATE: f64 = 0.25;

/// Where image subtitle lines usually are (fraction of the height from the
/// top): what `sub-scale` scales them about.
const IMAGE_SUB_ANCHOR: f64 = 0.9;

/// Image subtitles are read from this many seconds before the position, to
/// catch any already on screen there.
const SUBTITLE_LOOKBACK: f64 = 10.0;

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

/// A subtitle track to draw into the video.
#[derive(Clone, Debug, PartialEq)]
pub enum BurnIn {
    /// Bitmaps (Blu-ray PGS, DVD), laid over the picture.
    Image {
        /// The file they're in, if not the one being streamed.
        file: Option<String>,
        /// ffmpeg's stream index in that file.
        stream: i64,
        /// mpv's `sub-delay`, in seconds.
        delay: f64,
        style: SubStyle,
    },
    /// Text (ASS, SRT, WebVTT…), rendered with libass like mpv does,
    /// including fonts attached to the file.
    Text {
        /// The file they're in, if not the one being streamed.
        file: Option<String>,
        /// Which of that file's subtitle streams (0 = the first).
        index: usize,
        /// Seconds to add to a playback position to get subtitle time: the
        /// file's start time, minus mpv's `sub-delay`.
        shift: f64,
        /// ASS (styled by the file) rather than plain text (styled by mpv).
        ass: bool,
        /// The video's width / height, for plain text's canvas.
        aspect: f64,
        style: SubStyle,
    },
}

pub struct StreamSource {
    /// Absolute file path, or an http(s) URL.
    pub path: String,
    pub title: String,
    /// The file's video stream to send (ffmpeg's index), else the first.
    pub video_stream: Option<i64>,
    pub audio: AudioChoice,
    /// Subtitles to draw into the video, as mpv shows them.
    pub subtitles: Option<BurnIn>,
    /// Viewers get the subtitle tracks as files (see `subexport`), so
    /// nothing is drawn in and no subtitle text is sent.
    pub tracks_for_viewers: bool,
    /// The ffmpeg executable.
    pub ffmpeg: String,
    pub bitrate: Bitrate,
    pub clock: Clock,
    /// mpv's playback speed.
    pub speed: Clock,
    /// Bumped by the plugin each time mpv finishes a seek.
    pub seeks: Arc<AtomicU64>,
}

#[derive(Debug)]
pub enum PipelineEvent {
    /// Encoding works; `encoder` is the one in use.
    Started { encoder: String, kbps: u32 },
    /// This ffmpeg can't draw text subtitles (no libass): they're left out
    /// of the video, so viewers need them as text instead.
    TextSubtitlesUnsupported,
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
            .spawn(move || Runner { source, frames, events, stop: s, child: c, extras: Extras::default() }.run())
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
    /// Worked out once per stream: the bitrate and what drawing subtitles needs.
    extras: Extras,
}

/// Prepared once per stream, before ffmpeg first runs.
#[derive(Default)]
struct Extras {
    /// The streamed file's attached fonts, for subtitles from another file.
    fonts: Option<FontsDir>,
    /// libass style overrides: mpv's subtitle style options.
    force_style: String,
    /// The video bitrate, kb/s.
    kbps: u32,
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
                if !found.libass && matches!(self.source.subtitles, Some(BurnIn::Text { .. })) {
                    self.source.subtitles = None;
                    eprintln!("[mpvlink] this ffmpeg has no libass: sending subtitles as text");
                    let _ = self.events.send(PipelineEvent::TextSubtitlesUnsupported);
                }
                // mpv draws a separate subtitle file with the fonts attached
                // to the video (common with anime releases); so must we.
                if let Some(BurnIn::Text { file, index, ass, aspect, style, .. }) = &self.source.subtitles {
                    if file.is_some() && !self.source.path.contains("://") {
                        self.extras.fonts = FontsDir::extract(&self.source.ffmpeg, &self.source.path);
                    }
                    self.extras.force_style = if *ass {
                        let file = file.as_deref().unwrap_or(&self.source.path);
                        let header = ass_header(&self.source.ffmpeg, file, *index).unwrap_or_default();
                        style.ass(&AssHeader::parse(&header))
                    } else {
                        style.plain_text(*aspect)
                    };
                }
                self.extras.kbps = match self.source.bitrate {
                    Bitrate::Kbps(k) => k,
                    b => b.kbps(probe_source(&self.source.ffmpeg, &self.source.path, self.source.video_stream)),
                };
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
        let mut command = ffmpeg_command(&self.source, start, encoder, &self.extras);
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

    /// Send frames as playback reaches them. A thread of its own reads
    /// mpv's position into a [`SmoothClock`]: reading it waits on mpv, which
    /// may be busy, and frames shouldn't go out late because of that.
    fn pace(&self, start: f64, rx: &Receiver<TsFrame>, encoder: &str, announced: &mut bool) -> Ended {
        crate::jitter::timing_critical();
        let clock = Mutex::new(SmoothClock::default());
        let done = AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                while !done.load(Ordering::Relaxed) {
                    let position = (self.source.clock)();
                    let speed = (self.source.speed)().filter(|s| *s > 0.0).unwrap_or(1.0);
                    clock.lock().unwrap().update(Instant::now(), position, speed);
                    std::thread::sleep(CLOCK_POLL);
                }
            });
            let ended = self.pace_frames(start, rx, encoder, announced, &clock);
            done.store(true, Ordering::Relaxed);
            ended
        })
    }

    fn pace_frames(
        &self,
        start: f64,
        rx: &Receiver<TsFrame>,
        encoder: &str,
        announced: &mut bool,
        clock: &Mutex<SmoothClock>,
    ) -> Ended {
        let mut seeks = self.source.seeks.load(Ordering::SeqCst);
        // Timestamp of the first frame, which is at `start` in the file.
        let mut base: Option<i64> = None;
        let mut previous_video: Option<i64> = None;
        let mut position = start; // file position of the last frame sent
        let mut finished = false;
        // Behind schedule: dropping video until a keyframe to resume from.
        let mut skipping = false;
        let mut meter = SendMeter::default();
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
            let (now, speed) = {
                let mut clock = clock.lock().unwrap();
                (clock.predict(Instant::now()), clock.speed())
            };
            let Some(now) = now else {
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
            let due = frame.at <= now + DUE_EARLY || (!frame.audio && skipping);
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
                // The RTP timestamp advances by this after the frame, so
                // it's the time until the next frame where that's known.
                // Timestamps advance in real time (viewers time frames by
                // them), so at 2x speed a frame lasts half as long.
                let duration = match queues[kind].front() {
                    Some(next) if next.file_time > frame.file_time && next.file_time - frame.file_time < 1.0 => {
                        Duration::from_secs_f64(next.file_time - frame.file_time)
                    }
                    _ => frame.duration,
                };
                let duration = duration.div_f64(speed);
                meter.sent(duration);
                crate::telemetry::frame_sent(frame.data.len(), meter.last_spread_ms());
                // Tagged with its place in the file: viewers can play on the
                // file's timeline (see stream::VideoFormat).
                let _ = self.frames.send(Frame::video_at(&frame.data, frame.keyframe, duration, frame.file_time));
                if let Some(report) = meter.report() {
                    eprintln!("[mpvlink] sending: {report}");
                }
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
            let _ = self.events.send(PipelineEvent::Started { encoder: encoder.to_string(), kbps: self.extras.kbps });
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

fn ffmpeg_command(source: &StreamSource, start: f64, encoder: &str, extras: &Extras) -> Command {
    let mut c = Command::new(&source.ffmpeg);
    c.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
    if start > 0.0 {
        // Before -i: a fast seek that still starts exactly at `start`.
        c.args(["-ss", &format!("{start:.3}")]);
    }
    let video = source.video_stream.map_or_else(|| "0:v:0".to_string(), |i| format!("0:{i}"));
    c.args(["-i", &source.path]);
    // At most 1080p, 8-bit 4:2:0 (what every H.264 decoder handles).
    let scale = "scale=w='min(iw,1920)':h='min(ih,1080)':force_original_aspect_ratio=decrease:force_divisible_by=2,format=yuv420p";
    // Subtitles are drawn at the source resolution, then scaled with the picture.
    let graph = match &source.subtitles {
        None => None,
        Some(BurnIn::Image { file, stream, delay, style }) => {
            // The subtitles get their own input: a seek skips subtitles that
            // started before it but are still on screen, so it's read from a
            // little earlier, keeping everything read (-noaccurate_seek).
            // Its timestamps count from where it was sought to, and an
            // offset lines that up with the video, `delay` later. (A web
            // stream only gets that with a delay: it means reading the file
            // twice.)
            let input = if file.is_some() || *delay != 0.0 || !source.path.contains("://") {
                let from = start - delay;
                let seek = (from - SUBTITLE_LOOKBACK).max(0.0);
                if seek > 0.0 {
                    c.args(["-noaccurate_seek", "-ss", &format!("{seek:.3}")]);
                }
                let offset = seek - from;
                if offset.abs() >= 0.0005 {
                    c.args(["-itsoffset", &format!("{offset:.3}")]);
                }
                c.args(["-i", file.as_deref().unwrap_or(&source.path)]);
                1
            } else {
                0
            };
            // Scaled and raised. mpv scales each bitmap about its own center;
            // here the whole subtitle layer is scaled about where subtitle
            // lines usually are, which puts those in the same place.
            let (size, raise) = style.image();
            let mut subs = Vec::new();
            if size != 1.0 {
                subs.push(format!("scale=w=iw*{size}:h=ih*{size}"));
            }
            if style.gray {
                subs.push("colorchannelmixer=.3:.59:.11:0:.3:.59:.11:0:.3:.59:.11".to_string());
            }
            let subs = if subs.is_empty() { "null".to_string() } else { subs.join(",") };
            let mut y = if size != 1.0 { format!("{IMAGE_SUB_ANCHOR}*(H-h)") } else { "0".to_string() };
            if raise != 0.0 {
                y += &format!("-{raise:.4}*H");
            }
            // "pass" keeps the video going after the last subtitle.
            Some(format!(
                "[{input}:{stream}]{subs}[s];[{video}][s]overlay=x=(W-w)/2:y={y}:eof_action=pass,{scale}[v]"
            ))
        }
        Some(BurnIn::Text { file, index, shift, .. }) => {
            // libass reads the subtitles (and attached fonts) from the file
            // itself and draws by timestamp, but after -ss ours start at 0:
            // shift them to subtitle time and back.
            let file = filter_value(file.as_deref().unwrap_or(&source.path));
            let offset = start + shift;
            let to_subs = format!("setpts=PTS{offset:+.3}/TB");
            let back = format!("setpts=PTS{:+.3}/TB", -offset);
            let mut options = format!("filename={file}:si={index}");
            if let Some(fonts) = &extras.fonts {
                options += &format!(":fontsdir={}", filter_value(&fonts.0.to_string_lossy()));
            }
            if !extras.force_style.is_empty() {
                options += &format!(":force_style={}", filter_value(&extras.force_style));
            }
            Some(format!("[{video}]{to_subs},subtitles={options},{back},{scale}[v]"))
        }
    };
    match graph {
        Some(graph) => c.args(["-filter_complex", &graph, "-map", "[v]"]),
        None => c.args(["-map", &video, "-vf", scale]),
    };
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
    let kbps = extras.kbps;
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
    /// The `subtitles` filter (libass), for text subtitles.
    libass: bool,
}

/// The encoders this ffmpeg has.
fn available_encoders(ffmpeg: &str) -> std::io::Result<Encoders> {
    let list = |what: &str| -> std::io::Result<String> {
        let mut c = Command::new(ffmpeg);
        c.args(["-hide_banner", what]).stdin(Stdio::null()).stderr(Stdio::null());
        hide_console_window(&mut c);
        Ok(String::from_utf8_lossy(&c.output()?.stdout).into_owned())
    };
    let (encoders, filters) = (list("-encoders")?, list("-filters")?);
    let has = |listing: &str, name: &str| listing.split_whitespace().any(|word| word == name);
    Ok(Encoders {
        video: ENCODERS.iter().copied().filter(|e| has(&encoders, e)).collect(),
        opus: has(&encoders, "libopus"),
        libass: has(&filters, "subtitles"),
    })
}

/// The header of an ASS subtitle stream (styles, canvas size): the
/// `index`th subtitle stream of `file`. Only the header is read.
/// The bitrate and size of the video in `file`, from what `ffmpeg -i` says.
fn probe_source(ffmpeg: &str, file: &str, stream: Option<i64>) -> Option<bitrate::SourceVideo> {
    let mut c = Command::new(ffmpeg);
    c.args(["-hide_banner", "-nostdin", "-i", file]).stdin(Stdio::null()).stdout(Stdio::null());
    hide_console_window(&mut c);
    let out = c.output().ok()?;
    let found = bitrate::probe_source(&String::from_utf8_lossy(&out.stderr), stream);
    if found.is_none() {
        eprintln!("[mpvlink] couldn't tell the source's bitrate; streaming at the default");
    }
    found
}

fn ass_header(ffmpeg: &str, file: &str, index: usize) -> Option<String> {
    let mut c = Command::new(ffmpeg);
    c.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-i", file])
        .args(["-map", &format!("0:s:{index}"), "-c:s", "copy", "-frames:s", "0", "-f", "ass", "-"])
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    hide_console_window(&mut c);
    let out = c.output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned()).filter(|h| !h.is_empty())
}

/// A temporary folder of fonts, deleted when dropped.
struct FontsDir(PathBuf);

impl FontsDir {
    /// Copy the fonts attached to `video` (MKV attachments) into a new
    /// temporary folder; None if it has none.
    fn extract(ffmpeg: &str, video: &str) -> Option<Self> {
        static COUNT: AtomicU64 = AtomicU64::new(0);
        let n = COUNT.fetch_add(1, Ordering::SeqCst);
        let dir = Self(std::env::temp_dir().join(format!("mpvlink-fonts-{}-{n}", std::process::id())));
        std::fs::create_dir_all(&dir.0).ok()?;
        // Attachments are written (under their own names, into the current
        // folder) when the input opens; nothing else is decoded.
        let mut c = Command::new(ffmpeg);
        c.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-dump_attachment:t", "", "-i", video])
            .args(["-t", "0", "-f", "null", "-"])
            .current_dir(&dir.0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        hide_console_window(&mut c);
        let _ = c.status();
        let found = std::fs::read_dir(&dir.0).ok()?.count();
        (found > 0).then(|| {
            eprintln!("[mpvlink] using {found} fonts attached to the video for subtitles");
            dir
        })
    }
}

/// The fonts attached to `video` (MKV attachments): (file name, bytes).
pub fn attached_fonts(ffmpeg: &str, video: &str) -> Vec<(String, Vec<u8>)> {
    let Some(dir) = FontsDir::extract(ffmpeg, video) else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(&dir.0) else { return Vec::new() };
    entries
        .flatten()
        .filter_map(|e| Some((e.file_name().to_string_lossy().into_owned(), std::fs::read(e.path()).ok()?)))
        .collect()
}

impl Drop for FontsDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `value` as a filter option in a -filter_complex graph: escaped for the
/// option parser (`:` separates options), then for the graph parser
/// (`[` `]` are labels, `,` `;` separate filters). File paths need this.
fn filter_value(value: &str) -> String {
    let escape = |s: &str, special: &str| {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            if special.contains(c) {
                out.push('\\');
            }
            out.push(c);
        }
        out
    };
    escape(&escape(value, "\\':"), "\\'[],;")
}

/// ffmpeg is a console program; on Windows, don't flash a console window.
pub fn hide_console_window(c: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    let _ = c;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(subtitles: Option<BurnIn>) -> StreamSource {
        StreamSource {
            path: "/videos/Show [BD][1080p]/ep 1, part 2.mkv".into(),
            title: "ep".into(),
            video_stream: None,
            audio: AudioChoice::Off,
            subtitles,
            tracks_for_viewers: false,
            ffmpeg: "ffmpeg".into(),
            bitrate: Bitrate::Kbps(1000),
            clock: Arc::new(|| None),
            speed: Arc::new(|| None),
            seeks: Arc::default(),
        }
    }

    fn args(source: &StreamSource, start: f64) -> Vec<String> {
        ffmpeg_command(source, start, "libx264", &Extras { kbps: 1000, ..Extras::default() }).get_args().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    fn graph(args: &[String]) -> &str {
        let i = args.iter().position(|a| a == "-filter_complex").expect("a filter graph");
        &args[i + 1]
    }

    #[test]
    fn filter_values_are_escaped_for_both_parsers() {
        assert_eq!(filter_value("/a/b.mkv"), "/a/b.mkv");
        assert_eq!(filter_value("/x [BD]/a, b;c.mkv"), r"/x \[BD\]/a\, b\;c.mkv");
        // Option level first (\: \\ \'), then graph level (\\ \').
        assert_eq!(filter_value(r"C:\Videos\it's.mkv"), r"C\\:\\\\Videos\\\\it\\\'s.mkv");
    }

    #[test]
    fn text_subtitles_are_drawn_at_subtitle_time() {
        let text = BurnIn::Text { file: None, index: 1, shift: -0.5, ass: true, aspect: 1.0, style: SubStyle::default() };
        let args = args(&source(Some(text)), 600.0);
        assert!(graph(&args).starts_with(
            r"[0:v:0]setpts=PTS+599.500/TB,subtitles=filename=/videos/Show \[BD\]\[1080p\]/ep 1\, part 2.mkv:si=1,setpts=PTS-599.500/TB,scale="
        ));
        assert_eq!(args.iter().filter(|a| *a == "-i").count(), 1);
    }

    #[test]
    fn separate_image_subtitles_are_a_second_input() {
        let image = BurnIn::Image { file: Some("/videos/ep.sup".into()), stream: 0, delay: 0.0, style: SubStyle::default() };
        let args = args(&source(Some(image)), 12.0);
        let inputs: Vec<_> = args.windows(2).filter(|w| w[0] == "-i").map(|w| w[1].as_str()).collect();
        assert_eq!(inputs, [source(None).path.as_str(), "/videos/ep.sup"]);
        assert!(graph(&args).starts_with("[1:0]null[s];[0:v:0][s]overlay=x=(W-w)/2:y=0:eof_action=pass,scale="));
        assert_eq!(args.iter().filter(|a| *a == "-ss").count(), 2);
    }

    #[test]
    fn image_subtitles_take_delay_scale_and_position() {
        let style = SubStyle { scale: 1.5, pos: 90.0, ..SubStyle::default() };
        let image = |delay| BurnIn::Image { file: None, stream: 3, delay, style: style.clone() };
        // Delayed 2 s: the file again, as a second input, read from 10 s
        // before 2 s earlier, and offset to match.
        let delayed = args(&source(Some(image(2.0))), 30.0);
        let flag = |args: &[String], name: &str| -> Vec<String> {
            args.windows(2).filter(|w| w[0] == name).map(|w| w[1].clone()).collect()
        };
        assert_eq!(flag(&delayed, "-ss"), ["30.000", "18.000"]);
        assert_eq!(flag(&delayed, "-itsoffset"), ["-10.000"]);
        assert!(graph(&delayed).starts_with(
            "[1:3]scale=w=iw*1.5:h=ih*1.5[s];[0:v:0][s]overlay=x=(W-w)/2:y=0.9*(H-h)-0.1000*H:eof_action=pass,scale="
        ));
        // Near the start: from the start, offset by what's left.
        let early = args(&source(Some(image(4.0))), 1.0);
        assert_eq!(flag(&early, "-ss"), ["1.000"]);
        assert_eq!(flag(&early, "-itsoffset"), ["3.000"]);
        // A web stream without a delay: from the same input.
        let web = StreamSource { path: "https://example.com/a.mkv".into(), ..source(Some(image(0.0))) };
        assert!(graph(&args(&web, 10.0)).starts_with("[0:3]scale="));
    }

    #[test]
    fn text_style_overrides_are_escaped() {
        let text = BurnIn::Text { file: None, index: 0, shift: 0.0, ass: false, aspect: 1.0, style: SubStyle::default() };
        let extras = Extras { fonts: None, force_style: "FontName=Foo: Bar,MarginV=3".into(), kbps: 1000 };
        let args: Vec<String> = ffmpeg_command(&source(Some(text)), 0.0, "libx264", &extras)
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(graph(&args).contains(r":si=0:force_style=FontName=Foo\\: Bar\,MarginV=3,setpts="));
    }

    #[test]
    fn no_subtitles_no_graph() {
        let args = args(&source(None), 0.0);
        assert!(!args.iter().any(|a| a == "-filter_complex"));
    }
}
