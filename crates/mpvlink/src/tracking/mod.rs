//! Keeping an AniList list up to date with what's watched, like Taiga:
//! recognize the episode from the file name, follow how much of it plays,
//! and once most of it has, set the show's progress on the list. Each
//! update can be undone for a few seconds; updates that can't be sent (no
//! connection, AniList busy) wait in a queue and are retried.
//!
//! Works the same for your own files and for a stream you watch in a room
//! (the streamer tells viewers the file name and playback position, so
//! everyone's list is updated, each with their own account). A wrong match
//! can be changed (see [`Tracker::search`] and [`Tracker::choose`]).
//!
//! Network work happens on a worker thread; the plugin feeds the
//! [`Tracker`] playback events and polls it for messages to show.

pub mod anilist;
pub mod matcher;
pub mod recognize;
pub mod relations;
pub mod store;

use anilist::{ApiError, Client, Entry, Status};
use matcher::{Media, Resolved};
use relations::Relations;
use mpvlink_rtc::proto::AnimeEpisode;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};
use store::{Account, DONT_TRACK, Matches, Paths, Queue, Queued};

/// How much of an episode must have played for it to count: playback past
/// this share of it, and at least this share actually watched (not skipped).
const REACHED: f64 = 0.8;
const WATCHED: f64 = 0.5;
/// How long an update can be undone.
pub const UNDO_FOR: Duration = Duration::from_secs(10);
/// A position jump bigger than the time passed (plus this) is a seek, not playback.
const SEEK_SLACK: f64 = 2.0;
/// How often queued updates are tried again, and how soon after starting.
const RETRY_EVERY: Duration = Duration::from_secs(120);
const RETRY_AT_START: Duration = Duration::from_secs(5);
/// The key that opens the match picker (bound by the plugin).
pub const PICK_KEY: &str = "Ctrl+A";

/// The episode being followed.
struct Current {
    /// Identifies this episode's jobs (a newer file makes older answers stale).
    key: u64,
    file: String,
    duration: f64,
    anime: Option<AnimeEpisode>,
    /// Episodes in its season, when known.
    episodes: Option<u32>,
    /// The user picked the match (or "don't track"): the streamer's match
    /// doesn't replace it.
    chosen: bool,
    /// Playback seen so far: the last position and when, and how many
    /// seconds have actually played.
    last: Option<(Instant, f64)>,
    watched: f64,
    /// The list update was requested, and how it went.
    counted: bool,
    outcome: Option<ListState>,
    /// Lookup finished without a match (or the title isn't tracked).
    unmatched: bool,
    /// The latest playback position, seconds.
    position: f64,
}

/// Where the list update for what's playing stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListState {
    /// Not signed in: nothing is tracked.
    SignedOut,
    /// Looking the show up.
    Matching,
    /// No show matched (Ctrl+A to pick one), or the title isn't tracked.
    Unmatched,
    /// Waiting for enough to play.
    Counting,
    /// The update is being sent.
    Sending,
    /// The list was updated.
    Updated,
    /// The list already had it (or further).
    UpToDate,
    /// Couldn't be sent yet; it's in the retry queue.
    Queued,
    /// AniList refused it.
    Failed,
}

/// What's playing, as the statistics panel shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct Tracking {
    /// The show (or the file's name, when there's no match).
    pub title: String,
    pub episode: Option<u32>,
    pub episodes: Option<u32>,
    /// How far playback is, and how much has actually played (0 to 1).
    pub position: f64,
    pub watched: f64,
    pub state: ListState,
}

/// An update that can still be undone.
struct Undo {
    media_id: u32,
    before: Option<Entry>,
    after: Entry,
    title: String,
    until: Instant,
}

/// A show offered in the match picker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub id: u32,
    pub title: String,
    /// Format, year and episodes, e.g. "TV · 2016 · 25 episodes".
    pub detail: String,
}

enum Job {
    Resolve { key: u64, file: String },
    /// `queued`: a retry from the queue (no undo, quieter messages).
    Update { key: u64, token: String, item: Queued, queued: bool },
    Undo { token: String, media_id: u32, before: Option<Entry>, after: Entry, title: String },
    SignIn { token: String },
    Search { query: String },
    /// The user's match for `file` (None: don't track it).
    Choose { key: u64, file: String, media_id: Option<u32> },
}

enum Outcome {
    /// `title`: what the file name was read as, when it named an episode.
    Resolved { key: u64, found: Option<(AnimeEpisode, Option<u32>)>, title: Option<String>, ignored: bool },
    Updated { key: u64, item: Queued, before: Option<Entry>, after: Entry, queued: bool },
    Unchanged { key: u64, item: Queued, reason: String },
    UpdateFailed { key: u64, item: Queued, error: ApiError, queued: bool },
    Undone { title: String },
    SignedIn { token: String, user: String },
    Results { query: String, results: Vec<Choice> },
    Chosen { key: u64, found: Option<(AnimeEpisode, Option<u32>)> },
    Failed(String),
}

/// What the plugin should tell the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    /// A message to show.
    Message(String),
    /// An update that Ctrl+Z can undo for [`UNDO_FOR`]: show it, and enable the key.
    Undoable(String),
    /// The undo window closed: disable the key.
    UndoExpired,
    /// Signed in: the account changed.
    SignedIn(String),
    /// Search results for the match picker.
    Results { query: String, results: Vec<Choice> },
}

/// What the match picker needs to know about what's playing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Playing {
    /// The file's name (without folders).
    pub file: String,
    /// The title read from the name, if it names an episode.
    pub title: Option<String>,
    /// The current match.
    pub anime: Option<AnimeEpisode>,
}

pub struct Tracker {
    jobs: Sender<Job>,
    outcomes: Receiver<Outcome>,
    account: Account,
    paths: Paths,
    current: Option<Current>,
    undo: Option<Undo>,
    next_key: u64,
    /// Jobs sent and not yet answered.
    outstanding: usize,
    /// Notices raised outside `poll` (a match from the streamer), shown at the next one.
    pending: Vec<Notice>,
    /// Updates waiting to be sent, and when to try again.
    queue: Queue,
    next_retry: Option<Instant>,
}

impl Tracker {
    /// Start the worker. `paths`: where the account, matches and queue are kept.
    pub fn new(paths: Paths) -> Self {
        let (jobs, job_rx) = channel();
        let (outcome_tx, outcomes) = channel();
        let (matches_path, relations_path) = (paths.matches.clone(), paths.relations.clone());
        std::thread::Builder::new()
            .name("mpvlink-anilist".into())
            .spawn(move || work(job_rx, outcome_tx, matches_path, relations_path))
            .expect("spawning the AniList worker");
        let account = paths.account.as_deref().map(Account::load).unwrap_or_default();
        let queue = paths.queue.as_deref().map(Queue::load).unwrap_or_default();
        let next_retry = (!queue.0.is_empty()).then(|| Instant::now() + RETRY_AT_START);
        Self {
            jobs,
            outcomes,
            account,
            paths,
            current: None,
            undo: None,
            next_key: 0,
            outstanding: 0,
            pending: Vec::new(),
            queue,
            next_retry,
        }
    }

    pub fn signed_in(&self) -> bool {
        !self.account.token.is_empty()
    }

    /// The AniList user name ("" if not signed in or not checked yet).
    pub fn user(&self) -> &str {
        &self.account.user
    }

    pub fn token(&self) -> &str {
        &self.account.token
    }

    fn send(&mut self, job: Job) {
        if self.jobs.send(job).is_ok() {
            self.outstanding += 1;
        }
    }

    /// Check a pasted token (the answer comes as [`Notice::SignedIn`]), or
    /// sign out with "".
    pub fn sign_in(&mut self, token: &str) {
        let token = token.trim();
        if token.is_empty() {
            self.account = Account::default();
            self.save_account();
            return;
        }
        self.send(Job::SignIn { token: token.to_string() });
    }

    fn save_account(&self) {
        if let Some(path) = &self.paths.account
            && let Err(e) = self.account.save(path)
        {
            eprintln!("[mpvlink] saving the AniList sign-in failed: {e}");
        }
    }

    fn save_queue(&self) {
        if let Some(path) = &self.paths.queue
            && let Err(e) = self.queue.save(path)
        {
            eprintln!("[mpvlink] saving the AniList queue failed: {e}");
        }
    }

    /// A new file started playing (ours, or the stream we watch): follow it.
    /// `anime` is the streamer's match for it, if it sent one.
    pub fn begin(&mut self, file: &str, duration: f64, anime: Option<AnimeEpisode>) {
        self.next_key += 1;
        let key = self.next_key;
        let have_match = anime.is_some();
        if let Some(a) = anime.as_ref().filter(|_| self.signed_in()) {
            self.pending.push(recognized(a, a.episodes));
        }
        let episodes = anime.as_ref().and_then(|a| a.episodes);
        self.current = Some(Current {
            key,
            file: file.to_string(),
            duration,
            anime,
            episodes,
            chosen: false,
            last: None,
            watched: 0.0,
            counted: false,
            outcome: None,
            unmatched: false,
            position: 0.0,
        });
        // Nothing is looked up (nothing leaves this computer) unless signed
        // in, and only for names that are episodes.
        if self.signed_in() && !have_match && recognize::recognize(file).is_some() {
            self.send(Job::Resolve { key, file: file.to_string() });
        }
    }

    /// The file being followed, if any.
    pub fn file(&self) -> Option<&str> {
        self.current.as_ref().map(|c| c.file.as_str())
    }

    /// What's playing, for the match picker.
    pub fn playing(&self) -> Option<Playing> {
        let c = self.current.as_ref()?;
        let name = c.file.rsplit(['/', '\\']).next().unwrap_or(&c.file).to_string();
        Some(Playing { file: name, title: recognize::recognize(&c.file).map(|p| p.title), anime: c.anime.clone() })
    }

    /// The streamer's match arrived after the file did.
    pub fn hint(&mut self, anime: AnimeEpisode) {
        let signed_in = self.signed_in();
        if let Some(c) = self.current.as_mut()
            && !c.chosen
            && c.anime.as_ref() != Some(&anime)
        {
            c.episodes = anime.episodes.or(c.episodes);
            if signed_in && c.anime.is_none() {
                self.pending.push(recognized(&anime, c.episodes));
            }
            c.anime = Some(anime);
        }
    }

    /// The duration became known (or changed).
    pub fn set_duration(&mut self, duration: f64) {
        if let Some(c) = self.current.as_mut() {
            c.duration = duration;
        }
    }

    /// Nothing is playing any more.
    pub fn end(&mut self) {
        self.current = None;
    }

    /// What's playing and where its list update stands (None: nothing is).
    pub fn status(&self) -> Option<Tracking> {
        let c = self.current.as_ref()?;
        let share = |x: f64| if c.duration > 0.0 { (x / c.duration).clamp(0.0, 1.0) } else { 0.0 };
        let state = match (&c.outcome, &c.anime) {
            _ if !self.signed_in() => ListState::SignedOut,
            (Some(state), _) => *state,
            (None, Some(_)) if c.counted => ListState::Sending,
            (None, Some(_)) => ListState::Counting,
            (None, None) if c.unmatched || recognize::recognize(&c.file).is_none() => ListState::Unmatched,
            (None, None) => ListState::Matching,
        };
        let name = c.file.rsplit(['/', '\\']).next().unwrap_or(&c.file);
        Some(Tracking {
            title: c.anime.as_ref().map_or_else(|| name.to_string(), |a| a.title.clone()),
            episode: c.anime.as_ref().map(|a| a.episode),
            episodes: c.episodes,
            position: share(c.position),
            watched: share(c.watched),
            state,
        })
    }

    /// The match for what's playing, to tell viewers.
    pub fn anime(&self) -> Option<AnimeEpisode> {
        self.current.as_ref().and_then(|c| c.anime.clone())
    }

    /// Playback is at `position` seconds (paused or not): count what's
    /// played since the last call, and update the list once enough has.
    pub fn progress(&mut self, position: f64, paused: bool) {
        let now = Instant::now();
        let Some(c) = self.current.as_mut() else { return };
        if let Some((then, last)) = c.last
            && !paused
        {
            let advanced = position - last;
            let elapsed = now.duration_since(then).as_secs_f64();
            // Forward, and no further than real time allows: playback, not a seek.
            if advanced > 0.0 && advanced <= elapsed * 1.5 + SEEK_SLACK {
                c.watched += advanced;
            }
        }
        c.last = Some((now, position));
        c.position = position;
        if c.counted || c.duration <= 0.0 || position < c.duration * REACHED || c.watched < c.duration * WATCHED {
            return;
        }
        let (Some(anime), true) = (c.anime.clone(), !self.account.token.is_empty()) else { return };
        c.counted = true;
        let (key, episodes) = (c.key, c.episodes);
        let item = Queued { media_id: anime.media_id, episode: anime.episode, title: anime.title, episodes };
        let token = self.account.token.clone();
        self.send(Job::Update { key, token, item, queued: false });
    }

    /// Undo the last update, if it's still undoable.
    pub fn undo(&mut self) {
        let Some(u) = self.undo.take() else { return };
        if Instant::now() > u.until {
            return;
        }
        let token = self.account.token.clone();
        self.send(Job::Undo { token, media_id: u.media_id, before: u.before, after: u.after, title: u.title });
    }

    /// Search AniList for the match picker (answered with [`Notice::Results`]).
    pub fn search(&mut self, query: &str) {
        let query = query.trim();
        if !query.is_empty() && self.signed_in() {
            self.send(Job::Search { query: query.to_string() });
        }
    }

    /// The user's match for what's playing: a show, or None for "don't
    /// track this". Remembered for the title, like a found match.
    pub fn choose(&mut self, media_id: Option<u32>) {
        let Some(c) = self.current.as_mut() else { return };
        c.chosen = true;
        if media_id.is_none() {
            c.anime = None;
        }
        let (key, file) = (c.key, c.file.clone());
        self.send(Job::Choose { key, file, media_id });
    }

    /// Send what's waiting in the queue.
    fn retry(&mut self) {
        self.next_retry = None;
        if !self.signed_in() {
            return; // until someone signs in
        }
        let token = self.account.token.clone();
        for item in std::mem::take(&mut self.queue.0) {
            self.send(Job::Update { key: 0, token: token.clone(), item, queued: true });
        }
        self.save_queue();
    }

    /// When the plugin should next call [`poll`](Self::poll): soon while
    /// answers are due, at the end of an undo window or for a retry, else never.
    pub fn poll_in(&self) -> Option<Duration> {
        let now = Instant::now();
        let undo = self.undo.as_ref().map(|u| u.until.saturating_duration_since(now));
        let retry = self.next_retry.map(|t| t.saturating_duration_since(now));
        let answers = (self.outstanding > 0 || !self.pending.is_empty()).then_some(Duration::from_millis(100));
        [undo, retry, answers].into_iter().flatten().min()
    }

    /// Take the worker's answers; what to tell the user.
    pub fn poll(&mut self) -> Vec<Notice> {
        let mut notices = std::mem::take(&mut self.pending);
        let now = Instant::now();
        if self.undo.as_ref().is_some_and(|u| now > u.until) {
            self.undo = None;
            notices.push(Notice::UndoExpired);
        }
        if self.next_retry.is_some_and(|t| now >= t) {
            self.retry();
        }
        while let Ok(outcome) = self.outcomes.try_recv() {
            self.outstanding = self.outstanding.saturating_sub(1);
            self.handle(outcome, &mut notices);
        }
        notices
    }

    fn handle(&mut self, outcome: Outcome, notices: &mut Vec<Notice>) {
        match outcome {
            Outcome::Resolved { key, found, title, ignored } => {
                let Some(c) = self.current.as_mut().filter(|c| c.key == key && !c.chosen) else { return };
                match (found, title) {
                    (Some((anime, episodes)), _) => {
                        notices.push(recognized(&anime, episodes));
                        c.anime = Some(anime);
                        c.episodes = episodes;
                    }
                    (None, Some(title)) if !ignored => {
                        c.unmatched = true;
                        notices.push(Notice::Message(format!("AniList: couldn't find \"{title}\" · {PICK_KEY} to pick the show")));
                    }
                    _ => c.unmatched = true,
                }
            }
            Outcome::Chosen { key, found } => {
                let Some(c) = self.current.as_mut().filter(|c| c.key == key) else { return };
                match found {
                    Some((anime, episodes)) => {
                        notices.push(Notice::Message(format!("AniList: now matched to {} · episode {}", anime.title, anime.episode)));
                        c.anime = Some(anime);
                        c.episodes = episodes;
                    }
                    None => {
                        c.unmatched = true;
                        notices.push(Notice::Message("AniList: this show won't be tracked".into()));
                    }
                }
            }
            Outcome::Updated { key, item, before, after, queued } => {
                if let Some(c) = self.current.as_mut().filter(|c| c.key == key) {
                    c.episodes = item.episodes;
                    c.outcome = Some(ListState::Updated);
                }
                let of = item.episodes.map_or(String::new(), |n| format!(" of {n}"));
                let done = if after.status == Status::Completed { " (completed)" } else { "" };
                let text = format!("AniList: {} → episode {}{of}{done}", item.title, after.progress);
                if queued {
                    notices.push(Notice::Message(format!("{text} (sent now)")));
                } else {
                    notices.push(Notice::Undoable(format!("{text} · Ctrl+Z to undo")));
                    self.undo = Some(Undo { media_id: item.media_id, before, after, title: item.title, until: Instant::now() + UNDO_FOR });
                }
            }
            Outcome::Unchanged { key, item, reason } => {
                if let Some(c) = self.current.as_mut().filter(|c| c.key == key) {
                    c.outcome = Some(ListState::UpToDate);
                }
                eprintln!("[mpvlink] AniList: {} not updated: {reason}", item.title);
            }
            Outcome::UpdateFailed { key, item, error, queued } => {
                let retry = matches!(error, ApiError::Temporary(_) | ApiError::SignIn);
                if let Some(c) = self.current.as_mut().filter(|c| c.key == key) {
                    c.outcome = Some(if retry { ListState::Queued } else { ListState::Failed });
                }
                // Retries fail quietly; a first failure says what happens next.
                if !queued || !retry {
                    let next = if retry { "; will try again" } else { "" };
                    notices.push(Notice::Message(format!("AniList: couldn't update {} ({error}){next}", item.title)));
                }
                if retry {
                    self.queue.add(item);
                    self.save_queue();
                    if matches!(error, ApiError::Temporary(_)) {
                        self.next_retry.get_or_insert(Instant::now() + RETRY_EVERY);
                    }
                }
            }
            Outcome::Undone { title } => {
                notices.push(Notice::UndoExpired);
                notices.push(Notice::Message(format!("AniList: undid the update to {title}")));
            }
            Outcome::SignedIn { token, user } => {
                self.account = Account { token, user: user.clone() };
                self.save_account();
                notices.push(Notice::SignedIn(user));
                if !self.queue.0.is_empty() {
                    self.retry();
                }
            }
            Outcome::Results { query, results } => notices.push(Notice::Results { query, results }),
            Outcome::Failed(message) => notices.push(Notice::Message(format!("AniList: {message}"))),
        }
    }
}

/// "AniList: Show · episode 3 of 12" (also logged).
fn recognized(anime: &AnimeEpisode, episodes: Option<u32>) -> Notice {
    let of = episodes.map_or(String::new(), |n| format!(" of {n}"));
    let text = format!("AniList: {} · episode {}{of}", anime.title, anime.episode);
    eprintln!("[mpvlink] {text} (id {})", anime.media_id);
    Notice::Message(format!("{text} · {PICK_KEY} to change"))
}

/// What to set an entry to after watching `episode`, given what it was
/// (`before`) and how many episodes there are. None: leave it alone.
pub fn plan_update(before: Option<Entry>, episode: u32, episodes: Option<u32>) -> Option<(Status, u32)> {
    let last = episodes.is_some_and(|n| episode >= n);
    let watching = if last { Status::Completed } else { Status::Current };
    match before {
        // Finished already: a rewatch, which the list doesn't track here.
        Some(Entry { status: Status::Completed, .. }) => None,
        Some(e) if e.status == Status::Repeating => {
            (episode > e.progress).then_some((if last { Status::Completed } else { Status::Repeating }, episode))
        }
        // On the list and further along already (a rewatched episode).
        Some(e) if e.status == Status::Current && episode <= e.progress => None,
        // Not on the list, planned, paused or dropped: now watching.
        _ => Some((watching, episode)),
    }
}

/// The worker thread: answer jobs one at a time until the tracker is dropped.
fn work(jobs: Receiver<Job>, outcomes: Sender<Outcome>, matches_path: Option<PathBuf>, relations_path: Option<PathBuf>) {
    let client = Client::new();
    let mut matches = matches_path.as_deref().map(Matches::load).unwrap_or_default();
    let mut relations = Relations::load(relations_path.as_deref());
    // Refreshed when first needed (a lookup), not at startup: nothing is
    // fetched for someone not using tracking.
    let mut refreshed = false;
    let save = |matches: &Matches| {
        if let Some(path) = &matches_path {
            let _ = matches.save(path);
        }
    };
    while let Ok(job) = jobs.recv() {
        let outcome = match job {
            Job::Resolve { key, file } => {
                if !refreshed {
                    refreshed = true;
                    if let Some(path) = relations_path.as_deref().filter(|p| relations::stale(p)) {
                        match relations::download(client.agent(), path) {
                            Ok(fresh) => relations = fresh,
                            Err(e) => eprintln!("[mpvlink] {e}; using the copy we have"),
                        }
                    }
                }
                let title = recognize::recognize(&file).map(|p| p.title);
                let ignored = title.as_ref().is_some_and(|_| {
                    recognize::recognize(&file).is_some_and(|p| matches.0.get(&Matches::key(&p.title, p.season)) == Some(&DONT_TRACK))
                });
                match resolve(&client, &mut matches, &relations, &file) {
                    Ok(found) => {
                        save(&matches);
                        Outcome::Resolved { key, found, title, ignored }
                    }
                    Err(e) => {
                        eprintln!("[mpvlink] AniList lookup for {file} failed: {e}");
                        Outcome::Resolved { key, found: None, title: None, ignored }
                    }
                }
            }
            Job::Update { key, token, item, queued } => update(&client, &token, key, item, queued),
            Job::Undo { token, media_id, before, after, title } => {
                let result = match before {
                    Some(b) => client.save(&token, media_id, b.status, b.progress).map(|_| ()),
                    None => client.delete(&token, after.id),
                };
                match result {
                    Ok(()) => Outcome::Undone { title },
                    Err(e) => Outcome::Failed(format!("couldn't undo: {e}")),
                }
            }
            Job::SignIn { token } => match client.viewer(&token) {
                Ok(user) => Outcome::SignedIn { token, user },
                Err(e) => Outcome::Failed(format!("signing in failed: {e}")),
            },
            Job::Search { query } => match client.search(&query) {
                Ok(found) => Outcome::Results { query, results: found.iter().map(choice).collect() },
                Err(e) => Outcome::Failed(format!("searching failed: {e}")),
            },
            Job::Choose { key, file, media_id } => match recognize::recognize(&file) {
                None => Outcome::Failed("this file's name has no episode number, so it can't be tracked".into()),
                Some(parsed) => {
                    matches.0.insert(Matches::key(&parsed.title, parsed.season), media_id.unwrap_or(DONT_TRACK));
                    save(&matches);
                    match media_id {
                        None => Outcome::Chosen { key, found: None },
                        Some(id) => match client.fetch(id).and_then(|m| matcher::carry(m, parsed.episode, &relations, |id| client.fetch(id))) {
                            Ok(found) => Outcome::Chosen { key, found: found.map(episode_of) },
                            Err(e) => Outcome::Failed(format!("couldn't load that show: {e}")),
                        },
                    }
                }
            },
        };
        if outcomes.send(outcome).is_err() {
            break;
        }
    }
}

/// How a search result is offered in the picker.
fn choice(m: &Media) -> Choice {
    let format = m.format.as_deref().map(|f| match f {
        "TV_SHORT" => "TV short".to_string(),
        "MOVIE" => "Movie".to_string(),
        "SPECIAL" => "Special".to_string(),
        "MUSIC" => "Music".to_string(),
        other => other.to_string(),
    });
    let episodes = m.episodes.map(|n| if n == 1 { "1 episode".to_string() } else { format!("{n} episodes") });
    let detail = [format, m.year.map(|y| y.to_string()), episodes].into_iter().flatten().collect::<Vec<_>>().join(" · ");
    Choice { id: m.id, title: m.display.clone(), detail }
}

fn episode_of(Resolved { media: Media { id, display, episodes, .. }, episode }: Resolved) -> (AnimeEpisode, Option<u32>) {
    (AnimeEpisode { media_id: id, episode, title: display, episodes }, episodes)
}

/// Find the show and episode `file` is: from the matches cache, else by
/// searching AniList (and then remembering it).
fn resolve(
    client: &Client,
    matches: &mut Matches,
    relations: &Relations,
    file: &str,
) -> Result<Option<(AnimeEpisode, Option<u32>)>, ApiError> {
    let Some(parsed) = recognize::recognize(file) else { return Ok(None) };
    let key = Matches::key(&parsed.title, parsed.season);
    let resolved = match matches.0.get(&key) {
        Some(&DONT_TRACK) => None,
        // Known: that entry, with the episode carried into later seasons
        // if it's past this one's end.
        Some(&id) => matcher::carry(client.fetch(id)?, parsed.episode, relations, |id| client.fetch(id))?,
        None => {
            // Found by search: remember the title's own entry (before any
            // episode carrying), then carry.
            match matcher::find(&parsed, |q| client.search(q), |id| client.fetch(id))? {
                Some(media) => {
                    matches.0.insert(key, media.id);
                    matcher::carry(media, parsed.episode, relations, |id| client.fetch(id))?
                }
                None => None,
            }
        }
    };
    Ok(resolved.map(episode_of))
}

fn update(client: &Client, token: &str, key: u64, mut item: Queued, queued: bool) -> Outcome {
    // The episode count, if the match didn't come with one.
    if item.episodes.is_none() {
        item.episodes = client.fetch(item.media_id).ok().and_then(|m| m.episodes);
    }
    let before = match client.entry(token, item.media_id) {
        Ok(e) => e,
        Err(error) => return Outcome::UpdateFailed { key, item, error, queued },
    };
    let Some((status, progress)) = plan_update(before, item.episode, item.episodes) else {
        let reason = before.map_or("no change needed".into(), |b| format!("already {:?} at episode {}", b.status, b.progress));
        return Outcome::Unchanged { key, item, reason };
    };
    match client.save(token, item.media_id, status, progress) {
        Ok(after) => Outcome::Updated { key, item, before, after, queued },
        Err(error) => Outcome::UpdateFailed { key, item, error, queued },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(status: Status, progress: u32) -> Option<Entry> {
        Some(Entry { id: 1, status, progress })
    }

    #[test]
    fn plans_updates_like_taiga() {
        assert_eq!(plan_update(None, 1, Some(12)), Some((Status::Current, 1)), "added when first watched");
        assert_eq!(plan_update(entry(Status::Planning, 0), 3, Some(12)), Some((Status::Current, 3)));
        assert_eq!(plan_update(entry(Status::Current, 4), 5, Some(12)), Some((Status::Current, 5)));
        assert_eq!(plan_update(entry(Status::Current, 4), 12, Some(12)), Some((Status::Completed, 12)), "last episode");
        assert_eq!(plan_update(entry(Status::Current, 6), 5, Some(12)), None, "never backwards");
        assert_eq!(plan_update(entry(Status::Completed, 12), 3, Some(12)), None, "rewatching isn't tracked");
        assert_eq!(plan_update(entry(Status::Dropped, 2), 3, None), Some((Status::Current, 3)), "picked back up");
        assert_eq!(plan_update(entry(Status::Repeating, 2), 3, Some(12)), Some((Status::Repeating, 3)));
        assert_eq!(plan_update(None, 30, None), Some((Status::Current, 30)), "unknown length: never completes");
    }

    fn tracker() -> Tracker {
        let mut t = Tracker::new(Paths::default());
        t.account = Account { token: "t".into(), user: "Ana".into() };
        t
    }

    #[test]
    fn counts_only_what_actually_plays() {
        let mut t = tracker();
        let anime = AnimeEpisode { media_id: 5, episode: 2, title: "Show".into(), episodes: None };
        t.begin("Show - 02.mkv", 100.0, Some(anime));
        // Seek straight to 90%: past the mark, but nothing watched.
        t.progress(0.0, false);
        t.progress(90.0, false);
        assert!(!t.current.as_ref().unwrap().counted);
        assert_eq!(t.current.as_ref().unwrap().watched, 0.0);
        // Watch normally: counts once watched >= 50% and position >= 80%.
        let c = t.current.as_mut().unwrap();
        c.watched = 49.0;
        c.last = Some((Instant::now() - Duration::from_secs(2), 90.0));
        t.progress(92.0, false);
        assert!(t.current.as_ref().unwrap().counted, "51 s watched, at 92%");
        assert_eq!(t.outstanding, 1, "one update job sent");
    }

    #[test]
    fn not_signed_in_means_nothing_is_sent() {
        let mut t = Tracker::new(Paths::default());
        t.begin("[Group] Show - 05.mkv", 1400.0, None);
        assert_eq!(t.outstanding, 0, "no lookup without an account");
    }

    fn item(media_id: u32, episode: u32) -> Queued {
        Queued { media_id, episode, title: "Show".into(), episodes: Some(12) }
    }

    #[test]
    fn failed_updates_are_queued_and_retried() {
        let mut t = tracker();
        let mut notices = Vec::new();
        let temporary = ApiError::Temporary("offline".into());
        t.handle(Outcome::UpdateFailed { key: 0, item: item(5, 3), error: temporary.clone(), queued: false }, &mut notices);
        assert_eq!(t.queue.0, vec![item(5, 3)]);
        assert!(t.next_retry.is_some(), "a retry is scheduled");
        assert!(matches!(&notices[0], Notice::Message(m) if m.contains("will try again")));

        // A retry that fails again stays queued, quietly.
        notices.clear();
        t.handle(Outcome::UpdateFailed { key: 0, item: item(5, 4), error: temporary, queued: true }, &mut notices);
        assert!(notices.is_empty());
        assert_eq!(t.queue.0, vec![item(5, 4)], "the further episode");

        // Retrying sends everything queued.
        t.next_retry = Some(Instant::now());
        t.poll();
        assert!(t.queue.0.is_empty());
        assert_eq!(t.outstanding, 1);

        // AniList refusing the update itself isn't retried.
        t.handle(Outcome::UpdateFailed { key: 0, item: item(6, 1), error: ApiError::Rejected("no".into()), queued: false }, &mut notices);
        assert!(t.queue.0.is_empty());
    }

    #[test]
    fn a_chosen_match_isnt_replaced_by_the_streamers() {
        let mut t = tracker();
        let theirs = AnimeEpisode { media_id: 1, episode: 2, title: "Wrong".into(), episodes: None };
        t.begin("Show - 02.mkv", 100.0, Some(theirs.clone()));
        t.choose(None);
        assert_eq!(t.anime(), None, "don't track");
        t.hint(theirs);
        assert_eq!(t.anime(), None, "still not tracked");
    }

    /// Against the real AniList (no account needed): `cargo test -p mpvlink
    /// live_anilist -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_anilist() {
        let client = Client::new();
        let mut matches = Matches::default();
        let relations = Relations::bundled();
        for (file, want) in [
            ("Ansatsu Kyoushitsu [BD][HEVC][10bit][1080p]/Ansatsu Kyoushitsu 1x18.mkv", Some((20755, 18))),
            ("Ansatsu Kyoushitsu [BD][HEVC][10bit][1080p]/Ansatsu Kyoushitsu 2x14.mkv", Some((21170, 14))),
            ("Ansatsu Kyoushitsu [BD][HEVC][10bit][1080p]/Ansatsu Kyoushitsu 2x03.mkv", Some((21170, 3))),
            ("[SubsPlease] Sousou no Frieren - 28 (1080p) [ABCD1234].mkv", None),
            ("Shingeki no Kyojin S3 - 05.mkv", Some((99147, 5))),
            ("[HorribleSubs] Mob Psycho 100 S2 - 01 [720p].mkv", None),
            ("Kaiji - Ultimate Survivor - 05 [BD 1080p].mkv", None),
            // anime-relations: continuous numbering across seasons.
            ("Shingeki no Kyojin - 40.mkv", Some((99147, 3))),
            ("Shingeki no Kyojin S2 - 26.mkv", Some((20958, 1))),
            ("Shingeki no Kyojin - 55.mkv", Some((104578, 6))),
        ] {
            let found = resolve(&client, &mut matches, &relations, file).unwrap();
            println!("{file}\n    -> {:?}", found.as_ref().map(|(a, n)| (a.media_id, a.episode, &a.title, n)));
            if let Some(want) = want {
                assert_eq!(found.map(|(a, _)| (a.media_id, a.episode)), Some(want), "{file}");
            }
        }
        println!("cached: {:?}", matches.0);

        // The weekly refresh of anime-relations.
        let path = std::env::temp_dir().join(format!("mpvlink-relations-{}.txt", std::process::id()));
        assert!(relations::stale(&path), "missing counts as stale");
        let fresh = relations::download(client.agent(), &path).unwrap();
        assert!(fresh.len() > 500 && !relations::stale(&path));
        assert_eq!(Relations::load(Some(&path)), fresh, "loads the downloaded copy");
        std::fs::remove_file(&path).unwrap();
    }
}
