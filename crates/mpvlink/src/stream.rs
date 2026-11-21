//! The `mpvlink://ROOM/PEER` stream protocol: how received video gets into
//! mpv, plus `mpvlink://ROOM/PEER/audio` for its audio (an extra audio track
//! of the same playback). Each room has one video and one audio queue,
//! carrying whichever stream is being watched; the peer part of the URL just
//! makes switching streams a new file for mpv.
//!
//! The WebRTC session pushes whole H.264 frames into a [`FrameQueue`]; mpv's
//! demuxer pulls bytes out through the stream callbacks below, as if reading
//! a raw .h264 file that never ends.
//!
//! Each frame has a time to be shown (see [`crate::jitter`]): mpv can't read
//! a frame before then, which evens out uneven arrival. With smoothing off,
//! that's the moment it arrives.
//!
//! Latency matters more than completeness here: the queue holds at most a few
//! frames that are due. If mpv falls behind (or isn't reading, e.g. paused), the backlog is
//! thrown away and the queue waits for the next keyframe, so playback resumes
//! at "now" instead of drifting further behind.

use crate::ffi::{MPV_ERROR_LOADING_FAILED, mpv_stream_cb_info};
use crate::{mpegts, webm};
use bytes::Bytes;
use std::collections::{HashMap, VecDeque};
use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const PROTOCOL: &str = "mpvlink";

/// About a quarter second at 30 fps. Beyond this many due frames, the reader
/// is behind.
const MAX_QUEUED_FRAMES: usize = 8;
/// A backstop for frames held back for smoothing (over a second at 60 fps).
const MAX_HELD_FRAMES: usize = 80;

/// How the video reaches mpv.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VideoFormat {
    /// Raw H.264 (Annex B): no timestamps; frames show as they arrive.
    #[default]
    Raw,
    /// Matroska with timestamps, so subtitle tracks can be timed to the
    /// stream (see [`FrameQueue::subtitle_offset`]).
    ///
    /// The timestamps follow real time (when each frame is due), not the
    /// file: they never jump (a seek, pause or speed change on the streamer's
    /// side would make mpv wait or rush), and since the first frames are
    /// stamped close together, every later frame is "late" to mpv and shows
    /// at once, as raw H.264 does. How far that timeline is from the file's
    /// is the subtitle offset.
    Matroska,
}

/// Matroska: how many frames start the stream stamped 1 ms apart, putting
/// mpv's clock that far behind (about a second) so later frames show the
/// moment they arrive.
const HEAD_START_FRAMES: u32 = 25;

/// A frame waiting to be read.
struct Queued {
    data: Bytes,
    keyframe: bool,
    /// When it may be read.
    show: Instant,
    /// Its position in the streamed file, seconds (if the sender says).
    position: Option<f64>,
}

#[derive(Default)]
struct Inner {
    frames: VecDeque<Queued>,
    /// The frame being read, as bytes for mpv, and how much is read.
    out: Vec<u8>,
    offset: usize,
    format: VideoFormat,
    /// Matroska: the header went out (it needs the first keyframe's
    /// parameter sets); the timeline so far (ms) and when the last frame
    /// was due; frames stamped so far.
    header_sent: bool,
    /// How far the timeline is from the file (stream - file, ms), as of the
    /// latest frame handed to mpv.
    offset_ms: Option<f64>,
    stream_ms: f64,
    last_show: Option<Instant>,
    stamped: u32,
    /// No more data until `reset`; readers get EOF once drained.
    ended: bool,
    /// Discard frames until a keyframe (after dropping backlog).
    need_keyframe: bool,
}

impl Inner {
    /// The bytes mpv gets for `frame`.
    fn format(&mut self, frame: Queued) -> Vec<u8> {
        if self.format == VideoFormat::Raw {
            return frame.data.to_vec();
        }
        let mut out = Vec::new();
        if !self.header_sent {
            let find = |t: u8| mpvlink_rtc::h264::nal_units(&frame.data).find(|n| n[0] & 0x1F == t).map(<[u8]>::to_vec);
            let (Some(sps), Some(pps)) = (find(7), find(8)) else { return out }; // can't start here
            out.extend(webm::video_header(&sps, &pps));
            self.header_sent = true;
        }
        // Real time since the last frame was due (1 ms during the head start).
        if let Some(last) = self.last_show {
            let gap = frame.show.saturating_duration_since(last).as_secs_f64() * 1000.0;
            self.stream_ms += if self.stamped < HEAD_START_FRAMES { 1.0 } else { gap.max(1.0) };
        }
        self.last_show = Some(frame.show);
        self.stamped += 1;
        out.extend(webm::video_cluster(self.stream_ms.round() as u64, frame.keyframe, &frame.data));
        if let Some(position) = frame.position {
            self.offset_ms = Some(self.stream_ms - position * 1000.0);
        }
        out
    }
}

#[derive(Default)]
pub struct FrameQueue {
    inner: Mutex<Inner>,
    readable: Condvar,
}

impl FrameQueue {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { inner: Mutex::new(Inner { need_keyframe: true, ..Default::default() }), ..Default::default() })
    }

    /// Queue a complete frame, to be read from `show` on; `position`: where
    /// it is in the streamed file. Returns true if the queue is waiting for a
    /// keyframe, in which case the caller should ask the sender for one.
    pub fn push(&self, frame: Bytes, keyframe: bool, show: Instant, position: Option<f64>) -> bool {
        let mut q = self.inner.lock().unwrap();
        let now = Instant::now();
        let due = q.frames.iter().take_while(|f| f.show <= now).count();
        if due >= MAX_QUEUED_FRAMES || q.frames.len() >= MAX_HELD_FRAMES {
            // (A frame partly read is in `out`, so the byte stream stays intact.)
            q.frames.clear();
            q.need_keyframe = true;
        }
        if q.need_keyframe && !keyframe {
            return true;
        }
        q.need_keyframe = false;
        q.frames.push_back(Queued { data: frame, keyframe, show, position });
        self.readable.notify_all();
        false
    }

    /// Matroska: how far the stream's timeline is from the streamed file's,
    /// in seconds (stream minus file), as of the latest frame: what mpv's
    /// `sub-delay` must be for subtitle tracks of the file to line up.
    pub fn subtitle_offset(&self) -> Option<f64> {
        let q = self.inner.lock().unwrap();
        (q.format == VideoFormat::Matroska).then_some(q.offset_ms?).map(|ms| ms / 1000.0)
    }

    /// Signal end of stream: readers get EOF after the remaining frames.
    pub fn end(&self) {
        self.inner.lock().unwrap().ended = true;
        self.readable.notify_all();
    }

    /// Start a fresh stream (e.g. a new publisher): clear everything (but
    /// keep the reader's format).
    pub fn reset(&self) {
        let mut q = self.inner.lock().unwrap();
        let format = q.format;
        *q = Inner { need_keyframe: true, format, ..Default::default() };
    }

    /// A new reader starts at "now", in `format`: drop the backlog, resume
    /// at a keyframe.
    fn attach_reader(&self, format: VideoFormat) {
        let mut q = self.inner.lock().unwrap();
        *q = Inner { need_keyframe: true, format, ..Default::default() };
    }

    /// Blocking read for mpv. Returns bytes read, 0 at EOF, -1 if cancelled.
    fn read(&self, buf: &mut [u8], cancelled: &AtomicBool) -> i64 {
        // This (mpv's) thread waits for frames' times.
        crate::jitter::timing_critical();
        let mut q = self.inner.lock().unwrap();
        loop {
            if cancelled.load(Ordering::Relaxed) {
                return -1;
            }
            // The rest of the frame being read.
            if q.offset < q.out.len() {
                let offset = q.offset;
                let n = buf.len().min(q.out.len() - offset);
                buf[..n].copy_from_slice(&q.out[offset..offset + n]);
                q.offset += n;
                return n as i64;
            }
            // The next frame, once it's due.
            let early = q.frames.front().map(|f| f.show.saturating_duration_since(Instant::now()));
            if let Some(early) = early.filter(|e| !e.is_zero()) {
                q = self.readable.wait_timeout(q, early).unwrap().0;
                continue;
            }
            if let Some(frame) = q.frames.pop_front() {
                q.out = q.format(frame);
                q.offset = 0;
                continue;
            }
            if q.ended {
                return 0;
            }
            // The timeout is only a backstop; push/end/cancel all notify.
            q = self.readable.wait_timeout(q, Duration::from_millis(250)).unwrap().0;
        }
    }

    fn wake(&self) {
        self.readable.notify_all();
    }
}

// ---- Audio ------------------------------------------------------------------

/// Audio waiting to be read beyond this is backlog (a burst after a network
/// hiccup, or mpv's clock running slower than the sender's): the oldest is
/// dropped so audio stays close to live.
const MAX_QUEUED_AUDIO_MS: u32 = 120;
const OPUS_RATE: u32 = 48_000;

/// mpv holds the picture back while it waits for audio, so audio must never
/// stop: when none has arrived for a moment (the streamer's file has no sound
/// there, or the network hiccups), the reader gets silence instead, at the
/// pace it's played.
const SILENCE_AFTER: Duration = Duration::from_millis(60);
/// A 20 ms Opus packet of silence (CELT, mono; decoders take it in a stereo stream).
const OPUS_SILENCE: [u8; 3] = [0xF8, 0xFF, 0xFE];
const SILENCE_SAMPLES: u32 = 960;

#[derive(Default)]
struct AudioInner {
    /// Opus packets not yet handed to the reader, with their sample counts
    /// and when they may be (held back as long as video is, for lip sync).
    packets: VecDeque<(Bytes, u32, Instant)>,
    /// WebM bytes being read (the header, then one cluster at a time).
    out: Vec<u8>,
    offset: usize,
    /// Samples handed out so far: the next cluster's timestamp. Counted as
    /// packets are read, so dropped packets leave no gaps.
    samples: u64,
    /// Someone is reading; packets are only kept while that's so.
    attached: bool,
    ended: bool,
    last_packet: Option<Instant>,
    /// When the reader last got audio (real or silence), and whether it was silence.
    last_output: Option<Instant>,
    filling: bool,
}

impl AudioInner {
    /// Hand `packet` to the reader as the next WebM cluster.
    fn output(&mut self, packet: &[u8], samples: u32, silence: bool) {
        let timecode_ms = self.samples * 1000 / u64::from(OPUS_RATE);
        self.out = webm::cluster(timecode_ms, packet);
        self.offset = 0;
        self.samples += u64::from(samples);
        // While filling, step the clock by exactly the silence handed out, so
        // wake-up lateness doesn't add up into fill slower than real time.
        let now = Instant::now();
        self.last_output = match self.last_output {
            Some(t) if silence && self.filling => Some((t + Duration::from_millis(20)).min(now)),
            _ => Some(now),
        };
        self.filling = silence;
    }
}

/// Opus packets in, a live WebM byte stream out (see [`webm`]).
#[derive(Default)]
pub struct AudioQueue {
    inner: Mutex<AudioInner>,
    readable: Condvar,
}

impl AudioQueue {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Queue an Opus packet, to be played from `play` on.
    pub fn push(&self, packet: Bytes, play: Instant) {
        let samples = mpegts::opus_samples(&packet).unwrap_or(960);
        let mut q = self.inner.lock().unwrap();
        let now = Instant::now();
        q.last_packet = Some(now);
        if !q.attached {
            return;
        }
        q.packets.push_back((packet, samples, play));
        // Only audio that's due counts as backlog.
        let limit = MAX_QUEUED_AUDIO_MS * (OPUS_RATE / 1000);
        let due = |q: &AudioInner| q.packets.iter().take_while(|p| p.2 <= now).map(|p| p.1).sum::<u32>();
        while due(&q) > limit {
            q.packets.pop_front();
        }
        self.readable.notify_all();
    }

    /// Whether audio has been arriving lately (the stream has sound).
    pub fn active(&self) -> bool {
        self.inner.lock().unwrap().last_packet.is_some_and(|t| t.elapsed() < Duration::from_secs(1))
    }

    /// Signal end of stream: readers get EOF after what's queued.
    pub fn end(&self) {
        self.inner.lock().unwrap().ended = true;
        self.readable.notify_all();
    }

    /// Drop what's pending (a new connection is coming, maybe from someone
    /// else). A reader that's already attached carries on, with no gap in its
    /// timestamps; `active` waits for audio on the new connection.
    pub fn reset(&self) {
        let mut q = self.inner.lock().unwrap();
        q.packets.clear();
        q.ended = false;
        q.last_packet = None;
    }

    /// A new reader starts with the header, then audio from now on.
    fn attach_reader(&self) {
        let mut q = self.inner.lock().unwrap();
        let last_packet = q.last_packet;
        *q = AudioInner {
            out: webm::header(),
            attached: true,
            last_packet,
            last_output: Some(Instant::now()),
            ..Default::default()
        };
    }

    /// Blocking read for mpv. Returns bytes read, 0 at EOF, -1 if cancelled.
    fn read(&self, buf: &mut [u8], cancelled: &AtomicBool) -> i64 {
        crate::jitter::timing_critical();
        let mut q = self.inner.lock().unwrap();
        loop {
            if cancelled.load(Ordering::Relaxed) {
                return -1;
            }
            if q.offset < q.out.len() {
                let n = buf.len().min(q.out.len() - q.offset);
                buf[..n].copy_from_slice(&q.out[q.offset..q.offset + n]);
                q.offset += n;
                return n as i64;
            }
            let now = Instant::now();
            if let Some((packet, samples, _)) = q.packets.pop_front_if(|p| p.2 <= now) {
                q.output(&packet, samples, false);
                continue;
            }
            let next = q.packets.front().map(|p| p.2.saturating_duration_since(now));
            if q.ended {
                return 0;
            }
            // Nothing to play: keep the audio clock going with silence (one
            // packet per 20 ms once it's been quiet for a moment).
            let quiet_for = q.last_output.map_or(Duration::ZERO, |t| t.elapsed());
            let due = if q.filling { Duration::from_millis(20) } else { SILENCE_AFTER };
            if quiet_for >= due {
                q.output(&OPUS_SILENCE, SILENCE_SAMPLES, true);
                continue;
            }
            let wait = (due - quiet_for).min(next.unwrap_or(Duration::MAX));
            q = self.readable.wait_timeout(q, wait).unwrap().0;
        }
    }

    fn wake(&self) {
        self.readable.notify_all();
    }
}

// ---- Room -> queue routing -------------------------------------------------

type Queues = (Arc<FrameQueue>, Arc<AudioQueue>);

fn routes() -> &'static Mutex<HashMap<String, Queues>> {
    static ROUTES: OnceLock<Mutex<HashMap<String, Queues>>> = OnceLock::new();
    ROUTES.get_or_init(Default::default)
}

/// Make `mpvlink://ROOM/...` read from these queues.
pub fn publish(room: &str, video: Arc<FrameQueue>, audio: Arc<AudioQueue>) {
    routes().lock().unwrap().insert(room.to_string(), (video, audio));
}

pub fn unpublish(room: &str) {
    routes().lock().unwrap().remove(room);
}

pub fn url(room: &str, peer: u64) -> String {
    format!("{PROTOCOL}://{room}/{peer}")
}

pub fn audio_url(room: &str, peer: u64) -> String {
    format!("{}/audio", url(room, peer))
}

/// The video as Matroska, with timestamps (see [`VideoFormat::Matroska`]).
pub fn matroska_url(room: &str, peer: u64) -> String {
    format!("{}/mkv", url(room, peer))
}

/// The subtitle offset of `room`'s stream (see [`FrameQueue::subtitle_offset`]).
pub fn subtitle_offset(room: &str) -> Option<f64> {
    routes().lock().unwrap().get(room).and_then(|(video, _)| video.subtitle_offset())
}

// ---- mpv stream callbacks ---------------------------------------------------
//
// mpv calls these from its demuxer thread (cancel from another thread). The
// protocol stays registered for the life of the mpv core, so the callbacks
// must not depend on plugin state that could go away first.

enum Source {
    Video(Arc<FrameQueue>),
    Audio(Arc<AudioQueue>),
}

struct Reader {
    source: Source,
    cancelled: AtomicBool,
}

/// `mpv_stream_cb_open_ro_fn` for [`PROTOCOL`].
pub unsafe extern "C" fn open(_user_data: *mut c_void, uri: *mut c_char, info: *mut mpv_stream_cb_info) -> c_int {
    let uri = unsafe { CStr::from_ptr(uri) }.to_string_lossy();
    let rest = uri.trim_start_matches(PROTOCOL).trim_start_matches("://");
    let mut parts = rest.split('/');
    let room = parts.next().unwrap_or_default();
    let kind = parts.nth(1);
    let audio = kind == Some("audio");
    let matroska = kind == Some("mkv");
    let Some((video_queue, audio_queue)) = routes().lock().unwrap().get(room).cloned() else {
        return MPV_ERROR_LOADING_FAILED;
    };
    let source = if audio {
        audio_queue.attach_reader();
        Source::Audio(audio_queue)
    } else {
        let format = if matroska { VideoFormat::Matroska } else { VideoFormat::Raw };
        video_queue.attach_reader(format);
        Source::Video(video_queue)
    };
    let reader = Box::new(Reader { source, cancelled: AtomicBool::new(false) });
    let info = unsafe { &mut *info };
    info.cookie = Box::into_raw(reader).cast();
    info.read_fn = Some(read);
    info.close_fn = Some(close);
    info.cancel_fn = Some(cancel);
    // No seek/size: it's a live stream.
    0
}

unsafe extern "C" fn read(cookie: *mut c_void, buf: *mut c_char, nbytes: u64) -> i64 {
    let reader = unsafe { &*(cookie as *const Reader) };
    let buf = unsafe { std::slice::from_raw_parts_mut(buf.cast::<u8>(), nbytes as usize) };
    match &reader.source {
        Source::Video(q) => q.read(buf, &reader.cancelled),
        Source::Audio(q) => q.read(buf, &reader.cancelled),
    }
}

unsafe extern "C" fn close(cookie: *mut c_void) {
    drop(unsafe { Box::from_raw(cookie as *mut Reader) });
}

unsafe extern "C" fn cancel(cookie: *mut c_void) {
    let reader = unsafe { &*(cookie as *const Reader) };
    reader.cancelled.store(true, Ordering::Relaxed);
    match &reader.source {
        Source::Video(q) => q.wake(),
        Source::Audio(q) => q.wake(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_all(q: &FrameQueue) -> Vec<u8> {
        let never = AtomicBool::new(false);
        let mut out = Vec::new();
        let mut buf = [0u8; 3];
        q.end();
        loop {
            match q.read(&mut buf, &never) {
                0 => return out,
                n => out.extend_from_slice(&buf[..n as usize]),
            }
        }
    }

    #[test]
    fn starts_at_a_keyframe_and_reads_in_order() {
        let q = FrameQueue::new();
        assert!(q.push(Bytes::from_static(b"p0"), false, Instant::now(), None), "needs a keyframe first");
        assert!(!q.push(Bytes::from_static(b"KEY1"), true, Instant::now(), None));
        assert!(!q.push(Bytes::from_static(b"p2"), false, Instant::now(), None));
        assert_eq!(read_all(&q), b"KEY1p2");
    }

    #[test]
    fn overflow_drops_backlog_and_waits_for_keyframe() {
        let q = FrameQueue::new();
        q.push(Bytes::from_static(b"K"), true, Instant::now(), None);
        for _ in 1..MAX_QUEUED_FRAMES {
            q.push(Bytes::from_static(b"p"), false, Instant::now(), None);
        }
        assert!(q.push(Bytes::from_static(b"x"), false, Instant::now(), None), "overflow -> keyframe wanted");
        assert!(q.push(Bytes::from_static(b"y"), false, Instant::now(), None));
        assert!(!q.push(Bytes::from_static(b"K2"), true, Instant::now(), None));
        assert_eq!(read_all(&q), b"K2");
    }

    #[test]
    fn overflow_keeps_partially_read_frame() {
        let q = FrameQueue::new();
        q.push(Bytes::from_static(b"KEYFRAME"), true, Instant::now(), None);
        let never = AtomicBool::new(false);
        let mut buf = [0u8; 3];
        assert_eq!(q.read(&mut buf, &never), 3);
        for _ in 0..MAX_QUEUED_FRAMES {
            q.push(Bytes::from_static(b"p"), false, Instant::now(), None);
        }
        q.push(Bytes::from_static(b"K2"), true, Instant::now(), None);
        assert_eq!(read_all(&q), b"FRAMEK2");
    }

    #[test]
    fn frames_wait_until_due_and_held_ones_dont_overflow() {
        let q = FrameQueue::new();
        let start = Instant::now();
        let later = start + Duration::from_millis(60);
        q.push(Bytes::from_static(b"K"), true, later, None);
        // Held frames don't count as backlog.
        for _ in 0..MAX_QUEUED_FRAMES {
            assert!(!q.push(Bytes::from_static(b"p"), false, later, None));
        }
        let never = AtomicBool::new(false);
        assert_eq!(q.read(&mut [0u8; 4], &never), 1);
        assert!(start.elapsed() >= Duration::from_millis(60), "read waited for the frame's time");
    }

    #[test]
    fn cancel_unblocks_reader() {
        let q = FrameQueue::new();
        let reader = Arc::new(Reader { source: Source::Video(q.clone()), cancelled: AtomicBool::new(false) });
        let r = reader.clone();
        let t = std::thread::spawn(move || q.read(&mut [0u8; 4], &r.cancelled));
        std::thread::sleep(Duration::from_millis(50));
        unsafe { cancel(Arc::as_ptr(&reader) as *mut c_void) };
        assert_eq!(t.join().unwrap(), -1);
    }

    fn read_audio(q: &AudioQueue) -> Vec<u8> {
        let never = AtomicBool::new(false);
        let mut out = Vec::new();
        let mut buf = [0u8; 7];
        q.end();
        loop {
            match q.read(&mut buf, &never) {
                0 => return out,
                n => out.extend_from_slice(&buf[..n as usize]),
            }
        }
    }

    #[test]
    fn audio_starts_with_header_and_has_continuous_timestamps() {
        let q = AudioQueue::new();
        q.push(Bytes::from_static(&[0xFC, 0]), Instant::now());
        assert!(q.active());
        q.attach_reader(); // packets before a reader are dropped
        for i in 0..3u8 {
            q.push(Bytes::from(vec![0xFC, i]), Instant::now()); // 20 ms each
        }
        let bytes = read_audio(&q);
        let mut expected = webm::header();
        for (i, ms) in [0u64, 20, 40].into_iter().enumerate() {
            expected.extend(webm::cluster(ms, &[0xFC, i as u8]));
        }
        assert_eq!(bytes, expected);
    }

    #[test]
    fn audio_backlog_is_trimmed_without_timestamp_gaps() {
        let q = AudioQueue::new();
        q.attach_reader();
        for i in 0..20u8 {
            q.push(Bytes::from(vec![0xFC, i]), Instant::now()); // 400 ms in all
        }
        let bytes = read_audio(&q);
        let mut expected = webm::header();
        // Only the newest 120 ms (6 packets) remain, numbered from 0 ms.
        for (n, i) in (14..20u8).enumerate() {
            expected.extend(webm::cluster(n as u64 * 20, &[0xFC, i]));
        }
        assert_eq!(bytes, expected);
    }

    #[test]
    fn audio_waits_until_its_time() {
        let q = AudioQueue::new();
        q.attach_reader();
        let never = AtomicBool::new(false);
        let mut buf = [0u8; 4096];
        q.read(&mut buf, &never); // the header
        let start = Instant::now();
        // Held back 40 ms (less than the wait before silence fills in).
        q.push(Bytes::from_static(&[0xFC, 9]), start + Duration::from_millis(40));
        let n = q.read(&mut buf, &never);
        assert!(start.elapsed() >= Duration::from_millis(40), "waited for its time");
        assert!(buf[..n as usize].ends_with(&[0xFC, 9]), "the packet, not silence");
    }

    #[test]
    fn audio_fills_gaps_with_silence() {
        let q = AudioQueue::new();
        q.attach_reader();
        let never = AtomicBool::new(false);
        let mut buf = vec![0u8; 4096];
        let header = webm::header();
        assert_eq!(q.read(&mut buf, &never) as usize, header.len());
        // Nothing arrives: after a moment the reader gets silence, not a hang.
        let started = Instant::now();
        let n = q.read(&mut buf, &never) as usize;
        assert!(started.elapsed() >= SILENCE_AFTER);
        assert_eq!(&buf[..n], webm::cluster(0, &OPUS_SILENCE).as_slice());
        // Real audio afterwards continues the timeline.
        q.push(Bytes::from_static(&[0xFC, 7]), Instant::now());
        let n = q.read(&mut buf, &never) as usize;
        assert_eq!(&buf[..n], webm::cluster(20, &[0xFC, 7]).as_slice());
    }
}
