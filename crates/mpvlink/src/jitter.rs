//! Even video timing over the internet.
//!
//! Frames reach a viewer with uneven gaps (network jitter, plus the
//! streamer's own send timing), and shown the moment they arrive, those
//! gaps show as stutter. Each frame's RTP timestamp says when it should be
//! shown relative to the others, so the viewer can hold every frame until
//! "its timestamp + a fixed offset", where the offset covers the slowest
//! recent arrivals. Motion is then even, at the cost of that much delay.
//!
//! The streamer stamps frames in real time (a frame's duration divided by
//! the playback speed), so timestamps advance like the wall clock while
//! playing. A pause or seek leaves a gap the timestamps don't have; that
//! shows up as a jump in transit time, and the buffer starts over.

use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// RTP clock rate for video.
const VIDEO_RATE: f64 = 90_000.0;
/// How far back arrival times are remembered.
const WINDOW: f64 = 10.0;
/// Transit time this far from the recent minimum is a pause or a seek (or
/// a long network stall), not jitter: start over.
const REBASE: f64 = 0.5;
/// Automatic delay: the spread that covers this share of recent frames,
/// plus a margin, within limits. All of them: even 1% of frames arriving
/// too late would be a visible hitch every few seconds. A one-off spike
/// raises the delay for a while (the window's length), then it eases back.
const AUTO_PERCENTILE: f64 = 1.0;
const AUTO_MARGIN: f64 = 0.008;
const AUTO_RANGE: (f64, f64) = (0.005, 0.300);
/// The offset holds steady (every change moves a frame off its slot), and
/// changes only when it must: up when it's short of the target (frames
/// would be late), down only when it's this far above it. Changes go in
/// small steps per frame, too small to see.
const SHRINK_SLACK: f64 = 0.010;
const SLEW_UP: f64 = 0.002;
const SLEW_DOWN: f64 = 0.0003;
/// How often timing reports are logged.
pub const REPORT_EVERY: Duration = Duration::from_secs(10);

/// How much a viewer delays video to even out its timing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Smoothing {
    /// Show frames the moment they arrive.
    Off,
    /// Just enough delay for the jitter measured lately.
    #[default]
    Auto,
    /// A fixed delay beyond the fastest arrivals, in milliseconds.
    Fixed(u32),
}

impl Smoothing {
    /// Parse `off`, `auto` or a delay like `100` / `100ms`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let t = text.trim().to_ascii_lowercase();
        match t.as_str() {
            "off" | "no" | "0" => Ok(Self::Off),
            "auto" | "yes" => Ok(Self::Auto),
            _ => match t.strip_suffix("ms").unwrap_or(&t).trim().parse::<u32>() {
                Ok(ms @ 1..=1000) => Ok(Self::Fixed(ms)),
                _ => Err("use off, auto or a delay in ms (up to 1000)".into()),
            },
        }
    }

    fn encode(self) -> u32 {
        match self {
            Self::Off => 0,
            Self::Auto => u32::MAX,
            Self::Fixed(ms) => ms,
        }
    }

    fn decode(n: u32) -> Self {
        match n {
            0 => Self::Off,
            u32::MAX => Self::Auto,
            ms => Self::Fixed(ms),
        }
    }
}

/// How it's written in settings: `off`, `auto` or `100`.
impl fmt::Display for Smoothing {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Off => write!(f, "off"),
            Self::Auto => write!(f, "auto"),
            Self::Fixed(ms) => write!(f, "{ms}"),
        }
    }
}

static SMOOTHING: AtomicU32 = AtomicU32::new(u32::MAX);
/// How long video being watched is held back on average, in microseconds,
/// so audio can be held back as much (keeping lip sync).
static VIDEO_HOLD_US: AtomicU32 = AtomicU32::new(0);

/// How long received audio should wait before playing: as long as video
/// is held back for smoothing.
pub fn video_hold() -> Duration {
    Duration::from_micros(u64::from(VIDEO_HOLD_US.load(Ordering::Relaxed)))
}

/// Set the smoothing for video being watched (takes effect immediately).
pub fn set_smoothing(s: Smoothing) {
    SMOOTHING.store(s.encode(), Ordering::Relaxed);
}

pub fn smoothing() -> Smoothing {
    Smoothing::decode(SMOOTHING.load(Ordering::Relaxed))
}

/// Ask the OS to wake the current thread on time. macOS otherwise lets
/// timed waits run late, by up to about 10 ms, to save power ("timer
/// coalescing", more so for apps in the background), which would undo the
/// even timing this module works out. Once per thread is enough.
pub fn timing_critical() {
    #[cfg(target_os = "macos")]
    {
        thread_local!(static DONE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) });
        if DONE.replace(true) {
            return;
        }
        unsafe extern "C" {
            fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
        }
        const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;
        // SAFETY: only changes the calling thread's scheduling class.
        unsafe { pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) };
    }
}

/// Unwraps 32-bit RTP timestamps into a continuous count.
#[derive(Default)]
struct RtpClock {
    last: Option<u32>,
    total: i64,
}

impl RtpClock {
    fn unwrap(&mut self, ts: u32) -> i64 {
        if let Some(last) = self.last {
            self.total += i64::from(ts.wrapping_sub(last) as i32);
        }
        self.last = Some(ts);
        self.total
    }
}

/// Recent transit times (arrival minus timestamp, in seconds): how far
/// each frame strayed from the fastest one.
#[derive(Default)]
struct Window {
    samples: VecDeque<(f64, f64)>,
}

impl Window {
    /// Add a sample; true if it's so far off that the window started over.
    fn push(&mut self, at: f64, transit: f64) -> bool {
        let rebased = self.min().is_some_and(|min| (transit - min).abs() > REBASE);
        if rebased {
            self.samples.clear();
        }
        self.samples.push_back((at, transit));
        while self.samples.front().is_some_and(|(t, _)| *t < at - WINDOW) {
            self.samples.pop_front();
        }
        rebased
    }

    fn min(&self) -> Option<f64> {
        self.samples.iter().map(|(_, t)| *t).reduce(f64::min)
    }

    /// The spread above the minimum that covers share `p` of the samples.
    fn spread(&self, p: f64) -> f64 {
        let min = self.min().unwrap_or(0.0);
        let mut spreads: Vec<f64> = self.samples.iter().map(|(_, t)| t - min).collect();
        percentile(&mut spreads, p)
    }
}

fn percentile(values: &mut [f64], p: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    values[((values.len() - 1) as f64 * p).round() as usize]
}

/// Timing figures gathered between reports.
#[derive(Default)]
struct Period {
    spreads: Vec<f64>,
    late: u32,
    rebases: u32,
    delay_sum: f64,
}

impl Period {
    fn summary(&mut self) -> String {
        let n = self.spreads.len();
        let ms = |s: f64| (s * 1000.0).round();
        let (p50, p98, max) = (
            percentile(&mut self.spreads, 0.5),
            percentile(&mut self.spreads, 0.98),
            percentile(&mut self.spreads, 1.0),
        );
        format!(
            "{n} frames, timing spread median {} / 98% {} / max {} ms",
            ms(p50),
            ms(p98),
            ms(max),
        )
    }
}

/// Decides when each received video frame is shown.
pub struct Playout {
    epoch: Instant,
    rtp: RtpClock,
    window: Window,
    /// Shown at timestamp + offset (seconds since `epoch`), once known.
    offset: Option<f64>,
    period: Period,
    period_start: Instant,
    /// Average time frames are held, seconds (for [`video_hold`]).
    hold: f64,
    /// How the latest frame was timed, for the statistics panel.
    last: crate::telemetry::FrameMark,
}

impl Default for Playout {
    fn default() -> Self {
        Self::new()
    }
}

impl Playout {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            epoch: now,
            rtp: RtpClock::default(),
            window: Window::default(),
            offset: None,
            period: Period::default(),
            period_start: now,
            hold: 0.0,
            last: crate::telemetry::FrameMark { spread_ms: 0.0, delay_ms: None, late: false },
        }
    }

    /// How the latest frame was timed.
    pub fn last_frame(&self) -> crate::telemetry::FrameMark {
        self.last
    }

    /// Track how long frames are held, for audio to match: a running
    /// average, or nothing at all with smoothing off.
    fn held(&mut self, hold: Option<f64>) {
        const SMOOTH: f64 = 0.05;
        self.hold = hold.map_or(0.0, |h| self.hold + (h - self.hold) * SMOOTH);
        VIDEO_HOLD_US.store((self.hold * 1e6) as u32, Ordering::Relaxed);
    }

    /// When to show a frame that arrived at `arrival` with RTP timestamp
    /// `ts`. Never before it arrived.
    pub fn schedule(&mut self, arrival: Instant, ts: u32, smoothing: Smoothing) -> Instant {
        let at = arrival.saturating_duration_since(self.epoch).as_secs_f64();
        let stamp = self.rtp.unwrap(ts) as f64 / VIDEO_RATE;
        let transit = at - stamp;
        if self.window.push(at, transit) {
            self.offset = None;
            self.period.rebases += 1;
        }
        let min = self.window.min().unwrap_or(transit);
        self.period.spreads.push(transit - min);
        self.last = crate::telemetry::FrameMark { spread_ms: (transit - min) * 1000.0, delay_ms: None, late: false };

        let delay = match smoothing {
            Smoothing::Off => {
                self.offset = None;
                self.held(None);
                return arrival;
            }
            Smoothing::Auto => {
                (self.window.spread(AUTO_PERCENTILE) + AUTO_MARGIN).clamp(AUTO_RANGE.0, AUTO_RANGE.1)
            }
            Smoothing::Fixed(ms) => f64::from(ms) / 1000.0,
        };
        let target = min + delay;
        let offset = match self.offset {
            None => target,
            Some(o) if target > o => o + (target - o).min(SLEW_UP),
            Some(o) if o - target > SHRINK_SLACK => o - SLEW_DOWN,
            Some(o) => o,
        };
        self.offset = Some(offset);
        self.period.delay_sum += offset - min;

        let show = stamp + offset;
        self.last.delay_ms = Some((offset - min) * 1000.0);
        self.last.late = show <= at;
        if show <= at {
            self.period.late += 1;
            self.held(Some(0.0));
            return arrival;
        }
        self.held(Some(show - at));
        self.epoch + Duration::from_secs_f64(show)
    }

    /// A summary of timing since the last one, every [`REPORT_EVERY`].
    pub fn report(&mut self, smoothing: Smoothing) -> Option<String> {
        if self.period_start.elapsed() < REPORT_EVERY || self.period.spreads.is_empty() {
            return None;
        }
        self.period_start = Instant::now();
        let mut p = std::mem::take(&mut self.period);
        let n = p.spreads.len();
        let mut line = p.summary();
        if smoothing != Smoothing::Off {
            let delay = (p.delay_sum / n as f64 * 1000.0).round();
            line += &format!("; smoothing {smoothing}: delay {delay} ms, {} late", p.late);
        }
        if p.rebases > 0 {
            line += &format!(", {} restarts (pause/seek)", p.rebases);
        }
        Some(line)
    }
}

/// The streamer's side: how evenly frames go out, compared with their
/// timestamps (before the network adds its own jitter).
pub struct SendMeter {
    epoch: Instant,
    stamp: f64,
    last_spread: f64,
    window: Window,
    period: Period,
    period_start: Instant,
}

impl Default for SendMeter {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            epoch: now,
            stamp: 0.0,
            last_spread: 0.0,
            window: Window::default(),
            period: Period::default(),
            period_start: now,
        }
    }
}

impl SendMeter {
    /// A frame sent now, advancing the timestamp by `duration` afterwards.
    pub fn sent(&mut self, duration: Duration) {
        let at = self.epoch.elapsed().as_secs_f64();
        let transit = at - self.stamp;
        self.stamp += duration.as_secs_f64();
        if self.window.push(at, transit) {
            self.period.rebases += 1;
        }
        let min = self.window.min().unwrap_or(transit);
        self.period.spreads.push(transit - min);
        self.last_spread = transit - min;
    }

    /// How far off an even pace the latest frame went out, ms.
    pub fn last_spread_ms(&self) -> f64 {
        self.last_spread * 1000.0
    }

    /// A summary every [`REPORT_EVERY`].
    pub fn report(&mut self) -> Option<String> {
        if self.period_start.elapsed() < REPORT_EVERY || self.period.spreads.is_empty() {
            return None;
        }
        self.period_start = Instant::now();
        Some(std::mem::take(&mut self.period).summary())
    }
}

/// Old position readings forgotten after this long. Long, so the best
/// reading is rarely replaced (each replacement nudges the clock); mpv's
/// clock and the system's keep pace well enough for that.
const CLOCK_WINDOW: f64 = 10.0;
/// A reading that hasn't moved for this long (more than any gap between
/// displayed frames) is a pause: the clock stops there.
const CLOCK_STILL: f64 = 0.12;
/// A reading this far off the smooth clock is a seek, pause or speed
/// change: start over from it.
const CLOCK_JUMP: f64 = 0.2;
/// How far the smooth clock may run ahead of mpv's reading: more than the
/// longest a reading stays put while playing (a 24 fps frame held for three
/// 60 Hz refreshes is 50 ms, more if mpv is late), so it doesn't step, but
/// not much more, so a pause stops it within a couple of frames.
const CLOCK_MAX_LEAD: f64 = 0.1;

/// mpv's playback position (`time-pos`) moves in steps: it changes when a
/// frame is displayed, and at 24 fps on a 60 Hz screen those come 33 and
/// 50 ms apart in turn. Sending frames when that reading reaches them
/// passes the unevenness on. This clock runs smoothly through the steps:
/// readings lag the true position by between 0 and one display interval,
/// so the least-lagging recent reading, carried forward at the playback
/// speed, is the position.
///
/// Readings come in through [`update`](Self::update) (from a thread that
/// may wait on mpv); [`predict`](Self::predict) needs no reading, so frames
/// can go out on time whatever mpv is busy with.
#[derive(Default)]
pub struct SmoothClock {
    epoch: Option<Instant>,
    /// (wall time, wall time at which the position was 0) per reading.
    readings: VecDeque<(f64, f64)>,
    speed: f64,
    /// The latest reading (None: no position, e.g. while loading), and
    /// when it last changed.
    reading: Option<f64>,
    moved_at: f64,
    /// The last prediction since the clock last started over.
    last: Option<f64>,
}

impl SmoothClock {
    fn wall(&mut self, at: Instant) -> f64 {
        let epoch = *self.epoch.get_or_insert(at);
        at.saturating_duration_since(epoch).as_secs_f64()
    }

    /// mpv read `position` (None if it has none) at `speed`, just before `at`.
    pub fn update(&mut self, at: Instant, position: Option<f64>, speed: f64) {
        let wall = self.wall(at);
        if position != self.reading {
            self.moved_at = wall;
        }
        self.reading = position;
        let Some(position) = position else {
            self.readings.clear();
            self.last = None;
            return;
        };
        let zero = wall - position / speed;
        let best = self.readings.iter().map(|(_, z)| *z).reduce(f64::min);
        if speed != self.speed || best.is_none_or(|b| (zero - b).abs() > CLOCK_JUMP) {
            self.readings.clear();
            self.speed = speed;
            self.last = None;
        }
        self.readings.push_back((wall, zero));
        while self.readings.front().is_some_and(|(w, _)| *w < wall - CLOCK_WINDOW) {
            self.readings.pop_front();
        }
    }

    /// The playback speed of the latest reading.
    pub fn speed(&self) -> f64 {
        if self.speed > 0.0 { self.speed } else { 1.0 }
    }

    /// The position at `at`, or None without one.
    pub fn predict(&mut self, at: Instant) -> Option<f64> {
        let wall = self.wall(at);
        let position = self.reading?;
        if wall - self.moved_at > CLOCK_STILL {
            // Paused (or stalled): the reading is the position.
            self.last = Some(position);
            return Some(position);
        }
        let best = self.readings.iter().map(|(_, z)| *z).reduce(f64::min)?;
        let mut smooth = ((wall - best) * self.speed).clamp(position, position + CLOCK_MAX_LEAD * self.speed);
        if let Some(last) = self.last {
            smooth = smooth.max(last); // never backwards
        }
        self.last = Some(smooth);
        Some(smooth)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: f64 = 1.0 / 24.0;

    /// Feed frames sent every FRAME seconds, arriving `jitter(i)` late;
    /// returns (arrival, shown) times in seconds.
    fn run(playout: &mut Playout, s: Smoothing, frames: usize, jitter: impl Fn(usize) -> f64) -> Vec<(f64, f64)> {
        let epoch = playout.epoch;
        (0..frames)
            .map(|i| {
                let arrival = 1.0 + i as f64 * FRAME + jitter(i);
                let ts = (i as f64 * FRAME * VIDEO_RATE) as u32;
                let shown = playout.schedule(epoch + Duration::from_secs_f64(arrival), ts, s);
                (arrival, (shown - epoch).as_secs_f64())
            })
            .collect()
    }

    /// A repeatable jitter pattern: mostly small, an occasional 60 ms.
    fn bumpy(i: usize) -> f64 {
        match (i * 7919) % 100 {
            0..=1 => 0.060,
            n => f64::from(n as u32 % 20) / 1000.0,
        }
    }

    #[test]
    fn off_shows_frames_on_arrival() {
        let mut p = Playout::new();
        for (arrival, shown) in run(&mut p, Smoothing::Off, 50, bumpy) {
            assert!((shown - arrival).abs() < 1e-6);
        }
    }

    #[test]
    fn auto_evens_out_the_gaps() {
        let mut p = Playout::new();
        let out = run(&mut p, Smoothing::Auto, 24 * 30, bumpy);
        // After it has settled: gaps between shown frames are all ~1 frame.
        let settled = &out[24 * 20..];
        for pair in settled.windows(2) {
            let gap = pair[1].1 - pair[0].1;
            assert!((gap - FRAME).abs() < 0.005, "uneven gap {gap}");
        }
        for (arrival, shown) in settled {
            assert!(shown >= arrival);
            assert!(shown - arrival < 0.1, "delay stays modest");
        }
        assert!(p.report(Smoothing::Auto).is_none(), "no report before the period is over");
    }

    #[test]
    fn fixed_delay_and_never_before_arrival() {
        let mut p = Playout::new();
        let out = run(&mut p, Smoothing::Fixed(100), 48, |_| 0.0);
        let (arrival, shown) = out[47];
        assert!((shown - arrival - 0.1).abs() < 1e-3);
        // A frame much later than the rest is shown when it arrives.
        let mut p = Playout::new();
        let out = run(&mut p, Smoothing::Fixed(20), 30, |i| if i == 25 { 0.2 } else { 0.0 });
        assert!((out[25].1 - out[25].0).abs() < 1e-6);
    }

    #[test]
    fn a_pause_starts_over() {
        let mut p = Playout::new();
        // 2 s pause after frame 50: arrivals jump, timestamps don't.
        let out = run(&mut p, Smoothing::Fixed(50), 100, |i| if i > 50 { 2.0 } else { 0.0 });
        let (arrival, shown) = out[99];
        assert!((shown - arrival - 0.05).abs() < 1e-3, "back to a 50 ms delay, not 2 s late");
        assert_eq!(p.period.rebases, 1);
    }

    #[test]
    fn smooth_clock_runs_through_the_steps() {
        let mut c = SmoothClock::default();
        let epoch = Instant::now();
        // 60 Hz display of 24 fps video: the reading advances to each frame
        // when it's shown (3, 2, 3, 2... refreshes), and is polled every 10 ms.
        let shown = |t: f64| {
            let refresh = (t * 60.0).floor();
            let frame = ((refresh / 2.5).floor()) / 24.0;
            frame.min(t)
        };
        let mut worst: f64 = 0.0;
        for i in 0..300 {
            let t = i as f64 * 0.01;
            let at = epoch + Duration::from_secs_f64(t);
            c.update(at, Some(shown(t)), 1.0);
            let now = c.predict(at + Duration::from_millis(3)).unwrap() - 0.003;
            if t > 1.0 {
                worst = worst.max((now - t).abs());
            }
        }
        assert!(worst < 0.006, "follows real time closely: off by {worst}");
        // Paused: stops within a frame or so of the reading.
        let mut last = 0.0;
        for i in 0..50 {
            let at = epoch + Duration::from_secs_f64(3.0 + i as f64 * 0.01);
            c.update(at, Some(3.0), 1.0);
            last = c.predict(at).unwrap();
        }
        assert!((last - 3.0).abs() < 1e-9, "settles on the paused position");
        // A seek back is followed at once.
        let at = epoch + Duration::from_secs_f64(3.6);
        c.update(at, Some(1.0), 1.0);
        assert!((c.predict(at).unwrap() - 1.0).abs() < 1e-9);
        c.update(at, None, 1.0);
        assert_eq!(c.predict(at), None, "no position while loading");
    }

    #[test]
    fn timestamps_wrap() {
        let mut c = RtpClock::default();
        assert_eq!(c.unwrap(u32::MAX - 10), 0);
        assert_eq!(c.unwrap(5), 16);
        assert_eq!(c.unwrap(3), 14, "a reordered frame goes back");
    }

    #[test]
    fn parses_settings() {
        assert_eq!(Smoothing::parse("off"), Ok(Smoothing::Off));
        assert_eq!(Smoothing::parse("Auto"), Ok(Smoothing::Auto));
        assert_eq!(Smoothing::parse("100ms"), Ok(Smoothing::Fixed(100)));
        assert!(Smoothing::parse("5000").is_err());
        for s in [Smoothing::Off, Smoothing::Auto, Smoothing::Fixed(250)] {
            assert_eq!(Smoothing::parse(&s.to_string()), Ok(s));
            assert_eq!(Smoothing::decode(s.encode()), s);
        }
    }
}
