//! mpvlink: mpv plugin for joining low-latency WebRTC rooms.
//!
//! mpv loads this library (via `--script=` or the scripts/ directory), calls
//! `mpv_open_cplugin` on a dedicated thread, and unloads the plugin when it returns.
//!
//! Key bindings:
//!   Ctrl+j                              toggle the join/create room menu (Tab switches fields)
//!   Ctrl+,                              (in the menu) open settings
//!   script-binding mpvlink/toggle-menu  toggle the menu, for use in input.conf
//!   script-binding mpvlink/settings     open the menu at the settings page
//!
//! Options (`--script-opts=key=value,...`); these override saved settings:
//!   mpvlink-server=wss://host       signaling server (default: saved setting, else ws://127.0.0.1:8787)
//!   mpvlink-stun=stun:host:port     STUN server, or "no" for none (default: Google's public one)
//!   mpvlink-ffmpeg=/path/to/ffmpeg  ffmpeg for streaming your own videos (default: saved setting, else PATH)
//!   mpvlink-kbps=6000               bitrate of your stream
//!
//! In a room, the member list (top left) shows an arrow at whose stream you're
//! watching; click a name to switch. With the arrow on yourself, a video you
//! open (e.g. drag into the window) is streamed to the room.

mod ass;
mod ffi;
mod ffmpeg;
mod menu;
mod mpegts;
mod mpv;
mod profile;
mod room;
mod roster;
mod session;
mod stream;
mod webm;

use menu::{Action, Layout, Menu, SettingsContext, SettingsField, SettingsValues};
use mpv::{Event, Mpv};
use profile::Profile;
use mpvlink_rtc::proto::PeerId;
use session::{Session, SessionConfig, SessionEvent};
use std::ffi::c_int;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// osd-overlay ids. The menu draws over the member list.
const MENU_OVERLAY: &str = "1";
const ROSTER_OVERLAY: &str = "2";
const SUBTITLE_OVERLAY: &str = "3";
const OBSERVE_OSD: u64 = 1;
const OBSERVE_MOUSE: u64 = 2;
const OBSERVE_SUB: u64 = 3;
const OBSERVE_IDLE: u64 = 4;
const OBSERVE_TRACKS: u64 = 5;

const GLOBAL_SECTION: &str = "mpvlink-global";
const MENU_SECTION: &str = "mpvlink-menu";
const ROSTER_SECTION: &str = "mpvlink-roster";

/// Bitrate of our own stream unless --script-opts says otherwise.
const DEFAULT_KBPS: u32 = 6000;

/// Client message the session uses to report back to the plugin thread.
const SESSION_MESSAGE: &str = "mpvlink-session";

/// Signaling server when none is configured: one running on this computer.
pub(crate) const DEFAULT_SERVER: &str = "ws://127.0.0.1:8787";

/// Per-file options for playing a room's stream with as little delay as mpv
/// allows: raw H.264 with no probing or buffering, frames shown as soon as
/// they're decoded. Audio (an extra track, live WebM) plays alongside on its
/// own small buffer: video never waits for it, nor is it dropped to match it.
const LOW_LATENCY_OPTIONS: &[&str] = &[
    "demuxer-lavf-format=h264",
    "demuxer-lavf-probesize=32",
    "demuxer-lavf-analyzeduration=0",
    "demuxer-lavf-probe-info=nostreams",
    "demuxer-lavf-o=fflags=+nobuffer",
    // Raw H.264 has no timestamps, so the demuxer makes them up from a frame
    // rate. A very high one (1 ms per frame) keeps video "behind" mpv's
    // clock, so each frame is shown the moment it's decoded; at the default
    // 25 fps, mpv would pace 30 fps video too slowly once there's audio.
    "demuxer-lavf-o-append=framerate=1000",
    "cache=no",
    // Don't pause to "buffer" when the next frame hasn't arrived yet: it's live.
    "cache-pause=no",
    "untimed=yes",
    "vd-lavc-threads=1",
    "video-latency-hacks=yes",
    "interpolation=no",
    // The audio track is WebM, read by mpv's own Matroska demuxer (the lavf
    // format above is only for the video).
    "audio-demuxer=mkv",
    "demuxer-readahead-secs=0",
    // Play audio and video independently: by default mpv times video to the
    // audio clock, which would hold the picture back by the audio buffer.
    // (`untimed` alone isn't enough once there's audio.)
    "video-sync=desync",
    "initial-audio-sync=no",
    "framedrop=no",
    "audio-buffer=0.1",
];

struct Plugin {
    mpv: Mpv,
    menu: Option<Menu>,
    /// The saved identity (display name), loaded at startup.
    profile: Profile,
    /// Where the profile is saved; None if there's no state directory (e.g.
    /// mpv was started with --no-config), in which case nothing is persisted.
    profile_path: Option<PathBuf>,
    /// Last drawn caret state, so blinking only redraws when it changes.
    caret_drawn: bool,
    /// The room we're in, if any.
    session: Option<Session>,
    /// The member list as last drawn (for clicks), and the row under the mouse.
    roster: Option<roster::Overlay>,
    hover: Option<PeerId>,
    /// The member list's click binding is on. Only while the mouse is over
    /// the list, so clicks elsewhere still reach mpv and other scripts.
    roster_clicks: bool,
    /// The subtitle line of the stream we're watching, as drawn.
    subtitle: String,
    /// Counts finished seeks, for our stream to follow.
    seeks: Arc<AtomicU64>,
}

impl Plugin {
    fn new(mpv: Mpv) -> Self {
        let profile_path = mpv.expand_path(profile::PROFILE_PATH);
        let profile = profile_path.as_deref().map(Profile::load).unwrap_or_default();
        Self {
            mpv,
            menu: None,
            profile,
            profile_path,
            caret_drawn: false,
            session: None,
            roster: None,
            hover: None,
            roster_clicks: false,
            subtitle: String::new(),
            seeks: Arc::new(AtomicU64::new(0)),
        }
    }

    fn log_err(&self, what: &str, r: mpv::Result<()>) {
        if let Err(e) = r {
            eprintln!("[{}] {what} failed: {e}", self.mpv.name());
        }
    }

    /// `script-binding <us>/<name>`: mpv sends us a "key-binding" client message.
    fn binding(&self, name: &str) -> String {
        format!("script-binding {}/{name}", self.mpv.name())
    }

    fn install_global_bindings(&self) {
        // "default" = only if the user hasn't bound Ctrl+j themselves.
        let contents = format!("Ctrl+j {}", self.binding("toggle-menu"));
        self.log_err("define-section", self.mpv.command(&["define-section", GLOBAL_SECTION, &contents, "default"]));
        self.log_err("enable-section", self.mpv.command(&["enable-section", GLOBAL_SECTION]));
        // Enabled only while the mouse is over the member list.
        let contents = format!("MBTN_LEFT {}", self.binding("roster-click"));
        self.log_err("define-section", self.mpv.command(&["define-section", ROSTER_SECTION, &contents, "force"]));
    }

    fn open_menu(&mut self) {
        if self.menu.is_some() {
            return;
        }
        // The OSD needs a window; in --idle mode without a file there may be none.
        if self.mpv.get_bool("vo-configured") == Some(false) {
            self.log_err("force-window", self.mpv.set_string("force-window", "yes"));
        }

        // While open, the menu takes all input ("exclusive"), so typing "q"
        // enters a Q instead of quitting. ANY_UNICODE catches any key that
        // produces text; mpv tells us the text in the key-binding message.
        let b = |n: &str| self.binding(n);
        let contents = [
            format!("ANY_UNICODE repeatable {}", b("text")),
            format!("BS repeatable {}", b("backspace")),
            format!("Ctrl+BS {}", b("clear")),
            format!("Alt+BS {}", b("clear")),
            format!("Meta+BS {}", b("clear")),
            format!("TAB {}", b("next-field")),
            format!("Shift+TAB {}", b("prev-field")),
            format!("ENTER {}", b("submit")),
            format!("KP_ENTER {}", b("submit")),
            format!("ESC {}", b("close")),
            format!("Ctrl+j {}", b("close-menu")),
            format!("Ctrl+, {}", b("settings")),
            format!("Ctrl+v {}", b("paste")),
            format!("Meta+v {}", b("paste")),
            format!("MBTN_LEFT {}", b("click")),
        ]
        .join("\n");
        // define-section/enable-section are documented as deprecated for
        // outside use, but they're still how mpv's own scripts bind keys and
        // the only way to get exclusive input from a client.
        self.log_err("define-section", self.mpv.command(&["define-section", MENU_SECTION, &contents, "force"]));
        self.log_err("enable-section", self.mpv.command(&["enable-section", MENU_SECTION, "exclusive"]));

        self.set_roster_clicks(false);
        self.menu = Some(Menu::new(&self.profile.display_name));
        self.redraw();
    }

    fn close_menu(&mut self) {
        let Some(menu) = self.menu.take() else { return };
        // Remember the name even on cancel, so reopening doesn't lose it.
        self.remember_name(menu.name().trim());
        self.log_err("disable-section", self.mpv.command(&["disable-section", MENU_SECTION]));
        self.log_err("osd-overlay", self.mpv.command(&["osd-overlay", MENU_OVERLAY, "none", ""]));
    }

    /// Update the display name and save it for future launches. An empty name
    /// (e.g. cleared, then cancelled) isn't saved over a real one.
    fn remember_name(&mut self, name: &str) {
        if name.is_empty() || name == self.profile.display_name {
            return;
        }
        self.profile.display_name = name.to_string();
        self.save_profile();
    }

    fn save_profile(&self) {
        if let Some(path) = &self.profile_path
            && let Err(e) = self.profile.save(path)
        {
            eprintln!("[{}] saving profile to {} failed: {e}", self.mpv.name(), path.display());
            self.osd(&format!("Couldn't save settings: {e}"), 5000);
        }
    }

    fn layout(&self) -> Layout {
        let fields = self.menu.as_ref().map_or(0, Menu::field_count);
        Layout::new(
            self.mpv.get_f64("osd-dimensions/w").unwrap_or(0.0),
            self.mpv.get_f64("osd-dimensions/h").unwrap_or(0.0),
            fields,
        )
    }

    /// Mouse position in overlay coordinates, or None if it's outside the window.
    fn mouse(&self, layout: &Layout) -> Option<(f64, f64)> {
        let (x, y) = self.mouse_pixels()?;
        Some(layout.to_virtual(x, y))
    }

    /// Mouse position in window pixels, or None if it's outside the window.
    fn mouse_pixels(&self) -> Option<(f64, f64)> {
        // mouse-pos doesn't support sub-property access (mouse-pos/x), so read
        // the whole thing; as a string it's JSON: {"x":10,"y":20,"hover":true}
        let json = self.mpv.get_string("mouse-pos")?;
        if mpv::json_field(&json, "hover") == Some("false") {
            return None;
        }
        let x = mpv::json_field(&json, "x")?.parse().ok()?;
        let y = mpv::json_field(&json, "y")?.parse().ok()?;
        Some((x, y))
    }

    fn osd_size(&self) -> (f64, f64) {
        (self.mpv.get_f64("osd-dimensions/w").unwrap_or(0.0), self.mpv.get_f64("osd-dimensions/h").unwrap_or(0.0))
    }

    fn redraw(&mut self) {
        let Some(menu) = &self.menu else { return };
        let layout = self.layout();
        let data = menu.render(&layout);
        self.caret_drawn = menu.caret_visible();
        let (rx, ry) = (layout.res_x.to_string(), layout.res_y.to_string());
        // osd-overlay <id> <format> <data> <res_x> <res_y> <z>
        let r = self.mpv.command(&["osd-overlay", MENU_OVERLAY, "ass-events", &data, &rx, &ry, "1"]);
        self.log_err("osd-overlay", r);
    }

    /// Draw (or, outside a room, remove) the member list in the top-left corner.
    fn draw_roster(&mut self) {
        let Some(session) = &self.session else {
            self.log_err("osd-overlay", self.mpv.command(&["osd-overlay", ROSTER_OVERLAY, "none", ""]));
            self.roster = None;
            self.hover = None;
            self.set_roster_clicks(false);
            return;
        };
        let (w, h) = self.osd_size();
        let state = session.room_state();
        let out = roster::render(session.room(), &state, self.hover, w, h);
        let (rx, ry) = (out.res_x.to_string(), out.res_y.to_string());
        let r = self.mpv.command(&["osd-overlay", ROSTER_OVERLAY, "ass-events", &out.data, &rx, &ry, "0"]);
        self.log_err("osd-overlay", r);
        self.roster = Some(out);
    }

    /// The mouse moved: highlight the row under it, and take clicks only
    /// while it's over the member list.
    fn roster_mouse(&mut self) {
        let Some(roster) = &self.roster else { return };
        let (w, h) = self.osd_size();
        let pos = self
            .mouse_pixels()
            .filter(|_| w > 0.0 && h > 0.0 && self.menu.is_none())
            .map(|(x, y)| (x * f64::from(roster.res_x) / w, y * f64::from(roster.res_y) / h));
        let over = pos.is_some_and(|p| roster.frame.contains(p));
        let row = pos.and_then(|p| roster.row_at(p));
        self.set_roster_clicks(over);
        if row != self.hover {
            self.hover = row;
            self.draw_roster();
        }
    }

    fn set_roster_clicks(&mut self, on: bool) {
        if on == self.roster_clicks {
            return;
        }
        self.roster_clicks = on;
        let command = if on { "enable-section" } else { "disable-section" };
        self.log_err(command, self.mpv.command(&[command, ROSTER_SECTION]));
    }

    /// Show the subtitle line of the stream we're watching ("" hides it).
    fn draw_subtitle(&mut self, text: &str) {
        self.subtitle = text.to_string();
        if text.is_empty() {
            self.log_err("osd-overlay", self.mpv.command(&["osd-overlay", SUBTITLE_OVERLAY, "none", ""]));
            return;
        }
        // Styled like mpv's own subtitles: white with a black outline, bottom center.
        let (w, h) = self.osd_size();
        let res_y = 720.0;
        let res_x = if w > 0.0 && h > 0.0 { (res_y * w / h).round() } else { 1280.0 };
        let data = format!(
            r"{{\an2\pos({:.0},{:.0})\fs38\bord2.5\shad0\1c&HFFFFFF&\3c&H000000&}}{}",
            res_x / 2.0,
            res_y - 30.0,
            ass::escape(text)
        );
        let (rx, ry) = (res_x.to_string(), res_y.to_string());
        let r = self.mpv.command(&["osd-overlay", SUBTITLE_OVERLAY, "ass-events", &data, &rx, &ry, "0"]);
        self.log_err("osd-overlay", r);
    }

    fn apply(&mut self, action: Action) {
        match action {
            Action::None => {}
            Action::Redraw => self.redraw(),
            Action::Close => self.close_menu(),
            Action::Join { name, code } => {
                self.close_menu();
                self.join_room(&name, &code);
            }
            Action::Create { name } => {
                self.close_menu();
                self.create_room(&name);
            }
            Action::OpenSettings => {
                let values = SettingsValues { server: self.profile.server.clone(), ffmpeg: self.profile.ffmpeg.clone() };
                let context = SettingsContext {
                    server_override: self.script_opt("mpvlink-server"),
                    ffmpeg_override: self.script_opt("mpvlink-ffmpeg"),
                    ffmpeg_found: ffmpeg::locate(&self.ffmpeg_setting()),
                };
                if let Some(menu) = self.menu.as_mut() {
                    menu.open_settings(&values, context);
                }
                self.redraw();
            }
            Action::CloseSettings => {
                if let Some(menu) = self.menu.as_mut() {
                    menu.close_settings();
                }
                self.redraw();
            }
            Action::SaveSettings { values } => {
                // Check an ffmpeg path now, rather than when streaming fails.
                if !values.ffmpeg.is_empty()
                    && let Err(problem) = ffmpeg::locate(&self.expand_user_path(&values.ffmpeg))
                {
                    if let Some(menu) = self.menu.as_mut() {
                        menu.reject_setting(SettingsField::Ffmpeg, problem);
                    }
                    self.redraw();
                    return;
                }
                if let Some(menu) = self.menu.as_mut() {
                    menu.close_settings();
                }
                self.redraw();
                self.save_settings(values);
            }
        }
    }

    /// Persist the settings ("" = default) and say what changed and where it applies.
    fn save_settings(&mut self, values: SettingsValues) {
        let mut notes = Vec::new();
        if values.server != self.profile.server {
            self.profile.server = values.server;
            notes.push(format!("Signaling server: {}", self.settings().0));
            if self.script_opt("mpvlink-server").is_some() {
                notes.push("(--script-opts overrides the saved server)".into());
            } else if let Some(session) = &self.session {
                notes.push(format!("Applies the next time you join (still in room {}).", session.room()));
            }
        }
        if values.ffmpeg != self.profile.ffmpeg {
            self.profile.ffmpeg = values.ffmpeg;
            match ffmpeg::locate(&self.ffmpeg_setting()) {
                Ok(path) => notes.push(format!("Streaming uses {path}")),
                Err(problem) => notes.push(format!("ffmpeg: {problem}")),
            }
            if self.script_opt("mpvlink-ffmpeg").is_some() {
                notes.push("(--script-opts overrides the saved ffmpeg)".into());
            }
        }
        if notes.is_empty() {
            return;
        }
        self.save_profile();
        self.osd(&notes.join("\n"), 5000);
    }

    /// The ffmpeg to stream with: --script-opts, else the saved setting, else
    /// "ffmpeg" (found on PATH or in the usual places).
    fn ffmpeg_setting(&self) -> String {
        self.script_opt("mpvlink-ffmpeg")
            .or_else(|| (!self.profile.ffmpeg.is_empty()).then(|| self.profile.ffmpeg.clone()))
            .map(|p| self.expand_user_path(&p))
            .unwrap_or_else(|| "ffmpeg".into())
    }

    /// Expand a leading `~` (home directory) the way mpv does for its options.
    fn expand_user_path(&self, path: &str) -> String {
        if path.starts_with('~') {
            self.mpv.command_string(&["expand-path", path]).unwrap_or_else(|_| path.to_string())
        } else {
            path.to_string()
        }
    }

    fn create_room(&mut self, name: &str) {
        // The code is picked locally for now; the server creates a room on
        // first join. TODO: have the server mint codes so they're unique.
        let code = room::generate_code();
        println!("[{}] {name} created room {code}", self.mpv.name());
        let copied = self.copy_to_clipboard(&code);
        self.start_session(name, &code);
        // Streaming from mpv isn't implemented yet; for now the creator waits
        // in the room like any viewer until a publisher joins.
        let msg = format!(
            "Created room {code}{}\nWaiting for a stream (test: mpvlink-send --room {code})",
            if copied { " (code copied)" } else { "" },
        );
        self.osd(&msg, 10_000);
    }

    /// Put text on the system clipboard. The mpv 0.40 manual says writing
    /// clipboard/text is Windows-only, but it works on macOS too; pbcopy is a
    /// fallback for builds where it doesn't. (No Linux fallback yet.)
    fn copy_to_clipboard(&self, text: &str) -> bool {
        if self.mpv.set_string("clipboard/text", text).is_ok() {
            return true;
        }
        #[cfg(target_os = "macos")]
        {
            use std::io::Write;
            use std::process::{Command, Stdio};
            let child = Command::new("pbcopy").stdin(Stdio::piped()).spawn();
            if let Ok(mut child) = child {
                let wrote = child.stdin.take().is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
                return child.wait().is_ok_and(|status| status.success()) && wrote;
            }
        }
        false
    }

    fn join_room(&mut self, name: &str, code: &str) {
        println!("[{}] {name} joining room {code}", self.mpv.name());
        self.start_session(name, code);
        self.osd(&format!("Joining room {code}…"), 3000);
    }

    /// Leave any current room and join `code` as a viewer.
    fn start_session(&mut self, name: &str, code: &str) {
        self.session = None; // leave the old room first
        let (server, ice_servers) = self.settings();
        let config = SessionConfig { server, room: code.to_string(), name: name.to_string(), ice_servers };

        // Session events arrive on Tokio threads; bounce them to the plugin
        // thread as client messages so all mpv state changes happen here.
        let (remote, me) = (self.mpv.remote(), self.mpv.name().to_string());
        let notify = Arc::new(move |event: SessionEvent| {
            let (kind, text, extra) = match event {
                SessionEvent::Status(t) => ("status", t, String::new()),
                SessionEvent::WatchStarted { peer, name, audio } => {
                    ("watch", name, format!("{peer}:{}", if audio { "audio" } else { "" }))
                }
                SessionEvent::AudioStarted { peer } => ("audio", String::new(), peer.to_string()),
                SessionEvent::StreamEnded(t) => ("ended", t, String::new()),
                SessionEvent::Failed(t) => ("failed", t, String::new()),
                SessionEvent::RoomChanged => ("room", String::new(), String::new()),
                SessionEvent::Subtitle(t) => ("sub", t, String::new()),
                SessionEvent::StreamingFailed(t) => ("streamfail", t, String::new()),
            };
            let _ = remote.command(&["script-message-to", &me, SESSION_MESSAGE, kind, &text, &extra]);
        });
        match Session::start(config, notify) {
            // Stay open when a stream ends, rather than quitting at EOF.
            Ok(session) => {
                self.log_err("idle", self.mpv.set_string("idle", "yes"));
                self.session = Some(session);
            }
            Err(e) => self.osd(&format!("Couldn't start the session: {e:#}"), 8000),
        }
        self.draw_roster();
    }

    /// ["mpvlink-session", kind, text, extra] from the session's notify callback.
    fn on_session_message(&mut self, args: &[String]) {
        let kind = args.get(1).map(String::as_str).unwrap_or("");
        let text = args.get(2).map(String::as_str).unwrap_or("");
        let extra = args.get(3).map(String::as_str).unwrap_or("");
        let Some(session) = &self.session else { return };
        let room = session.room().to_string();
        match kind {
            "status" => self.osd(text, 4000),
            "watch" => {
                let (peer, audio) = extra.split_once(':').unwrap_or((extra, ""));
                let Ok(peer) = peer.parse::<PeerId>() else { return };
                // Stale if the user has switched to someone else since.
                if session.room_state().target != Some(peer) {
                    return;
                }
                let url = stream::url(&room, peer);
                // A reconnect can bring a fresh offer for a stream we're already
                // playing; its frames go into the same queue, so keep playing.
                if self.mpv.get_string("path").as_deref() == Some(url.as_str()) {
                    return;
                }
                let title = text.replace(',', " ");
                let mut options = LOW_LATENCY_OPTIONS.join(",") + &format!(",force-media-title={title} — room {room}");
                if audio == "audio" {
                    // -append adds one entry; plain audio-files is a path list
                    // and would split the URL at its ':'.
                    options += &format!(",audio-files-append={}", stream::audio_url(&room, peer));
                }
                self.log_err("loadfile", self.mpv.command(&["loadfile", &url, "replace", "-1", &options]));
            }
            "ended" => {
                self.draw_subtitle("");
                self.osd(text, 6000);
            }
            "audio" => {
                let Ok(peer) = extra.parse::<PeerId>() else { return };
                // Only for the stream that's playing, and only once (a
                // reconnect brings this again for a track that's there).
                let url = stream::audio_url(&room, peer);
                let playing = self.mpv.get_string("path").as_deref() == Some(stream::url(&room, peer).as_str());
                if playing && !self.has_external_track(&url) {
                    self.log_err("audio-add", self.mpv.command(&["audio-add", &url, "select"]));
                }
            }
            "room" => self.draw_roster(),
            "sub" => self.draw_subtitle(text),
            "streamfail" => self.osd(&format!("Streaming stopped: {text}"), 10_000),
            "failed" => {
                self.session = None;
                self.draw_roster();
                self.draw_subtitle("");
                self.osd(&format!("Left room {room}: {text}"), 8000);
            }
            _ => {}
        }
    }

    /// A file finished loading. With the arrow on ourselves, stream it.
    fn on_file_loaded(&mut self) {
        let Some(session) = &self.session else { return };
        let Some(path) = self.mpv.get_string("path") else { return };
        if path.starts_with(&format!("{}://", stream::PROTOCOL)) || !session.room_state().targets_me() {
            return;
        }
        let source = if let Some(rest) = path.strip_prefix("file://") {
            rest.to_string()
        } else if path.starts_with("http://") || path.starts_with("https://") {
            path.clone()
        } else if path.contains("://") {
            self.osd("This kind of source can't be streamed (only files and http(s) links).", 6000);
            return;
        } else if Path::new(&path).is_absolute() {
            path.clone()
        } else {
            let cwd = self.mpv.get_string("working-directory").unwrap_or_default();
            Path::new(&cwd).join(&path).to_string_lossy().into_owned()
        };
        // Stream the tracks being played (e.g. the chosen audio language).
        // External tracks (separate files) aren't part of `source`.
        let track = |kind: &str| -> Option<i64> {
            let internal = self.mpv.get_bool(&format!("current-tracks/{kind}/external")) == Some(false);
            internal.then(|| self.mpv.get_string(&format!("current-tracks/{kind}/ff-index"))?.parse().ok()).flatten()
        };
        let audio = match (self.mpv.get_string("aid").as_deref(), track("audio")) {
            (Some("no"), _) => ffmpeg::AudioChoice::Off,
            (_, Some(i)) => ffmpeg::AudioChoice::Stream(i),
            _ => ffmpeg::AudioChoice::First,
        };
        let remote = self.mpv.remote();
        session.start_streaming(ffmpeg::StreamSource {
            path: source,
            title: self.mpv.get_string("media-title").unwrap_or(path),
            video_stream: track("video"),
            audio,
            ffmpeg: self.ffmpeg_setting(),
            kbps: self.script_opt("mpvlink-kbps").and_then(|k| k.parse().ok()).unwrap_or(DEFAULT_KBPS),
            clock: Arc::new(move || remote.get_f64("time-pos")),
            seeks: self.seeks.clone(),
        });
        self.send_subtitle();
    }

    /// Whether a track loaded from `url` (e.g. our audio stream) is there.
    fn has_external_track(&self, url: &str) -> bool {
        let count = self.mpv.get_f64("track-list/count").unwrap_or(0.0) as usize;
        (0..count).any(|i| self.mpv.get_string(&format!("track-list/{i}/external-filename")).as_deref() == Some(url))
    }

    fn streaming(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.room_state().me().is_some_and(|m| m.streaming))
    }

    /// While streaming, pass the subtitle line we see on to our viewers.
    fn send_subtitle(&self) {
        let Some(session) = &self.session else { return };
        let text = match self.mpv.get_bool("sub-visibility") {
            Some(false) => String::new(),
            _ => self.mpv.get_string("sub-text").unwrap_or_default(),
        };
        session.subtitle(text);
    }

    /// A value from --script-opts (key=value,...), if set.
    fn script_opt(&self, key: &str) -> Option<String> {
        let opts = self.mpv.get_string("script-opts").unwrap_or_default();
        opts.split(',').find_map(|kv| kv.split_once('=').filter(|(k, _)| *k == key).map(|(_, v)| v.to_string()))
    }

    /// (signaling server URL, ICE server URLs). The server comes from
    /// --script-opts, else the saved setting, else the default.
    fn settings(&self) -> (String, Vec<String>) {
        let server = self
            .script_opt("mpvlink-server")
            .or_else(|| (!self.profile.server.is_empty()).then(|| self.profile.server.clone()))
            .unwrap_or_else(|| DEFAULT_SERVER.to_string());
        let ice = match self.script_opt("mpvlink-stun").as_deref() {
            Some("no" | "") => vec![],
            Some(url) => vec![url.to_string()],
            None => vec![mpvlink_rtc::peer::DEFAULT_STUN_SERVER.to_string()],
        };
        (server, ice)
    }

    /// Show a message at the top center (top-left is the member list's).
    fn osd(&self, text: &str, ms: u32) {
        // ${osd-ass-cc/0} turns on ASS tags for the rest of the text: \an8
        // centers it at the top, and a slightly smaller size keeps longer
        // messages clear of the member list. Client commands only expand ${...}
        // when asked to, and "$$" keeps a literal $ from starting a property.
        let text = format!(r"${{osd-ass-cc/0}}{{\an8\fscx80\fscy80}}{}", ass::escape(text).replace('$', "$$"));
        let r = self.mpv.command(&["expand-properties", "show-text", &text, &ms.to_string()]);
        self.log_err("show-text", r);
    }

    /// Handle a `key-binding` client message:
    /// ["key-binding", name, state, key name, key text, scale, arg]
    fn on_key_binding(&mut self, args: &[String]) {
        let name = args.get(1).map(String::as_str).unwrap_or("");
        let state = args.get(2).map(String::as_str).unwrap_or("");
        // d = down, r = repeat, p = press (when up/down can't be tracked); ignore u = up.
        if !matches!(state.chars().next(), Some('d' | 'r' | 'p')) {
            return;
        }

        match name {
            "toggle-menu" | "close-menu" => {
                if self.menu.is_some() { self.close_menu() } else if name == "toggle-menu" { self.open_menu() }
                return;
            }
            "roster-click" => {
                if let (Some(session), Some(id)) = (&self.session, self.hover) {
                    session.select(id);
                }
                return;
            }
            "settings" => {
                self.open_menu();
                if !self.menu.as_ref().is_some_and(Menu::in_settings) {
                    self.apply(Action::OpenSettings);
                }
                return;
            }
            _ => {}
        }

        let action = {
            let layout = self.layout();
            let mouse = self.mouse(&layout);
            let clipboard = (name == "paste").then(|| self.mpv.get_string("clipboard/text")).flatten();
            let Some(menu) = self.menu.as_mut() else { return };
            match name {
                "text" => menu.insert(args.get(4).map(String::as_str).unwrap_or("")),
                "backspace" => menu.backspace(),
                "clear" => menu.clear(),
                "next-field" => menu.focus_next(),
                "prev-field" => menu.focus_prev(),
                "submit" => menu.submit(),
                "close" => menu.escape(),
                "paste" => menu.insert(clipboard.as_deref().unwrap_or("")),
                "click" => mouse.map_or(Action::None, |pos| menu.click(pos, &layout)),
                _ => Action::None,
            }
        };
        self.apply(action);
    }

    fn on_property_change(&mut self, id: u64) {
        match id {
            OBSERVE_OSD => {
                self.redraw();
                self.draw_roster();
                let subtitle = self.subtitle.clone();
                self.draw_subtitle(&subtitle);
            }
            OBSERVE_MOUSE => {
                if self.menu.is_some() {
                    let layout = self.layout();
                    let mouse = self.mouse(&layout);
                    if let Some(menu) = self.menu.as_mut() {
                        let action = menu.mouse_move(mouse, &layout);
                        self.apply(action);
                    }
                } else {
                    self.roster_mouse();
                }
            }
            OBSERVE_SUB => {
                if self.streaming() {
                    self.send_subtitle();
                }
            }
            // Another audio (or video) track while streaming: restart the
            // stream with it, from the current position. Viewers stay connected.
            OBSERVE_TRACKS => {
                if self.streaming() && self.mpv.get_bool("idle-active") == Some(false) {
                    self.on_file_loaded();
                }
            }
            // Nothing playing any more: nothing to stream.
            OBSERVE_IDLE => {
                if self.mpv.get_bool("idle-active") == Some(true)
                    && self.streaming()
                    && let Some(session) = &self.session
                {
                    session.stop_streaming();
                }
            }
            _ => {}
        }
    }

    fn run(&mut self) {
        let r = self.mpv.add_stream_protocol(stream::PROTOCOL, stream::open);
        self.log_err("registering mpvlink:// protocol", r);
        self.install_global_bindings();
        // The menu and member list are laid out for the window's size.
        self.log_err("observe", self.mpv.observe(OBSERVE_OSD, "osd-dimensions"));
        self.log_err("observe", self.mpv.observe(OBSERVE_MOUSE, "mouse-pos"));
        self.log_err("observe", self.mpv.observe(OBSERVE_SUB, "sub-text"));
        self.log_err("observe", self.mpv.observe(OBSERVE_SUB, "sub-visibility"));
        self.log_err("observe", self.mpv.observe(OBSERVE_IDLE, "idle-active"));
        self.log_err("observe", self.mpv.observe(OBSERVE_TRACKS, "aid"));
        self.log_err("observe", self.mpv.observe(OBSERVE_TRACKS, "vid"));
        // Launched with nothing to play (e.g. `mpv --idle`): go straight to the menu.
        if self.mpv.get_bool("idle-active") == Some(true) {
            self.open_menu();
        }

        loop {
            // While the menu is open, wake up in time to blink the caret.
            let timeout = self.menu.as_ref().map_or(-1.0, |m| m.until_next_blink());
            match self.mpv.wait_event(timeout) {
                Event::Shutdown => break,
                Event::ClientMessage(args) => match args.first().map(String::as_str) {
                    Some("key-binding") => self.on_key_binding(&args),
                    Some(SESSION_MESSAGE) => self.on_session_message(&args),
                    _ => {}
                },
                Event::PropertyChange { id } => self.on_property_change(id),
                Event::FileLoaded => self.on_file_loaded(),
                Event::PlaybackRestart => {
                    self.seeks.fetch_add(1, Ordering::SeqCst);
                }
                _ => {}
            }
            if self.menu.as_ref().is_some_and(|m| m.caret_visible() != self.caret_drawn) {
                self.redraw();
            }
        }
        // The session's threads hold our mpv handle, which mpv destroys once
        // this function returns: stop them first.
        self.session = None;
    }
}

// mpv requires this exact safe-looking C signature, so allow the raw pointer argument.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn mpv_open_cplugin(ctx: *mut ffi::mpv_handle) -> c_int {
    if !ffi::functions_available() {
        eprintln!("mpvlink: this mpv doesn't support C plugins on Windows (needs mpv 0.37 or newer)");
        return -1;
    }
    // SAFETY: ctx is the handle mpv gave us, and it's only used on this thread.
    let mpv = unsafe { Mpv::from_raw(ctx) };
    Plugin::new(mpv).run();
    0
}
