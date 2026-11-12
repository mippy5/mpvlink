//! The "Join a room" menu: state, layout, rendering and input handling.
//!
//! This module is pure logic (no mpv calls) so it stays easy to test; the
//! plugin feeds it input and draws whatever `render` returns.

use crate::ass::{Ass, Color, Rect, TextStyle, escape, span};
use std::time::Instant;

pub const MIN_CODE_LEN: usize = 4;
pub const MAX_CODE_LEN: usize = 16;
pub const MAX_NAME_CHARS: usize = 32;

const CARET_BLINK_MS: u128 = 530;

// Palette.
const BACKDROP: Color = Color::rgb(0x000000).with_opacity(150);
const SHADOW: Color = Color::rgb(0x000000).with_opacity(140);
const PANEL: Color = Color::rgb(0x2B2D31);
const PANEL_EDGE: Color = Color::rgb(0x3F4147);
const FIELD: Color = Color::rgb(0x1E1F22);
const FIELD_EDGE: Color = Color::rgb(0x3F4147);
const ACCENT: Color = Color::rgb(0x5865F2);
const ACCENT_HOVER: Color = Color::rgb(0x4752C4);
const ACCENT_DISABLED: Color = Color::rgb(0x5865F2).with_opacity(110);
const GHOST_HOVER: Color = Color::rgb(0x3A3C42);
const SECONDARY: Color = Color::rgb(0x4E5058);
const SECONDARY_HOVER: Color = Color::rgb(0x6D6F78);
const TEXT: Color = Color::rgb(0xF2F3F5);
const TEXT_MUTED: Color = Color::rgb(0xB5BAC1);
const TEXT_FAINT: Color = Color::rgb(0x80848E);
const ERROR: Color = Color::rgb(0xFA777C);

#[cfg(target_os = "macos")]
const MONO_FONT: &str = "Menlo";
#[cfg(not(target_os = "macos"))]
const MONO_FONT: &str = "monospace";

#[cfg(target_os = "macos")]
const PASTE_HINT: &str = "Cmd+V to paste";
#[cfg(not(target_os = "macos"))]
const PASTE_HINT: &str = "Ctrl+V to paste";

/// What the plugin should do after the menu handled some input.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Redraw,
    Close,
    Join { name: String, code: String },
    /// Create a new room; the plugin picks the code.
    Create { name: String },
}

/// The menu's text fields, in Tab order.
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hover {
    None,
    Join,
    Cancel,
    Create,
}

/// Positions of everything, in overlay (virtual) pixels.
pub struct Layout {
    pub res_x: u32,
    pub res_y: u32,
    /// OSD size in real pixels, for converting mouse coordinates.
    osd_w: f64,
    osd_h: f64,
    panel: Rect,
    /// Indexed by `Field::index()`.
    fields: [Rect; 2],
    join: Rect,
    cancel: Rect,
    create: Rect,
}

impl Layout {
    const PANEL_W: f64 = 560.0;
    const PANEL_H: f64 = 440.0;
    const PAD: f64 = 32.0;
    const FIELD_H: f64 = 56.0;
    /// Vertical distance from one field's top to the next field's top
    /// (field + message line + next label).
    const FIELD_PITCH: f64 = 120.0;
    const FIRST_FIELD_Y: f64 = 144.0;

    /// Build a layout for an OSD of `osd_w` x `osd_h` real pixels.
    pub fn new(osd_w: f64, osd_h: f64) -> Self {
        let (osd_w, osd_h) = if osd_w > 0.0 && osd_h > 0.0 { (osd_w, osd_h) } else { (1280.0, 720.0) };
        // 720 virtual pixels tall, but grow the virtual canvas on narrow windows
        // so the panel (plus a margin) always fits horizontally.
        let res_y = 720.0_f64.max((Self::PANEL_W + 64.0) * osd_h / osd_w).round();
        let res_x = (res_y * osd_w / osd_h).round();

        let panel = Rect::new(
            ((res_x - Self::PANEL_W) / 2.0).round(),
            ((res_y - Self::PANEL_H) / 2.0).round(),
            Self::PANEL_W,
            Self::PANEL_H,
        );
        let inner_x = panel.x + Self::PAD;
        let inner_w = panel.w - 2.0 * Self::PAD;
        let field = |i: f64| {
            Rect::new(inner_x, panel.y + Self::FIRST_FIELD_Y + i * Self::FIELD_PITCH, inner_w, Self::FIELD_H)
        };
        let fields = [field(0.0), field(1.0)];
        let btn_y = panel.y + panel.h - Self::PAD - 44.0;
        let join = Rect::new(panel.x + panel.w - Self::PAD - 112.0, btn_y, 112.0, 44.0);
        let cancel = Rect::new(join.x - 8.0 - 96.0, btn_y, 96.0, 44.0);
        // Left-aligned, apart from Cancel/Join, since it's the other path.
        let create = Rect::new(inner_x, btn_y, 140.0, 44.0);

        Self { res_x: res_x as u32, res_y: res_y as u32, osd_w, osd_h, panel, fields, join, cancel, create }
    }

    fn field(&self, f: Field) -> Rect {
        self.fields[f.index()]
    }

    /// Convert a mouse position in real OSD pixels to overlay pixels.
    pub fn to_virtual(&self, x: f64, y: f64) -> (f64, f64) {
        (x * self.res_x as f64 / self.osd_w, y * self.res_y as f64 / self.osd_h)
    }
}

pub struct JoinMenu {
    name: String,
    code: String,
    focus: Field,
    /// A validation error and the field it belongs to.
    error: Option<(Field, String)>,
    hover: Hover,
    /// Reset on every edit or focus change so the caret is solid while typing.
    caret_epoch: Instant,
}

impl JoinMenu {
    /// `name` pre-fills the display name (e.g. from the last time the menu was
    /// open). If it's set, focus starts on the room code.
    pub fn new(name: &str) -> Self {
        let mut menu = Self {
            name: String::new(),
            code: String::new(),
            focus: Field::Name,
            error: None,
            hover: Hover::None,
            caret_epoch: Instant::now(),
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

    fn touch_caret(&mut self) {
        self.caret_epoch = Instant::now();
    }

    fn edited(&mut self) -> Action {
        if self.error.as_ref().is_some_and(|(f, _)| *f == self.focus) {
            self.error = None;
        }
        self.touch_caret();
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
        self.touch_caret();
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
        self.touch_caret();
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

    /// A left click at overlay coordinates.
    pub fn click(&mut self, pos: (f64, f64), layout: &Layout) -> Action {
        if layout.join.contains(pos) {
            return self.submit();
        }
        if layout.cancel.contains(pos) {
            return Action::Close;
        }
        if layout.create.contains(pos) {
            return self.create();
        }
        match Field::ALL.into_iter().find(|f| layout.field(*f).contains(pos)) {
            Some(f) => self.focus(f),
            None => Action::None,
        }
    }

    /// Update the hover state from overlay coordinates (`None` when the mouse
    /// left the window).
    pub fn mouse_move(&mut self, pos: Option<(f64, f64)>, layout: &Layout) -> Action {
        let hover = match pos {
            Some(p) if layout.join.contains(p) => Hover::Join,
            Some(p) if layout.cancel.contains(p) => Hover::Cancel,
            Some(p) if layout.create.contains(p) => Hover::Create,
            _ => Hover::None,
        };
        if hover == self.hover {
            return Action::None;
        }
        self.hover = hover;
        Action::Redraw
    }

    pub fn caret_visible(&self) -> bool {
        (self.caret_epoch.elapsed().as_millis() / CARET_BLINK_MS).is_multiple_of(2)
    }

    /// Seconds until the caret next blinks, for the event loop's wait timeout.
    pub fn until_next_blink(&self) -> f64 {
        let ms = self.caret_epoch.elapsed().as_millis();
        (CARET_BLINK_MS - ms % CARET_BLINK_MS) as f64 / 1000.0
    }

    pub fn render(&self, l: &Layout) -> String {
        let mut ass = Ass::new();
        let full = Rect::new(0.0, 0.0, l.res_x as f64, l.res_y as f64);

        // Backdrop, drop shadow, panel.
        ass.rect(full, 0.0, BACKDROP, None, 0.0);
        ass.rect(Rect { y: l.panel.y + 8.0, ..l.panel }.grow(4.0), 20.0, SHADOW, None, 16.0);
        ass.rect(l.panel, 16.0, PANEL, Some((1.0, PANEL_EDGE)), 0.0);

        // Heading.
        let x = l.panel.x + Layout::PAD;
        let y = l.panel.y + Layout::PAD;
        ass.text(x, y, &TextStyle { size: 30.0, bold: true, color: TEXT, ..Default::default() }, "Join a room");
        ass.text(
            x,
            y + 44.0,
            &TextStyle { size: 19.0, color: TEXT_MUTED, ..Default::default() },
            "Pick a name, then join with a code or start a new room.",
        );

        self.render_field(
            &mut ass,
            l,
            Field::Name,
            "DISPLAY NAME",
            "What should people call you?",
            "Shown to everyone in the room.",
            TextStyle { align: 4, size: 24.0, color: TEXT, ..Default::default() },
        );
        self.render_field(
            &mut ass,
            l,
            Field::Code,
            "ROOM CODE",
            "e.g. 7KQ2XF",
            &format!("Tab to switch · Enter to join · Esc to cancel · {PASTE_HINT}"),
            TextStyle { align: 4, size: 28.0, color: TEXT, spacing: 2.0, font: Some(MONO_FONT), ..Default::default() },
        );

        // Buttons: [Create room] ........ [Cancel] [Join]
        let create_fill = if self.hover == Hover::Create { SECONDARY_HOVER } else { SECONDARY };
        ass.rect(l.create, 8.0, create_fill, None, 0.0);
        let (crx, cry) = l.create.center();
        ass.text(crx, cry, &TextStyle { align: 5, size: 18.0, bold: true, color: TEXT, ..Default::default() }, "Create room");

        if self.hover == Hover::Cancel {
            ass.rect(l.cancel, 8.0, GHOST_HOVER, None, 0.0);
        }
        let (cx, cy) = l.cancel.center();
        ass.text(cx, cy, &TextStyle { align: 5, size: 18.0, bold: true, color: TEXT, ..Default::default() }, "Cancel");

        let join_fill = match (self.can_join(), self.hover == Hover::Join) {
            (false, _) => ACCENT_DISABLED,
            (true, true) => ACCENT_HOVER,
            (true, false) => ACCENT,
        };
        let join_text = if self.can_join() { TEXT } else { TEXT.with_opacity(140) };
        ass.rect(l.join, 8.0, join_fill, None, 0.0);
        let (jx, jy) = l.join.center();
        ass.text(jx, jy, &TextStyle { align: 5, size: 18.0, bold: true, color: join_text, ..Default::default() }, "Join");

        ass.finish()
    }

    /// Label, box (ring shows focus/error), value with caret or placeholder,
    /// and a helper line that turns into the error message when there is one.
    #[allow(clippy::too_many_arguments)]
    fn render_field(
        &self,
        ass: &mut Ass,
        l: &Layout,
        f: Field,
        label: &str,
        placeholder: &str,
        helper: &str,
        style: TextStyle,
    ) {
        let rect = l.field(f);
        let focused = self.focus == f;
        let error = self.error.as_ref().filter(|(ef, _)| *ef == f).map(|(_, msg)| msg.as_str());

        ass.text(
            rect.x,
            rect.y - 24.0,
            &TextStyle { size: 14.0, bold: true, color: TEXT_MUTED, spacing: 1.0, ..Default::default() },
            label,
        );

        let ring = match (error.is_some(), focused) {
            (true, _) => ERROR,
            (false, true) => ACCENT,
            (false, false) => FIELD_EDGE,
        };
        ass.rect(rect, 8.0, FIELD, Some((if focused || error.is_some() { 2.0 } else { 1.0 }, ring)), 0.0);

        let value = match f {
            Field::Name => &self.name,
            Field::Code => &self.code,
        };
        let mut body = escape(value);
        if focused {
            let caret = if self.caret_visible() { TEXT } else { TEXT.with_opacity(0) };
            body.push_str(&span("|", caret));
        }
        if value.is_empty() {
            body.push_str(&span(placeholder, TEXT_FAINT));
        }
        ass.text(rect.x + 18.0, rect.center().1, &style, &body);

        let (msg, color) = match error {
            Some(err) => (err, ERROR),
            None => (helper, TEXT_FAINT),
        };
        ass.text(rect.x, rect.y + rect.h + 10.0, &TextStyle { size: 15.0, color, ..Default::default() }, &escape(msg));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn join(name: &str, code: &str) -> Action {
        Action::Join { name: name.into(), code: code.into() }
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
        let l = Layout::new(1920.0, 1080.0);
        let mut m = JoinMenu::new("Kai");
        m.insert("room42");
        assert_eq!(m.click(l.field(Field::Name).center(), &l), Action::Redraw);
        assert_eq!(m.focus, Field::Name);
        assert_eq!(m.click(l.join.center(), &l), join("Kai", "ROOM42"));
        assert_eq!(m.click(l.cancel.center(), &l), Action::Close);
        assert_eq!(m.click((0.0, 0.0), &l), Action::None);
    }

    #[test]
    fn create_needs_only_a_name() {
        let l = Layout::new(1280.0, 720.0);
        let mut m = JoinMenu::new("");
        m.focus(Field::Code);
        m.insert("ab"); // too short to join, but irrelevant for creating
        assert_eq!(m.click(l.create.center(), &l), Action::Redraw);
        assert_eq!(m.focus, Field::Name);
        assert!(matches!(m.error, Some((Field::Name, _))));
        m.insert("Rio");
        assert_eq!(m.click(l.create.center(), &l), Action::Create { name: "Rio".into() });
    }

    #[test]
    fn create_button_hovers_and_does_not_overlap() {
        let l = Layout::new(1280.0, 720.0);
        assert!(l.create.x + l.create.w < l.cancel.x);
        let mut m = JoinMenu::new("");
        assert_eq!(m.mouse_move(Some(l.create.center()), &l), Action::Redraw);
        assert_eq!(m.hover, Hover::Create);
    }

    #[test]
    fn layout_fits_narrow_windows() {
        let l = Layout::new(400.0, 900.0);
        assert!(l.panel.x >= 0.0 && l.panel.x + l.panel.w <= l.res_x as f64);
        assert!(l.panel.y >= 0.0 && l.panel.y + l.panel.h <= l.res_y as f64);
    }

    #[test]
    fn layout_keeps_fields_above_buttons() {
        let l = Layout::new(1280.0, 720.0);
        let code = l.field(Field::Code);
        // Field + its message line must end before the buttons start.
        assert!(code.y + code.h + 10.0 + 20.0 <= l.join.y);
    }
}
