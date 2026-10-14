//! DVC (Dambrackas Video Compression) decoder, ported from the reference
//! viewer's `com.avocent.kvm.a.a`.
//!
//! The video channel frames each update as:
//!
//! ```text
//! "VID\0" | u16 type | u16 total_len | "deadbeef" | u16 h | u16 w | u8 flags | 3? | DVC data
//! ```
//!
//! `type` picks the color mode (0x8601 = DVC15/RGB555). `flags` bit0 = first
//! fragment, bit1 = last fragment; one screen may span several fragments whose
//! DVC opcode streams concatenate.
//!
//! The DVC opcode stream is decoded forward into a persistent ARGB framebuffer
//! in raster order. Each lead byte's top 3 bits select the op:
//!
//! ```text
//! 0x00 No-Change (skip N px, keep previous frame)   0x20 Copy-Left
//! 0x40 Copy-Above (row above, stride = width)        0x60 Make-Series (2-color)
//! 0x80+ Make-Pixel (literal, per color mode)
//! ```

/// DVC color mode, selected by the `VID` packet type. The IMM2 negotiates
/// DVC15 (RGB555); the others exist for slow-link / grayscale negotiations.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum ColorMode {
    Dvc15,    // RGB555, 2 bytes/px  (type 0x8601)
    Dvc7,     // 128-colour palette, 1 byte/px  (0x8602)
    Dvc7Gray, // 7-bit grayscale, 1 byte/px  (0x8603)
    Dvc23,    // 23-bit colour, 3 bytes/px  (0x860A)
}

impl ColorMode {
    pub(super) fn from_vid_type(t: u16) -> Self {
        match t {
            0x8602 => ColorMode::Dvc7,
            0x8603 => ColorMode::Dvc7Gray,
            0x860a => ColorMode::Dvc23,
            _ => ColorMode::Dvc15,
        }
    }
    /// DVC7/DVC7Gray add +2 to every run length (min encodable run of 2).
    fn run_bonus(self) -> usize {
        if matches!(self, ColorMode::Dvc7 | ColorMode::Dvc7Gray) {
            2
        } else {
            0
        }
    }
}

/// The DVC7 128-colour palette: a 5×5×5 RGB cube on the ramp {0,70,127,191,255}
/// plus 3 grays, permuted (recovered from com.avocent.kvm.a.a.a).
fn dvc7_palette() -> [u32; 128] {
    const RAMP: [u32; 5] = [0, 70, 127, 191, 255];
    let mut cube = [0xff00_0000u32; 128];
    for (blue, &b) in RAMP.iter().enumerate() {
        for (green, &g) in RAMP.iter().enumerate() {
            for (red, &r) in RAMP.iter().enumerate() {
                cube[25 * blue + 5 * green + red] = 0xff00_0000 | (r << 16) | (g << 8) | b;
            }
        }
    }
    cube[125] = 0xff5f_5f5f;
    cube[126] = 0xff9f_9f9f;
    cube[127] = 0xffdf_dfdf;
    const PERM: [u8; 128] = [
        0, 25, 50, 75, 5, 30, 55, 80, 10, 35, 60, 85, 15, 40, 65, 90, 1, 26, 51, 76, 6, 31, 56, 81, 11, 36,
        61, 86, 16, 41, 66, 91, 2, 27, 52, 77, 7, 32, 57, 82, 12, 37, 62, 87, 17, 42, 67, 92, 3, 28, 53, 78,
        8, 33, 58, 83, 13, 38, 63, 88, 18, 43, 68, 93, 100, 101, 102, 103, 105, 106, 107, 108, 110, 111, 112,
        113, 115, 116, 117, 118, 20, 45, 70, 95, 21, 46, 71, 96, 22, 47, 72, 97, 23, 48, 73, 98, 4, 29, 54,
        79, 9, 34, 59, 84, 14, 39, 64, 89, 19, 44, 69, 94, 120, 121, 122, 123, 104, 109, 114, 119, 24, 49,
        74, 99, 124, 125, 126, 127,
    ];
    let mut pal = [0xff00_0000u32; 128];
    for i in 0..128 {
        pal[i] = cube[PERM[i] as usize];
    }
    pal
}

/// Make-Pixel: decode one literal pixel in the active colour mode.
fn make_pixel(mode: ColorMode, lead: u8, data: &[u8], n: &mut usize, pal: &[u32; 128]) -> u32 {
    match mode {
        ColorMode::Dvc15 => {
            let hi = (lead & 0x7f) as u16;
            let lo = *data.get(*n).unwrap_or(&0) as u16;
            *n += 1;
            let v = (hi << 8) | lo; // 15-bit, MSB-first
            let r = (((v >> 10) & 0x1f) << 3) as u32;
            let g = (((v >> 5) & 0x1f) << 3) as u32;
            let b = ((v & 0x1f) << 3) as u32;
            0xff00_0000 | (r << 16) | (g << 8) | b
        }
        ColorMode::Dvc7 => pal[(lead & 0x7f) as usize],
        ColorMode::Dvc7Gray => {
            let g = ((lead & 0x7f) << 1) as u32;
            0xff00_0000 | (g << 16) | (g << 8) | g
        }
        ColorMode::Dvc23 => {
            let b0 = (lead & 0x7f) as u32;
            let b1 = *data.get(*n).unwrap_or(&0) as u32;
            let b2 = *data.get(*n + 1).unwrap_or(&0) as u32;
            *n += 2;
            let v = (b0 << 16) | (b1 << 8) | b2; // 23-bit, MSB-first
            let r = (v >> 15) & 0xff;
            let g = (v >> 7) & 0xff;
            let b = ((v & 0x7f) << 1) & 0xff;
            0xff00_0000 | (r << 16) | (g << 8) | b
        }
    }
}

/// DVC run length: low 5 bits of the lead, extended by following bytes whose
/// top 3 bits match the same op class (LSB-first 5-bit chunks, max 25 bits),
/// plus the mode's run bonus.
fn dvc_run_len(mode: ColorMode, op: u8, lead: u8, data: &[u8], n: &mut usize) -> usize {
    let mut len = (lead & 0x1f) as usize;
    let mut shift = 1;
    while shift < 5 {
        match data.get(*n) {
            Some(&nb) if (nb & 0xe0) == op => {
                *n += 1;
                len |= ((nb & 0x1f) as usize) << (shift * 5);
                shift += 1;
            }
            _ => break,
        }
    }
    len + mode.run_bonus()
}

/// Decode a concatenated DVC opcode stream into `fb` starting at `*cursor`
/// (raster order, stride = `width`). Advances `cursor`. Stops at end of data.
pub(super) fn dvc_decode_into(
    data: &[u8],
    fb: &mut [u32],
    width: usize,
    cursor: &mut usize,
    mode: ColorMode,
) {
    let pal = dvc7_palette();
    let mut n = 0usize;
    let cap = fb.len();
    while n < data.len() && *cursor < cap {
        let lead = data[n];
        n += 1;
        match lead & 0xe0 {
            0x00 => {
                // No Change — keep previous frame's pixels.
                let len = dvc_run_len(mode, 0x00, lead, data, &mut n);
                *cursor = (*cursor + len).min(cap);
            }
            0x20 => {
                // Copy Left — repeat the pixel immediately to the left.
                let len = dvc_run_len(mode, 0x20, lead, data, &mut n);
                let color = if *cursor == 0 { 0xff00_0000 } else { fb[*cursor - 1] };
                for _ in 0..len {
                    if *cursor >= cap {
                        break;
                    }
                    fb[*cursor] = color;
                    *cursor += 1;
                }
            }
            0x40 => {
                // Copy Above — copy the row above (stride = width).
                let len = dvc_run_len(mode, 0x40, lead, data, &mut n);
                for _ in 0..len {
                    if *cursor >= cap {
                        break;
                    }
                    if *cursor >= width {
                        fb[*cursor] = fb[*cursor - width];
                    }
                    *cursor += 1;
                }
            }
            0x60 => {
                // Make Series — two colors recovered from local history, a
                // selector bitmap chooses between them.
                let a = if *cursor == 0 { 0xff00_0000 } else { fb[*cursor - 1] };
                let mut b = a;
                let mut i = *cursor as isize - 1;
                while i >= 0 {
                    if fb[i as usize] != a {
                        b = fb[i as usize];
                        break;
                    }
                    i -= 1;
                }
                for bit in [0x08u8, 0x04, 0x02, 0x01] {
                    if *cursor >= cap {
                        break;
                    }
                    fb[*cursor] = if lead & bit != 0 { b } else { a };
                    *cursor += 1;
                }
                if lead & 0x10 != 0 {
                    loop {
                        let cb = *data.get(n).unwrap_or(&0);
                        n += 1;
                        for bit in [0x40u8, 0x20, 0x10, 0x08, 0x04, 0x02, 0x01] {
                            if *cursor >= cap {
                                break;
                            }
                            fb[*cursor] = if cb & bit != 0 { b } else { a };
                            *cursor += 1;
                        }
                        if cb & 0x80 == 0 {
                            break;
                        }
                    }
                }
            }
            _ => {
                // Make Pixel — literal, per color mode.
                let color = make_pixel(mode, lead, data, &mut n, &pal);
                if *cursor < cap {
                    fb[*cursor] = color;
                    *cursor += 1;
                }
            }
        }
    }
}
