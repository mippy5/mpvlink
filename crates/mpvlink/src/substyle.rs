//! mpv's subtitle style options (`sub-font`, `sub-scale`, `sub-pos`, …),
//! applied to subtitles that ffmpeg draws into a stream, the way mpv
//! applies them when it draws them itself.
//!
//! Text subtitles are drawn by libass in both, so the options become libass
//! style overrides (the `subtitles` filter's `force_style`), following mpv's
//! own mapping (`mp_ass_set_style` in mpv's sub/ass_mp.c). Image subtitles
//! only take the scale and position.

/// `sub-ass-override`: how much of the above applies to ASS subtitles.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum AssOverride {
    /// Nothing.
    No,
    /// `sub-ass-style-overrides` and `sub-pos`.
    Yes,
    /// Also `sub-scale` (mpv's default).
    #[default]
    Scale,
    /// Also every other `sub-*` style option.
    Force,
    /// Strips all styling in mpv; here the same as `Force` (libass in ffmpeg
    /// can't strip override tags).
    Strip,
}

impl AssOverride {
    pub fn parse(s: &str) -> Self {
        match s {
            "no" => Self::No,
            "yes" => Self::Yes,
            "force" => Self::Force,
            "strip" => Self::Strip,
            _ => Self::Scale,
        }
    }
}

/// mpv's subtitle style options, in mpv's units: sizes are "scaled pixels"
/// at a window height of 720, colors are mpv's `#AARRGGBB`.
#[derive(Clone, Debug, PartialEq)]
pub struct SubStyle {
    pub ass_override: AssOverride,
    pub font: String,
    pub font_size: f64,
    pub scale: f64,
    /// Whether `sub-scale` applies to ASS signs too, not only dialogue.
    pub scale_signs: bool,
    pub color: String,
    pub outline_color: String,
    pub back_color: String,
    pub outline_size: f64,
    pub shadow_offset: f64,
    /// ASS BorderStyle: 1 outline and shadow, 3 opaque box, 4 background box.
    pub border_style: u8,
    pub bold: bool,
    pub italic: bool,
    pub blur: f64,
    pub spacing: f64,
    pub margin_x: f64,
    pub margin_y: f64,
    /// Vertical position in % of the height; 100 = where it would be.
    pub pos: f64,
    /// -1 left, 0 center, 1 right.
    pub align_x: i8,
    /// -1 top, 0 center, 1 bottom.
    pub align_y: i8,
    /// `sub-ass-style-overrides`, passed on as they are (same syntax).
    pub overrides: Vec<String>,
    /// `sub-gray`: image subtitles in grayscale.
    pub gray: bool,
}

impl Default for SubStyle {
    /// mpv 0.40's defaults.
    fn default() -> Self {
        Self {
            ass_override: AssOverride::Scale,
            font: "sans-serif".into(),
            font_size: 38.0,
            scale: 1.0,
            scale_signs: false,
            color: "#FFFFFFFF".into(),
            outline_color: "#FF000000".into(),
            back_color: "#AF000000".into(),
            outline_size: 1.65,
            shadow_offset: 0.0,
            border_style: 1,
            bold: false,
            italic: false,
            blur: 0.0,
            spacing: 0.0,
            margin_x: 19.0,
            margin_y: 34.0,
            pos: 100.0,
            align_x: 0,
            align_y: 1,
            overrides: Vec::new(),
            gray: false,
        }
    }
}

/// mpv's subtitle sizes are given for this height.
const MPV_HEIGHT: f64 = 720.0;

impl SubStyle {
    /// Overrides for plain-text subtitles (SRT, WebVTT…), which ffmpeg turns
    /// into ASS with a generic style: replace it with mpv's, on mpv's
    /// 720-high canvas. `aspect` is the video's width / height.
    pub fn plain_text(&self, aspect: f64) -> String {
        let mut out = vec![format!("PlayResX={}", (MPV_HEIGHT * aspect).round()), format!("PlayResY={MPV_HEIGHT}")];
        out.extend(self.style_fields(1.0, true));
        // ffmpeg's style is bottom-center; mpv's position sets the margin.
        let margin = self.margin_y + self.pos_shift(MPV_HEIGHT);
        if self.pos != 100.0 && self.align_y == 1 {
            out.retain(|f| !f.starts_with("MarginV="));
            out.push(format!("MarginV={}", num(margin)));
        }
        if self.ass_override != AssOverride::No {
            out.extend(self.overrides.iter().cloned());
        }
        out.join(",")
    }

    /// Overrides for ASS subtitles with this header, as `sub-ass-override`
    /// says. Empty if there's nothing to change.
    pub fn ass(&self, header: &AssHeader) -> String {
        if self.ass_override == AssOverride::No {
            return String::new();
        }
        let force = matches!(self.ass_override, AssOverride::Force | AssOverride::Strip);
        let to_script = header.play_res_y / MPV_HEIGHT;
        let mut out = Vec::new();
        if force {
            out.extend(self.style_fields(to_script, false));
        }
        for style in header.styles.iter().filter(|s| !s.name.contains([',', '=']) && !s.name.is_empty()) {
            let name = &style.name;
            // mpv scales dialogue only (unless sub-scale-signs); best guess
            // here, like mpv's own guess: dialogue sits at the bottom.
            let dialogue = style.bottom();
            let scale = self.ass_override != AssOverride::Yes && self.scale != 1.0 && (dialogue || self.scale_signs);
            if scale {
                let size = if force { self.font_size * to_script } else { style.font_size };
                out.push(format!("{name}.Fontsize={}", num(size * self.scale)));
            }
            // sub-pos moves bottom-aligned lines (libass's line position).
            if self.pos != 100.0 && style.bottom() {
                let margin = if force { self.margin_y * to_script } else { style.margin_v };
                out.push(format!("{name}.MarginV={}", num(margin + self.pos_shift(header.play_res_y))));
            }
        }
        out.extend(self.overrides.iter().cloned());
        out.join(",")
    }

    /// For image subtitles: (scale, upward shift as a fraction of the
    /// height), as mpv applies them to bitmaps.
    pub fn image(&self) -> (f64, f64) {
        match self.ass_override {
            AssOverride::No => (1.0, 0.0),
            _ => (self.scale, (100.0 - self.pos) / 100.0),
        }
    }

    /// How much `sub-pos` raises a bottom line, on a canvas `height` high.
    fn pos_shift(&self, height: f64) -> f64 {
        (100.0 - self.pos) / 100.0 * height
    }

    /// mpv's style as ASS style fields, sizes multiplied by `to_script`.
    fn style_fields(&self, to_script: f64, plain: bool) -> Vec<String> {
        let size = |v: f64| num(v * to_script);
        let mut f = vec![
            format!("FontName={}", self.font),
            format!("Fontsize={}", size(self.font_size * if plain { self.scale } else { 1.0 })),
            format!("Bold={}", if self.bold { -1 } else { 0 }),
            format!("Italic={}", if self.italic { -1 } else { 0 }),
            format!("BorderStyle={}", self.border_style),
            format!("Outline={}", size(self.outline_size)),
            format!("Shadow={}", size(self.shadow_offset)),
            format!("Spacing={}", size(self.spacing)),
            format!("Blur={}", num(self.blur)),
        ];
        let colors = [
            ("PrimaryColour", &self.color),
            ("SecondaryColour", &self.color),
            ("OutlineColour", &self.outline_color),
            ("BackColour", &self.back_color),
        ];
        for (field, color) in colors {
            if let Some(c) = ass_color(color) {
                f.push(format!("{field}={c}"));
            }
        }
        if plain {
            // Numpad layout: 1-3 bottom, 4-6 middle, 7-9 top.
            let row = match self.align_y {
                1 => 0,
                0 => 1,
                _ => 2,
            };
            f.push(format!("Alignment={}", 2 + self.align_x as i32 + 3 * row));
            f.push(format!("MarginL={}", size(self.margin_x)));
            f.push(format!("MarginR={}", size(self.margin_x)));
            f.push(format!("MarginV={}", size(self.margin_y)));
        }
        f
    }
}

/// What an ASS script's header says, as far as we need it.
#[derive(Clone, Debug, PartialEq)]
pub struct AssHeader {
    pub play_res_y: f64,
    pub styles: Vec<AssStyle>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AssStyle {
    pub name: String,
    pub font_size: f64,
    /// Numpad layout (1-3 bottom).
    pub alignment: u8,
    pub margin_v: f64,
}

impl AssStyle {
    fn bottom(&self) -> bool {
        (1..=3).contains(&self.alignment)
    }
}

impl AssHeader {
    pub fn parse(text: &str) -> Self {
        let mut play_res_y = None;
        let mut format: Vec<String> = Vec::new();
        let mut styles = Vec::new();
        for line in text.lines().map(str::trim) {
            if line.starts_with("[Events]") {
                break;
            }
            let Some((key, value)) = line.split_once(':') else { continue };
            let value = value.trim();
            match key.trim() {
                "PlayResY" => play_res_y = value.parse().ok(),
                "Format" => format = value.split(',').map(|f| f.trim().to_ascii_lowercase()).collect(),
                "Style" => {
                    // The last field (Encoding) never has commas; only the
                    // name could, and it comes first.
                    let fields: Vec<&str> = value.splitn(format.len().max(1), ',').map(str::trim).collect();
                    let get = |name: &str| format.iter().position(|f| f == name).and_then(|i| fields.get(i).copied());
                    let Some(name) = get("name") else { continue };
                    styles.push(AssStyle {
                        name: name.to_string(),
                        font_size: get("fontsize").and_then(|v| v.parse().ok()).unwrap_or(20.0),
                        alignment: get("alignment").and_then(|v| v.parse().ok()).unwrap_or(2),
                        margin_v: get("marginv").and_then(|v| v.parse().ok()).unwrap_or(0.0),
                    });
                }
                _ => {}
            }
        }
        // libass's default when a script doesn't say.
        Self { play_res_y: play_res_y.filter(|&y: &f64| y > 0.0).unwrap_or(288.0), styles }
    }
}

/// mpv's `#AARRGGBB` (or `#RRGGBB`) as ASS's `&HAABBGGRR`, where ASS alpha
/// counts transparency (00 = opaque).
fn ass_color(mpv: &str) -> Option<String> {
    let hex = mpv.strip_prefix('#')?;
    let v = u32::from_str_radix(hex, 16).ok()?;
    let (a, rgb) = match hex.len() {
        8 => (v >> 24, v & 0xFF_FFFF),
        6 => (0xFF, v),
        _ => return None,
    };
    let (r, g, b) = (rgb >> 16, (rgb >> 8) & 0xFF, rgb & 0xFF);
    Some(format!("&H{:02X}{b:02X}{g:02X}{r:02X}", 255 - a))
}

/// A number without needless decimals.
fn num(v: f64) -> String {
    let s = format!("{v:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "[Script Info]\nPlayResX: 1280\nPlayResY: 720\n\n[V4+ Styles]\n\
        Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n\
        Style: Default,Cabin,52,&H00FFFFFF,&H000019FF,&H00020C20,&HBE000036,-1,0,0,0,100,100,0,0,1,2.5,1.2,2,160,160,42,1\n\
        Style: Sign #1,HannaHandwriting,50,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,0,0,5,0,0,0,1\n\n\
        [Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n";

    #[test]
    fn colors_become_ass_colors() {
        assert_eq!(ass_color("#FFFFFFFF").as_deref(), Some("&H00FFFFFF"));
        assert_eq!(ass_color("#AF000000").as_deref(), Some("&H50000000"));
        assert_eq!(ass_color("#FF102030").as_deref(), Some("&H00302010"));
        assert_eq!(ass_color("#102030").as_deref(), Some("&H00302010"));
        assert_eq!(ass_color("white"), None);
    }

    #[test]
    fn header_styles() {
        let h = AssHeader::parse(HEADER);
        assert_eq!(h.play_res_y, 720.0);
        assert_eq!(h.styles.len(), 2);
        assert_eq!(h.styles[0], AssStyle { name: "Default".into(), font_size: 52.0, alignment: 2, margin_v: 42.0 });
        assert_eq!(h.styles[1].alignment, 5);
        assert_eq!(AssHeader::parse("[Script Info]\n").play_res_y, 288.0);
    }

    #[test]
    fn ass_defaults_change_nothing() {
        assert_eq!(SubStyle::default().ass(&AssHeader::parse(HEADER)), "");
    }

    #[test]
    fn ass_scale_and_position_touch_dialogue_only() {
        let style = SubStyle { scale: 1.5, pos: 90.0, ..SubStyle::default() };
        // 10% of 720 = 72 up from the style's own margin.
        assert_eq!(style.ass(&AssHeader::parse(HEADER)), "Default.Fontsize=78,Default.MarginV=114");
        let signs = SubStyle { scale_signs: true, ..style.clone() };
        assert!(signs.ass(&AssHeader::parse(HEADER)).contains("Sign #1.Fontsize=75"));
        let off = SubStyle { ass_override: AssOverride::No, ..style };
        assert_eq!(off.ass(&AssHeader::parse(HEADER)), "");
    }

    #[test]
    fn ass_force_uses_mpv_style_in_script_units() {
        let style = SubStyle { ass_override: AssOverride::Force, overrides: vec!["Kerning=yes".into()], ..SubStyle::default() };
        let out = style.ass(&AssHeader { play_res_y: 360.0, styles: Vec::new() });
        assert!(out.starts_with("FontName=sans-serif,Fontsize=19,Bold=0,Italic=0,BorderStyle=1,Outline=0.82,"), "{out}");
        assert!(out.ends_with(",Kerning=yes"));
    }

    #[test]
    fn plain_text_gets_mpv_style() {
        let out = SubStyle::default().plain_text(16.0 / 9.0);
        assert_eq!(
            out,
            "PlayResX=1280,PlayResY=720,FontName=sans-serif,Fontsize=38,Bold=0,Italic=0,BorderStyle=1,Outline=1.65,Shadow=0,Spacing=0,Blur=0,\
             PrimaryColour=&H00FFFFFF,SecondaryColour=&H00FFFFFF,OutlineColour=&H00000000,BackColour=&H50000000,\
             Alignment=2,MarginL=19,MarginR=19,MarginV=34"
        );
        let top_left = SubStyle { align_x: -1, align_y: -1, scale: 2.0, pos: 50.0, ..SubStyle::default() }.plain_text(1.0);
        assert!(top_left.contains("Fontsize=76") && top_left.contains("Alignment=7"));
        // Position only moves bottom-aligned subtitles.
        assert!(top_left.ends_with("MarginV=34"));
        let raised = SubStyle { pos: 50.0, ..SubStyle::default() }.plain_text(1.0);
        assert!(raised.ends_with("MarginV=394"));
    }

    #[test]
    fn image_options() {
        assert_eq!(SubStyle::default().image(), (1.0, 0.0));
        let style = SubStyle { scale: 2.0, pos: 80.0, ..SubStyle::default() };
        let (scale, raise) = style.image();
        assert_eq!(scale, 2.0);
        assert!((raise - 0.2).abs() < 1e-9);
        assert_eq!(SubStyle { ass_override: AssOverride::No, ..style }.image(), (1.0, 0.0));
    }
}
