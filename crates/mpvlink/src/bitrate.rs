//! The bitrate of your stream: a fixed number, or based on the bitrate of
//! the video being streamed.
//!
//! Written as text the same way in the saved settings and in --script-opts:
//! `6000` (kb/s), `source`, or `1.5x` (times the source).

use std::fmt;

/// Kilobits per second when nothing is set.
pub const DEFAULT_KBPS: u32 = 6000;
/// Allowed custom bitrates, kb/s.
pub const KBPS_RANGE: (u32, u32) = (100, 100_000);
/// Allowed multipliers of the source bitrate.
pub const TIMES_RANGE: (f64, f64) = (0.1, 10.0);
/// What a source-based bitrate is kept within, kb/s.
const SOURCE_CLAMP: (f64, f64) = (500.0, 50_000.0);
/// The largest size streams are sent at (bigger videos are scaled down).
const MAX_SIZE: (f64, f64) = (1920.0, 1080.0);

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Bitrate {
    Kbps(u32),
    /// The source video's bitrate, times this.
    Times(f64),
}

impl Default for Bitrate {
    fn default() -> Self {
        Self::Kbps(DEFAULT_KBPS)
    }
}

impl Bitrate {
    pub const SOURCE: Bitrate = Bitrate::Times(1.0);

    /// Parse `6000`, `6000k`, `source`, `1.5x` or `x1.5`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let t = text.trim().to_ascii_lowercase();
        if t == "source" {
            return Ok(Self::SOURCE);
        }
        if let Some(n) = t.strip_suffix('x').or_else(|| t.strip_prefix('x')).or_else(|| t.strip_suffix('×')) {
            return parse_multiplier(n).map(Self::Times);
        }
        parse_kbps(t.strip_suffix('k').unwrap_or(&t)).map(Self::Kbps)
    }

    /// The kb/s to encode at, given the source video's bitrate and size
    /// (from [`probe_source`]); None when that's unknown.
    pub fn kbps(self, source: Option<SourceVideo>) -> u32 {
        match self {
            Self::Kbps(k) => k,
            Self::Times(times) => match source {
                Some(s) => (s.sent_kbps() * times).clamp(SOURCE_CLAMP.0, SOURCE_CLAMP.1).round() as u32,
                None => DEFAULT_KBPS,
            },
        }
    }
}

/// How it's written in settings: `6000`, `source` or `1.5x`.
impl fmt::Display for Bitrate {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Kbps(k) => write!(f, "{k}"),
            Self::Times(t) if *t == 1.0 => write!(f, "source"),
            Self::Times(t) => write!(f, "{}x", trim_number(*t)),
        }
    }
}

/// A multiplier as typed: a positive number within [`TIMES_RANGE`].
pub fn parse_multiplier(text: &str) -> Result<f64, String> {
    let (lo, hi) = TIMES_RANGE;
    match text.trim().parse::<f64>() {
        Ok(t) if (lo..=hi).contains(&t) => Ok((t * 100.0).round() / 100.0),
        _ => Err(format!("Enter a multiplier from {lo} to {hi}, e.g. 1.5.")),
    }
}

/// A bitrate as typed, in kb/s, within [`KBPS_RANGE`].
pub fn parse_kbps(text: &str) -> Result<u32, String> {
    let (lo, hi) = KBPS_RANGE;
    match text.trim().parse::<u32>() {
        Ok(k) if (lo..=hi).contains(&k) => Ok(k),
        _ => Err(format!("Enter a bitrate from {lo} to {hi} kb/s, e.g. {DEFAULT_KBPS}.")),
    }
}

/// `1.5`, `2`, `0.25`: no trailing zeros.
pub fn trim_number(n: f64) -> String {
    let s = format!("{n:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// The bitrate and size of the video being streamed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SourceVideo {
    pub kbps: f64,
    /// Width and height, if known.
    pub size: Option<(f64, f64)>,
}

impl SourceVideo {
    /// The source bitrate for the size we send: a video bigger than 1080p is
    /// scaled down, and needs proportionally fewer bits.
    fn sent_kbps(&self) -> f64 {
        let Some((w, h)) = self.size.filter(|(w, h)| *w > 0.0 && *h > 0.0) else { return self.kbps };
        let scale = (MAX_SIZE.0 / w).min(MAX_SIZE.1 / h).min(1.0);
        self.kbps * scale * scale
    }
}

/// Find the video's bitrate in what `ffmpeg -i` prints: the stream's own
/// figure (`… 4996 kb/s, …`, or a `BPS` tag as mkvmerge writes), else the
/// whole file's (`bitrate: 1363 kb/s`). `stream` is ffmpeg's index of the
/// video stream to look at; None means the first.
pub fn probe_source(info: &str, stream: Option<i64>) -> Option<SourceVideo> {
    let mut total = None;
    let mut found: Option<SourceVideo> = None;
    let mut in_target = false;
    for line in info.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("Duration:") {
            total = rest.split("bitrate:").nth(1).and_then(|b| b.trim().strip_suffix("kb/s")?.trim().parse::<f64>().ok());
        } else if let Some(rest) = t.strip_prefix("Stream #") {
            if found.is_some() {
                in_target = false;
                continue;
            }
            // "0:3[0x1](eng): Video: h264 …"
            let index: Option<i64> = rest.split(':').nth(1).and_then(|s| {
                s.chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok()
            });
            in_target = rest.contains(": Video:") && stream.is_none_or(|s| index == Some(s));
            if in_target {
                let pieces: Vec<&str> = rest.split(", ").collect();
                let kbps = pieces.iter().find_map(|p| p.trim().strip_suffix(" kb/s")?.trim().parse().ok());
                let size = pieces.iter().find_map(|p| {
                    let (w, h) = p.split_whitespace().next()?.split_once('x')?;
                    Some((w.parse().ok()?, h.parse().ok()?))
                });
                found = Some(SourceVideo { kbps: kbps.unwrap_or(0.0), size });
            }
        } else if in_target
            && let Some((key, value)) = t.split_once(':')
            && key.trim().starts_with("BPS")
            && let (Some(v), Some(f)) = (value.trim().parse::<f64>().ok(), found.as_mut())
            && f.kbps == 0.0
        {
            f.kbps = v / 1000.0;
        }
    }
    let mut video = found.unwrap_or(SourceVideo { kbps: 0.0, size: None });
    if video.kbps <= 0.0 {
        video.kbps = total.filter(|t| *t > 0.0)?;
    }
    Some(video)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_prints_settings() {
        assert_eq!(Bitrate::parse("6000"), Ok(Bitrate::Kbps(6000)));
        assert_eq!(Bitrate::parse(" 8000k "), Ok(Bitrate::Kbps(8000)));
        assert_eq!(Bitrate::parse("Source"), Ok(Bitrate::SOURCE));
        assert_eq!(Bitrate::parse("1.5x"), Ok(Bitrate::Times(1.5)));
        assert_eq!(Bitrate::parse("x2"), Ok(Bitrate::Times(2.0)));
        assert!(Bitrate::parse("50").is_err());
        assert!(Bitrate::parse("0x").is_err());
        assert!(Bitrate::parse("fast").is_err());
        for b in [Bitrate::Kbps(6000), Bitrate::SOURCE, Bitrate::Times(1.25)] {
            assert_eq!(Bitrate::parse(&b.to_string()), Ok(b));
        }
        assert_eq!(Bitrate::Times(1.25).to_string(), "1.25x");
        assert_eq!(Bitrate::SOURCE.to_string(), "source");
    }

    const MKV: &str = "Input #0, matroska,webm, from 'a.mkv':
  Duration: 00:23:16.57, start: 0.000000, bitrate: 1363 kb/s
  Stream #0:0: Video: hevc (Main 10), yuv420p10le(tv, bt709), 1920x1080, SAR 1:1 DAR 16:9, 23.98 fps (default)
      Metadata:
        BPS             : 1228228
  Stream #0:1(jpn): Audio: flac, 48000 Hz, stereo, s32 (24 bit) (default)
      Metadata:
        BPS             : 900000
";

    const MP4: &str = "  Duration: 00:00:01.00, start: 0.000000, bitrate: 37716 kb/s
  Stream #0:0[0x1](und): Video: h264 (High) (avc1 / 0x31637661), yuv420p(progressive), 3840x2160 [SAR 1:1 DAR 16:9], 37626 kb/s, 25 fps (default)
  Stream #0:1[0x2](und): Audio: aac (LC) (mp4a / 0x6134706D), 44100 Hz, mono, fltp, 70 kb/s (default)
";

    #[test]
    fn finds_the_video_bitrate() {
        let mkv = probe_source(MKV, None).unwrap();
        assert!((mkv.kbps - 1228.228).abs() < 1e-6, "the video's BPS tag, not the audio's");
        assert_eq!(mkv.size, Some((1920.0, 1080.0)));
        assert_eq!(probe_source(MKV, Some(0)), Some(mkv));
        let mp4 = probe_source(MP4, Some(0)).unwrap();
        assert_eq!(mp4, SourceVideo { kbps: 37626.0, size: Some((3840.0, 2160.0)) });
        // No figure for the stream: the whole file's.
        let bare = "  Duration: 00:01:00.00, start: 0.0, bitrate: 2500 kb/s\n  Stream #0:0: Video: vp9, yuv420p, 1280x720\n";
        assert_eq!(probe_source(bare, None).unwrap().kbps, 2500.0);
        assert_eq!(probe_source("nothing useful", None), None);
    }

    #[test]
    fn source_bitrates_follow_the_sent_size() {
        let mkv = probe_source(MKV, None);
        assert_eq!(Bitrate::SOURCE.kbps(mkv), 1228);
        assert_eq!(Bitrate::Times(1.5).kbps(mkv), 1842);
        // 4K is sent at 1080p: a quarter of the pixels.
        assert_eq!(Bitrate::SOURCE.kbps(probe_source(MP4, None)), 9407);
        assert_eq!(Bitrate::Times(0.1).kbps(mkv), 500, "kept to a sensible minimum");
        assert_eq!(Bitrate::SOURCE.kbps(None), DEFAULT_KBPS, "unknown source");
        assert_eq!(Bitrate::Kbps(3000).kbps(None), 3000);
    }
}
