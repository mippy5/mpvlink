//! Numbers for the statistics panel, recorded where they happen (receiving
//! frames, sending them, polling the connections) and read by the plugin
//! when it draws the panel. One set per process, like the plugin itself.

use mpvlink_rtc::stats::LinkStats;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Frames shown in the timing graph: about five seconds at 24 fps.
pub const FRAME_HISTORY: usize = 120;
/// Seconds shown in the bitrate graph.
pub const RATE_HISTORY: usize = 60;

static TELEMETRY: Mutex<Telemetry> = Mutex::new(Telemetry { watching: None, streaming: None });

/// A copy of everything, for drawing.
pub fn snapshot() -> Telemetry {
    TELEMETRY.lock().unwrap().clone()
}

fn with<R>(f: impl FnOnce(&mut Telemetry) -> R) -> R {
    f(&mut TELEMETRY.lock().unwrap())
}

#[derive(Clone, Debug, Default)]
pub struct Telemetry {
    pub watching: Option<Watching>,
    pub streaming: Option<Streaming>,
}

/// How one received frame was timed (see [`crate::jitter`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameMark {
    /// How much later than the fastest recent frame it arrived, ms.
    pub spread_ms: f64,
    /// The smoothing delay it was held to, ms (None with smoothing off).
    pub delay_ms: Option<f64>,
    /// It arrived after its time, so it was shown late (a hitch).
    pub late: bool,
}

/// Watching someone's stream.
#[derive(Clone, Debug)]
pub struct Watching {
    pub name: String,
    pub frames: VecDeque<FrameMark>,
    arrivals: VecDeque<Instant>,
    /// Incoming kb/s, one per second.
    pub kbps: VecDeque<f64>,
    pub link: Option<LinkStats>,
    /// Media bytes received since the last rate figure, and when that was.
    bytes: u64,
    since: Instant,
    /// Since this stream started: packet gaps seen, packets that came out of
    /// order, and frames shown late.
    pub losses: u32,
    pub out_of_order: u32,
    pub late: u32,
}

impl Watching {
    /// Frames per second, over the last second.
    pub fn fps(&self) -> f64 {
        rate(&self.arrivals)
    }
}

/// Our own stream.
#[derive(Clone, Debug)]
pub struct Streaming {
    pub encoder: String,
    pub target_kbps: u32,
    /// How unevenly each frame went out, ms (see [`crate::jitter::SendMeter`]).
    pub sends: VecDeque<f64>,
    sent_at: VecDeque<Instant>,
    bytes_this_second: u64,
    /// Encoded video kb/s, and kb/s sent to all viewers together, one per second.
    pub encoded_kbps: VecDeque<f64>,
    pub upload_kbps: VecDeque<f64>,
    pub viewers: Vec<ViewerLink>,
}

impl Streaming {
    pub fn fps(&self) -> f64 {
        rate(&self.sent_at)
    }
}

#[derive(Clone, Debug)]
pub struct ViewerLink {
    pub name: String,
    pub link: LinkStats,
}

/// Events per second over the last second.
fn rate(times: &VecDeque<Instant>) -> f64 {
    let now = Instant::now();
    times.iter().filter(|t| now.duration_since(**t) <= Duration::from_secs(1)).count() as f64
}

fn push_capped<T>(q: &mut VecDeque<T>, value: T, cap: usize) {
    q.push_back(value);
    while q.len() > cap {
        q.pop_front();
    }
}

fn mark_time(q: &mut VecDeque<Instant>) {
    let now = Instant::now();
    q.push_back(now);
    while q.front().is_some_and(|t| now.duration_since(*t) > Duration::from_secs(2)) {
        q.pop_front();
    }
}

// ---- Watching -----------------------------------------------------------------

/// Started watching `name` (a new connection).
pub fn watch_started(name: &str) {
    with(|t| {
        t.watching = Some(Watching {
            name: name.to_string(),
            frames: VecDeque::new(),
            arrivals: VecDeque::new(),
            kbps: VecDeque::new(),
            link: None,
            bytes: 0,
            since: Instant::now(),
            losses: 0,
            out_of_order: 0,
            late: 0,
        });
    });
}

pub fn watch_stopped() {
    with(|t| t.watching = None);
}

/// Received media (a video frame or an audio packet) of `bytes`.
pub fn media_received(bytes: usize) {
    with(|t| {
        if let Some(w) = t.watching.as_mut() {
            w.bytes += bytes as u64;
        }
    });
}

pub fn frame_received(mark: FrameMark) {
    with(|t| {
        let Some(w) = t.watching.as_mut() else { return };
        push_capped(&mut w.frames, mark, FRAME_HISTORY);
        mark_time(&mut w.arrivals);
        w.late += u32::from(mark.late);
    });
}

pub fn packets(losses: u32, out_of_order: u32) {
    with(|t| {
        if let Some(w) = t.watching.as_mut() {
            w.losses += losses;
            w.out_of_order += out_of_order;
        }
    });
}

/// The connection's stats, read about once a second; also closes the
/// incoming rate figure (counted here from what arrives: the connection's
/// own byte counters come up short when receiving).
pub fn watch_link(link: LinkStats) {
    with(|t| {
        let Some(w) = t.watching.as_mut() else { return };
        let now = Instant::now();
        let secs = now.duration_since(w.since).as_secs_f64().max(0.001);
        push_capped(&mut w.kbps, w.bytes as f64 * 8.0 / 1000.0 / secs, RATE_HISTORY);
        (w.bytes, w.since) = (0, now);
        w.link = Some(link);
    });
}

// ---- Streaming ----------------------------------------------------------------

/// ffmpeg started encoding our stream.
pub fn stream_started(encoder: &str, target_kbps: u32) {
    with(|t| match t.streaming.as_mut() {
        // A restart (seek, track change): keep the history.
        Some(s) => {
            s.encoder = encoder.to_string();
            s.target_kbps = target_kbps;
        }
        None => {
            t.streaming = Some(Streaming {
                encoder: encoder.to_string(),
                target_kbps,
                sends: VecDeque::new(),
                sent_at: VecDeque::new(),
                bytes_this_second: 0,
                encoded_kbps: VecDeque::new(),
                upload_kbps: VecDeque::new(),
                viewers: Vec::new(),
            })
        }
    });
}

pub fn stream_stopped() {
    with(|t| t.streaming = None);
}

/// A video frame of `bytes` went out, `spread_ms` off an even pace.
pub fn frame_sent(bytes: usize, spread_ms: f64) {
    with(|t| {
        let Some(s) = t.streaming.as_mut() else { return };
        push_capped(&mut s.sends, spread_ms, FRAME_HISTORY);
        mark_time(&mut s.sent_at);
        s.bytes_this_second += bytes as u64;
    });
}

/// Once a second while streaming: the viewers' connections (id, name,
/// stats, kb/s sent to them); also closes the second's bitrate figures.
pub fn stream_tick(viewers: Vec<(u64, String, LinkStats, f64)>, elapsed: Duration) {
    with(|t| {
        let Some(s) = t.streaming.as_mut() else { return };
        let secs = elapsed.as_secs_f64().max(0.001);
        let encoded = s.bytes_this_second as f64 * 8.0 / 1000.0 / secs;
        s.bytes_this_second = 0;
        push_capped(&mut s.encoded_kbps, encoded, RATE_HISTORY);
        let upload = viewers.iter().map(|v| v.3).sum();
        push_capped(&mut s.upload_kbps, upload, RATE_HISTORY);
        s.viewers = viewers.into_iter().map(|(_, name, link, _)| ViewerLink { name, link }).collect();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_and_histories() {
        let mut q = VecDeque::new();
        for i in 0..200 {
            push_capped(&mut q, i, FRAME_HISTORY);
        }
        assert_eq!(q.len(), FRAME_HISTORY);
        assert_eq!(q.front(), Some(&80));
        let mut times = VecDeque::new();
        for _ in 0..24 {
            mark_time(&mut times);
        }
        assert_eq!(rate(&times), 24.0);
    }
}
