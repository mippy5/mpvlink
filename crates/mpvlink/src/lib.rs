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

mod ass;
mod ffi;
mod menu;
mod mpv;
mod profile;
mod room;
mod session;
mod stream;

use menu::{Action, Layout, Menu};
use mpv::{Event, Mpv};
use profile::Profile;
use session::{Session, SessionConfig, SessionEvent};
use std::ffi::c_int;
use std::path::PathBuf;

const OVERLAY_ID: &str = "1";
const OBSERVE_OSD: u64 = 1;
const OBSERVE_MOUSE: u64 = 2;

const GLOBAL_SECTION: &str = "mpvlink-global";
const MENU_SECTION: &str = "mpvlink-menu";

/// Client message the session uses to report back to the plugin thread.
const SESSION_MESSAGE: &str = "mpvlink-session";

/// Signaling server when none is configured: one running on this computer.
pub(crate) const DEFAULT_SERVER: &str = "ws://127.0.0.1:8787";

/// Per-file options for playing a room's stream with as little delay as mpv
/// allows: raw H.264 with no probing or buffering, frames shown as soon as
/// they're decoded (no A/V sync to wait for; there's no audio yet).
const LOW_LATENCY_OPTIONS: &[&str] = &[
    "demuxer-lavf-format=h264",
    "demuxer-lavf-probesize=32",
    "demuxer-lavf-analyzeduration=0",
    "demuxer-lavf-probe-info=nostreams",
    "demuxer-lavf-o=fflags=+nobuffer",
    "cache=no",
    // Don't pause to "buffer" when the next frame hasn't arrived yet: it's live.
    "cache-pause=no",
    "untimed=yes",
    "vd-lavc-threads=1",
    "video-latency-hacks=yes",
    "interpolation=no",
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
}

impl Plugin {
    fn new(mpv: Mpv) -> Self {
        let profile_path = mpv.expand_path(profile::PROFILE_PATH);
        let profile = profile_path.as_deref().map(Profile::load).unwrap_or_default();
        Self { mpv, menu: None, profile, profile_path, caret_drawn: false, session: None }
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

        self.log_err("observe", self.mpv.observe(OBSERVE_OSD, "osd-dimensions"));
        self.log_err("observe", self.mpv.observe(OBSERVE_MOUSE, "mouse-pos"));

        self.menu = Some(Menu::new(&self.profile.display_name));
        self.redraw();
    }

    fn close_menu(&mut self) {
        let Some(menu) = self.menu.take() else { return };
        // Remember the name even on cancel, so reopening doesn't lose it.
        self.remember_name(menu.name().trim());
        self.mpv.unobserve(OBSERVE_OSD);
        self.mpv.unobserve(OBSERVE_MOUSE);
        self.log_err("disable-section", self.mpv.command(&["disable-section", MENU_SECTION]));
        self.log_err("osd-overlay", self.mpv.command(&["osd-overlay", OVERLAY_ID, "none", ""]));
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
        // mouse-pos doesn't support sub-property access (mouse-pos/x), so read
        // the whole thing; as a string it's JSON: {"x":10,"y":20,"hover":true}
        let json = self.mpv.get_string("mouse-pos")?;
        if mpv::json_field(&json, "hover") == Some("false") {
            return None;
        }
        let x = mpv::json_field(&json, "x")?.parse().ok()?;
        let y = mpv::json_field(&json, "y")?.parse().ok()?;
        Some(layout.to_virtual(x, y))
    }

    fn redraw(&mut self) {
        let Some(menu) = &self.menu else { return };
        let layout = self.layout();
        let data = menu.render(&layout);
        self.caret_drawn = menu.caret_visible();
        let (rx, ry) = (layout.res_x.to_string(), layout.res_y.to_string());
        // osd-overlay <id> <format> <data> <res_x> <res_y> <z>
        let r = self.mpv.command(&["osd-overlay", OVERLAY_ID, "ass-events", &data, &rx, &ry, "0"]);
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
                let overridden_by = self.script_opt("mpvlink-server");
                if let Some(menu) = self.menu.as_mut() {
                    menu.open_settings(&self.profile.server, overridden_by.as_deref());
                }
                self.redraw();
            }
            Action::CloseSettings => {
                if let Some(menu) = self.menu.as_mut() {
                    menu.close_settings();
                }
                self.redraw();
            }
            Action::SaveSettings { server } => {
                if let Some(menu) = self.menu.as_mut() {
                    menu.close_settings();
                }
                self.redraw();
                self.save_server(server);
            }
        }
    }

    /// Persist the signaling server setting ("" = default) and say where it applies.
    fn save_server(&mut self, server: String) {
        if server == self.profile.server {
            return;
        }
        self.profile.server = server;
        self.save_profile();
        let effective = self.settings().0;
        let mut msg = format!("Signaling server: {effective}");
        if self.script_opt("mpvlink-server").is_some() {
            msg.push_str("\n(--script-opts overrides the saved setting)");
        } else if let Some(session) = &self.session {
            msg.push_str(&format!("\nApplies the next time you join (still in room {}).", session.room()));
        }
        self.osd(&msg, 5000);
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
        let notify = std::sync::Arc::new(move |event: SessionEvent| {
            let (kind, text) = match event {
                SessionEvent::Status(t) => ("status", t),
                SessionEvent::StreamStarted => ("started", String::new()),
                SessionEvent::StreamEnded(t) => ("ended", t),
                SessionEvent::Failed(t) => ("failed", t),
            };
            let _ = remote.command(&["script-message-to", &me, SESSION_MESSAGE, kind, &text]);
        });
        match Session::start(config, notify) {
            // Stay open when a stream ends, rather than quitting at EOF.
            Ok(session) => {
                self.log_err("idle", self.mpv.set_string("idle", "yes"));
                self.session = Some(session);
            }
            Err(e) => self.osd(&format!("Couldn't start the session: {e:#}"), 8000),
        }
    }

    /// ["mpvlink-session", kind, text] from the session's notify callback.
    fn on_session_message(&mut self, args: &[String]) {
        let kind = args.get(1).map(String::as_str).unwrap_or("");
        let text = args.get(2).map(String::as_str).unwrap_or("");
        let Some(room) = self.session.as_ref().map(|s| s.room().to_string()) else { return };
        match kind {
            "status" => self.osd(text, 4000),
            "started" => {
                let url = stream::url(&room);
                // A reconnect can bring a fresh offer for a stream we're already
                // playing; its frames go into the same queue, so keep playing.
                if self.mpv.get_string("path").as_deref() == Some(url.as_str()) {
                    return;
                }
                let options = LOW_LATENCY_OPTIONS.join(",") + &format!(",force-media-title=Room {room}");
                self.log_err("loadfile", self.mpv.command(&["loadfile", &url, "replace", "-1", &options]));
            }
            "ended" => self.osd(text, 6000),
            "failed" => {
                self.session = None;
                self.osd(&format!("Left room {room}: {text}"), 8000);
            }
            _ => {}
        }
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

    fn osd(&self, text: &str, ms: u32) {
        self.log_err("show-text", self.mpv.command(&["show-text", text, &ms.to_string()]));
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
            OBSERVE_OSD => self.redraw(),
            OBSERVE_MOUSE => {
                let layout = self.layout();
                let mouse = self.mouse(&layout);
                if let Some(menu) = self.menu.as_mut() {
                    let action = menu.mouse_move(mouse, &layout);
                    self.apply(action);
                }
            }
            _ => {}
        }
    }

    fn run(&mut self) {
        let r = self.mpv.add_stream_protocol(stream::PROTOCOL, stream::open);
        self.log_err("registering mpvlink:// protocol", r);
        self.install_global_bindings();
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
