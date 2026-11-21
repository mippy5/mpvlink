//! What episode of what show a file is, from its name alone, like Taiga
//! does: release names follow loose conventions (`[Group] Title - 05 [1080p]`,
//! `Title S2E05`, `Title 2x05`) that anitomy knows.

use anitomy_ng::{ElementKind, Options, parse_path};

/// An episode as named in a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parsed {
    pub title: String,
    /// The season, when the name gives one past the first.
    pub season: Option<u32>,
    pub episode: u32,
    pub year: Option<u32>,
}

/// Read `name` (a file name or path). None when it doesn't name an episode
/// of something: no title, no episode number (a movie, a camera clip), or a
/// fractional one (a recap like 12.5, which lists don't count).
///
/// Only the file name is read, plus the folder it's in when the file name
/// has no title (`Show/01.mkv`): the rest of a path is personal, and must
/// never end up in a search.
pub fn recognize(name: &str) -> Option<Parsed> {
    let mut parts = name.rsplit(['/', '\\']).filter(|p| !p.is_empty());
    let file = parts.next()?;
    parse(file).or_else(|| parse(&format!("{}/{file}", parts.next()?)))
}

fn parse(name: &str) -> Option<Parsed> {
    let elements = parse_path(name, Options::default());
    let first = |kind: ElementKind| elements.iter().find(|e| e.kind == kind).map(|e| e.value.trim());
    let title = first(ElementKind::Title).filter(|t| !t.is_empty() && !t.contains(['/', '\\']))?.to_string();
    // "01v2" is episode 1 (a revised release); "01-02" (two elements, or
    // one with a dash) covers both, so it counts up to the last.
    let mut episode = None;
    for e in elements.iter().filter(|e| e.kind == ElementKind::Episode) {
        for part in e.value.split(['-', '~', '&']) {
            if part.contains('.') {
                return None;
            }
            let n: Option<u32> = part.trim().chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok();
            episode = episode.max(n);
        }
    }
    let episode = episode?;
    let number = |kind| first(kind).and_then(|v| v.chars().filter(char::is_ascii_digit).collect::<String>().parse().ok());
    Some(Parsed {
        title,
        season: number(ElementKind::Season).filter(|s| *s > 1),
        episode,
        year: number(ElementKind::Year),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(title: &str, season: Option<u32>, episode: u32) -> Option<Parsed> {
        Some(Parsed { title: title.into(), season, episode, year: None })
    }

    #[test]
    fn common_release_names() {
        assert_eq!(
            recognize("/videos/Ansatsu Kyoushitsu [BD][HEVC][10bit][1080p]/Ansatsu Kyoushitsu 2x14.mkv"),
            parsed("Ansatsu Kyoushitsu", Some(2), 14)
        );
        assert_eq!(
            recognize("[SubsPlease] Sousou no Frieren - 28 (1080p) [ABCD1234].mkv"),
            parsed("Sousou no Frieren", None, 28)
        );
        assert_eq!(recognize("Shingeki no Kyojin S3 - 05.mkv"), parsed("Shingeki no Kyojin", Some(3), 5));
        assert_eq!(recognize("[Judas] Gyakkyou Muriyari Kakusei - S01E07.mkv"), parsed("Gyakkyou Muriyari Kakusei", None, 7));
        assert_eq!(recognize("[Group] Show - 01v2 [720p].mkv"), parsed("Show", None, 1));
        assert_eq!(recognize("[Group] Show - 01-02 [720p].mkv"), parsed("Show", None, 2));
    }

    #[test]
    fn reads_only_the_file_name_and_its_folder() {
        let p = recognize("/Users/someone/Movies/holiday 2023/clip 3.mp4").unwrap();
        assert_eq!(p.title, "clip", "no folders in the title");
        assert_eq!(recognize("/Users/someone/Downloads/Show/01.mkv"), parsed("Show", None, 1), "title from the folder");
        assert_eq!(recognize(r"C:\Videos\Show\Show - 04.mkv"), parsed("Show", None, 4));
        assert_eq!(recognize("/tmp/6174d959-e77b-407c-8dfe-69abb375a4d8/scratchpad/bob.mp4"), None);
    }

    #[test]
    fn not_episodes() {
        assert_eq!(recognize("20231215_104447.mp4"), None, "no title");
        assert_eq!(recognize("[Group] Cowboy Bebop The Movie (2001) [BD 1080p].mkv"), None, "no episode");
        assert_eq!(recognize("[Group] Show - 12.5 [720p].mkv"), None, "a recap");
    }
}
