//! The plugin's menu: a panel drawn on the OSD with three pages, "Join a
//! room", "Settings" and "Advanced settings". Shared layout and drawing
//! widgets live here.
//!
//! This module is pure logic (no mpv calls) so it stays easy to test; the
//! plugin feeds it input and draws whatever `render` returns.

mod advanced;
mod join;
mod picker;
mod settings;

pub use advanced::{AdvancedContext, AdvancedMenu, AdvancedValues};
pub use picker::PickerMenu;
pub use join::JoinMenu;
pub use settings::{SettingsContext, SettingsField, SettingsMenu, SettingsValues};

use crate::ass::{Ass, Color, Rect, TextStyle, escape, span};
use std::time::Instant;

const CARET_BLINK_MS: u128 = 530;

// Palette.
pub(crate) const BACKDROP: Color = Color::rgb(0x000000).with_opacity(150);
pub(crate) const SHADOW: Color = Color::rgb(0x000000).with_opacity(140);
pub(crate) const PANEL: Color = Color::rgb(0x2B2D31);
pub(crate) const PANEL_EDGE: Color = Color::rgb(0x3F4147);
pub(crate) const FIELD: Color = Color::rgb(0x1E1F22);
pub(crate) const FIELD_EDGE: Color = Color::rgb(0x3F4147);
pub(crate) const ACCENT: Color = Color::rgb(0x5865F2);
pub(crate) const ACCENT_HOVER: Color = Color::rgb(0x4752C4);
pub(crate) const ACCENT_DISABLED: Color = Color::rgb(0x5865F2).with_opacity(110);
pub(crate) const GHOST_HOVER: Color = Color::rgb(0x3A3C42);
pub(crate) const SECONDARY: Color = Color::rgb(0x4E5058);
pub(crate) const SECONDARY_HOVER: Color = Color::rgb(0x6D6F78);
pub(crate) const TEXT: Color = Color::rgb(0xF2F3F5);
pub(crate) const TEXT_MUTED: Color = Color::rgb(0xB5BAC1);
pub(crate) const TEXT_FAINT: Color = Color::rgb(0x80848E);
pub(crate) const ERROR: Color = Color::rgb(0xFA777C);
pub(crate) const WARNING: Color = Color::rgb(0xF0B232);

#[cfg(target_os = "macos")]
const MONO_FONT: &str = "Menlo";
#[cfg(windows)]
const MONO_FONT: &str = "Consolas";
#[cfg(not(any(target_os = "macos", windows)))]
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
    /// Close the whole menu.
    Close,
    Join { name: String, code: String },
    /// Create a new room; the plugin picks the code.
    Create { name: String },
    /// Switch to the settings page (the plugin supplies the current values).
    OpenSettings,
    /// Leave the settings page without saving.
    CloseSettings,
    /// Save settings and go back. Empty values mean "use the default".
    SaveSettings { values: SettingsValues },
    /// Open AniList's page for getting a sign-in code (in the browser).
    OpenAniList,
    /// Switch from settings to the advanced page.
    OpenAdvanced,
    /// Back to the settings page without saving.
    CloseAdvanced,
    /// Save the advanced settings and go back to the settings page.
    SaveAdvanced { values: AdvancedValues },
    /// Match picker: search AniList for `query`.
    SearchShows { query: String },
    /// Match picker: what's playing is this show (None: don't track it).
    PickShow { media_id: Option<u32> },
}

// ---- Layout -----------------------------------------------------------------

/// Something under the mouse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    None,
    /// The main button at the bottom right (Join / Save).
    Primary,
    /// Cancel, left of the primary button.
    Cancel,
    /// The bottom-left button (Create room / Use default).
    Left,
    /// The small button at the top right (Settings / Advanced).
    Header,
    Field(usize),
}

/// Positions of everything, in overlay (virtual) pixels. Every page uses the
/// same panel structure: heading, `fields` stacked text fields, a button row.
pub struct Layout {
    pub res_x: u32,
    pub res_y: u32,
    /// OSD size in real pixels, for converting mouse coordinates.
    osd_w: f64,
    osd_h: f64,
    panel: Rect,
    fields: Vec<Rect>,
    primary: Rect,
    cancel: Rect,
    left: Rect,
    header: Rect,
}

impl Layout {
    const PANEL_W: f64 = 560.0;
    const PAD: f64 = 32.0;
    const FIELD_H: f64 = 56.0;
    /// Vertical distance from one field's top to the next field's top
    /// (field + message line + next label).
    const FIELD_PITCH: f64 = 120.0;
    const FIRST_FIELD_Y: f64 = 144.0;
    /// From the last field's bottom to the buttons: its message line + a gap.
    const FIELD_TO_BUTTONS: f64 = 44.0;
    const BUTTON_H: f64 = 44.0;

    /// Build a layout with `fields` text fields for an OSD of `osd_w` x `osd_h` real pixels.
    pub fn new(osd_w: f64, osd_h: f64, fields: usize) -> Self {
        let (osd_w, osd_h) = if osd_w > 0.0 && osd_h > 0.0 { (osd_w, osd_h) } else { (1280.0, 720.0) };
        let panel_h = Self::FIRST_FIELD_Y
            + Self::FIELD_PITCH * fields.saturating_sub(1) as f64
            + Self::FIELD_H
            + Self::FIELD_TO_BUTTONS
            + Self::BUTTON_H
            + Self::PAD;
        // 720 virtual pixels tall, but grow the virtual canvas (shrinking
        // everything a little) so the panel, plus a margin, always fits: on
        // narrow windows, and for pages with many fields.
        let res_y = 720.0_f64.max((Self::PANEL_W + 64.0) * osd_h / osd_w).max(panel_h + 40.0).round();
        let res_x = (res_y * osd_w / osd_h).round();
        let panel = Rect::new(
            ((res_x - Self::PANEL_W) / 2.0).round(),
            ((res_y - panel_h) / 2.0).round(),
            Self::PANEL_W,
            panel_h,
        );
        let inner_x = panel.x + Self::PAD;
        let inner_w = panel.w - 2.0 * Self::PAD;
        let fields = (0..fields)
            .map(|i| {
                let y = panel.y + Self::FIRST_FIELD_Y + i as f64 * Self::FIELD_PITCH;
                Rect::new(inner_x, y, inner_w, Self::FIELD_H)
            })
            .collect();
        let btn_y = panel.y + panel.h - Self::PAD - Self::BUTTON_H;
        let primary = Rect::new(panel.x + panel.w - Self::PAD - 112.0, btn_y, 112.0, Self::BUTTON_H);
        let cancel = Rect::new(primary.x - 8.0 - 96.0, btn_y, 96.0, Self::BUTTON_H);
        // Left-aligned, apart from Cancel/primary, since it's a different path.
        let left = Rect::new(inner_x, btn_y, 140.0, Self::BUTTON_H);
        let header = Rect::new(panel.x + panel.w - Self::PAD - 100.0, panel.y + Self::PAD - 4.0, 100.0, 36.0);

        Self { res_x: res_x as u32, res_y: res_y as u32, osd_w, osd_h, panel, fields, primary, cancel, left, header }
    }

    fn field(&self, i: usize) -> Rect {
        self.fields[i]
    }

    /// Convert a mouse position in real OSD pixels to overlay pixels.
    pub fn to_virtual(&self, x: f64, y: f64) -> (f64, f64) {
        (x * self.res_x as f64 / self.osd_w, y * self.res_y as f64 / self.osd_h)
    }

    /// What's at this position. Pages ignore hits on things they don't draw.
    pub fn hit(&self, pos: (f64, f64)) -> Hit {
        if self.primary.contains(pos) {
            Hit::Primary
        } else if self.cancel.contains(pos) {
            Hit::Cancel
        } else if self.left.contains(pos) {
            Hit::Left
        } else if self.header.contains(pos) {
            Hit::Header
        } else {
            self.fields.iter().position(|f| f.contains(pos)).map_or(Hit::None, Hit::Field)
        }
    }
}

// ---- Widgets ----------------------------------------------------------------

/// A blinking text caret. Reset on every edit or focus change so it's solid
/// while typing.
struct Caret {
    epoch: Instant,
}

impl Caret {
    fn new() -> Self {
        Self { epoch: Instant::now() }
    }

    fn touch(&mut self) {
        self.epoch = Instant::now();
    }

    fn visible(&self) -> bool {
        (self.epoch.elapsed().as_millis() / CARET_BLINK_MS).is_multiple_of(2)
    }

    /// Seconds until the caret next blinks, for the event loop's wait timeout.
    fn until_next_blink(&self) -> f64 {
        let ms = self.epoch.elapsed().as_millis();
        (CARET_BLINK_MS - ms % CARET_BLINK_MS) as f64 / 1000.0
    }
}

/// Backdrop, panel, title and subtitle.
fn draw_panel(ass: &mut Ass, l: &Layout, title: &str, subtitle: &str) {
    let full = Rect::new(0.0, 0.0, l.res_x as f64, l.res_y as f64);
    ass.rect(full, 0.0, BACKDROP, None, 0.0);
    ass.rect(Rect { y: l.panel.y + 8.0, ..l.panel }.grow(4.0), 20.0, SHADOW, None, 16.0);
    ass.rect(l.panel, 16.0, PANEL, Some((1.0, PANEL_EDGE)), 0.0);

    let x = l.panel.x + Layout::PAD;
    let y = l.panel.y + Layout::PAD;
    ass.text(x, y, &TextStyle { size: 30.0, bold: true, color: TEXT, ..Default::default() }, &escape(title));
    ass.text(x, y + 44.0, &TextStyle { size: 19.0, color: TEXT_MUTED, ..Default::default() }, &escape(subtitle));
}

/// How a text field should be drawn.
struct FieldView<'a> {
    label: &'a str,
    value: &'a str,
    placeholder: &'a str,
    /// Shown under the field when there's no error.
    helper: &'a str,
    helper_color: Color,
    error: Option<&'a str>,
    focused: bool,
    caret_visible: bool,
    style: TextStyle<'a>,
    /// If the value is longer than this many characters, show only its end
    /// (where the caret is), prefixed with an ellipsis.
    max_visible_chars: Option<usize>,
}

/// Label, box (ring shows focus/error), value with caret or placeholder, and a
/// helper line that turns into the error message when there is one.
fn draw_field(ass: &mut Ass, rect: Rect, f: &FieldView) {
    ass.text(
        rect.x,
        rect.y - 24.0,
        &TextStyle { size: 14.0, bold: true, color: TEXT_MUTED, spacing: 1.0, ..Default::default() },
        f.label,
    );

    let ring = match (f.error.is_some(), f.focused) {
        (true, _) => ERROR,
        (false, true) => ACCENT,
        (false, false) => FIELD_EDGE,
    };
    let ring_width = if f.focused || f.error.is_some() { 2.0 } else { 1.0 };
    ass.rect(rect, 8.0, FIELD, Some((ring_width, ring)), 0.0);

    let shown = match f.max_visible_chars {
        Some(max) if f.value.chars().count() > max => {
            let skip = f.value.chars().count() - (max - 1);
            format!("…{}", f.value.chars().skip(skip).collect::<String>())
        }
        _ => f.value.to_string(),
    };
    let mut body = escape(&shown);
    if f.focused {
        // Accent-colored so it can't be mistaken for an "l" or "I" in the text.
        let caret = if f.caret_visible { ACCENT } else { ACCENT.with_opacity(0) };
        body.push_str(&span("|", caret));
    }
    if f.value.is_empty() {
        body.push_str(&span(f.placeholder, TEXT_FAINT));
    }
    ass.text(rect.x + 18.0, rect.center().1, &f.style, &body);

    let (msg, color) = match f.error {
        Some(err) => (err, ERROR),
        None => (f.helper, f.helper_color),
    };
    ass.text(rect.x, rect.y + rect.h + 10.0, &TextStyle { size: 15.0, color, ..Default::default() }, &escape(msg));
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ButtonKind {
    /// Filled accent color; dimmed when disabled.
    Primary { enabled: bool },
    /// Filled gray; dimmed when disabled.
    Secondary { enabled: bool },
    /// Outlined, so it reads as a button where a filled one would be too
    /// loud (the small Settings / Advanced buttons at the top).
    Outline,
    /// Text only; background on hover.
    Ghost,
}

fn draw_button(ass: &mut Ass, rect: Rect, label: &str, kind: ButtonKind, hovered: bool) {
    let (fill, text) = match kind {
        ButtonKind::Primary { enabled: false } => (Some(ACCENT_DISABLED), TEXT.with_opacity(140)),
        ButtonKind::Primary { enabled: true } => (Some(if hovered { ACCENT_HOVER } else { ACCENT }), TEXT),
        ButtonKind::Secondary { enabled: false } => (Some(SECONDARY.with_opacity(110)), TEXT.with_opacity(140)),
        ButtonKind::Secondary { enabled: true } => (Some(if hovered { SECONDARY_HOVER } else { SECONDARY }), TEXT),
        ButtonKind::Outline => (Some(if hovered { GHOST_HOVER } else { PANEL }), TEXT),
        ButtonKind::Ghost => (hovered.then_some(GHOST_HOVER), TEXT),
    };
    let outline = (kind == ButtonKind::Outline).then_some((1.5, SECONDARY_HOVER));
    if let Some(fill) = fill {
        ass.rect(rect, 8.0, fill, outline, 0.0);
    }
    let (x, y) = rect.center();
    ass.text(x, y, &TextStyle { align: 5, size: 18.0, bold: true, color: text, ..Default::default() }, label);
}

// ---- The menu ---------------------------------------------------------------

/// The open menu: the join page, with the settings page on top when open,
/// and the advanced page on top of that. Leaving a page returns to the one
/// below with its fields as they were.
pub struct Menu {
    join: JoinMenu,
    settings: Option<SettingsMenu>,
    advanced: Option<AdvancedMenu>,
    /// The AniList match picker: opened on its own (from playback), on top
    /// of everything while it's open.
    picker: Option<PickerMenu>,
}

/// Forward a call to whichever page is showing.
macro_rules! page {
    ($self:ident.$method:ident($($arg:expr),*)) => {
        match (&mut $self.picker, &mut $self.advanced, &mut $self.settings) {
            (Some(picker), _, _) => picker.$method($($arg),*),
            (None, Some(advanced), _) => advanced.$method($($arg),*),
            (None, None, Some(settings)) => settings.$method($($arg),*),
            (None, None, None) => $self.join.$method($($arg),*),
        }
    };
}

impl Menu {
    /// `needs_server`: no signaling server is set yet, so joining waits
    /// until one is (the join page sends people to Settings).
    pub fn new(name: &str, needs_server: bool) -> Self {
        Self { join: JoinMenu::new(name, needs_server), settings: None, advanced: None, picker: None }
    }

    /// Show the AniList match picker; the search it opens with.
    pub fn open_picker(&mut self, mut picker: PickerMenu) -> Action {
        let search = picker.first_search();
        self.picker = Some(picker);
        search
    }

    /// Search results for the picker.
    pub fn set_results(&mut self, query: &str, results: Vec<crate::tracking::Choice>) {
        if let Some(p) = self.picker.as_mut() {
            p.set_results(query, results);
        }
    }

    /// A signaling server was saved (or cleared).
    pub fn set_needs_server(&mut self, needs: bool) {
        self.join.set_needs_server(needs);
    }

    /// The display name as typed so far (untrimmed).
    pub fn name(&self) -> &str {
        self.join.name()
    }

    /// Show the settings page with the saved `values` ("" = default).
    pub fn open_settings(&mut self, values: &SettingsValues, context: SettingsContext) {
        self.settings = Some(SettingsMenu::new(values, context));
    }

    /// The plugin found a problem with a setting: show it, stay on the page.
    pub fn reject_setting(&mut self, field: SettingsField, message: String) {
        if let Some(settings) = self.settings.as_mut() {
            settings.reject(field, message);
        }
    }

    pub fn close_settings(&mut self) {
        self.settings = None;
        self.advanced = None;
    }

    /// Show the advanced page with the saved `values` ("" = default).
    pub fn open_advanced(&mut self, values: &AdvancedValues, context: AdvancedContext) {
        if self.settings.is_some() {
            self.advanced = Some(AdvancedMenu::new(values, context));
        }
    }

    /// Back to the settings page.
    pub fn close_advanced(&mut self) {
        self.advanced = None;
    }

    pub fn in_settings(&self) -> bool {
        self.settings.is_some()
    }

    /// Text fields on the current page, for the layout.
    pub fn field_count(&self) -> usize {
        if self.picker.is_some() {
            return PickerMenu::FIELDS;
        }
        match (&self.advanced, &self.settings) {
            (Some(_), _) => AdvancedMenu::FIELDS,
            (None, Some(_)) => SettingsMenu::FIELDS,
            (None, None) => JoinMenu::FIELDS,
        }
    }

    pub fn insert(&mut self, text: &str) -> Action {
        page!(self.insert(text))
    }

    pub fn backspace(&mut self) -> Action {
        page!(self.backspace())
    }

    pub fn clear(&mut self) -> Action {
        page!(self.clear())
    }

    pub fn focus_next(&mut self) -> Action {
        page!(self.focus_next())
    }

    pub fn focus_prev(&mut self) -> Action {
        page!(self.focus_prev())
    }

    /// Enter.
    pub fn submit(&mut self) -> Action {
        page!(self.submit())
    }

    /// Left (-1) or Right (+1) arrow: only the advanced page's bitrate
    /// choice uses them.
    pub fn arrow(&mut self, by: isize) -> Action {
        if self.picker.is_some() {
            return Action::None;
        }
        self.advanced.as_mut().map_or(Action::None, |a| a.arrow(by))
    }

    /// Up (-1) or Down (+1) arrow: the picker's results.
    pub fn vertical(&mut self, by: isize) -> Action {
        self.picker.as_mut().map_or(Action::None, |p| p.move_selection(by))
    }

    /// Esc: back out of the advanced page or settings, or close the menu
    /// (the picker was opened on its own, so it closes the menu).
    pub fn escape(&mut self) -> Action {
        if self.picker.is_some() {
            return Action::Close;
        }
        match (&self.advanced, &self.settings) {
            (Some(_), _) => Action::CloseAdvanced,
            (None, Some(_)) => Action::CloseSettings,
            (None, None) => Action::Close,
        }
    }

    pub fn click(&mut self, pos: (f64, f64), layout: &Layout) -> Action {
        let hit = layout.hit(pos);
        if let Some(picker) = self.picker.as_mut() {
            return picker.click(hit, pos, layout);
        }
        match (&mut self.advanced, &mut self.settings) {
            (Some(advanced), _) => advanced.click(hit, pos, layout),
            (None, Some(settings)) => settings.click(hit),
            (None, None) => self.join.click(hit),
        }
    }

    pub fn mouse_move(&mut self, pos: Option<(f64, f64)>, layout: &Layout) -> Action {
        let hit = pos.map_or(Hit::None, |p| layout.hit(p));
        if let Some(picker) = self.picker.as_mut() {
            return picker.hover(hit, pos, layout);
        }
        match (&mut self.advanced, &mut self.settings) {
            (Some(advanced), _) => advanced.hover(hit),
            (None, Some(settings)) => settings.hover(hit),
            (None, None) => self.join.hover(hit),
        }
    }

    fn caret(&self) -> &Caret {
        if let Some(p) = &self.picker {
            return &p.caret;
        }
        match (&self.advanced, &self.settings) {
            (Some(a), _) => &a.caret,
            (None, Some(s)) => &s.caret,
            (None, None) => &self.join.caret,
        }
    }

    pub fn caret_visible(&self) -> bool {
        self.caret().visible()
    }

    pub fn until_next_blink(&self) -> f64 {
        self.caret().until_next_blink()
    }

    pub fn render(&self, layout: &Layout) -> String {
        if let Some(p) = &self.picker {
            return p.render(layout);
        }
        match (&self.advanced, &self.settings) {
            (Some(a), _) => a.render(layout),
            (None, Some(s)) => s.render(layout),
            (None, None) => self.join.render(layout),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_fits_narrow_windows() {
        for fields in [1, 2, 3, 4, 5, 6] {
            let l = Layout::new(400.0, 900.0, fields);
            assert!(l.panel.x >= 0.0 && l.panel.x + l.panel.w <= l.res_x as f64);
            assert!(l.panel.y >= 0.0 && l.panel.y + l.panel.h <= l.res_y as f64);
        }
    }

    #[test]
    fn layout_keeps_fields_above_buttons_and_buttons_apart() {
        for fields in [1, 2, 3, 4, 5, 6] {
            let l = Layout::new(1280.0, 720.0, fields);
            let last = l.field(fields - 1);
            // Field + its message line must end before the buttons start.
            assert!(last.y + last.h + 10.0 + 20.0 <= l.primary.y);
            assert!(l.left.x + l.left.w < l.cancel.x);
            assert!(l.header.y + l.header.h < l.field(0).y - 24.0, "header button clears the first label");
        }
    }

    #[test]
    fn settings_page_round_trip_keeps_join_fields() {
        let mut m = Menu::new("Ana", false);
        m.insert("room1");
        assert_eq!(m.field_count(), 2);
        let values = SettingsValues { server: "wss://old.example".into(), ..Default::default() };
        m.open_settings(&values, SettingsContext { ffmpeg_found: Err(String::new()), ..Default::default() });
        assert_eq!(m.field_count(), 3);
        m.insert("/x"); // goes to the settings field, not the room code
        m.open_advanced(&AdvancedValues::default(), AdvancedContext::default());
        assert_eq!(m.field_count(), 6);
        assert_eq!(m.escape(), Action::CloseAdvanced);
        m.close_advanced();
        assert_eq!(m.escape(), Action::CloseSettings);
        m.close_settings();
        assert_eq!(m.escape(), Action::Close);
        assert_eq!(m.submit(), Action::Join { name: "Ana".into(), code: "ROOM1".into() });
    }
}
