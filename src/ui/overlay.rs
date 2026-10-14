//! "Key sent" overlay: the chord most recently sent to the remote, drawn as a
//! row of keycaps in a translucent pill at the bottom-right of the console,
//! held while any key is down and then faded away.

use anyhow::Result;
use fontdue::{Font, FontSettings, Metrics};
use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::draw::blend_pixel;

/// Inter Medium (SIL OFL 1.1, see `assets/fonts/OFL-Inter.txt`), embedded so
/// the overlay looks the same on every OS. It covers every keycap label,
/// arrows included.
const KEYCAP_FONT: &[u8] = include_bytes!("../../assets/fonts/Inter-Medium.ttf");

/// How long the toast stays fully visible after the last key is released.
const HOLD: Duration = Duration::from_millis(1200);
const FADE: Duration = Duration::from_millis(450);

pub struct KeyToast {
    fonts: Vec<Font>,
    glyphs: HashMap<(char, u32), (Metrics, Vec<u8>)>,
    labels: Vec<String>,
    holding: bool,
    since: Instant, // when the toast last changed / was released
    pub enabled: bool,
}

impl KeyToast {
    pub fn new() -> Result<Self> {
        let font = Font::from_bytes(KEYCAP_FONT, FontSettings::default())
            .map_err(|e| anyhow::anyhow!("parsing the embedded keycap font: {e}"))?;
        let fonts = vec![font];
        Ok(KeyToast {
            fonts,
            glyphs: HashMap::new(),
            labels: Vec::new(),
            holding: false,
            since: Instant::now() - HOLD - FADE, // start hidden
            enabled: true,
        })
    }

    /// Show `labels` (one per keycap). `holding` keeps it fully visible until a
    /// later call with `holding = false` starts the hold-then-fade clock.
    pub fn show(&mut self, labels: Vec<String>, holding: bool) {
        self.labels = labels;
        self.holding = holding;
        self.since = Instant::now();
    }

    /// Current opacity, 0 (gone) .. 1.
    fn alpha(&self) -> f32 {
        if self.holding {
            return 1.0;
        }
        let t = self.since.elapsed();
        if t <= HOLD {
            1.0
        } else {
            1.0 - ((t - HOLD).as_secs_f32() / FADE.as_secs_f32()).min(1.0)
        }
    }

    /// Rasterise (and cache) `c` at `px`, from the first font that has it.
    fn glyph(&mut self, c: char, px: u32) -> &(Metrics, Vec<u8>) {
        let fonts = &self.fonts;
        self.glyphs.entry((c, px)).or_insert_with(|| {
            let font = fonts.iter().find(|f| f.lookup_glyph_index(c) != 0).unwrap_or(&fonts[0]);
            font.rasterize(c, px as f32)
        })
    }

    fn text_width(&mut self, s: &str, px: u32) -> i32 {
        s.chars().map(|c| self.glyph(c, px).0.advance_width.round() as i32).sum()
    }

    /// Draw `s` with its baseline at `base_y`, starting at `x`.
    #[allow(clippy::too_many_arguments)]
    fn draw_text(
        &mut self,
        fb: &mut [u32],
        w: usize,
        h: usize,
        s: &str,
        x: i32,
        base_y: i32,
        px: u32,
        color: u32,
        a: f32,
    ) {
        let mut pen = x;
        for c in s.chars() {
            let (m, bitmap) = self.glyph(c, px).clone();
            let gx = pen + m.xmin;
            let gy = base_y - m.height as i32 - m.ymin;
            for row in 0..m.height {
                for col in 0..m.width {
                    let cov = bitmap[row * m.width + col];
                    if cov != 0 {
                        let ca = a * cov as f32 / 255.0;
                        blend_pixel(fb, w, h, gx + col as i32, gy + row as i32, color, ca);
                    }
                }
            }
            pen += m.advance_width.round() as i32;
        }
    }

    pub fn draw(&mut self, fb: &mut [u32], w: usize, h: usize) {
        let a = self.alpha();
        if !self.enabled || self.labels.is_empty() || a <= 0.0 {
            return;
        }
        // Type scale tracks the console width: 15px at 1024 wide, 13..20px.
        let px = (w as u32 * 15 / 1024).clamp(13, 20);
        let (ascent, descent) = self.fonts[0]
            .horizontal_line_metrics(px as f32)
            .map(|lm| (lm.ascent.round() as i32, (-lm.descent).round() as i32))
            .unwrap_or((px as i32, px as i32 / 4));
        let unit = px as i32;
        let cap_pad_x = unit * 6 / 10;
        let cap_h = ascent + descent + unit * 7 / 10;
        let gap = unit * 4 / 10;
        let pad = unit * 6 / 10;
        let margin = unit;

        let labels = self.labels.clone();
        let widths: Vec<i32> =
            labels.iter().map(|l| (self.text_width(l, px) + cap_pad_x * 2).max(cap_h)).collect();
        let content_w: i32 = widths.iter().sum::<i32>() + gap * (widths.len() as i32 - 1);
        let pill_w = content_w + pad * 2;
        let pill_h = cap_h + pad * 2;
        let px0 = w as i32 - margin - pill_w;
        let py0 = h as i32 - margin - pill_h;

        round_rect(fb, w, h, px0, py0, pill_w, pill_h, pill_h / 3, 0x161616, 0.84 * a);
        let mut x = px0 + pad;
        let cap_y = py0 + pad;
        let shadow = (unit / 8).max(2);
        for (label, cw) in labels.iter().zip(&widths) {
            // Keycap: a darker lip under a lighter face.
            let r = unit / 4;
            round_rect(fb, w, h, x, cap_y + shadow, *cw, cap_h - shadow, r, 0x050505, 0.9 * a);
            round_rect(fb, w, h, x, cap_y, *cw, cap_h - shadow, r, 0x3a3a3c, a);
            let tw = self.text_width(label, px);
            let base = cap_y + (cap_h - shadow - (ascent + descent)) / 2 + ascent;
            self.draw_text(fb, w, h, label, x + (cw - tw) / 2, base, px, 0xF2F2F2, a);
            x += cw + gap;
        }
    }
}

/// Alpha-blended rounded rectangle with corner radius `r`.
#[allow(clippy::too_many_arguments)]
fn round_rect(
    fb: &mut [u32],
    w: usize,
    h: usize,
    x: i32,
    y: i32,
    rw: i32,
    rh: i32,
    r: i32,
    color: u32,
    a: f32,
) {
    let r = r.clamp(0, rw.min(rh) / 2);
    for yy in 0..rh {
        for xx in 0..rw {
            let cx = xx.clamp(r, rw - 1 - r);
            let cy = yy.clamp(r, rh - 1 - r);
            let (dx, dy) = ((xx - cx) as f32, (yy - cy) as f32);
            if dx * dx + dy * dy <= (r * r) as f32 {
                blend_pixel(fb, w, h, x + xx, y + yy, color, a);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fonts_cover_every_label_glyph() {
        let t = KeyToast::new().unwrap();
        for c in "ABCZ0189 FNum.Enter←↑→↓\\#`'[]".chars().filter(|c| *c != ' ') {
            assert!(t.fonts.iter().any(|f| f.lookup_glyph_index(c) != 0), "no glyph for {c:?}");
        }
    }

    #[test]
    fn draws_into_the_bottom_right_and_fades() {
        let (w, h) = (640usize, 480usize);
        let mut t = KeyToast::new().unwrap();
        let mut fb = vec![0u32; w * h];
        t.show(vec!["Ctrl".into(), "Alt".into(), "F2".into()], true);
        t.draw(&mut fb, w, h);
        let lit: Vec<usize> = (0..w * h).filter(|&i| fb[i] != 0).collect();
        assert!(!lit.is_empty());
        assert!(lit.iter().all(|&i| i % w > w / 2 && i / w > h / 2), "toast outside bottom-right");

        // Released long ago -> fully faded, nothing drawn.
        t.show(vec!["A".into()], false);
        t.since = Instant::now() - HOLD - FADE - Duration::from_millis(1);
        let mut fb2 = vec![0u32; w * h];
        t.draw(&mut fb2, w, h);
        assert!(fb2.iter().all(|&p| p == 0));
    }
}
