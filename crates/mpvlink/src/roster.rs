//! The member list shown in the top-left corner while you're in a room: a
//! small semi-transparent box with the room code and everyone in it. An
//! arrow marks whose stream you're watching; clicking a name moves it.
//!
//! Pure drawing (no mpv calls); the plugin puts the result in an osd-overlay
//! and uses the returned row areas for clicks.

use crate::ass::{Ass, Color, Rect, TextStyle, escape, span};
use crate::menu::{ACCENT, TEXT, TEXT_FAINT, TEXT_MUTED, WARNING};
use crate::session::{Member, RoomState};
use mpvlink_rtc::proto::PeerId;

const BOX: Color = Color::rgb(0x1E1F22).with_opacity(165);
const HOVER: Color = Color::rgb(0xFFFFFF).with_opacity(24);
const ONLINE: Color = Color::rgb(0x23A55A);
const LIVE: Color = Color::rgb(0xED4245);

/// Layout in virtual pixels, on a canvas 720 high (so the box scales with the
/// window, like mpv's own OSD).
const RES_Y: f64 = 720.0;
const MARGIN: f64 = 16.0;
const WIDTH: f64 = 236.0;
const PAD_X: f64 = 12.0;
const PAD_Y: f64 = 8.0;
const HEADER_H: f64 = 24.0;
const ROW_H: f64 = 26.0;
const NOTE_H: f64 = 22.0;
const NAME_SIZE: f64 = 16.0;
/// Columns: arrow, status dot, then the name.
const DOT_X: f64 = 14.0;
const NAME_X: f64 = 28.0;
const LIVE_W: f64 = 36.0;
/// Past this many people, the rest are summarized as "+N more".
const MAX_ROWS: usize = 10;

pub struct Overlay {
    pub data: String,
    pub res_x: u32,
    pub res_y: u32,
    /// The whole box, in overlay coordinates.
    pub frame: Rect,
    /// Each visible member's row, for clicks.
    pub rows: Vec<(PeerId, Rect)>,
}

impl Overlay {
    /// The member whose row is at `pos` (overlay coordinates).
    pub fn row_at(&self, pos: (f64, f64)) -> Option<PeerId> {
        self.rows.iter().find(|(_, r)| r.contains(pos)).map(|(id, _)| *id)
    }
}

/// Draw the box for `room` on an OSD of `osd_w` x `osd_h` real pixels, with
/// `hover`'s row highlighted.
pub fn render(room: &str, state: &RoomState, hover: Option<PeerId>, osd_w: f64, osd_h: f64) -> Overlay {
    let (osd_w, osd_h) = if osd_w > 0.0 && osd_h > 0.0 { (osd_w, osd_h) } else { (1280.0, 720.0) };
    let res_x = (RES_Y * osd_w / osd_h).round();

    let shown = state.members.len().min(MAX_ROWS);
    let hidden = state.members.len() - shown;
    // "Connecting…" stands in for the list until the server welcomes us.
    let rows = if state.members.is_empty() { 1 } else { shown };
    let hint = drop_hint(state);
    let notes = usize::from(hidden > 0) + usize::from(state.reconnecting) + usize::from(hint.is_some());
    let height = PAD_Y * 2.0 + HEADER_H + rows as f64 * ROW_H + notes as f64 * NOTE_H;
    let frame = Rect::new(MARGIN, MARGIN, WIDTH, height);
    let inner = Rect::new(frame.x + PAD_X, frame.y, frame.w - 2.0 * PAD_X, frame.h);

    let mut ass = Ass::new();
    ass.rect(frame, 8.0, BOX, None, 0.0);

    // Header: room code, and how many people are in it.
    let header_y = frame.y + PAD_Y + HEADER_H / 2.0;
    let small = |align, color| TextStyle { align, size: 12.0, bold: true, spacing: 1.0, color, ..Default::default() };
    ass.text(inner.x, header_y, &small(4, TEXT_MUTED), &escape(&format!("ROOM {room}")));
    if !state.members.is_empty() {
        let count = state.members.len().to_string();
        ass.text(inner.x + inner.w, header_y, &small(6, TEXT_FAINT), &count);
    }

    let mut y = frame.y + PAD_Y + HEADER_H;
    if state.members.is_empty() {
        let style = TextStyle { align: 4, size: 14.0, color: TEXT_MUTED, ..Default::default() };
        ass.text(inner.x, y + ROW_H / 2.0, &style, "Connecting…");
    }
    let mut row_areas = Vec::new();
    for member in &state.members[..shown] {
        let row = Rect::new(frame.x + 4.0, y + 1.0, frame.w - 8.0, ROW_H - 2.0);
        if hover == Some(member.id) {
            ass.rect(row, 5.0, HOVER, None, 0.0);
        }
        let targeted = state.target == Some(member.id);
        draw_member(&mut ass, member, inner, y + ROW_H / 2.0, targeted, state.reconnecting);
        row_areas.push((member.id, row));
        y += ROW_H;
    }

    let note = |color| TextStyle { align: 4, size: 13.0, color, ..Default::default() };
    if hidden > 0 {
        ass.text(inner.x, y + NOTE_H / 2.0, &note(TEXT_FAINT), &format!("+{hidden} more"));
        y += NOTE_H;
    }
    if let Some(hint) = hint {
        ass.text(inner.x, y + NOTE_H / 2.0, &note(TEXT_FAINT), hint);
        y += NOTE_H;
    }
    if state.reconnecting {
        ass.text(inner.x, y + NOTE_H / 2.0, &note(WARNING), "Reconnecting to the server…");
    }

    Overlay { data: ass.finish(), res_x: res_x as u32, res_y: RES_Y as u32, frame, rows: row_areas }
}

/// Shown while the arrow points at you and you're not streaming yet.
fn drop_hint(state: &RoomState) -> Option<&'static str> {
    (state.targets_me() && state.me().is_some_and(|m| !m.streaming)).then_some("Drop a video here to stream it")
}

/// One row: arrow (if watching them), status dot, name (elided to fit),
/// "(you)", and a LIVE tag for anyone streaming. `cy` is the row's center.
fn draw_member(ass: &mut Ass, member: &Member, inner: Rect, cy: f64, targeted: bool, stale: bool) {
    if targeted {
        let (x, y) = (inner.x, cy - 5.0);
        ass.shape(&format!("m {x:.1} {y:.1} l {:.1} {:.1} l {x:.1} {:.1}", x + 8.0, y + 5.0, y + 10.0), ACCENT);
    }
    // Grey dots while reconnecting: the list may be out of date.
    let dot = if stale { TEXT_FAINT } else { ONLINE };
    ass.rect(Rect::new(inner.x + DOT_X, cy - 4.0, 8.0, 8.0), 4.0, dot, None, 0.0);

    let you = if member.you { " (you)" } else { "" };
    let name_x = inner.x + NAME_X;
    let mut room_for_name = inner.x + inner.w - name_x - text_width(you, NAME_SIZE);
    if member.streaming {
        room_for_name -= LIVE_W + 8.0;
    }
    let name = elide(&member.name, NAME_SIZE, room_for_name);
    let body = escape(&name) + &if you.is_empty() { String::new() } else { span(you, TEXT_FAINT) };
    // The width estimate is approximate; clipping keeps any overshoot inside the box.
    let clip = Rect::new(name_x, cy - ROW_H / 2.0, inner.x + inner.w - name_x, ROW_H);
    let color = if targeted { TEXT } else { TEXT_MUTED };
    let style = TextStyle { align: 4, size: NAME_SIZE, color, bold: targeted, clip: Some(clip), ..Default::default() };
    ass.text(name_x, cy, &style, &body);

    if member.streaming {
        let tag = Rect::new(inner.x + inner.w - LIVE_W, cy - 8.0, LIVE_W, 16.0);
        ass.rect(tag, 4.0, LIVE, None, 0.0);
        let (tx, ty) = tag.center();
        let style = TextStyle { align: 5, size: 11.0, bold: true, spacing: 0.5, color: TEXT, ..Default::default() };
        ass.text(tx, ty, &style, "LIVE");
    }
}

/// Rough rendered width of `text` at font size `size`. There's no way to
/// measure text from here, so this uses typical proportional-font widths
/// (calibrated against mpv's default OSD font, erring slightly wide).
fn text_width(text: &str, size: f64) -> f64 {
    text.chars()
        .map(|c| match c {
            'i' | 'l' | 'j' | 't' | 'f' | 'I' | '.' | ',' | ':' | ';' | '\'' | '|' | '!' | ' ' => 0.3,
            'm' | 'w' | 'M' | 'W' => 0.85,
            'A'..='Z' | '0'..='9' => 0.65,
            c if c.is_ascii() => 0.52,
            // CJK, emoji and other wide scripts.
            c if c >= '\u{1100}' => 1.0,
            _ => 0.6,
        })
        .sum::<f64>()
        * size
        * 0.88
}

/// `text`, shortened with an ellipsis if it's wider than `max` at `size`.
fn elide(text: &str, size: f64, max: f64) -> String {
    if text_width(text, size) <= max {
        return text.to_string();
    }
    let budget = max - text_width("…", size);
    let mut out = String::new();
    let mut width = 0.0;
    for c in text.chars() {
        width += text_width(c.encode_utf8(&mut [0; 4]), size);
        if width > budget {
            break;
        }
        out.push(c);
    }
    out.trim_end().to_string() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(id: u64, name: &str, streaming: bool, you: bool) -> Member {
        Member { id, name: name.into(), streaming, you }
    }

    #[test]
    fn elides_long_names_only() {
        assert_eq!(elide("Ana", 16.0, 150.0), "Ana");
        let long = elide("Bartholomew the Magnificent of House Long", 16.0, 150.0);
        assert!(long.ends_with('…') && long.chars().count() < 40, "{long}");
        assert!(text_width(&long, 16.0) <= 150.0);
        // Wide characters run out of room sooner.
        assert!(elide("配信者のとても長い名前です", 16.0, 150.0).ends_with('…'));
    }

    #[test]
    fn renders_members_and_states() {
        let mut state = RoomState::default();
        let empty = render("ABC123", &state, None, 1280.0, 720.0).data;
        assert!(empty.contains("ROOM ABC123") && empty.contains("Connecting…"));

        state.members = vec![member(1, "Streamer", true, false), member(2, "Me {\\b1}", false, true)];
        state.target = Some(2);
        let out = render("ABC123", &state, None, 1920.0, 1080.0);
        assert_eq!((out.res_x, out.res_y), (1280, 720));
        assert!(out.data.contains("LIVE") && out.data.contains("(you)"));
        assert!(out.data.contains(r"Me \{") && !out.data.contains(r"{\b1}"), "names are escaped: {}", out.data);
        assert!(out.data.contains("Drop a video"), "arrow on me, not streaming: hint");
        assert!(!out.data.contains("Reconnecting"));

        state.target = Some(1);
        let out = render("ABC123", &state, None, 1280.0, 720.0).data;
        assert!(!out.contains("Drop a video"));

        state.reconnecting = true;
        state.members.extend((3..20).map(|i| member(i, &format!("V{i}"), false, false)));
        let out = render("ABC123", &state, None, 1280.0, 720.0).data;
        assert!(out.contains("Reconnecting") && out.contains("+9 more"), "{out}");
    }

    #[test]
    fn rows_are_clickable_and_hover_highlights() {
        let state = RoomState {
            members: vec![member(4, "A", true, false), member(7, "B", false, true)],
            target: Some(4),
            ..Default::default()
        };
        let plain = render("ROOM", &state, None, 1280.0, 720.0);
        let (_, first) = plain.rows[0];
        let (_, second) = plain.rows[1];
        assert_eq!(plain.row_at(first.center()), Some(4));
        assert_eq!(plain.row_at(second.center()), Some(7));
        assert_eq!(plain.row_at((600.0, 400.0)), None);
        let hovered = render("ROOM", &state, Some(7), 1280.0, 720.0);
        assert!(hovered.data.len() > plain.data.len(), "hover adds a highlight");
    }
}
