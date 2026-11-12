//! Helpers for building `osd-overlay` content in ASS format.
//!
//! Each call appends one ASS event (one line). Coordinates are in the overlay's
//! virtual resolution (PlayResX/PlayResY) and positioned from the top-left.

use std::fmt::Write;

#[derive(Clone, Copy, Debug, Default)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub const fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { x, y, w, h }
    }

    pub fn contains(&self, (px, py): (f64, f64)) -> bool {
        px >= self.x && px <= self.x + self.w && py >= self.y && py <= self.y + self.h
    }

    pub fn center(&self) -> (f64, f64) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    pub fn grow(&self, by: f64) -> Rect {
        Rect::new(self.x - by, self.y - by, self.w + 2.0 * by, self.h + 2.0 * by)
    }
}

/// An sRGB colour plus opacity (0 = transparent, 255 = opaque).
#[derive(Clone, Copy, Debug)]
pub struct Color {
    pub rgb: u32,
    pub opacity: u8,
}

impl Color {
    pub const fn rgb(rgb: u32) -> Self {
        Self { rgb, opacity: 255 }
    }

    pub const fn with_opacity(self, opacity: u8) -> Self {
        Self { rgb: self.rgb, opacity }
    }

    /// ASS colour override value: `&HBBGGRR&`.
    fn bgr(self) -> String {
        let (r, g, b) = ((self.rgb >> 16) & 0xff, (self.rgb >> 8) & 0xff, self.rgb & 0xff);
        format!("&H{b:02X}{g:02X}{r:02X}&")
    }

    /// ASS alpha override value: `&HAA&`, where 00 is opaque.
    fn alpha(self) -> String {
        format!("&H{:02X}&", 255 - self.opacity)
    }
}

pub struct TextStyle<'a> {
    /// Numpad-style alignment of the anchor point (7 = top-left, 4 = middle-left, 5 = center).
    pub align: u8,
    pub size: f64,
    pub color: Color,
    pub bold: bool,
    /// Extra letter spacing in virtual pixels.
    pub spacing: f64,
    /// `None` keeps mpv's OSD font.
    pub font: Option<&'a str>,
    /// Only draw the part of the text inside this rectangle.
    pub clip: Option<Rect>,
}

impl Default for TextStyle<'_> {
    fn default() -> Self {
        Self {
            align: 7,
            size: 20.0,
            color: Color::rgb(0xFFFFFF),
            bold: false,
            spacing: 0.0,
            font: None,
            clip: None,
        }
    }
}

#[derive(Default)]
pub struct Ass {
    out: String,
}

impl Ass {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn finish(self) -> String {
        self.out
    }

    fn start_event(&mut self) {
        if !self.out.is_empty() {
            self.out.push('\n');
        }
    }

    /// A filled rounded rectangle, with an optional outline and blur (for shadows).
    pub fn rect(&mut self, r: Rect, radius: f64, fill: Color, outline: Option<(f64, Color)>, blur: f64) {
        self.start_event();
        let (bord, bcolor) = outline.unwrap_or((0.0, fill));
        let _ = write!(
            self.out,
            r"{{\an7\pos(0,0)\shad0\blur{blur}\bord{bord}\1c{}\1a{}\3c{}\3a{}\p1}}{}{{\p0}}",
            fill.bgr(),
            fill.alpha(),
            bcolor.bgr(),
            bcolor.alpha(),
            rounded_rect_path(r, radius),
        );
    }

    /// A filled shape from ASS drawing commands (e.g. `m 0 0 l 10 5 l 0 10`).
    pub fn shape(&mut self, path: &str, fill: Color) {
        self.start_event();
        let _ = write!(
            self.out,
            r"{{\an7\pos(0,0)\shad0\blur0\bord0\1c{}\1a{}\p1}}{path}{{\p0}}",
            fill.bgr(),
            fill.alpha(),
        );
    }

    /// Text anchored at (x, y). `body` may contain pre-built ASS spans (see
    /// [`span`]); plain user text must go through [`escape`] first.
    pub fn text(&mut self, x: f64, y: f64, style: &TextStyle, body: &str) {
        self.start_event();
        let _ = write!(
            self.out,
            r"{{\an{}\pos({x:.0},{y:.0})\bord0\shad0\blur0\fs{:.0}\b{}\fsp{}\1c{}\1a{}",
            style.align,
            style.size,
            style.bold as u8,
            style.spacing,
            style.color.bgr(),
            style.color.alpha(),
        );
        if let Some(font) = style.font {
            let _ = write!(self.out, r"\fn{font}");
        }
        if let Some(c) = style.clip {
            let _ = write!(self.out, r"\clip({:.0},{:.0},{:.0},{:.0})", c.x, c.y, c.x + c.w, c.y + c.h);
        }
        self.out.push('}');
        self.out.push_str(body);
    }
}

/// An inline span of text with its own colour, for mixing colours on one line.
pub fn span(text: &str, color: Color) -> String {
    format!(r"{{\1c{}\1a{}}}{}", color.bgr(), color.alpha(), escape(text))
}

/// Escape text so ASS doesn't interpret it as override tags or line breaks.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            // A word joiner after the backslash stops sequences like \N from being parsed.
            '\\' => out.push_str("\\\u{2060}"),
            '{' => out.push_str("\\{"),
            '}' => out.push_str("\\}"),
            '\n' => out.push_str("\\N"),
            _ => out.push(c),
        }
    }
    out
}

/// ASS drawing commands for a rounded rectangle (cubic Bézier corners).
fn rounded_rect_path(r: Rect, radius: f64) -> String {
    let rad = radius.min(r.w / 2.0).min(r.h / 2.0).max(0.0);
    // Control point offset from the corner for a circular-looking arc.
    let k = rad * (1.0 - 0.552_284_75);
    let (x0, y0, x1, y1) = (r.x, r.y, r.x + r.w, r.y + r.h);
    let p = |v: f64| format!("{v:.1}");
    [
        format!("m {} {}", p(x0 + rad), p(y0)),
        format!("l {} {}", p(x1 - rad), p(y0)),
        format!("b {} {} {} {} {} {}", p(x1 - k), p(y0), p(x1), p(y0 + k), p(x1), p(y0 + rad)),
        format!("l {} {}", p(x1), p(y1 - rad)),
        format!("b {} {} {} {} {} {}", p(x1), p(y1 - k), p(x1 - k), p(y1), p(x1 - rad), p(y1)),
        format!("l {} {}", p(x0 + rad), p(y1)),
        format!("b {} {} {} {} {} {}", p(x0 + k), p(y1), p(x0), p(y1 - k), p(x0), p(y1 - rad)),
        format!("l {} {}", p(x0), p(y0 + rad)),
        format!("b {} {} {} {} {} {}", p(x0), p(y0 + k), p(x0 + k), p(y0), p(x0 + rad), p(y0)),
    ]
    .join(" ")
}
