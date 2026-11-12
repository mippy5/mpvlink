//! The "Join a room" page: display name + room code, Join / Create room.

use super::{
    Action, ButtonKind, Caret, FieldView, Hit, Layout, MONO_FONT, PASTE_HINT, TEXT, TEXT_FAINT, draw_button,
    draw_field, draw_panel,
};
use crate::ass::{Ass, TextStyle};

pub const MIN_CODE_LEN: usize = 4;
pub const MAX_CODE_LEN: usize = 16;
pub const MAX_NAME_CHARS: usize = 32;

/// The page's text fields, in Tab order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Name,
    Code,
}

impl Field {
    const ALL: [Field; 2] = [Field::Name, Field::Code];

    fn index(self) -> usize {
        self as usize
    }

    fn next(self) -> Field {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    fn prev(self) -> Field {
        Self::ALL[(self.index() + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

pub struct JoinMenu {
    name: String,
    code: String,
    focus: Field,
    /// A validation error and the field it belongs to.
    error: Option<(Field, String)>,
    hover: Hit,
    pub(super) caret: Caret,
}

impl JoinMenu {
    pub const FIELDS: usize = Field::ALL.len();

    /// `name` pre-fills the display name (e.g. from the last time the menu was
    /// open). If it's set, focus starts on the room code.
    pub fn new(name: &str) -> Self {
        let mut menu = Self {
            name: String::new(),
            code: String::new(),
            focus: Field::Name,
            error: None,
            hover: Hit::None,
            caret: Caret::new(),
        };
        menu.insert_into(Field::Name, name);
        if !menu.name.trim().is_empty() {
            menu.focus = Field::Code;
        }
        menu
    }

    /// The display name as typed so far (untrimmed).
    pub fn name(&self) -> &str {
        &self.name
    }

    fn value_mut(&mut self, f: Field) -> &mut String {
        match f {
            Field::Name => &mut self.name,
            Field::Code => &mut self.code,
        }
    }

    /// Check the name; on success returns it trimmed.
    fn validate_name(&self) -> Result<String, (Field, String)> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err((Field::Name, "Enter a display name.".into()));
        }
        Ok(name.to_string())
    }

    /// Check both fields for joining; on success returns the cleaned-up (name, code).
    fn validate(&self) -> Result<(String, String), (Field, String)> {
        let name = self.validate_name()?;
        if self.code.is_empty() {
            return Err((Field::Code, "Enter a room code.".into()));
        }
        if self.code.len() < MIN_CODE_LEN {
            return Err((Field::Code, format!("Room codes are at least {MIN_CODE_LEN} characters.")));
        }
        Ok((name, self.code.clone()))
    }

    fn can_join(&self) -> bool {
        self.validate().is_ok()
    }

    fn edited(&mut self) -> Action {
        if self.error.as_ref().is_some_and(|(f, _)| *f == self.focus) {
            self.error = None;
        }
        self.caret.touch();
        Action::Redraw
    }

    /// Append text to a field, applying that field's rules. Returns whether
    /// anything was added.
    fn insert_into(&mut self, f: Field, text: &str) -> bool {
        let value = self.value_mut(f);
        let before = value.len();
        match f {
            // Display names: any printable text, single-line, up to MAX_NAME_CHARS.
            Field::Name => {
                let mut count = value.chars().count();
                for c in text.chars() {
                    if count >= MAX_NAME_CHARS {
                        break;
                    }
                    let c = if c.is_whitespace() { ' ' } else { c };
                    if c.is_control() || (c == ' ' && (value.is_empty() || value.ends_with(' '))) {
                        continue; // no leading or repeated spaces
                    }
                    value.push(c);
                    count += 1;
                }
            }
            // Room codes: case-insensitive letters and digits; everything else
            // (spaces, dashes, etc.) is dropped.
            Field::Code => {
                for c in text.chars().filter(char::is_ascii_alphanumeric) {
                    if value.len() >= MAX_CODE_LEN {
                        break;
                    }
                    value.push(c.to_ascii_uppercase());
                }
            }
        }
        value.len() != before
    }

    /// Insert typed or pasted text into the focused field.
    pub fn insert(&mut self, text: &str) -> Action {
        if self.insert_into(self.focus, text) { self.edited() } else { Action::None }
    }

    pub fn backspace(&mut self) -> Action {
        let f = self.focus;
        if self.value_mut(f).pop().is_none() {
            return Action::None;
        }
        self.edited()
    }

    pub fn clear(&mut self) -> Action {
        let f = self.focus;
        if self.value_mut(f).is_empty() {
            return Action::None;
        }
        self.value_mut(f).clear();
        self.edited()
    }

    pub fn focus(&mut self, f: Field) -> Action {
        if self.focus == f {
            return Action::None;
        }
        self.focus = f;
        self.caret.touch();
        Action::Redraw
    }

    pub fn focus_next(&mut self) -> Action {
        self.focus(self.focus.next())
    }

    pub fn focus_prev(&mut self) -> Action {
        self.focus(self.focus.prev())
    }

    /// Focus a field and show an error on it.
    fn fail(&mut self, (field, msg): (Field, String)) -> Action {
        self.focus = field;
        self.error = Some((field, msg));
        self.caret.touch();
        Action::Redraw
    }

    /// Join if everything is valid; otherwise focus the first problem and
    /// show its error.
    pub fn submit(&mut self) -> Action {
        match self.validate() {
            Ok((name, code)) => Action::Join { name, code },
            Err(e) => self.fail(e),
        }
    }

    /// Create a new room. Only the name is needed; any typed code is ignored.
    pub fn create(&mut self) -> Action {
        match self.validate_name() {
            Ok(name) => Action::Create { name },
            Err(e) => self.fail(e),
        }
    }

    pub fn click(&mut self, hit: Hit) -> Action {
        match hit {
            Hit::Primary => self.submit(),
            Hit::Cancel => Action::Close,
            Hit::Left => self.create(),
            Hit::Header => Action::OpenSettings,
            Hit::Field(i) => self.focus(Field::ALL[i]),
            Hit::None => Action::None,
        }
    }

    pub fn hover(&mut self, hit: Hit) -> Action {
        let hit = if matches!(hit, Hit::Field(_)) { Hit::None } else { hit };
        if hit == self.hover {
            return Action::None;
        }
        self.hover = hit;
        Action::Redraw
    }

    pub fn render(&self, l: &Layout) -> String {
        let mut ass = Ass::new();
        draw_panel(&mut ass, l, "Join a room", "Pick a name, then join with a code or start a new room.");
        draw_button(&mut ass, l.header, "Settings", ButtonKind::Ghost, self.hover == Hit::Header);

        let field = |f: Field, label, placeholder, helper, style| FieldView {
            label,
            value: match f {
                Field::Name => &self.name,
                Field::Code => &self.code,
            },
            placeholder,
            helper,
            helper_color: TEXT_FAINT,
            error: self.error.as_ref().filter(|(ef, _)| *ef == f).map(|(_, msg)| msg.as_str()),
            focused: self.focus == f,
            caret_visible: self.caret.visible(),
            style,
            max_visible_chars: None,
        };
        draw_field(
            &mut ass,
            l.field(Field::Name.index()),
            &field(
                Field::Name,
                "DISPLAY NAME",
                "What should people call you?",
                "Shown to everyone in the room.",
                TextStyle { align: 4, size: 24.0, color: TEXT, ..Default::default() },
            ),
        );
        let hint = format!("Tab to switch · Enter to join · Esc to cancel · {PASTE_HINT}");
        draw_field(
            &mut ass,
            l.field(Field::Code.index()),
            &field(
                Field::Code,
                "ROOM CODE",
                "e.g. 7KQ2XF",
                &hint,
                TextStyle { align: 4, size: 28.0, color: TEXT, spacing: 2.0, font: Some(MONO_FONT), ..Default::default() },
            ),
        );

        // Buttons: [Create room] ........ [Cancel] [Join]
        draw_button(&mut ass, l.left, "Create room", ButtonKind::Secondary, self.hover == Hit::Left);
        draw_button(&mut ass, l.cancel, "Cancel", ButtonKind::Ghost, self.hover == Hit::Cancel);
        let primary = ButtonKind::Primary { enabled: self.can_join() };
        draw_button(&mut ass, l.primary, "Join", primary, self.hover == Hit::Primary);

        ass.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn join(name: &str, code: &str) -> Action {
        Action::Join { name: name.into(), code: code.into() }
    }

    fn layout() -> Layout {
        Layout::new(1920.0, 1080.0, JoinMenu::FIELDS)
    }

    #[test]
    fn code_is_normalized_and_limited() {
        let mut m = JoinMenu::new("Sam");
        assert_eq!(m.focus, Field::Code);
        m.insert("ab-c d1 ");
        assert_eq!(m.code, "ABCD1");
        m.insert(&"x".repeat(40));
        assert_eq!(m.code.len(), MAX_CODE_LEN);
        assert_eq!(m.insert("!"), Action::None);
    }

    #[test]
    fn name_allows_unicode_but_not_control_or_extra_spaces() {
        let mut m = JoinMenu::new("");
        assert_eq!(m.focus, Field::Name);
        m.insert("  Zoë\tthe {cat}\n");
        assert_eq!(m.name, "Zoë the {cat} ");
        m.insert(&"é".repeat(100));
        assert_eq!(m.name.chars().count(), MAX_NAME_CHARS);
    }

    #[test]
    fn tab_cycles_focus_and_edits_follow_it() {
        let mut m = JoinMenu::new("");
        m.insert("ann");
        m.focus_next();
        m.insert("room");
        m.focus_next();
        m.backspace();
        assert_eq!((m.name.as_str(), m.code.as_str()), ("an", "ROOM"));
        m.focus_prev();
        assert_eq!(m.focus, Field::Code);
    }

    #[test]
    fn submit_focuses_first_invalid_field() {
        let mut m = JoinMenu::new("");
        m.focus(Field::Code);
        m.insert("abcd");
        assert_eq!(m.submit(), Action::Redraw);
        assert_eq!(m.focus, Field::Name);
        assert!(matches!(m.error, Some((Field::Name, _))));

        m.insert("  ");
        assert!(m.error.is_some(), "whitespace-only names don't count as an edit");
        m.insert("Lee ");
        assert!(m.error.is_none(), "editing the bad field clears its error");
        assert_eq!(m.submit(), join("Lee", "ABCD"), "name is trimmed");
    }

    #[test]
    fn short_code_error_stays_until_code_is_edited() {
        let mut m = JoinMenu::new("Lee");
        m.insert("ab");
        assert_eq!(m.submit(), Action::Redraw);
        m.focus(Field::Name);
        m.insert("x");
        assert!(matches!(m.error, Some((Field::Code, _))));
    }

    #[test]
    fn clicks_hit_buttons_and_fields() {
        let l = layout();
        let mut m = JoinMenu::new("Kai");
        m.insert("room42");
        assert_eq!(m.click(l.hit(l.field(0).center())), Action::Redraw);
        assert_eq!(m.focus, Field::Name);
        assert_eq!(m.click(l.hit(l.primary.center())), join("Kai", "ROOM42"));
        assert_eq!(m.click(l.hit(l.cancel.center())), Action::Close);
        assert_eq!(m.click(l.hit(l.header.center())), Action::OpenSettings);
        assert_eq!(m.click(l.hit((0.0, 0.0))), Action::None);
    }

    #[test]
    fn create_needs_only_a_name() {
        let l = layout();
        let mut m = JoinMenu::new("");
        m.focus(Field::Code);
        m.insert("ab"); // too short to join, but irrelevant for creating
        assert_eq!(m.click(l.hit(l.left.center())), Action::Redraw);
        assert_eq!(m.focus, Field::Name);
        assert!(matches!(m.error, Some((Field::Name, _))));
        m.insert("Rio");
        assert_eq!(m.click(l.hit(l.left.center())), Action::Create { name: "Rio".into() });
    }

    #[test]
    fn buttons_hover() {
        let l = layout();
        let mut m = JoinMenu::new("");
        assert_eq!(m.hover(l.hit(l.left.center())), Action::Redraw);
        assert_eq!(m.hover, Hit::Left);
        assert_eq!(m.hover(l.hit(l.left.center())), Action::None, "no redraw if unchanged");
        assert_eq!(m.hover(l.hit(l.field(0).center())), Action::Redraw);
        assert_eq!(m.hover, Hit::None, "fields don't highlight");
    }
}
