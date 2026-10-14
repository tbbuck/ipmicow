//! Status screens (POWERED OFF / reconnecting toast).
//!
//! Dependency-free: a tiny 5x7 bitmap font covering exactly the characters these two
//! screens use, plus a handful of primitive fills. The window is just a u32 buffer, so
//! we draw straight into it before handing it to minifb.

/// 5x7 glyphs for the characters the status screens need. Each row is a 5-bit mask
/// (bit 4 = leftmost column). Anything unmapped (incl. space) renders blank.
fn glyph(c: char) -> [u8; 7] {
    match c {
        'P' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000],
        'O' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        'W' => [0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001],
        'E' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111],
        'R' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001],
        'D' => [0b11100, 0b10010, 0b10001, 0b10001, 0b10001, 0b10010, 0b11100],
        'F' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000],
        'r' => [0b00000, 0b00000, 0b10110, 0b11000, 0b10000, 0b10000, 0b10000],
        'e' => [0b00000, 0b00000, 0b01110, 0b10001, 0b11111, 0b10000, 0b01110],
        'c' => [0b00000, 0b00000, 0b01110, 0b10001, 0b10000, 0b10001, 0b01110],
        'o' => [0b00000, 0b00000, 0b01110, 0b10001, 0b10001, 0b10001, 0b01110],
        'n' => [0b00000, 0b00000, 0b11110, 0b10001, 0b10001, 0b10001, 0b10001],
        't' => [0b01000, 0b01000, 0b11110, 0b01000, 0b01000, 0b01001, 0b00110],
        'i' => [0b00100, 0b00000, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100],
        'g' => [0b00000, 0b01110, 0b10001, 0b01111, 0b00001, 0b00001, 0b01110],
        _ => [0; 7],
    }
}

/// Draw `text` with its top-left at (x,y), each font pixel a `scale`x`scale` block.
/// Returns the x just past the final glyph (handy for placing a trailing spinner).
fn draw_text(
    fb: &mut [u32],
    fbw: usize,
    fbh: usize,
    text: &str,
    x: i32,
    y: i32,
    scale: i32,
    color: u32,
) -> i32 {
    let mut cx = x;
    for ch in text.chars() {
        let g = glyph(ch);
        for (row, bits) in g.iter().enumerate() {
            for col in 0i32..5 {
                if bits & (1 << (4 - col)) != 0 {
                    fill_rect(fb, fbw, fbh, cx + col * scale, y + row as i32 * scale, scale, scale, color);
                }
            }
        }
        cx += 6 * scale; // 5 columns + 1 of spacing
    }
    cx
}

/// Black canvas with "POWERED OFF" in big, softly-off-white, centered text.
pub fn draw_powered_off(fb: &mut [u32], w: usize, h: usize) {
    for p in fb.iter_mut() {
        *p = 0x000000;
    }
    let text = "POWERED OFF";
    let cols = text.chars().count() as i32 * 6 - 1; // last glyph has no trailing gap
    let scale = ((w as i32 * 55 / 100) / cols).max(2);
    let tw = cols * scale;
    let th = 7 * scale;
    let x = (w as i32 - tw) / 2;
    let y = (h as i32 - th) / 2;
    draw_text(fb, w, h, text, x, y, scale, 0xEDEDED);
}

/// Bottom-left "reconnecting ⟳" toast that slides up out of the bottom edge.
/// `slide` is 0 (hidden) .. 1 (fully out); `phase` advances the spinner.
pub fn draw_reconnecting(fb: &mut [u32], w: usize, h: usize, slide: f32, phase: u32) {
    let scale = (w as i32 / 320).clamp(2, 4);
    let text = "reconnecting";
    let text_w = text.chars().count() as i32 * 6 * scale;
    let pad = 6 * scale;
    let spin_r = 4 * scale;
    let gap = 4 * scale;
    let content_w = text_w + gap + spin_r * 2;
    let pill_h = 7 * scale + pad;
    let pill_w = content_w + pad * 2;
    let margin = 5 * scale;

    // Ease-out the slide and map it from just-below-the-window up to its resting spot.
    let eased = 1.0 - (1.0 - slide) * (1.0 - slide);
    let rest_y = h as i32 - pill_h - margin;
    let py = (h as f32 + (rest_y - h as i32) as f32 * eased) as i32;
    let px = margin;

    draw_pill(fb, w, h, px, py, pill_w, pill_h, 0x161616, 0.84);
    let ty = py + (pill_h - 7 * scale) / 2;
    let after = draw_text(fb, w, h, text, px + pad, ty, scale, 0xE6E6E6);
    draw_spinner(fb, w, h, after + gap + spin_r, py + pill_h / 2, spin_r, scale.max(2), phase);
}

/// Filled, alpha-blended rounded rectangle (the toast background).
fn draw_pill(fb: &mut [u32], fbw: usize, fbh: usize, x: i32, y: i32, pw: i32, ph: i32, color: u32, a: f32) {
    let r = (ph / 3).clamp(3, ph / 2);
    for yy in 0..ph {
        for xx in 0..pw {
            // Clamp into the straight-edged core; outside it, test the corner radius.
            let cx = xx.clamp(r, pw - 1 - r);
            let cy = yy.clamp(r, ph - 1 - r);
            let (dx, dy) = ((xx - cx) as f32, (yy - cy) as f32);
            if dx * dx + dy * dy <= (r * r) as f32 {
                blend_pixel(fb, fbw, fbh, x + xx, y + yy, color, a);
            }
        }
    }
}

/// Eight dots around a circle with a bright "head" trailing into dim — a classic
/// rotating spinner, advanced one position every few ticks.
fn draw_spinner(fb: &mut [u32], fbw: usize, fbh: usize, cx: i32, cy: i32, radius: i32, dot: i32, phase: u32) {
    const N: i32 = 8;
    let head = (phase / 3) as i32 % N;
    for k in 0..N {
        let ang = k as f32 * (std::f32::consts::TAU / N as f32) - std::f32::consts::FRAC_PI_2;
        let dx = (ang.cos() * radius as f32).round() as i32;
        let dy = (ang.sin() * radius as f32).round() as i32;
        let age = (head - k).rem_euclid(N) as f32; // 0 = head (brightest)
        let v = (40.0 + (1.0 - age / N as f32) * 215.0) as u32;
        fill_disc(fb, fbw, fbh, cx + dx, cy + dy, dot, (v << 16) | (v << 8) | v);
    }
}

/// Solid `w`x`h` rectangle at (x,y), clipped to the framebuffer.
fn fill_rect(fb: &mut [u32], fbw: usize, fbh: usize, x: i32, y: i32, w: i32, h: i32, color: u32) {
    for yy in y..y + h {
        if yy < 0 || yy as usize >= fbh {
            continue;
        }
        for xx in x..x + w {
            if xx >= 0 && (xx as usize) < fbw {
                fb[yy as usize * fbw + xx as usize] = color;
            }
        }
    }
}

/// Filled disc of radius `r` centered at (cx,cy).
fn fill_disc(fb: &mut [u32], fbw: usize, fbh: usize, cx: i32, cy: i32, r: i32, color: u32) {
    for yy in -r..=r {
        for xx in -r..=r {
            if xx * xx + yy * yy <= r * r {
                fill_rect(fb, fbw, fbh, cx + xx, cy + yy, 1, 1, color);
            }
        }
    }
}

/// Alpha-blend `color` over one framebuffer pixel.
pub fn blend_pixel(fb: &mut [u32], fbw: usize, fbh: usize, x: i32, y: i32, color: u32, a: f32) {
    if x < 0 || y < 0 || x as usize >= fbw || y as usize >= fbh {
        return;
    }
    let i = y as usize * fbw + x as usize;
    let (a, inv) = (a.clamp(0.0, 1.0), 1.0 - a.clamp(0.0, 1.0));
    let d = fb[i];
    let mix = |sh: u32| {
        let s = ((color >> sh) & 0xff) as f32;
        let dd = ((d >> sh) & 0xff) as f32;
        (s * a + dd * inv) as u32
    };
    fb[i] = (mix(16) << 16) | (mix(8) << 8) | mix(0);
}
