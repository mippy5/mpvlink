//! The subtitle tracks of the file being streamed, as files for viewers
//! (who then pick and style them like any subtitle track; see
//! `stream::VideoFormat::Matroska`). Text tracks keep their format (ASS,
//! SRT, WebVTT); image tracks go as `.sup` (PGS); the file's attached fonts
//! go along when there are ASS tracks, so styled subtitles look as meant.
//!
//! Extraction reads the whole file once (one ffmpeg run for every track),
//! so it runs on a thread of its own.

use mpvlink_rtc::files::{Pack, PackFile};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Packs bigger than this leave out the tracks that don't fit (image
/// subtitles are the big ones).
const MAX_PACK_BYTES: usize = 200 << 20;

/// A subtitle track as mpv lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Track {
    /// ffmpeg's stream index, for tracks inside the file.
    pub ff_index: Option<i64>,
    /// The file, for subtitle files mpv loaded separately.
    pub external: Option<String>,
    pub codec: String,
    pub title: String,
    pub lang: String,
    pub default: bool,
    /// The one mpv shows now (viewers start with it).
    pub selected: bool,
}

/// How a track is written out: ffmpeg codec (None: copy), muxer, file extension.
type Output = (Option<&'static str>, &'static str, &'static str);

/// How a track with `codec` is written out. None: it can't be (DVD and DVB
/// subtitles).
fn output(codec: &str) -> Option<Output> {
    Some(match codec {
        "ass" | "ssa" => (None, "ass", "ass"),
        "subrip" => (None, "srt", "srt"),
        "webvtt" => (None, "webvtt", "vtt"),
        "mov_text" | "text" => (Some("srt"), "srt", "srt"),
        "hdmv_pgs_subtitle" => (None, "sup", "sup"),
        _ => return None,
    })
}

/// The tracks of `source` (a path, or an http(s) URL, whose own tracks
/// aren't extracted: that would download the whole file), as a pack.
pub fn export(ffmpeg: &str, source: &str, tracks: &[Track]) -> Pack {
    let dir = scratch_dir();
    let mut files = Vec::new();
    let remote = source.contains("://");

    // Tracks inside the file: one ffmpeg run writes them all.
    let internal: Vec<(&Track, Output)> = tracks
        .iter()
        .filter(|t| t.external.is_none() && t.ff_index.is_some() && !remote)
        .filter_map(|t| Some((t, output(&t.codec)?)))
        .collect();
    if !internal.is_empty() && std::fs::create_dir_all(&dir).is_ok() {
        let mut c = Command::new(ffmpeg);
        c.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-i", source]);
        for (i, (track, (codec, muxer, ext))) in internal.iter().enumerate() {
            c.args(["-map", &format!("0:{}", track.ff_index.unwrap_or(0))]);
            c.args(["-c:s", codec.unwrap_or("copy"), "-f", muxer]);
            c.arg(dir.join(format!("track{i}.{ext}")));
        }
        c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
        crate::ffmpeg::hide_console_window(&mut c);
        match c.output() {
            Ok(out) if !out.status.success() => {
                eprintln!("[mpvlink] extracting subtitle tracks: {}", String::from_utf8_lossy(&out.stderr).trim());
            }
            Err(e) => eprintln!("[mpvlink] couldn't run ffmpeg to extract subtitles: {e}"),
            _ => {}
        }
        for (i, (track, (_, _, ext))) in internal.iter().enumerate() {
            if let Ok(data) = std::fs::read(dir.join(format!("track{i}.{ext}"))) {
                files.push(subtitle_file(format!("track{i}.{ext}"), track, data));
            }
        }
    }

    // Subtitle files mpv loaded separately (local ones).
    for (i, track) in tracks.iter().enumerate() {
        let Some(path) = track.external.as_deref().map(|p| p.strip_prefix("file://").unwrap_or(p)) else { continue };
        if path.contains("://") {
            continue;
        }
        let ext = Path::new(path).extension().map_or("srt".into(), |e| e.to_string_lossy().to_lowercase());
        if let Ok(data) = std::fs::read(path) {
            files.push(subtitle_file(format!("external{i}.{ext}"), track, data));
        }
    }

    // Fonts, for styled (ASS) subtitles.
    let styled = tracks.iter().any(|t| matches!(t.codec.as_str(), "ass" | "ssa"));
    if styled && !remote {
        for (name, data) in crate::ffmpeg::attached_fonts(ffmpeg, source) {
            files.push(PackFile { name, meta: json!({ "kind": "font" }), data: data.into() });
        }
    }
    let _ = std::fs::remove_dir_all(&dir);

    // Within the size limit: fonts and text first, then the rest by size.
    files.sort_by_key(|f| (f.meta["kind"] != "font", f.data.len()));
    let mut total = 0;
    files.retain(|f| {
        total += f.data.len();
        total <= MAX_PACK_BYTES
    });
    Pack { files }
}

fn subtitle_file(name: String, track: &Track, data: Vec<u8>) -> PackFile {
    let meta = json!({
        "kind": "subtitle",
        "title": track.title,
        "lang": track.lang,
        "codec": track.codec,
        "default": track.default,
        "selected": track.selected,
    });
    PackFile { name, meta, data: data.into() }
}

fn scratch_dir() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!("mpvlink-export-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(output("ass"), Some((None, "ass", "ass")));
        assert_eq!(output("mov_text"), Some((Some("srt"), "srt", "srt")));
        assert_eq!(output("hdmv_pgs_subtitle").map(|o| o.2), Some("sup"));
        assert_eq!(output("dvd_subtitle"), None);
    }

    /// `SUBEXPORT_FILE=... cargo test -p mpvlink subexport_real -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn subexport_real() {
        let file = std::env::var("SUBEXPORT_FILE").unwrap();
        let track = Track {
            ff_index: Some(2),
            external: None,
            codec: "ass".into(),
            title: String::new(),
            lang: "eng".into(),
            default: true,
            selected: true,
        };
        let pack = export("ffmpeg", &file, &[track]);
        for f in &pack.files {
            println!("{} {} bytes {}", f.name, f.data.len(), f.meta);
        }
        assert!(pack.files.iter().any(|f| f.meta["kind"] == "subtitle" && !f.data.is_empty()));
    }
}
