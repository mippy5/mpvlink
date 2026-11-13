//! The "Settings" page: the signaling server address and the ffmpeg used for
//! streaming your own videos.

use super::{
    Action, ButtonKind, Caret, FieldView, Hit, Layout, MONO_FONT, PASTE_HINT, TEXT, TEXT_FAINT, WARNING,
    draw_button, draw_field, draw_panel,
};
use crate::ass::{Ass, TextStyle};
use mpvlink_proto::validate_server_url;

/// Long enough for any sensible URL or path.
pub const MAX_VALUE_CHARS: usize = 256;

/// How many characters of a value fit in a field at the font size used.
const VISIBLE_CHARS: usize = 36;

/// The page's text fields, in Tab order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsField {
    Server,
    Ffmpeg,
}

impl SettingsField {
    const ALL: [SettingsField; 2] = [SettingsField::Server, SettingsField::Ffmpeg];

    fn index(self) -> usize {
        self as usize
    }
}

/// Saved values ("" = the default) as the page opens with them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SettingsValues {
    pub server: String,
    pub ffmpeg: String,
}

/// What the page shows besides the values themselves.
#[derive(Clone, Debug)]
pub struct SettingsContext {
    /// Values from --script-opts, which take precedence over saved ones.
    pub server_override: Option<String>,
    pub ffmpeg_override: Option<String>,
    /// The ffmpeg the current setting finds, or why none was found.
    pub ffmpeg_found: Result<String, String>,
}

impl Default for SettingsContext {
    fn default() -> Self {
        Self { server_override: None, ffmpeg_override: None, ffmpeg_found: Err("No ffmpeg found.".into()) }
    }
}

pub struct SettingsMenu {
    values: [String; 2],
    context: SettingsContext,
    focus: SettingsField,
    /// A validation error and the field it belongs to.
    error: Option<(SettingsField, String)>,
    hover: Hit,
    pub(super) caret: Caret,
}

impl SettingsMenu {
    pub const FIELDS: usize = SettingsField::ALL.len();

    pub fn new(values: &SettingsValues, context: SettingsContext) -> Self {
        let mut menu = Self {
            values: Default::default(),
            context,
            focus: SettingsField::Server,
            error: None,
            hover: Hit::None,
            caret: Caret::new(),
        };
        menu.insert_into(SettingsField::Server, &values.server);
        menu.insert_into(SettingsField::Ffmpeg, &values.ffmpeg);
        menu
    }

    fn value(&self, f: SettingsField) -> &str {
        &self.values[f.index()]
    }

    /// Append text to a field. URLs have no spaces, so those are dropped from
    /// the server; paths may have them (C:\Program Files\...), so the ffmpeg
    /// field keeps them. Control characters never make it in. Returns whether
    /// anything was added.
    fn insert_into(&mut self, f: SettingsField, text: &str) -> bool {
        let value = &mut self.values[f.index()];
        let before = value.len();
        let room = MAX_VALUE_CHARS.saturating_sub(value.chars().count());
        let keep = |c: &char| !c.is_control() && (f == SettingsField::Ffmpeg || !c.is_whitespace());
        value.extend(text.chars().filter(keep).take(room));
        value.len() != before
    }

    fn edited(&mut self) -> Action {
        if self.error.as_ref().is_some_and(|(f, _)| *f == self.focus) {
            self.error = None;
        }
        self.caret.touch();
        Action::Redraw
    }

    fn focus(&mut self, f: SettingsField) -> Action {
        if f == self.focus {
            return Action::None;
        }
        self.focus = f;
        self.caret.touch();
        Action::Redraw
    }

    pub fn insert(&mut self, text: &str) -> Action {
        if self.insert_into(self.focus, text) { self.edited() } else { Action::None }
    }

    pub fn backspace(&mut self) -> Action {
        if self.values[self.focus.index()].pop().is_none() {
            return Action::None;
        }
        self.edited()
    }

    pub fn clear(&mut self) -> Action {
        if self.value(self.focus).is_empty() {
            return Action::None;
        }
        self.values[self.focus.index()].clear();
        self.edited()
    }

    pub fn focus_next(&mut self) -> Action {
        let all = SettingsField::ALL;
        self.focus(all[(self.focus.index() + 1) % all.len()])
    }

    pub fn focus_prev(&mut self) -> Action {
        let all = SettingsField::ALL;
        self.focus(all[(self.focus.index() + all.len() - 1) % all.len()])
    }

    /// Show a problem with a field (found by the plugin, e.g. an ffmpeg that
    /// doesn't run) and put the cursor there.
    pub fn reject(&mut self, f: SettingsField, message: String) {
        self.error = Some((f, message));
        self.focus = f;
        self.caret.touch();
    }

    /// Save if the values look valid. Empty means the default.
    pub fn submit(&mut self) -> Action {
        let server = self.value(SettingsField::Server).trim().to_string();
        if !server.is_empty()
            && let Err(problem) = validate_server_url(&server)
        {
            self.reject(SettingsField::Server, problem);
            return Action::Redraw;
        }
        // Windows' "Copy as path" adds quotes; they're not part of the path.
        let ffmpeg = self.value(SettingsField::Ffmpeg).trim().trim_matches('"').trim().to_string();
        Action::SaveSettings { values: SettingsValues { server, ffmpeg } }
    }

    pub fn click(&mut self, hit: Hit) -> Action {
        match hit {
            Hit::Primary => self.submit(),
            Hit::Cancel => Action::CloseSettings,
            // "Use default": clear the focused field; saving stores the default.
            Hit::Left => {
                self.values[self.focus.index()].clear();
                self.edited()
            }
            Hit::Field(i) => SettingsField::ALL.get(i).map_or(Action::None, |f| self.focus(*f)),
            Hit::Header | Hit::None => Action::None,
        }
    }

    pub fn hover(&mut self, hit: Hit) -> Action {
        let hit = if matches!(hit, Hit::Primary | Hit::Cancel | Hit::Left) { hit } else { Hit::None };
        if hit == self.hover {
            return Action::None;
        }
        self.hover = hit;
        Action::Redraw
    }

    fn error_for(&self, f: SettingsField) -> Option<&str> {
        self.error.as_ref().filter(|(ef, _)| *ef == f).map(|(_, m)| m.as_str())
    }

    pub fn render(&self, l: &Layout) -> String {
        let mut ass = Ass::new();
        draw_panel(&mut ass, l, "Settings", "Changes apply the next time you join a room or stream a video.");
        let mono = || TextStyle { align: 4, size: 20.0, color: TEXT, font: Some(MONO_FONT), ..Default::default() };

        let default_server = format!("{} (this computer)", crate::DEFAULT_SERVER);
        let (helper, helper_color) = match &self.context.server_override {
            Some(url) => (format!("Overridden by --script-opts: {url}"), WARNING),
            None => (format!("e.g. wss://signal.example.com · Tab for the next field · {PASTE_HINT}"), TEXT_FAINT),
        };
        let server = SettingsField::Server;
        draw_field(
            &mut ass,
            l.field(server.index()),
            &FieldView {
                label: "SIGNALING SERVER",
                value: self.value(server),
                placeholder: &default_server,
                helper: &helper,
                helper_color,
                error: self.error_for(server),
                focused: self.focus == server,
                caret_visible: self.focus == server && self.caret.visible(),
                style: mono(),
                max_visible_chars: Some(VISIBLE_CHARS),
            },
        );

        let (helper, helper_color) = match (&self.context.ffmpeg_override, &self.context.ffmpeg_found) {
            (Some(path), _) => (format!("Overridden by --script-opts: {path}"), WARNING),
            (None, Ok(path)) => (format!("Using {path} · Enter to save"), TEXT_FAINT),
            (None, Err(problem)) => (format!("{problem} Streaming needs ffmpeg."), WARNING),
        };
        let ffmpeg = SettingsField::Ffmpeg;
        draw_field(
            &mut ass,
            l.field(ffmpeg.index()),
            &FieldView {
                label: "FFMPEG (FOR STREAMING YOUR VIDEOS)",
                value: self.value(ffmpeg),
                placeholder: "Find it automatically",
                helper: &helper,
                helper_color,
                error: self.error_for(ffmpeg),
                focused: self.focus == ffmpeg,
                caret_visible: self.focus == ffmpeg && self.caret.visible(),
                style: mono(),
                max_visible_chars: Some(VISIBLE_CHARS),
            },
        );

        // Buttons: [Use default] ........ [Cancel] [Save]
        draw_button(&mut ass, l.left, "Use default", ButtonKind::Secondary, self.hover == Hit::Left);
        draw_button(&mut ass, l.cancel, "Cancel", ButtonKind::Ghost, self.hover == Hit::Cancel);
        draw_button(&mut ass, l.primary, "Save", ButtonKind::Primary { enabled: true }, self.hover == Hit::Primary);

        ass.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> Layout {
        Layout::new(1280.0, 720.0, SettingsMenu::FIELDS)
    }

    fn menu(server: &str, ffmpeg: &str) -> SettingsMenu {
        let context = SettingsContext { ffmpeg_found: Ok("/usr/bin/ffmpeg".into()), ..Default::default() };
        SettingsMenu::new(&SettingsValues { server: server.into(), ffmpeg: ffmpeg.into() }, context)
    }

    fn saved(server: &str, ffmpeg: &str) -> Action {
        Action::SaveSettings { values: SettingsValues { server: server.into(), ffmpeg: ffmpeg.into() } }
    }

    #[test]
    fn saves_valid_urls_and_rejects_bad_ones() {
        let mut m = menu("", "");
        m.insert("signal.example.com");
        assert_eq!(m.submit(), Action::Redraw);
        assert!(m.error_for(SettingsField::Server).is_some());
        m.clear();
        assert!(m.error.is_none(), "editing clears the error");
        m.insert(" wss://signal.example.com \n");
        assert_eq!(m.value(SettingsField::Server), "wss://signal.example.com", "spaces are dropped");
        assert_eq!(m.submit(), saved("wss://signal.example.com", ""));
    }

    #[test]
    fn ffmpeg_paths_keep_spaces_and_lose_quotes() {
        let mut m = menu("", "");
        assert_eq!(m.focus_next(), Action::Redraw);
        m.insert(r#""C:\Program Files\ffmpeg\bin\ffmpeg.exe""#);
        assert_eq!(m.submit(), saved("", r"C:\Program Files\ffmpeg\bin\ffmpeg.exe"));
    }

    #[test]
    fn tab_and_clicks_move_focus_and_use_default_clears_the_focused_field() {
        let mut m = menu("wss://old.example", "/opt/ffmpeg");
        let l = layout();
        assert_eq!(m.click(l.hit(l.field(1).center())), Action::Redraw);
        assert_eq!(m.focus, SettingsField::Ffmpeg);
        assert_eq!(m.click(l.hit(l.left.center())), Action::Redraw);
        assert_eq!(m.submit(), saved("wss://old.example", ""), "only the ffmpeg field was cleared");
        m.focus_prev();
        assert_eq!(m.focus, SettingsField::Server);
        assert_eq!(m.click(l.hit(l.cancel.center())), Action::CloseSettings);
    }

    #[test]
    fn plugin_can_reject_a_value() {
        let mut m = menu("", "/nope");
        m.reject(SettingsField::Ffmpeg, "Couldn't run it.".into());
        assert_eq!(m.focus, SettingsField::Ffmpeg);
        assert!(m.render(&layout()).contains("Couldn't run it."));
    }

    #[test]
    fn long_values_are_capped_and_rendered_from_the_end() {
        let mut m = menu("", "");
        m.insert(&format!("wss://{}.example.com", "a".repeat(400)));
        assert_eq!(m.value(SettingsField::Server).chars().count(), MAX_VALUE_CHARS);
        m.backspace();
        m.insert("Z");
        let ass = m.render(&layout());
        assert!(ass.contains("…"), "elided");
        assert!(ass.contains("aZ"), "the end (where you type) stays visible");
    }

    #[test]
    fn shows_overrides_and_what_was_found() {
        let context = SettingsContext {
            server_override: Some("wss://cli.example".into()),
            ffmpeg_override: None,
            ffmpeg_found: Err("No ffmpeg found.".into()),
        };
        let ass = SettingsMenu::new(&SettingsValues::default(), context).render(&layout());
        assert!(ass.contains("Overridden by --script-opts: wss://cli.example"));
        assert!(ass.contains("No ffmpeg found. Streaming needs ffmpeg."));
        assert!(menu("", "").render(&layout()).contains("Using /usr/bin/ffmpeg"));
    }
}
