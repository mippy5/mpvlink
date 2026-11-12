//! The user's local identity, saved between launches.
//!
//! Stored as `key=value` lines in `~~state/mpvlink/profile` (mpv's state
//! directory: ~/.config/mpv on macOS, ~/.local/state/mpv on Linux). Unknown
//! keys are ignored, so fields can be added later without breaking old files.

use std::fs;
use std::io;
use std::path::Path;

/// mpv path (expanded with `expand-path`) of the profile file.
pub const PROFILE_PATH: &str = "~~state/mpvlink/profile";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Profile {
    pub display_name: String,
    /// Signaling server URL; empty means the default.
    pub server: String,
}

impl Profile {
    /// Load the profile, or an empty one if the file is missing or unreadable.
    pub fn load(path: &Path) -> Self {
        fs::read_to_string(path).map(|text| Self::parse(&text)).unwrap_or_default()
    }

    /// Write the profile atomically (temp file + rename), creating the
    /// directory if needed, so a crash mid-write can't leave a torn file.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, self.serialize())?;
        fs::rename(&tmp, path)
    }

    fn parse(text: &str) -> Self {
        let mut profile = Self::default();
        for line in text.lines() {
            let line = line.trim_start();
            if line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else { continue };
            let value = value.trim().to_string();
            match key.trim() {
                "name" => profile.display_name = value,
                "server" => profile.server = value,
                _ => {}
            }
        }
        profile
    }

    fn serialize(&self) -> String {
        // Values never contain newlines (the menus filter them out), but strip
        // control characters anyway so one value can't inject extra keys.
        let clean = |v: &str| v.chars().filter(|c| !c.is_control()).collect::<String>().trim().to_string();
        let mut out = String::from("# mpvlink profile\n");
        for (key, value) in [("name", &self.display_name), ("server", &self.server)] {
            let value = clean(value);
            if !value.is_empty() {
                out.push_str(&format!("{key}={value}\n"));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_text() {
        let p = Profile { display_name: "Zoë = the {cat}".into(), server: "wss://signal.example.com:8443/x?a=b".into() };
        assert_eq!(Profile::parse(&p.serialize()), p);
        let no_server = Profile { display_name: "Ana".into(), server: String::new() };
        assert!(!no_server.serialize().contains("server="), "default isn't written");
        assert_eq!(Profile::parse(&no_server.serialize()), no_server);
    }

    #[test]
    fn parse_ignores_comments_unknown_keys_and_junk() {
        let p = Profile::parse("# hi\nfuture_key=1\ngarbage\n  name =  Ana  \n");
        assert_eq!(p.display_name, "Ana");
        assert_eq!(Profile::parse(""), Profile::default());
    }

    #[test]
    fn serialize_strips_newlines() {
        let p = Profile { display_name: "Ana\nname=Evil".into(), server: String::new() };
        assert_eq!(Profile::parse(&p.serialize()).display_name, "Ananame=Evil");
    }

    #[test]
    fn save_and_load_from_disk() {
        let dir = std::env::temp_dir().join(format!("mpvlink-test-{}", std::process::id()));
        let path = dir.join("nested/profile");
        assert_eq!(Profile::load(&path), Profile::default(), "missing file loads empty");

        let p = Profile { display_name: "Dana".into(), server: "wss://x.example".into() };
        p.save(&path).unwrap();
        assert_eq!(Profile::load(&path), p);
        assert!(!path.with_extension("tmp").exists());

        fs::remove_dir_all(&dir).unwrap();
    }
}
