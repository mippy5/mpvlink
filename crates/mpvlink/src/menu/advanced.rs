//! The "Advanced settings" page (opened from Settings): the bitrate of your
//! stream and the STUN server. Like the Settings page, it covers what
//! --script-opts can set.

use super::{
    ACCENT, Action, ButtonKind, Caret, FIELD, FIELD_EDGE, FieldView, Hit, Layout, MONO_FONT, PASTE_HINT, TEXT,
    TEXT_FAINT, TEXT_MUTED, WARNING, draw_button, draw_field, draw_panel,
};
use crate::ass::{Ass, Rect, TextStyle};
use crate::bitrate::{self, Bitrate};
use crate::menu::settings::MAX_VALUE_CHARS;

/// How many characters of the STUN server fit in its field.
const VISIBLE_CHARS: usize = 36;
/// Long enough for any multiplier or bitrate.
const MAX_AMOUNT_CHARS: usize = 8;
/// The multiplier when none is typed.
const DEFAULT_TIMES: &str = "1.5";

/// The page's fields, in Tab order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdvancedField {
    /// Match source / × Source / Custom.
    Mode,
    /// The multiplier or the custom bitrate.
    Amount,
    Stun,
}

impl AdvancedField {
    const ALL: [AdvancedField; 3] = [AdvancedField::Mode, AdvancedField::Amount, AdvancedField::Stun];

    fn index(self) -> usize {
        self as usize
    }
}

/// How the stream's bitrate is chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitrateMode {
    Source,
    Times,
    Custom,
}

impl BitrateMode {
    const ALL: [BitrateMode; 3] = [BitrateMode::Source, BitrateMode::Times, BitrateMode::Custom];

    fn label(self) -> &'static str {
        match self {
            Self::Source => "Match source",
            Self::Times => "× Source",
            Self::Custom => "Custom",
        }
    }
}

/// Saved values ("" = the default): the bitrate as `6000`, `source` or
/// `1.5x`, and the STUN server (`no` for none).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AdvancedValues {
    pub bitrate: String,
    pub stun: String,
}

/// Values from --script-opts, which take precedence over saved ones.
#[derive(Clone, Debug, Default)]
pub struct AdvancedContext {
    pub bitrate_override: Option<String>,
    pub stun_override: Option<String>,
}

pub struct AdvancedMenu {
    mode: BitrateMode,
    /// What's typed for each mode that takes a number, kept apart so
    /// switching modes doesn't lose either.
    times: String,
    kbps: String,
    stun: String,
    context: AdvancedContext,
    focus: AdvancedField,
    error: Option<(AdvancedField, String)>,
    hover: Hit,
    pub(super) caret: Caret,
}

impl AdvancedMenu {
    pub const FIELDS: usize = AdvancedField::ALL.len();

    pub fn new(values: &AdvancedValues, context: AdvancedContext) -> Self {
        let (mode, times, kbps) = match Bitrate::parse(&values.bitrate) {
            Ok(Bitrate::SOURCE) => (BitrateMode::Source, String::new(), String::new()),
            Ok(Bitrate::Times(t)) => (BitrateMode::Times, bitrate::trim_number(t), String::new()),
            Ok(Bitrate::Kbps(k)) => (BitrateMode::Custom, String::new(), k.to_string()),
            Err(_) => (BitrateMode::Custom, String::new(), String::new()),
        };
        let mut menu = Self {
            mode,
            times,
            kbps,
            stun: String::new(),
            context,
            focus: AdvancedField::Mode,
            error: None,
            hover: Hit::None,
            caret: Caret::new(),
        };
        menu.insert_into(AdvancedField::Stun, &values.stun);
        menu
    }

    /// The text field holding the focused value, if it is one.
    fn text_mut(&mut self, f: AdvancedField) -> Option<&mut String> {
        match (f, self.mode) {
            (AdvancedField::Amount, BitrateMode::Times) => Some(&mut self.times),
            (AdvancedField::Amount, BitrateMode::Custom) => Some(&mut self.kbps),
            (AdvancedField::Stun, _) => Some(&mut self.stun),
            _ => None,
        }
    }

    /// Append text to a field: digits (and a point for a multiplier) for
    /// the amount, anything but spaces for the STUN server. On the bitrate
    /// choice, a space picks the next option.
    fn insert_into(&mut self, f: AdvancedField, text: &str) -> bool {
        if f == AdvancedField::Mode {
            return text == " " && self.step_mode(1);
        }
        let mode = self.mode;
        let Some(value) = self.text_mut(f) else { return false };
        let before = value.len();
        let (keep, max): (fn(&char) -> bool, usize) = match (f, mode) {
            (AdvancedField::Amount, BitrateMode::Times) => (|c| c.is_ascii_digit() || *c == '.', MAX_AMOUNT_CHARS),
            (AdvancedField::Amount, _) => (char::is_ascii_digit, MAX_AMOUNT_CHARS),
            _ => (|c| !c.is_control() && !c.is_whitespace(), MAX_VALUE_CHARS),
        };
        let room = max.saturating_sub(value.chars().count());
        value.extend(text.chars().filter(keep).take(room));
        value.len() != before
    }

    /// Move the bitrate choice by `by` (wrapping); whether it changed.
    fn step_mode(&mut self, by: isize) -> bool {
        let all = BitrateMode::ALL;
        let i = all.iter().position(|m| *m == self.mode).unwrap_or(0) as isize;
        self.set_mode(all[(i + by).rem_euclid(all.len() as isize) as usize])
    }

    fn set_mode(&mut self, mode: BitrateMode) -> bool {
        if mode == self.mode {
            return false;
        }
        self.mode = mode;
        if self.error.as_ref().is_some_and(|(f, _)| *f != AdvancedField::Stun) {
            self.error = None;
        }
        true
    }

    fn edited(&mut self) -> Action {
        if self.error.as_ref().is_some_and(|(f, _)| *f == self.focus) {
            self.error = None;
        }
        self.caret.touch();
        Action::Redraw
    }

    /// Fields that can take focus: no amount when matching the source.
    fn focusable(&self, f: AdvancedField) -> bool {
        !(f == AdvancedField::Amount && self.mode == BitrateMode::Source)
    }

    fn focus(&mut self, f: AdvancedField) -> Action {
        if f == self.focus || !self.focusable(f) {
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
        match self.text_mut(self.focus).and_then(String::pop) {
            Some(_) => self.edited(),
            None => Action::None,
        }
    }

    pub fn clear(&mut self) -> Action {
        match self.text_mut(self.focus) {
            Some(value) if !value.is_empty() => {
                value.clear();
                self.edited()
            }
            _ => Action::None,
        }
    }

    /// Left/Right arrows: change the bitrate choice when it has focus.
    pub fn arrow(&mut self, by: isize) -> Action {
        if self.focus == AdvancedField::Mode && self.step_mode(by) { self.edited() } else { Action::None }
    }

    fn focus_step(&mut self, by: usize) -> Action {
        let all = AdvancedField::ALL;
        let mut i = self.focus.index();
        loop {
            i = (i + by) % all.len();
            if self.focusable(all[i]) {
                return self.focus(all[i]);
            }
        }
    }

    pub fn focus_next(&mut self) -> Action {
        self.focus_step(1)
    }

    pub fn focus_prev(&mut self) -> Action {
        self.focus_step(AdvancedField::ALL.len() - 1)
    }

    fn reject(&mut self, f: AdvancedField, message: String) -> Action {
        self.error = Some((f, message));
        self.focus = f;
        self.caret.touch();
        Action::Redraw
    }

    /// Save if the values are valid. Empty means the default.
    pub fn submit(&mut self) -> Action {
        let bitrate = match self.mode {
            BitrateMode::Source => Bitrate::SOURCE.to_string(),
            BitrateMode::Times => {
                let typed = if self.times.is_empty() { DEFAULT_TIMES } else { &self.times };
                match bitrate::parse_multiplier(typed) {
                    Ok(t) => Bitrate::Times(t).to_string(),
                    Err(problem) => return self.reject(AdvancedField::Amount, problem),
                }
            }
            BitrateMode::Custom if self.kbps.is_empty() => String::new(),
            BitrateMode::Custom => match bitrate::parse_kbps(&self.kbps) {
                Ok(k) => k.to_string(),
                Err(problem) => return self.reject(AdvancedField::Amount, problem),
            },
        };
        let stun = match self.stun.trim() {
            "" => String::new(),
            s if s.eq_ignore_ascii_case("no") || s.eq_ignore_ascii_case("none") => "no".into(),
            s if s.starts_with("stun:") || s.starts_with("stuns:") => s.into(),
            s if s.contains("://") || s.starts_with("turn") => {
                return self.reject(AdvancedField::Stun, "Use a stun: address, e.g. stun:stun.example.com:3478.".into());
            }
            // "host:port" without the scheme.
            s => format!("stun:{s}"),
        };
        Action::SaveAdvanced { values: AdvancedValues { bitrate, stun } }
    }

    pub fn click(&mut self, hit: Hit, pos: (f64, f64), l: &Layout) -> Action {
        match hit {
            Hit::Primary => self.submit(),
            Hit::Cancel => Action::CloseAdvanced,
            // "Use default": clear the focused value (for the bitrate
            // choice, back to the default fixed bitrate).
            Hit::Left => {
                match self.focus {
                    AdvancedField::Mode => {
                        self.set_mode(BitrateMode::Custom);
                        self.kbps.clear();
                    }
                    f => {
                        if let Some(value) = self.text_mut(f) {
                            value.clear();
                        }
                    }
                }
                self.edited()
            }
            Hit::Field(0) => {
                let focused = self.focus(AdvancedField::Mode);
                let picked = segments(l.field(0)).iter().position(|s| s.contains(pos));
                match picked {
                    Some(i) if self.set_mode(BitrateMode::ALL[i]) => self.edited(),
                    _ => focused,
                }
            }
            Hit::Field(i) => AdvancedField::ALL.get(i).map_or(Action::None, |f| self.focus(*f)),
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

    fn error_for(&self, f: AdvancedField) -> Option<&str> {
        self.error.as_ref().filter(|(ef, _)| *ef == f).map(|(_, m)| m.as_str())
    }

    pub fn render(&self, l: &Layout) -> String {
        let mut ass = Ass::new();
        draw_panel(&mut ass, l, "Advanced settings", "A new bitrate applies right away; STUN when you next join.");
        let mono = || TextStyle { align: 4, size: 20.0, color: TEXT, font: Some(MONO_FONT), ..Default::default() };
        let faint = |s: &str| (s.to_string(), TEXT_FAINT);
        let overridden = |v: &str| (format!("Overridden by --script-opts: {v}"), WARNING);

        // The bitrate choice: three segments in a field-sized box.
        let mode = AdvancedField::Mode;
        let rect = l.field(mode.index());
        let focused = self.focus == mode;
        let label = TextStyle { size: 14.0, bold: true, color: TEXT_MUTED, spacing: 1.0, ..Default::default() };
        ass.text(rect.x, rect.y - 24.0, &label, "STREAM BITRATE");
        let ring = if focused { (2.0, ACCENT) } else { (1.0, FIELD_EDGE) };
        ass.rect(rect, 8.0, FIELD, Some(ring), 0.0);
        for (seg, m) in segments(rect).into_iter().zip(BitrateMode::ALL) {
            let chosen = m == self.mode;
            if chosen {
                ass.rect(seg, 6.0, ACCENT, None, 0.0);
            }
            let color = if chosen { TEXT } else { TEXT_MUTED };
            let (x, y) = seg.center();
            ass.text(x, y, &TextStyle { align: 5, size: 18.0, bold: chosen, color, ..Default::default() }, m.label());
        }
        let (helper, color) = match &self.context.bitrate_override {
            Some(v) => overridden(v),
            None if focused => faint("←/→ or Space to choose · Tab for the next field"),
            None => faint("Click to choose"),
        };
        let small = TextStyle { size: 15.0, color, ..Default::default() };
        ass.text(rect.x, rect.y + rect.h + 10.0, &small, &crate::ass::escape(&helper));

        // The multiplier or bitrate.
        let amount = AdvancedField::Amount;
        let (label, value, placeholder, helper) = match self.mode {
            BitrateMode::Source => (
                "BITRATE",
                "",
                "The source video's bitrate",
                "Scaled down for videos larger than 1080p (they're sent at 1080p).",
            ),
            BitrateMode::Times => (
                "MULTIPLIER",
                self.times.as_str(),
                DEFAULT_TIMES,
                "The source's bitrate times this. More than 1 makes up for H.264 vs HEVC/AV1.",
            ),
            BitrateMode::Custom => (
                "BITRATE (KB/S)",
                self.kbps.as_str(),
                "6000",
                "Kilobits per second, e.g. 6000 (the default) or 12000.",
            ),
        };
        let amount_focused = self.focus == amount;
        draw_field(
            &mut ass,
            l.field(amount.index()),
            &FieldView {
                label,
                value,
                placeholder,
                helper,
                helper_color: TEXT_FAINT,
                error: self.error_for(amount),
                focused: amount_focused,
                caret_visible: amount_focused && self.caret.visible(),
                style: mono(),
                max_visible_chars: None,
            },
        );

        let stun = AdvancedField::Stun;
        let (helper, helper_color) = match &self.context.stun_override {
            Some(v) => overridden(v),
            None => faint(&format!("\"no\" for none · {PASTE_HINT} · Enter to save")),
        };
        let placeholder = format!("{} (default)", mpvlink_rtc::peer::DEFAULT_STUN_SERVER);
        draw_field(
            &mut ass,
            l.field(stun.index()),
            &FieldView {
                label: "STUN SERVER (FOR CONNECTING THROUGH ROUTERS)",
                value: &self.stun,
                placeholder: &placeholder,
                helper: &helper,
                helper_color,
                error: self.error_for(stun),
                focused: self.focus == stun,
                caret_visible: self.focus == stun && self.caret.visible(),
                style: mono(),
                max_visible_chars: Some(VISIBLE_CHARS),
            },
        );

        draw_button(&mut ass, l.left, "Use default", ButtonKind::Secondary, self.hover == Hit::Left);
        draw_button(&mut ass, l.cancel, "Cancel", ButtonKind::Ghost, self.hover == Hit::Cancel);
        draw_button(&mut ass, l.primary, "Save", ButtonKind::Primary { enabled: true }, self.hover == Hit::Primary);
        ass.finish()
    }
}

/// The three segments of the bitrate choice inside its box.
fn segments(r: Rect) -> [Rect; 3] {
    const INSET: f64 = 5.0;
    const GAP: f64 = 4.0;
    let w = (r.w - 2.0 * INSET - 2.0 * GAP) / 3.0;
    std::array::from_fn(|i| Rect::new(r.x + INSET + i as f64 * (w + GAP), r.y + INSET, w, r.h - 2.0 * INSET))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> Layout {
        Layout::new(1280.0, 720.0, AdvancedMenu::FIELDS)
    }

    fn menu(bitrate: &str, stun: &str) -> AdvancedMenu {
        AdvancedMenu::new(&AdvancedValues { bitrate: bitrate.into(), stun: stun.into() }, AdvancedContext::default())
    }

    fn saved(bitrate: &str, stun: &str) -> Action {
        Action::SaveAdvanced { values: AdvancedValues { bitrate: bitrate.into(), stun: stun.into() } }
    }

    #[test]
    fn opens_with_the_saved_values() {
        assert_eq!(menu("", "").mode, BitrateMode::Custom);
        assert_eq!(menu("", "").submit(), saved("", ""), "defaults stay defaults");
        assert_eq!(menu("source", "no").submit(), saved("source", "no"));
        assert_eq!(menu("1.25x", "").times, "1.25");
        assert_eq!(menu("8000", "stun:a.example:3478").submit(), saved("8000", "stun:a.example:3478"));
    }

    #[test]
    fn keys_pick_the_mode_and_type_the_amount() {
        let mut m = menu("", "");
        assert_eq!(m.insert(" "), Action::Redraw, "space cycles");
        assert_eq!(m.mode, BitrateMode::Source);
        assert_eq!(m.focus_next(), Action::Redraw);
        assert_eq!(m.focus, AdvancedField::Stun, "no amount to type when matching the source");
        m.focus_prev();
        assert_eq!(m.arrow(1), Action::Redraw);
        assert_eq!(m.mode, BitrateMode::Times);
        assert_eq!(m.submit(), saved("1.5x", ""), "the default multiplier");
        m.focus_next();
        m.insert("2a.5x");
        assert_eq!(m.submit(), saved("2.5x", ""));
        m.focus_prev();
        m.arrow(1);
        m.focus_next();
        m.insert("12000");
        assert_eq!(m.submit(), saved("12000", ""));
        m.focus_prev();
        m.arrow(-1);
        assert_eq!(m.times, "2.5", "each mode keeps what was typed");
    }

    #[test]
    fn rejects_bad_amounts_and_stun_addresses() {
        let mut m = menu("50", "");
        m.focus_next();
        m.insert("50");
        assert_eq!(m.submit(), Action::Redraw);
        assert!(m.error_for(AdvancedField::Amount).is_some());
        assert!(m.render(&layout()).contains("Enter a bitrate"));
        m.clear();
        assert!(m.error.is_none());

        let mut m = menu("", "turn:relay.example");
        assert_eq!(m.submit(), Action::Redraw);
        assert_eq!(m.focus, AdvancedField::Stun);
        m.clear();
        m.insert("stun.example.com:3478");
        assert_eq!(m.submit(), saved("", "stun:stun.example.com:3478"), "the scheme is added");
    }

    #[test]
    fn clicks_pick_segments_and_buttons() {
        let l = layout();
        let mut m = menu("", "");
        let segs = segments(l.field(0));
        let pos = segs[0].center();
        assert_eq!(m.click(l.hit(pos), pos, &l), Action::Redraw);
        assert_eq!(m.mode, BitrateMode::Source);
        let pos = segs[1].center();
        m.click(l.hit(pos), pos, &l);
        assert_eq!(m.mode, BitrateMode::Times);
        // Use default with the choice focused: back to the fixed default.
        let pos = l.left.center();
        m.click(l.hit(pos), pos, &l);
        assert_eq!(m.submit(), saved("", ""));
        let pos = l.cancel.center();
        assert_eq!(m.click(l.hit(pos), pos, &l), Action::CloseAdvanced);
    }

    #[test]
    fn shows_overrides() {
        let context = AdvancedContext { bitrate_override: Some("source".into()), stun_override: Some("no".into()) };
        let ass = AdvancedMenu::new(&AdvancedValues::default(), context).render(&layout());
        assert!(ass.contains("Overridden by --script-opts: source"));
        assert!(ass.contains("Overridden by --script-opts: no"));
    }
}
