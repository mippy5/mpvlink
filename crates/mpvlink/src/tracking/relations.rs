//! Taiga's anime-relations data (https://github.com/erengy/anime-relations,
//! public domain): which episode numbers belong to another entry. Release
//! groups often keep counting across seasons ("Shingeki no Kyojin - 26" is
//! the second season's first episode), or number specials as episode 0.
//!
//! A snapshot is built in; a fresh copy is downloaded weekly into mpv's
//! state folder (only once tracking is in use).

use std::path::Path;
use std::time::{Duration, SystemTime};

/// The snapshot built into mpvlink.
const BUNDLED: &str = include_str!("anime-relations.txt");
const URL: &str = "https://raw.githubusercontent.com/erengy/anime-relations/master/anime-relations.txt";
pub const PATH: &str = "~~state/mpvlink/anime-relations.txt";
/// How old the downloaded copy may get before it's fetched again.
const REFRESH_AFTER: Duration = Duration::from_secs(7 * 24 * 3600);

/// Episodes `first..=last` (`last` None: and on).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Range {
    first: u32,
    last: Option<u32>,
}

impl Range {
    fn parse(s: &str) -> Option<Self> {
        let (first, last) = match s.split_once('-') {
            Some((a, "?")) => (a.parse().ok()?, None),
            Some((a, b)) => (a.parse().ok()?, Some(b.parse().ok()?)),
            None => {
                let n = s.parse().ok()?;
                (n, Some(n))
            }
        };
        Some(Self { first, last })
    }

    fn contains(&self, episode: u32) -> bool {
        episode >= self.first && self.last.is_none_or(|last| episode <= last)
    }
}

/// Episodes `from` of AniList entry `from_id` are episodes `to` of `to_id`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rule {
    from_id: u32,
    from: Range,
    to_id: u32,
    to: Range,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Relations {
    rules: Vec<Rule>,
}

impl Relations {
    /// The rules in anime-relations' format (lines like
    /// `- 16498|7442|16498:26-37 -> 25777|8671|20958:1-12!`). Rules without
    /// an AniList id, and lines that don't parse, are skipped.
    pub fn parse(text: &str) -> Self {
        let mut rules = Vec::new();
        let mut in_rules = false;
        for line in text.lines().map(str::trim) {
            if line.starts_with("::") {
                in_rules = line == "::rules";
                continue;
            }
            let Some(rule) = line.strip_prefix("- ").filter(|_| in_rules) else { continue };
            let Some((lhs, rhs)) = rule.split_once("->") else { continue };
            let (rhs, to_itself) = match rhs.trim().strip_suffix('!') {
                Some(r) => (r, true),
                None => (rhs.trim(), false),
            };
            let side = |s: &str, source: Option<u32>| -> Option<(u32, Range)> {
                let (ids, episodes) = s.trim().split_once(':')?;
                let anilist = ids.split('|').nth(2)?;
                let id = match anilist {
                    "~" => source?,
                    id => id.parse().ok()?,
                };
                Some((id, Range::parse(episodes)?))
            };
            let Some((from_id, from)) = side(lhs, None) else { continue };
            let Some((to_id, to)) = side(rhs, Some(from_id)) else { continue };
            rules.push(Rule { from_id, from, to_id, to });
            // "!": the destination's own entry numbered the same way (a file
            // named as season 2 but counted from season 1) redirects too.
            if to_itself && to_id != from_id {
                rules.push(Rule { from_id: to_id, from, to_id, to });
            }
        }
        Self { rules }
    }

    /// The snapshot built into mpvlink.
    pub fn bundled() -> Self {
        Self::parse(BUNDLED)
    }

    /// The downloaded copy if there is one, else the built-in snapshot.
    pub fn load(path: Option<&Path>) -> Self {
        let downloaded = path.and_then(|p| std::fs::read_to_string(p).ok()).map(|t| Self::parse(&t));
        downloaded.filter(|r| !r.rules.is_empty()).unwrap_or_else(Self::bundled)
    }

    /// Where episode `episode` of entry `id` really belongs, if a rule moves it.
    pub fn redirect(&self, id: u32, episode: u32) -> Option<(u32, u32)> {
        let rule = self.rules.iter().find(|r| r.from_id == id && r.from.contains(episode))?;
        let offset = episode - rule.from.first;
        // A range maps one to one; a single episode maps to the start.
        let to = match rule.to.last {
            Some(last) if last == rule.to.first => rule.to.first,
            _ => rule.to.first + offset,
        };
        Some((rule.to_id, to))
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.rules.len()
    }
}

/// Whether the copy at `path` is missing or old enough to fetch again.
pub fn stale(path: &Path) -> bool {
    let modified = std::fs::metadata(path).and_then(|m| m.modified());
    modified.map_or(true, |t| SystemTime::now().duration_since(t).unwrap_or_default() > REFRESH_AFTER)
}

/// Fetch the latest rules and save them at `path`; the parsed rules.
pub fn download(agent: &ureq::Agent, path: &Path) -> Result<Relations, String> {
    let mut response = agent.get(URL).call().map_err(|e| format!("couldn't download anime-relations ({e})"))?;
    if response.status() != 200 {
        return Err(format!("couldn't download anime-relations (HTTP {})", response.status()));
    }
    let text = response.body_mut().read_to_string().map_err(|e| format!("anime-relations download cut off ({e})"))?;
    let relations = Relations::parse(&text);
    if relations.rules.is_empty() {
        return Err("the downloaded anime-relations had no rules".into());
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &text).and_then(|_| std::fs::rename(&tmp, path)).map_err(|e| format!("couldn't save anime-relations ({e})"))?;
    Ok(relations)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
::meta
- version: 1.3.0
::rules
# Shingeki no Kyojin -> ~ Season 2
- 16498|7442|16498:26-37 -> 25777|8671|20958:1-12!
# Ansatsu Kyoushitsu (TV) -> ~: Deai no Jikan
- 19759|7973|19759:0 -> 28405|10020|21303:1
# special numbered as the last episode, same entry
- 33820|12478|21898:13 -> ~|~|~:1
# open-ended
- 1|2|300:13-? -> 4|5|600:1-?!
# no AniList id: skipped
- 6|7|?:2 -> 8|9|?:1
";

    #[test]
    fn parses_and_redirects() {
        let r = Relations::parse(SAMPLE);
        assert_eq!(r.len(), 6, "four rules, two of them with '!' copies; the one without AniList ids skipped");
        assert_eq!(r.redirect(16498, 26), Some((20958, 1)), "continuous numbering into season 2");
        assert_eq!(r.redirect(16498, 37), Some((20958, 12)));
        assert_eq!(r.redirect(16498, 25), None, "season 1 itself");
        assert_eq!(r.redirect(20958, 30), Some((20958, 5)), "'!': season 2 numbered from season 1");
        assert_eq!(r.redirect(19759, 0), Some((21303, 1)), "episode 0 is a special");
        assert_eq!(r.redirect(21898, 13), Some((21898, 1)), "'~' is the same entry");
        assert_eq!(r.redirect(300, 40), Some((600, 28)), "open-ended ranges");
    }

    #[test]
    fn the_bundled_snapshot_parses() {
        let r = Relations::bundled();
        assert!(r.len() > 500, "{} rules", r.len());
        assert_eq!(r.redirect(16498, 40), Some((99147, 3)), "Attack on Titan 40 is Season 3, episode 3");
    }
}
