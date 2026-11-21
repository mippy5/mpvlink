//! Finding the AniList entry for a parsed file name.
//!
//! Search by title, score the results on how closely a title or synonym
//! matches (and on format and year), then find the right season: AniList
//! has one entry per season, linked as sequels, while files may say
//! "season 2" or just keep counting episodes past the first season's end.

use super::recognize::Parsed;
use super::relations::Relations;

/// What's needed of an AniList entry.
#[derive(Clone, Debug, PartialEq)]
pub struct Media {
    pub id: u32,
    /// TV, TV_SHORT, MOVIE, SPECIAL, OVA, ONA, MUSIC.
    pub format: Option<String>,
    pub episodes: Option<u32>,
    pub year: Option<u32>,
    /// Romaji, English and native titles, then synonyms.
    pub titles: Vec<String>,
    /// The title to show (English, else romaji).
    pub display: String,
    /// Sequels, with their formats.
    pub sequels: Vec<(u32, Option<String>)>,
}

impl Media {
    /// A series-like entry (the kind with numbered episodes).
    fn is_series(&self) -> bool {
        matches!(self.format.as_deref(), Some("TV" | "TV_SHORT" | "ONA") | None)
    }

    /// The next season: a series-like sequel.
    fn next_season(&self) -> Option<u32> {
        self.sequels.iter().find(|(_, f)| matches!(f.as_deref(), Some("TV" | "TV_SHORT" | "ONA"))).map(|(id, _)| *id)
    }
}

/// The entry and episode a file is.
#[derive(Clone, Debug, PartialEq)]
pub struct Resolved {
    pub media: Media,
    pub episode: u32,
}

/// The least score a search result needs to count as the show.
const MIN_SCORE: f64 = 0.8;
/// How many seasons (or sequels) to walk through at most.
const MAX_HOPS: usize = 10;

/// Find what `parsed` is. `search` looks up titles; `fetch` gets one
/// entry by id. Ok(None): nothing matched well enough.
#[cfg(test)]
pub fn resolve<E>(
    parsed: &Parsed,
    relations: &Relations,
    search: impl FnMut(&str) -> Result<Vec<Media>, E>,
    mut fetch: impl FnMut(u32) -> Result<Media, E>,
) -> Result<Option<Resolved>, E> {
    match find(parsed, search, &mut fetch)? {
        Some(media) => carry(media, parsed.episode, relations, fetch),
        None => Ok(None),
    }
}

/// The entry for the file's title and season (before working out which
/// entry its episode number belongs to).
pub fn find<E>(
    parsed: &Parsed,
    mut search: impl FnMut(&str) -> Result<Vec<Media>, E>,
    mut fetch: impl FnMut(u32) -> Result<Media, E>,
) -> Result<Option<Media>, E> {
    let results = search(&parsed.title)?;
    let Some((mut media, score)) = best(&results, &parsed.title, parsed.year) else { return Ok(None) };
    if score < MIN_SCORE {
        return Ok(None);
    }

    // A later season: AniList often titles it outright ("… 2nd Season",
    // "… Season 3"), else walk the sequels from the first.
    if let Some(season) = parsed.season {
        let named = results
            .iter()
            .filter(|m| m.is_series())
            .map(|m| (m, season_title_score(m, &parsed.title, season)))
            // A season's own title, not the first season's (which also
            // looks a lot like "Title 2").
            .filter(|(m, s)| *s >= MIN_SCORE && *s > m.titles.iter().map(|t| similarity(t, &parsed.title)).fold(0.0, f64::max))
            .max_by(|a, b| a.1.total_cmp(&b.1));
        match named {
            Some((m, _)) => media = m.clone(),
            None => {
                for _ in 1..season {
                    let Some(next) = media.next_season() else { return Ok(None) };
                    media = fetch(next)?;
                }
            }
        }
    }

    Ok(Some(media))
}

/// Episode `episode` counted from `media`: where anime-relations says it
/// belongs, or (counted past its end) in a later season. Ok(None): more
/// episodes than there are.
pub fn carry<E>(
    mut media: Media,
    mut episode: u32,
    relations: &Relations,
    mut fetch: impl FnMut(u32) -> Result<Media, E>,
) -> Result<Option<Resolved>, E> {
    if let Some((to, mapped)) = relations.redirect(media.id, episode) {
        if to != media.id {
            media = fetch(to)?;
        }
        episode = mapped;
    }
    for _ in 0..MAX_HOPS {
        match (media.episodes, media.next_season()) {
            (Some(count), Some(next)) if episode > count => {
                episode -= count;
                media = fetch(next)?;
            }
            _ => break,
        }
    }
    if media.episodes.is_some_and(|count| episode > count) {
        return Ok(None); // more episodes than the show has: not this show
    }
    Ok(Some(Resolved { media, episode }))
}

/// The best-scoring result for `title`, with its score.
fn best<'a>(results: &'a [Media], title: &str, year: Option<u32>) -> Option<(Media, f64)> {
    results
        .iter()
        .map(|m| {
            let mut score = m.titles.iter().map(|t| similarity(t, title)).fold(0.0, f64::max);
            // Prefer the series itself over its movies and specials.
            if !m.is_series() {
                score -= 0.15;
            }
            if year.is_some() && m.year == year {
                score += 0.1;
            }
            (m, score)
        })
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(m, s): (&'a Media, f64)| (m.clone(), s))
}

/// How well `m` matches `title` as its season `season`: a title of the
/// show plus a season marker ("2nd Season", "Season 2", "2", "II").
fn season_title_score(m: &Media, title: &str, season: u32) -> f64 {
    let ordinal = match season % 10 {
        1 if season % 100 != 11 => "st",
        2 if season % 100 != 12 => "nd",
        3 if season % 100 != 13 => "rd",
        _ => "th",
    };
    let roman = ["", "i", "ii", "iii", "iv", "v", "vi", "vii", "viii", "ix", "x"].get(season as usize).copied().unwrap_or("");
    let forms = [
        format!("{title} {season}{ordinal} season"),
        format!("{title} season {season}"),
        format!("{title} {season}"),
        format!("{title} {roman}"),
    ];
    m.titles.iter().flat_map(|t| forms.iter().map(move |f| similarity(t, f))).fold(0.0, f64::max)
}

/// Lowercase letters and digits, words separated by single spaces.
fn normalize(s: &str) -> String {
    let mut out = String::new();
    for word in s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()) {
        if !out.is_empty() {
            out.push(' ');
        }
        out.extend(word.chars().flat_map(char::to_lowercase));
    }
    out
}

/// 0 to 1: 1 for the same title (ignoring case and punctuation), else the
/// Sørensen–Dice coefficient of their letter pairs.
pub fn similarity(a: &str, b: &str) -> f64 {
    let (a, b) = (normalize(a), normalize(b));
    if a == b {
        return 1.0;
    }
    let pairs = |s: &str| -> Vec<(char, char)> {
        let chars: Vec<char> = s.chars().filter(|c| *c != ' ').collect();
        chars.windows(2).map(|w| (w[0], w[1])).collect()
    };
    let (pa, mut pb) = (pairs(&a), pairs(&b));
    if pa.is_empty() || pb.is_empty() {
        return 0.0;
    }
    let total = pa.len() + pb.len();
    let mut shared = 0;
    for p in &pa {
        if let Some(i) = pb.iter().position(|q| q == p) {
            pb.swap_remove(i);
            shared += 1;
        }
    }
    2.0 * shared as f64 / total as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn media(id: u32, format: &str, episodes: u32, titles: &[&str], sequel: Option<u32>) -> Media {
        Media {
            id,
            format: Some(format.into()),
            episodes: Some(episodes),
            year: None,
            titles: titles.iter().map(|t| t.to_string()).collect(),
            display: titles[0].into(),
            sequels: sequel.map(|s| (s, Some("TV".to_string()))).into_iter().collect(),
        }
    }

    /// A small AniList: Assassination Classroom (2 seasons + a movie) and
    /// Attack on Titan (seasons 1 to 3).
    fn library() -> HashMap<u32, Media> {
        [
            media(20755, "TV", 22, &["Ansatsu Kyoushitsu", "Assassination Classroom"], Some(21170)),
            media(21170, "TV", 25, &["Ansatsu Kyoushitsu 2nd Season", "Assassination Classroom Second Season"], None),
            media(101102, "MOVIE", 1, &["Ansatsu Kyoushitsu Movie: 365-Nichi no Jikan"], None),
            media(16498, "TV", 25, &["Shingeki no Kyojin", "Attack on Titan"], Some(20958)),
            media(20958, "TV", 12, &["Shingeki no Kyojin Season 2"], Some(99147)),
            media(99147, "TV", 12, &["Shingeki no Kyojin Season 3"], None),
        ]
        .into_iter()
        .map(|m| (m.id, m))
        .collect()
    }

    fn run(title: &str, season: Option<u32>, episode: u32, only_first_seasons: bool) -> Option<(u32, u32)> {
        let lib = library();
        let parsed = Parsed { title: title.into(), season, episode, year: None };
        let search = |q: &str| -> Result<Vec<Media>, ()> {
            let mut found: Vec<Media> = lib
                .values()
                .filter(|m| m.titles.iter().any(|t| similarity(t, q) > 0.5))
                .filter(|m| !only_first_seasons || [20755, 16498, 101102].contains(&m.id))
                .cloned()
                .collect();
            found.sort_by_key(|m| m.id);
            Ok(found)
        };
        let fetch = |id: u32| lib.get(&id).cloned().ok_or(());
        resolve(&parsed, &Relations::default(), search, fetch).unwrap().map(|r| (r.media.id, r.episode))
    }

    #[test]
    fn matches_titles_loosely() {
        assert_eq!(similarity("Steins;Gate", "steins gate"), 1.0);
        assert!(similarity("Shingeki no Kyojin", "Shingeki no Kyoujin") > 0.8, "romanization variants");
        assert!(similarity("Kaiji", "Shingeki no Kyojin") < 0.3);
    }

    #[test]
    fn finds_seasons_by_title_or_by_sequel() {
        assert_eq!(run("Ansatsu Kyoushitsu", None, 14, false), Some((20755, 14)));
        assert_eq!(run("Ansatsu Kyoushitsu", Some(2), 14, false), Some((21170, 14)), "titled 2nd Season");
        assert_eq!(run("Ansatsu Kyoushitsu", Some(2), 14, true), Some((21170, 14)), "via the sequel link");
        assert_eq!(run("Shingeki no Kyojin", Some(3), 5, true), Some((99147, 5)));
    }

    #[test]
    fn episodes_past_a_season_continue_into_the_next() {
        assert_eq!(run("Ansatsu Kyoushitsu", None, 30, false), Some((21170, 8)));
        assert_eq!(run("Shingeki no Kyojin", None, 40, false), Some((99147, 3)));
        assert_eq!(run("Ansatsu Kyoushitsu", None, 99, false), None, "more episodes than there are");
    }

    #[test]
    fn unknown_shows_dont_match() {
        assert_eq!(run("Kaiji Ultimate Survivor", None, 5, false), None);
    }
}
