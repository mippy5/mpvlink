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
            if let Some((key, value)) = line.split_once('=')
                && key.trim() == "name"
            {
                profile.display_name = value.trim().to_string();
            }
        }
        profile
    }

    fn serialize(&self) -> String {
        // Names never contain newlines (the menu turns whitespace into
        // spaces), but strip them anyway so one can't inject extra keys.
        let name: String = self.display_name.chars().filter(|c| !c.is_control()).collect();
        format!("# mpvlink profile\nname={}\n", name.trim())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_text() {
        let p = Profile { display_name: "Zoë = the {cat}".into() };
        assert_eq!(Profile::parse(&p.serialize()), p);
    }

    #[test]
    fn parse_ignores_comments_unknown_keys_and_junk() {
        let p = Profile::parse("# hi\nfuture_key=1\ngarbage\n  name =  Ana  \n");
        assert_eq!(p.display_name, "Ana");
        assert_eq!(Profile::parse(""), Profile::default());
    }

    #[test]
    fn serialize_strips_newlines() {
        let p = Profile { display_name: "Ana\nname=Evil".into() };
        assert_eq!(Profile::parse(&p.serialize()).display_name, "Ananame=Evil");
    }

    #[test]
    fn save_and_load_from_disk() {
        let dir = std::env::temp_dir().join(format!("mpvlink-test-{}", std::process::id()));
        let path = dir.join("nested/profile");
        assert_eq!(Profile::load(&path), Profile::default(), "missing file loads empty");

        let p = Profile { display_name: "Dana".into() };
        p.save(&path).unwrap();
        assert_eq!(Profile::load(&path), p);
        assert!(!path.with_extension("tmp").exists());

        fs::remove_dir_all(&dir).unwrap();
    }
}
