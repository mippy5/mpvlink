//! The "AniList match" page: change which show what's playing is tracked
//! as. Search AniList (prefilled with the title read from the file name),
//! then pick a result, or say it isn't anime ("Don't track").
//!
//! It uses the shared three-field layout: the search box in the first
//! field's place, and the results in the space of the other two.

use super::{
    ACCENT, Action, ButtonKind, Caret, FieldView, Hit, Layout, PASTE_HINT, TEXT, TEXT_FAINT, TEXT_MUTED, draw_button,
    draw_field, draw_panel,
};
use crate::ass::{Ass, Color, Rect, TextStyle, escape};
use crate::tracking::Choice;

const MAX_QUERY_CHARS: usize = 100;
/// Results shown (AniList returns up to 10).
const MAX_ROWS: usize = 5;
const ROW_H: f64 = 44.0;
const ROW_HOVER: Color = Color::rgb(0xFFFFFF).with_opacity(18);
const ROW_SELECTED: Color = ACCENT.with_opacity(90);

pub struct PickerMenu {
    query: String,
    /// The file playing (its name), and the show it's matched to now.
    file: String,
    current: Option<(u32, String)>,
    results: Vec<Choice>,
    /// The search whose results are shown, or that's running.
    searched: Option<String>,
    searching: bool,
    selected: Option<usize>,
    hover: Hit,
    hover_row: Option<usize>,
    pub(super) caret: Caret,
}

impl PickerMenu {
    pub const FIELDS: usize = 3;

    /// `title`: what the file name was read as (the first search).
    /// `current`: the show it's matched to now, if any (id, title).
    pub fn new(file: &str, title: &str, current: Option<(u32, String)>) -> Self {
        let mut menu = Self {
            query: String::new(),
            file: file.to_string(),
            current,
            results: Vec::new(),
            searched: None,
            searching: false,
            selected: None,
            hover: Hit::None,
            hover_row: None,
            caret: Caret::new(),
        };
        menu.insert(title);
        menu
    }

    /// The search for the title it opened with (the plugin runs it right away).
    pub fn first_search(&mut self) -> Action {
        self.search()
    }

    fn search(&mut self) -> Action {
        let query = self.query.trim().to_string();
        if query.is_empty() {
            return Action::None;
        }
        self.searched = Some(query.clone());
        self.searching = true;
        Action::SearchShows { query }
    }

    /// Results arrived (ignored if they're for an older search).
    pub fn set_results(&mut self, query: &str, results: Vec<Choice>) {
        if self.searched.as_deref() != Some(query) {
            return;
        }
        self.searching = false;
        self.results = results.into_iter().take(MAX_ROWS).collect();
        self.selected = None;
    }

    pub fn insert(&mut self, text: &str) -> Action {
        let room = MAX_QUERY_CHARS.saturating_sub(self.query.chars().count());
        let before = self.query.len();
        self.query.extend(text.chars().filter(|c| !c.is_control()).take(room));
        if self.query.len() == before {
            return Action::None;
        }
        self.caret.touch();
        Action::Redraw
    }

    pub fn backspace(&mut self) -> Action {
        if self.query.pop().is_none() {
            return Action::None;
        }
        self.caret.touch();
        Action::Redraw
    }

    pub fn clear(&mut self) -> Action {
        if self.query.is_empty() {
            return Action::None;
        }
        self.query.clear();
        self.caret.touch();
        Action::Redraw
    }

    // One field: Tab does nothing.
    pub fn focus_next(&mut self) -> Action {
        Action::None
    }

    pub fn focus_prev(&mut self) -> Action {
        Action::None
    }

    /// Up/Down: move through the results.
    pub fn move_selection(&mut self, by: isize) -> Action {
        if self.results.is_empty() {
            return Action::None;
        }
        let n = self.results.len() as isize;
        let next = match self.selected {
            None if by > 0 => 0,
            None => n - 1,
            Some(i) => (i as isize + by).rem_euclid(n),
        };
        self.selected = Some(next as usize);
        Action::Redraw
    }

    /// Enter: pick the highlighted result, else search for what's typed.
    pub fn submit(&mut self) -> Action {
        // A changed query searches again, even with a result highlighted.
        let changed = self.searched.as_deref() != Some(self.query.trim());
        match self.selected.and_then(|i| self.results.get(i)) {
            Some(choice) if !changed => Action::PickShow { media_id: Some(choice.id) },
            _ => self.search(),
        }
    }

    /// Where the results go: the space of the second and third fields.
    fn rows_area(l: &Layout) -> Rect {
        let (a, b) = (l.field(1), l.field(2));
        let top = a.y - 24.0;
        Rect::new(a.x, top, a.w, b.y + b.h + 28.0 - top)
    }

    fn row(l: &Layout, i: usize) -> Rect {
        let area = Self::rows_area(l);
        Rect::new(area.x - 8.0, area.y + i as f64 * ROW_H, area.w + 16.0, ROW_H - 2.0)
    }

    fn row_at(&self, l: &Layout, pos: (f64, f64)) -> Option<usize> {
        (0..self.results.len()).find(|i| Self::row(l, *i).contains(pos))
    }

    pub fn click(&mut self, hit: Hit, pos: (f64, f64), l: &Layout) -> Action {
        if let Some(i) = self.row_at(l, pos) {
            return Action::PickShow { media_id: Some(self.results[i].id) };
        }
        match hit {
            Hit::Primary => self.submit(),
            Hit::Cancel => Action::Close,
            Hit::Left => Action::PickShow { media_id: None },
            _ => Action::None,
        }
    }

    pub fn hover(&mut self, hit: Hit, pos: Option<(f64, f64)>, l: &Layout) -> Action {
        let row = pos.and_then(|p| self.row_at(l, p));
        let hit = if row.is_none() && matches!(hit, Hit::Primary | Hit::Cancel | Hit::Left) { hit } else { Hit::None };
        if hit == self.hover && row == self.hover_row {
            return Action::None;
        }
        (self.hover, self.hover_row) = (hit, row);
        Action::Redraw
    }

    pub fn render(&self, l: &Layout) -> String {
        let mut ass = Ass::new();
        let subtitle = format!("Playing: {}", elide(&self.file, 52));
        draw_panel(&mut ass, l, "AniList match", &subtitle);

        let helper = match &self.current {
            Some((_, title)) => format!("Now: {} · Enter to search", elide(title, 40)),
            None => format!("Not matched to a show · Enter to search · {PASTE_HINT}"),
        };
        draw_field(
            &mut ass,
            l.field(0),
            &FieldView {
                label: "SEARCH ANILIST",
                value: &self.query,
                placeholder: "Show title",
                helper: &helper,
                helper_color: TEXT_FAINT,
                error: None,
                focused: true,
                caret_visible: self.caret.visible(),
                style: TextStyle { align: 4, size: 22.0, color: TEXT, ..Default::default() },
                max_visible_chars: Some(40),
            },
        );

        let area = Self::rows_area(l);
        let note = |ass: &mut Ass, text: &str| {
            let style = TextStyle { align: 4, size: 16.0, color: TEXT_MUTED, ..Default::default() };
            ass.text(area.x, area.y + ROW_H / 2.0, &style, text);
        };
        if self.searching {
            note(&mut ass, "Searching…");
        } else if self.searched.is_some() && self.results.is_empty() {
            note(&mut ass, "Nothing found. Try another title (English or Japanese).");
        }
        for (i, choice) in self.results.iter().enumerate() {
            let row = Self::row(l, i);
            if self.selected == Some(i) {
                ass.rect(row, 6.0, ROW_SELECTED, None, 0.0);
            } else if self.hover_row == Some(i) {
                ass.rect(row, 6.0, ROW_HOVER, None, 0.0);
            }
            let is_current = self.current.as_ref().is_some_and(|(id, _)| *id == choice.id);
            let mark = if is_current { "✓ " } else { "" };
            let clip = Some(Rect::new(row.x, row.y, row.w - 8.0, row.h));
            let title = TextStyle { align: 4, size: 17.0, bold: true, color: TEXT, clip, ..Default::default() };
            ass.text(row.x + 10.0, row.y + 14.0, &title, &escape(&format!("{mark}{}", choice.title)));
            let detail = TextStyle { align: 4, size: 13.0, color: TEXT_FAINT, clip, ..Default::default() };
            ass.text(row.x + 10.0, row.y + 32.0, &detail, &escape(&choice.detail));
        }

        let choosing = self.selected.is_some() && self.searched.as_deref() == Some(self.query.trim());
        draw_button(&mut ass, l.left, "Don't track", ButtonKind::Secondary { enabled: true }, self.hover == Hit::Left);
        draw_button(&mut ass, l.cancel, "Cancel", ButtonKind::Ghost, self.hover == Hit::Cancel);
        let primary = if choosing { "Choose" } else { "Search" };
        draw_button(&mut ass, l.primary, primary, ButtonKind::Primary { enabled: true }, self.hover == Hit::Primary);
        ass.finish()
    }
}

/// `text`, cut to `max` characters with an ellipsis.
fn elide(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max - 1).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(id: u32, title: &str) -> Choice {
        Choice { id, title: title.into(), detail: "TV · 2016 · 25 episodes".into() }
    }

    fn layout() -> Layout {
        Layout::new(1280.0, 720.0, PickerMenu::FIELDS)
    }

    #[test]
    fn search_then_pick() {
        let l = layout();
        let mut m = PickerMenu::new("Ansatsu Kyoushitsu 2x14.mkv", "Ansatsu Kyoushitsu", Some((20755, "Assassination Classroom".into())));
        assert_eq!(m.first_search(), Action::SearchShows { query: "Ansatsu Kyoushitsu".into() });
        assert!(m.render(&l).contains("Searching…"));
        m.set_results("something older", vec![choice(1, "Stale")]);
        assert!(m.results.is_empty(), "results for an older search are dropped");
        m.set_results("Ansatsu Kyoushitsu", vec![choice(20755, "Assassination Classroom"), choice(21170, "Second Season")]);
        let ass = m.render(&l);
        assert!(ass.contains("✓ Assassination Classroom"), "the current match is marked");

        // Arrow down twice, Enter: the second result.
        m.move_selection(1);
        m.move_selection(1);
        assert_eq!(m.submit(), Action::PickShow { media_id: Some(21170) });
        // Typing changes the query: Enter searches again instead.
        m.insert(" 2");
        assert_eq!(m.submit(), Action::SearchShows { query: "Ansatsu Kyoushitsu 2".into() });
    }

    #[test]
    fn clicks() {
        let l = layout();
        let mut m = PickerMenu::new("x.mkv", "X", None);
        m.first_search();
        m.set_results("X", vec![choice(7, "X"), choice(8, "X 2")]);
        let pos = PickerMenu::row(&l, 1).center();
        assert_eq!(m.click(l.hit(pos), pos, &l), Action::PickShow { media_id: Some(8) });
        let pos = l.left.center();
        assert_eq!(m.click(l.hit(pos), pos, &l), Action::PickShow { media_id: None }, "Don't track");
        let pos = l.cancel.center();
        assert_eq!(m.click(l.hit(pos), pos, &l), Action::Close);
    }

    #[test]
    fn rows_fit_between_the_field_and_the_buttons() {
        let l = layout();
        let last = PickerMenu::row(&l, MAX_ROWS - 1);
        assert!(last.y + last.h <= l.primary.y, "rows end above the buttons");
        assert!(PickerMenu::row(&l, 0).y >= l.field(0).y + l.field(0).h + 20.0, "and below the search field's helper");
    }
}
