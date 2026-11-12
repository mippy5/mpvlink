//! The "Settings" page: the signaling server address.

use super::{
    Action, ButtonKind, Caret, FieldView, Hit, Layout, MONO_FONT, PASTE_HINT, TEXT, TEXT_FAINT, WARNING,
    draw_button, draw_field, draw_panel,
};
use crate::ass::{Ass, TextStyle};
use mpvlink_proto::validate_server_url;

/// Long enough for any sensible URL.
pub const MAX_SERVER_CHARS: usize = 256;

/// How many characters of the URL fit in the field at the font size used.
const VISIBLE_CHARS: usize = 36;

pub struct SettingsMenu {
    server: String,
    /// The server from --script-opts, which overrides the saved one.
    overridden_by: Option<String>,
    error: Option<String>,
    hover: Hit,
    pub(super) caret: Caret,
}

impl SettingsMenu {
    pub const FIELDS: usize = 1;

    pub fn new(server: &str, overridden_by: Option<&str>) -> Self {
        let mut menu = Self {
            server: String::new(),
            overridden_by: overridden_by.map(str::to_string),
            error: None,
            hover: Hit::None,
            caret: Caret::new(),
        };
        menu.insert_text(server);
        menu
    }

    /// URLs have no spaces; drop them (and control characters) from typed or
    /// pasted text. Returns whether anything was added.
    fn insert_text(&mut self, text: &str) -> bool {
        let before = self.server.len();
        let room = MAX_SERVER_CHARS.saturating_sub(self.server.chars().count());
        self.server.extend(text.chars().filter(|c| !c.is_whitespace() && !c.is_control()).take(room));
        self.server.len() != before
    }

    fn edited(&mut self) -> Action {
        self.error = None;
        self.caret.touch();
        Action::Redraw
    }

    pub fn insert(&mut self, text: &str) -> Action {
        if self.insert_text(text) { self.edited() } else { Action::None }
    }

    pub fn backspace(&mut self) -> Action {
        if self.server.pop().is_none() {
            return Action::None;
        }
        self.edited()
    }

    pub fn clear(&mut self) -> Action {
        if self.server.is_empty() {
            return Action::None;
        }
        self.server.clear();
        self.edited()
    }

    // One field: nothing to move focus to.
    pub fn focus_next(&mut self) -> Action {
        Action::None
    }

    pub fn focus_prev(&mut self) -> Action {
        Action::None
    }

    /// Save if the address is valid (or empty, meaning the default).
    pub fn submit(&mut self) -> Action {
        let server = self.server.trim().to_string();
        if server.is_empty() {
            return Action::SaveSettings { server };
        }
        match validate_server_url(&server) {
            Ok(()) => Action::SaveSettings { server },
            Err(problem) => {
                self.error = Some(problem);
                self.caret.touch();
                Action::Redraw
            }
        }
    }

    pub fn click(&mut self, hit: Hit) -> Action {
        match hit {
            Hit::Primary => self.submit(),
            Hit::Cancel => Action::CloseSettings,
            // "Use default": clear the field; saving then stores the default.
            Hit::Left => {
                self.server.clear();
                self.edited()
            }
            Hit::Header | Hit::Field(_) | Hit::None => Action::None,
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

    pub fn render(&self, l: &Layout) -> String {
        let mut ass = Ass::new();
        draw_panel(&mut ass, l, "Settings", "Where mpvlink meets other people. Applies the next time you join.");

        let default_hint = format!("{} (this computer)", crate::DEFAULT_SERVER);
        let (helper, helper_color) = match &self.overridden_by {
            Some(url) => (format!("Overridden by --script-opts: {url}"), WARNING),
            None => (format!("e.g. wss://signal.example.com · Enter to save · Esc to go back · {PASTE_HINT}"), TEXT_FAINT),
        };
        draw_field(
            &mut ass,
            l.field(0),
            &FieldView {
                label: "SIGNALING SERVER",
                value: &self.server,
                placeholder: &default_hint,
                helper: &helper,
                helper_color,
                error: self.error.as_deref(),
                focused: true,
                caret_visible: self.caret.visible(),
                style: TextStyle { align: 4, size: 20.0, color: TEXT, font: Some(MONO_FONT), ..Default::default() },
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

    #[test]
    fn saves_valid_urls_and_rejects_bad_ones() {
        let mut m = SettingsMenu::new("", None);
        m.insert("signal.example.com");
        assert_eq!(m.submit(), Action::Redraw);
        assert!(m.error.is_some());
        m.clear();
        assert!(m.error.is_none(), "editing clears the error");
        m.insert(" wss://signal.example.com \n");
        assert_eq!(m.server, "wss://signal.example.com", "spaces are dropped");
        assert_eq!(m.submit(), Action::SaveSettings { server: "wss://signal.example.com".into() });
    }

    #[test]
    fn empty_means_default() {
        let mut m = SettingsMenu::new("wss://old.example", None);
        let l = layout();
        assert_eq!(m.click(l.hit(l.left.center())), Action::Redraw, "Use default clears the field");
        assert_eq!(m.click(l.hit(l.primary.center())), Action::SaveSettings { server: String::new() });
        assert_eq!(m.click(l.hit(l.cancel.center())), Action::CloseSettings);
    }

    #[test]
    fn long_urls_are_capped_and_rendered_from_the_end() {
        let mut m = SettingsMenu::new("", None);
        m.insert(&format!("wss://{}.example.com", "a".repeat(400)));
        assert_eq!(m.server.chars().count(), MAX_SERVER_CHARS);
        m.backspace();
        m.insert("Z");
        let ass = m.render(&layout());
        assert!(ass.contains("…"), "elided");
        assert!(ass.contains("aZ"), "the end (where you type) stays visible");
    }

    #[test]
    fn shows_override_note() {
        let m = SettingsMenu::new("", Some("wss://cli.example"));
        assert!(m.render(&layout()).contains("Overridden by --script-opts: wss://cli.example"));
    }
}
