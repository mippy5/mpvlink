//! What tracking keeps between launches, in mpv's state folder:
//! `mpvlink/anilist` (the sign-in, readable only by you),
//! `mpvlink/anilist-matches` (which show each title turned out to be, so
//! each series is looked up once) and `mpvlink/anilist-queue` (updates
//! that couldn't be sent yet).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const ACCOUNT_PATH: &str = "~~state/mpvlink/anilist";
pub const MATCHES_PATH: &str = "~~state/mpvlink/anilist-matches";
pub const QUEUE_PATH: &str = "~~state/mpvlink/anilist-queue";

/// Where tracking keeps its files (None: nowhere, e.g. no state folder).
#[derive(Clone, Debug, Default)]
pub struct Paths {
    pub account: Option<PathBuf>,
    pub matches: Option<PathBuf>,
    pub queue: Option<PathBuf>,
    /// The downloaded anime-relations data.
    pub relations: Option<PathBuf>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub token: String,
    /// The AniList user name, once the token was checked.
    #[serde(default)]
    pub user: String,
}

impl Account {
    pub fn load(path: &Path) -> Self {
        fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    /// Save, readable only by the owner (it's a key to their list).
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if self.token.is_empty() {
            return match fs::remove_file(path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
        write_private(path, &serde_json::to_string(self).map_err(io::Error::other)?)
    }
}

/// An update that couldn't be sent yet: set this show to this episode.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Queued {
    pub media_id: u32,
    pub episode: u32,
    pub title: String,
    #[serde(default)]
    pub episodes: Option<u32>,
}

/// Updates waiting to be sent, one per show (the furthest episode).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Queue(pub Vec<Queued>);

impl Queue {
    pub fn load(path: &Path) -> Self {
        fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if self.0.is_empty() {
            return match fs::remove_file(path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
        write_private(path, &serde_json::to_string(self).map_err(io::Error::other)?)
    }

    /// Add an update; for a show already waiting, keep the further episode.
    pub fn add(&mut self, item: Queued) {
        match self.0.iter_mut().find(|q| q.media_id == item.media_id) {
            Some(q) if q.episode >= item.episode => {}
            Some(q) => *q = item,
            None => self.0.push(item),
        }
    }
}

/// Confirmed matches: "normalized title|season" -> AniList id of that season
/// ([`DONT_TRACK`]: the user said it isn't anime).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Matches(pub HashMap<String, u32>);

/// A match to nothing: don't track this title.
pub const DONT_TRACK: u32 = 0;

impl Matches {
    pub fn key(title: &str, season: Option<u32>) -> String {
        let title: String = title.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ");
        format!("{title}|{}", season.unwrap_or(1))
    }

    pub fn load(path: &Path) -> Self {
        fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        write_private(path, &serde_json::to_string(self).map_err(io::Error::other)?)
    }
}

/// Write atomically (temp file + rename), creating the folder, with
/// owner-only permissions where the OS has them.
fn write_private(path: &Path, text: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp: PathBuf = path.with_extension("tmp");
    {
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&tmp)?;
        io::Write::write_all(&mut file, text.as_bytes())?;
    }
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_privately() {
        let dir = std::env::temp_dir().join(format!("mpvlink-store-{}", std::process::id()));
        let path = dir.join("anilist");
        assert_eq!(Account::load(&path), Account::default());
        let account = Account { token: "abc".into(), user: "Ana".into() };
        account.save(&path).unwrap();
        assert_eq!(Account::load(&path), account);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        Account::default().save(&path).unwrap();
        assert!(!path.exists(), "signing out removes the file");

        let mut m = Matches::default();
        m.0.insert(Matches::key("Ansatsu Kyoushitsu!", Some(2)), 21170);
        m.save(&dir.join("matches")).unwrap();
        assert_eq!(Matches::load(&dir.join("matches")).0.get("ansatsu kyoushitsu|2"), Some(&21170));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn queue_keeps_the_furthest_episode_per_show() {
        let item = |media_id, episode| Queued { media_id, episode, title: "S".into(), episodes: None };
        let mut q = Queue::default();
        q.add(item(1, 3));
        q.add(item(1, 2));
        q.add(item(2, 7));
        q.add(item(1, 4));
        assert_eq!(q.0, vec![item(1, 4), item(2, 7)]);
    }
}
