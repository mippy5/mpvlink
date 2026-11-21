//! The statistics panel in the top-right corner (a debugging aid, off by
//! default): connection and timing figures for the stream you're watching or
//! sending, with two graphs. Drawn like the member list, and shown and
//! hidden along with it.
//!
//! Pure drawing (no mpv calls), from a [`Telemetry`] snapshot.

use crate::ass::{Ass, Color, Rect, TextStyle, escape};
use crate::menu::{ACCENT, TEXT, TEXT_FAINT, TEXT_MUTED, WARNING};
use crate::telemetry::{FRAME_HISTORY, RATE_HISTORY, Streaming, Telemetry, Watching};
use crate::tracking::{ListState, PICK_KEY, Tracking};

const BOX: Color = Color::rgb(0x1E1F22).with_opacity(165);
const GRAPH_BG: Color = Color::rgb(0x000000).with_opacity(70);
const BAR: Color = Color::rgb(0x23A55A);
const LATE: Color = Color::rgb(0xED4245);
const AREA: Color = ACCENT.with_opacity(170);

/// The same canvas as the member list: 720 high, so it scales with the window.
const RES_Y: f64 = 720.0;
const MARGIN: f64 = 16.0;
const WIDTH: f64 = 268.0;
const PAD_X: f64 = 12.0;
const PAD_Y: f64 = 8.0;
const HEADER_H: f64 = 24.0;
const LINE_H: f64 = 19.0;
const LABEL_H: f64 = 18.0;
const GRAPH_H: f64 = 46.0;
const GAP: f64 = 8.0;
const TEXT_SIZE: f64 = 13.0;
/// Viewers listed by name; the rest are summarized.
const MAX_VIEWERS: usize = 4;

pub struct Overlay {
    pub data: String,
    pub res_x: u32,
    pub res_y: u32,
}

/// One line of figures: text, and its colour.
type Line = (String, Color);

/// Draw the panel for an OSD of `osd_w` x `osd_h` real pixels, with what's
/// playing for AniList tracking (`tracking`) at the top.
pub fn render(t: &Telemetry, tracking: Option<&Tracking>, osd_w: f64, osd_h: f64) -> Overlay {
    let (osd_w, osd_h) = if osd_w > 0.0 && osd_h > 0.0 { (osd_w, osd_h) } else { (1280.0, 720.0) };
    let res_x = (RES_Y * osd_w / osd_h).round();

    let (title, mut lines, graphs) = match (&t.watching, &t.streaming) {
        (Some(w), _) => ("WATCHING", watching_lines(w), watching_graphs(w)),
        (None, Some(s)) => ("STREAMING", streaming_lines(s), streaming_graphs(s)),
        (None, None) => ("STATISTICS", vec![("Not watching or streaming.".to_string(), TEXT_MUTED)], vec![]),
    };
    if let Some(tracking) = tracking {
        let mut now = now_watching(tracking);
        now.append(&mut lines);
        lines = now;
    }

    let graphs_h: f64 = graphs.iter().map(|_| LABEL_H + GRAPH_H + GAP).sum();
    let height = PAD_Y * 2.0 + HEADER_H + lines.len() as f64 * LINE_H + graphs_h;
    let frame = Rect::new(res_x - MARGIN - WIDTH, MARGIN, WIDTH, height);
    let inner = Rect::new(frame.x + PAD_X, frame.y, frame.w - 2.0 * PAD_X, frame.h);

    let mut ass = Ass::new();
    ass.rect(frame, 8.0, BOX, None, 0.0);
    let header = TextStyle { align: 4, size: 12.0, bold: true, spacing: 1.0, color: TEXT_MUTED, ..Default::default() };
    ass.text(inner.x, frame.y + PAD_Y + HEADER_H / 2.0, &header, title);

    let mut y = frame.y + PAD_Y + HEADER_H;
    for (text, color) in &lines {
        let style = TextStyle { align: 4, size: TEXT_SIZE, color: *color, clip: Some(inner), ..Default::default() };
        ass.text(inner.x, y + LINE_H / 2.0, &style, &escape(text));
        y += LINE_H;
    }
    for graph in &graphs {
        y += GAP;
        let label = TextStyle { align: 4, size: 11.0, bold: true, spacing: 0.5, color: TEXT_FAINT, ..Default::default() };
        ass.text(inner.x, y + LABEL_H / 2.0, &label, &escape(&graph.label));
        let scale = TextStyle { align: 6, size: 11.0, color: TEXT_FAINT, ..Default::default() };
        ass.text(inner.x + inner.w, y + LABEL_H / 2.0, &scale, &escape(&graph.scale_label));
        y += LABEL_H;
        let area = Rect::new(inner.x, y, inner.w, GRAPH_H);
        draw_graph(&mut ass, area, graph);
        y += GRAPH_H;
    }

    Overlay { data: ass.finish(), res_x: res_x as u32, res_y: RES_Y as u32 }
}

// ---- Figures --------------------------------------------------------------------

/// "Now watching": the show and episode, and how its list update stands.
fn now_watching(t: &Tracking) -> Vec<Line> {
    let episode = match (t.episode, t.episodes) {
        (Some(e), Some(n)) => format!("Ep {e}/{n} · "),
        (Some(e), None) => format!("Ep {e} · "),
        _ => String::new(),
    };
    let pct = |x: f64| (x * 100.0).round();
    let (state, color) = match t.state {
        ListState::SignedOut => ("AniList: not signed in".to_string(), TEXT_FAINT),
        ListState::Matching => ("AniList: looking it up…".to_string(), TEXT_MUTED),
        ListState::Unmatched => (format!("AniList: not matched · {PICK_KEY} to pick"), WARNING),
        ListState::Counting => (
            format!("counts at 80% · now {:.0}%, watched {:.0}%", pct(t.position), pct(t.watched)),
            TEXT_MUTED,
        ),
        ListState::Sending => ("AniList: updating…".to_string(), TEXT_MUTED),
        ListState::Updated => ("AniList: updated ✓".to_string(), TEXT_MUTED),
        ListState::UpToDate => ("AniList: already on your list".to_string(), TEXT_MUTED),
        ListState::Queued => ("AniList: update queued, will retry".to_string(), WARNING),
        ListState::Failed => ("AniList: update refused".to_string(), WARNING),
    };
    vec![(t.title.clone(), TEXT), (format!("{episode}{state}"), color)]
}

fn ms(seconds: f64) -> String {
    let ms = seconds * 1000.0;
    if ms < 10.0 { format!("{ms:.1} ms") } else { format!("{ms:.0} ms") }
}

fn rate(kbps: f64) -> String {
    if kbps >= 1000.0 { format!("{:.2} Mb/s", kbps / 1000.0) } else { format!("{kbps:.0} kb/s") }
}

/// The average of the last few seconds: single seconds swing a lot (a
/// keyframe can double one), so the text figures use this; the graphs show
/// each second.
fn recent(q: &std::collections::VecDeque<f64>) -> f64 {
    const SECONDS: usize = 5;
    let n = q.len().min(SECONDS);
    if n == 0 { 0.0 } else { q.iter().rev().take(n).sum::<f64>() / n as f64 }
}

fn watching_lines(w: &Watching) -> Vec<Line> {
    let mut lines = vec![(format!("From {}", w.name), TEXT)];
    let link = w.link.clone().unwrap_or_default();
    let rtt = link.rtt.map_or("…".to_string(), ms);
    lines.push((format!("Network RTT {rtt}"), TEXT_MUTED));
    // How unevenly frames arrive (what smoothing has to cover).
    let mut spreads: Vec<f64> = w.frames.iter().map(|f| f.spread_ms).collect();
    spreads.sort_by(f64::total_cmp);
    if let Some(p98) = spreads.get(((spreads.len().max(1) - 1) as f64 * 0.98) as usize) {
        lines.push((format!("Arrival spread {p98:.0} ms (98% of frames)"), TEXT_MUTED));
    }
    lines.push((format!("In {} · {:.0} fps", rate(recent(&w.kbps)), w.fps()), TEXT_MUTED));

    let sent = link.packets_received as i64 + link.packets_lost;
    let lost_pct = if sent > 0 { link.packets_lost as f64 * 100.0 / sent as f64 } else { 0.0 };
    let loss_color = if w.losses > 0 { WARNING } else { TEXT_MUTED };
    lines.push((
        format!("Lost {} packets ({lost_pct:.1}%) · {} gaps · {} out of order", link.packets_lost, w.losses, w.out_of_order),
        loss_color,
    ));

    let delay = w.frames.back().and_then(|f| f.delay_ms);
    // Late frames among those in the graph (the last few seconds), and in all.
    let recent_late = w.frames.iter().filter(|f| f.late).count();
    let (smoothing, color) = match delay {
        Some(d) => (
            format!("Smoothing {} · {d:.0} ms · late {recent_late} now, {} total", crate::jitter::smoothing(), w.late),
            if recent_late > 0 { WARNING } else { TEXT_MUTED },
        ),
        None => ("Smoothing off".to_string(), TEXT_MUTED),
    };
    lines.push((smoothing, color));
    if let Some(rtt) = link.rtt {
        let total = rtt * 500.0 + delay.unwrap_or(0.0);
        lines.push((format!("Delay ≈ {total:.0} ms (½ RTT + smoothing)"), TEXT_MUTED));
    }
    lines
}

fn streaming_lines(s: &Streaming) -> Vec<Line> {
    let viewers = match s.viewers.len() {
        0 => "nobody yet".to_string(),
        1 => "1 viewer".to_string(),
        n => format!("{n} viewers"),
    };
    let mut lines = vec![(format!("To {viewers}"), TEXT)];
    lines.push((format!("{} · target {}", s.encoder, rate(f64::from(s.target_kbps))), TEXT_MUTED));
    lines.push((format!("Encoded {} · {:.0} fps", rate(recent(&s.encoded_kbps)), s.fps()), TEXT_MUTED));
    lines.push((format!("Upload {}", rate(recent(&s.upload_kbps))), TEXT_MUTED));
    for v in s.viewers.iter().take(MAX_VIEWERS) {
        let rtt = v.link.rtt.map_or("…".to_string(), ms);
        let lost = v.link.remote_fraction_lost.map_or(String::new(), |f| format!(" · lost {:.1}%", f * 100.0));
        let color = if v.link.remote_fraction_lost.is_some_and(|f| f > 0.0) { WARNING } else { TEXT_MUTED };
        lines.push((format!("{}: RTT {rtt}{lost}", v.name), color));
    }
    if s.viewers.len() > MAX_VIEWERS {
        lines.push((format!("+{} more", s.viewers.len() - MAX_VIEWERS), TEXT_FAINT));
    }
    lines
}

// ---- Graphs ---------------------------------------------------------------------

/// A graph: bars (one per frame) with an optional threshold line, or an area
/// (one point per second).
struct Graph {
    label: String,
    scale_label: String,
    kind: GraphKind,
}

enum GraphKind {
    /// Values in ms, a horizontal line (the smoothing delay), and which bars
    /// to draw red.
    Bars { values: Vec<f64>, line: Option<f64>, red: Vec<bool>, max: f64, slots: usize },
    Area { values: Vec<f64>, max: f64, slots: usize },
}

/// A round scale top at or above `v`.
fn nice_max(v: f64, floor: f64) -> f64 {
    let v = v.max(floor);
    let step = 10f64.powf(v.log10().floor());
    [1.0, 2.0, 5.0, 10.0].iter().map(|m| m * step).find(|m| *m >= v).unwrap_or(v)
}

fn watching_graphs(w: &Watching) -> Vec<Graph> {
    let values: Vec<f64> = w.frames.iter().map(|f| f.spread_ms).collect();
    let red: Vec<bool> = w.frames.iter().map(|f| f.late).collect();
    let line = w.frames.back().and_then(|f| f.delay_ms);
    let top = values.iter().copied().fold(line.unwrap_or(0.0), f64::max);
    let max = nice_max(top * 1.1, 20.0);
    let kbps: Vec<f64> = w.kbps.iter().copied().collect();
    let kmax = nice_max(kbps.iter().copied().fold(0.0, f64::max) * 1.1, 500.0);
    vec![
        Graph {
            label: "FRAME ARRIVAL (RED = LATE)".into(),
            scale_label: format!("{max:.0} ms"),
            kind: GraphKind::Bars { values, line, red, max, slots: FRAME_HISTORY },
        },
        Graph {
            label: "INCOMING, LAST MINUTE".into(),
            scale_label: rate(kmax),
            kind: GraphKind::Area { values: kbps, max: kmax, slots: RATE_HISTORY },
        },
    ]
}

fn streaming_graphs(s: &Streaming) -> Vec<Graph> {
    let values: Vec<f64> = s.sends.iter().copied().collect();
    let max = nice_max(values.iter().copied().fold(0.0, f64::max) * 1.1, 10.0);
    let upload: Vec<f64> = if s.viewers.is_empty() { s.encoded_kbps.iter().copied().collect() } else { s.upload_kbps.iter().copied().collect() };
    let kmax = nice_max(upload.iter().copied().fold(0.0, f64::max) * 1.1, 500.0);
    let label = if s.viewers.is_empty() { "ENCODED, LAST MINUTE" } else { "UPLOAD, LAST MINUTE" };
    vec![
        Graph {
            label: "SEND TIMING".into(),
            scale_label: format!("{max:.0} ms"),
            kind: GraphKind::Bars { red: vec![false; values.len()], values, line: None, max, slots: FRAME_HISTORY },
        },
        Graph { label: label.into(), scale_label: rate(kmax), kind: GraphKind::Area { values: upload, max: kmax, slots: RATE_HISTORY } },
    ]
}

fn draw_graph(ass: &mut Ass, r: Rect, graph: &Graph) {
    ass.rect(r, 3.0, GRAPH_BG, None, 0.0);
    let bottom = r.y + r.h;
    let y_of = |v: f64, max: f64| bottom - (v / max).clamp(0.0, 1.0) * r.h;
    match &graph.kind {
        GraphKind::Bars { values, line, red, max, slots } => {
            // Newest on the right; one slot per frame.
            let w = r.w / *slots as f64;
            let start = r.x + r.w - values.len() as f64 * w;
            let (mut green_path, mut red_path) = (String::new(), String::new());
            for (i, (v, late)) in values.iter().zip(red).enumerate() {
                let x0 = start + i as f64 * w;
                let x1 = x0 + (w - 0.5).max(0.5);
                let top = y_of(*v, *max).min(bottom - 1.0); // even zero shows a sliver
                let path = if *late { &mut red_path } else { &mut green_path };
                path.push_str(&format!("m {x0:.1} {bottom:.1} l {x1:.1} {bottom:.1} {x1:.1} {top:.1} {x0:.1} {top:.1} "));
            }
            if !green_path.is_empty() {
                ass.shape(&green_path, BAR);
            }
            if !red_path.is_empty() {
                ass.shape(&red_path, LATE);
            }
            if let Some(line) = line {
                let y = y_of(*line, *max);
                ass.rect(Rect::new(r.x, y - 0.75, r.w, 1.5), 0.0, WARNING, None, 0.0);
            }
        }
        GraphKind::Area { values, max, slots } => {
            if values.len() < 2 {
                return;
            }
            let step = r.w / (*slots - 1) as f64;
            let start = r.x + r.w - (values.len() - 1) as f64 * step;
            let mut path = format!("m {start:.1} {bottom:.1}");
            for (i, v) in values.iter().enumerate() {
                path.push_str(&format!(" l {:.1} {:.1}", start + i as f64 * step, y_of(*v, *max)));
            }
            path.push_str(&format!(" l {:.1} {bottom:.1}", r.x + r.w));
            ass.shape(&path, AREA);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::{self, FrameMark};

    #[test]
    fn scales_round_up() {
        assert_eq!(nice_max(37.0, 20.0), 50.0);
        assert_eq!(nice_max(3.0, 20.0), 20.0);
        assert_eq!(nice_max(2400.0, 500.0), 5000.0);
    }

    #[test]
    fn draws_each_mode() {
        let idle = render(&Telemetry::default(), None, 1280.0, 720.0);
        assert!(idle.data.contains("Not watching or streaming."));

        telemetry::watch_started("Ana");
        for i in 0..30 {
            telemetry::frame_received(FrameMark { spread_ms: f64::from(i % 7), delay_ms: Some(20.0), late: i == 9 });
        }
        let t = telemetry::snapshot();
        telemetry::watch_stopped();
        let out = render(&t, None, 1920.0, 1080.0);
        assert!(out.data.contains("From Ana"));
        assert!(out.data.contains("FRAME ARRIVAL"));
        assert!(out.data.contains("late 1 now, 1 total"));
        assert_eq!(out.res_x, 1280);
    }

    #[test]
    fn shows_now_watching() {
        let tracking = Tracking {
            title: "Assassination Classroom Second Season".into(),
            episode: Some(14),
            episodes: Some(25),
            position: 0.42,
            watched: 0.30,
            state: ListState::Counting,
        };
        let out = render(&Telemetry::default(), Some(&tracking), 1280.0, 720.0);
        assert!(out.data.contains("Assassination Classroom Second Season"));
        assert!(out.data.contains("Ep 14/25 · counts at 80% · now 42%, watched 30%"));
        let queued = Tracking { state: ListState::Queued, ..tracking };
        assert!(render(&Telemetry::default(), Some(&queued), 1280.0, 720.0).data.contains("queued"));
    }
}
