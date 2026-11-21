//! AniList's GraphQL API (https://docs.anilist.co): searching shows, and
//! reading and updating the signed-in user's list. Blocking: called from
//! the tracking worker thread, never the player's.

use super::matcher::Media;
use serde_json::{Value, json};
use std::time::Duration;

const URL: &str = "https://graphql.anilist.co";

/// Where people get a token to paste into the settings: AniList shows it
/// after they approve mpvlink ("implicit grant", redirect to its PIN page).
pub fn authorize_url(client_id: &str) -> String {
    format!("https://anilist.co/api/v2/oauth/authorize?client_id={client_id}&response_type=token")
}

/// Why a request failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApiError {
    /// Worth trying again later: no connection, AniList busy or down.
    Temporary(String),
    /// The sign-in no longer works (expired or revoked): sign in again.
    SignIn,
    /// AniList refused the request itself.
    Rejected(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::Temporary(m) | Self::Rejected(m) => f.write_str(m),
            Self::SignIn => f.write_str("the AniList sign-in has expired or was revoked; sign in again in Settings"),
        }
    }
}

type Result<T> = std::result::Result<T, ApiError>;

/// A list entry's status (AniList's MediaListStatus).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Current,
    Planning,
    Completed,
    Dropped,
    Paused,
    Repeating,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Self::Current => "CURRENT",
            Self::Planning => "PLANNING",
            Self::Completed => "COMPLETED",
            Self::Dropped => "DROPPED",
            Self::Paused => "PAUSED",
            Self::Repeating => "REPEATING",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "CURRENT" => Self::Current,
            "PLANNING" => Self::Planning,
            "COMPLETED" => Self::Completed,
            "DROPPED" => Self::Dropped,
            "PAUSED" => Self::Paused,
            "REPEATING" => Self::Repeating,
            _ => return None,
        })
    }
}

/// A show on the user's list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub id: u32,
    pub status: Status,
    pub progress: u32,
}

pub struct Client {
    agent: ureq::Agent,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

const MEDIA_FIELDS: &str = "id format episodes seasonYear title { romaji english native } synonyms \
    relations { edges { relationType node { id type format } } }";

impl Client {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(8)))
            .http_status_as_error(false)
            .user_agent("mpvlink")
            .build();
        Self { agent: config.into() }
    }

    /// The HTTP agent (for other downloads with the same settings).
    pub fn agent(&self) -> &ureq::Agent {
        &self.agent
    }

    /// Run a query; its `data`, or a message for the user.
    fn query(&self, token: Option<&str>, query: &str, variables: Value) -> Result<Value> {
        let body = json!({ "query": query, "variables": variables }).to_string();
        let mut request = self.agent.post(URL).header("Content-Type", "application/json").header("Accept", "application/json");
        if let Some(token) = token {
            request = request.header("Authorization", &format!("Bearer {token}"));
        }
        let temporary = |m: String| ApiError::Temporary(m);
        let mut response = request.send(&body).map_err(|e| temporary(format!("couldn't reach AniList ({e})")))?;
        let status = response.status().as_u16();
        let text = response.body_mut().read_to_string().map_err(|e| temporary(format!("AniList's reply was cut off ({e})")))?;
        if status == 429 {
            return Err(temporary("AniList is busy (too many requests)".into()));
        }
        if status >= 500 {
            return Err(temporary(format!("AniList is having trouble (HTTP {status})")));
        }
        let reply: Value = serde_json::from_str(&text).map_err(|_| temporary(format!("unexpected reply from AniList (HTTP {status})")))?;
        if let Some(message) = reply["errors"][0]["message"].as_str() {
            return Err(match message {
                "Invalid token" | "Unauthorized." => ApiError::SignIn,
                _ if status == 401 => ApiError::SignIn,
                _ => ApiError::Rejected(format!("AniList: {message}")),
            });
        }
        Ok(reply["data"].clone())
    }

    /// Shows whose titles match `title`.
    pub fn search(&self, title: &str) -> Result<Vec<Media>> {
        let q = format!("query($s: String) {{ Page(perPage: 10) {{ media(search: $s, type: ANIME) {{ {MEDIA_FIELDS} }} }} }}");
        let data = self.query(None, &q, json!({ "s": title }))?;
        Ok(data["Page"]["media"].as_array().map(|a| a.iter().filter_map(media).collect()).unwrap_or_default())
    }

    /// One show.
    pub fn fetch(&self, id: u32) -> Result<Media> {
        let q = format!("query($id: Int) {{ Media(id: $id, type: ANIME) {{ {MEDIA_FIELDS} }} }}");
        let data = self.query(None, &q, json!({ "id": id }))?;
        media(&data["Media"]).ok_or_else(|| ApiError::Rejected(format!("AniList has no show {id}")))
    }

    /// The signed-in user's name (checks the token).
    pub fn viewer(&self, token: &str) -> Result<String> {
        let data = self.query(Some(token), "query { Viewer { name } }", json!({}))?;
        data["Viewer"]["name"].as_str().map(str::to_string).ok_or(ApiError::SignIn)
    }

    /// The user's entry for a show, if it's on their list.
    pub fn entry(&self, token: &str, media_id: u32) -> Result<Option<Entry>> {
        let q = "query($id: Int) { Media(id: $id) { mediaListEntry { id status progress } } }";
        let data = self.query(Some(token), q, json!({ "id": media_id }))?;
        Ok(entry(&data["Media"]["mediaListEntry"]))
    }

    /// Set a show's status and progress (adding it to the list if needed).
    pub fn save(&self, token: &str, media_id: u32, status: Status, progress: u32) -> Result<Entry> {
        let q = "mutation($id: Int, $status: MediaListStatus, $progress: Int) { \
            SaveMediaListEntry(mediaId: $id, status: $status, progress: $progress) { id status progress } }";
        let data = self.query(Some(token), q, json!({ "id": media_id, "status": status.as_str(), "progress": progress }))?;
        entry(&data["SaveMediaListEntry"]).ok_or_else(|| ApiError::Temporary("AniList didn't confirm the update".into()))
    }

    /// Remove an entry from the list (undoing one we added).
    pub fn delete(&self, token: &str, entry_id: u32) -> Result<()> {
        let q = "mutation($id: Int) { DeleteMediaListEntry(id: $id) { deleted } }";
        self.query(Some(token), q, json!({ "id": entry_id })).map(|_| ())
    }
}

fn number(v: &Value) -> Option<u32> {
    v.as_u64().and_then(|n| u32::try_from(n).ok())
}

fn entry(v: &Value) -> Option<Entry> {
    Some(Entry { id: number(&v["id"])?, status: Status::parse(v["status"].as_str()?)?, progress: number(&v["progress"]).unwrap_or(0) })
}

fn media(v: &Value) -> Option<Media> {
    let title = &v["title"];
    let mut titles: Vec<String> = ["romaji", "english", "native"].iter().filter_map(|k| title[*k].as_str().map(str::to_string)).collect();
    titles.extend(v["synonyms"].as_array().into_iter().flatten().filter_map(|s| s.as_str().map(str::to_string)));
    let display = title["english"].as_str().or(title["romaji"].as_str())?.to_string();
    let sequels = v["relations"]["edges"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["relationType"] == "SEQUEL" && e["node"]["type"] == "ANIME")
        .filter_map(|e| Some((number(&e["node"]["id"])?, e["node"]["format"].as_str().map(str::to_string))))
        .collect();
    Some(Media {
        id: number(&v["id"])?,
        format: v["format"].as_str().map(str::to_string),
        episodes: number(&v["episodes"]),
        year: number(&v["seasonYear"]),
        titles,
        display,
        sequels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_media_and_entries() {
        let v = json!({
            "id": 21170, "format": "TV", "episodes": 25, "seasonYear": 2016,
            "title": { "romaji": "Ansatsu Kyoushitsu 2nd Season", "english": "Assassination Classroom Second Season", "native": null },
            "synonyms": ["Klasa skrytobójców 2"],
            "relations": { "edges": [
                { "relationType": "PREQUEL", "node": { "id": 20755, "type": "ANIME", "format": "TV" } },
                { "relationType": "SEQUEL", "node": { "id": 5, "type": "MANGA", "format": "MANGA" } },
                { "relationType": "SEQUEL", "node": { "id": 6, "type": "ANIME", "format": "MOVIE" } }
            ] }
        });
        let m = media(&v).unwrap();
        assert_eq!(m.display, "Assassination Classroom Second Season");
        assert_eq!(m.titles.len(), 3);
        assert_eq!(m.sequels, vec![(6, Some("MOVIE".into()))], "anime sequels only");
        assert_eq!(entry(&json!({ "id": 9, "status": "CURRENT", "progress": 3 })), Some(Entry { id: 9, status: Status::Current, progress: 3 }));
        assert_eq!(entry(&Value::Null), None);
    }
}
