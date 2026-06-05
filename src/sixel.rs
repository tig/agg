//! Sixel graphics decoding.
//!
//! Sixel images are carried in a DCS (Device Control String) of the form
//! `ESC P <macro-params> q <sixel-data> ST`. Each data character in the range
//! `?`..=`~` encodes one column of six vertically stacked pixels: subtracting
//! `0x3f` yields a six-bit value whose least-significant bit is the topmost
//! pixel. Bands of six pixels are stacked top-to-bottom, separated by the
//! graphics-newline command `-`.
//!
//! This module decodes the data portion (everything from the macro parameters
//! up to, but not including, the string terminator) into a tightly packed RGBA
//! buffer. Pixels never written by the stream are left transparent so an image
//! composites cleanly over the terminal background.

use rgb::{RGB8, RGBA8};

const TRANSPARENT: RGBA8 = RGBA8::new(0, 0, 0, 0);

/// A decoded sixel image as a row-major, tightly packed RGBA pixel buffer.
#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<RGBA8>,
}

/// Decode the body of a sixel DCS — the bytes between the `ESC P` introducer
/// and the string terminator. Returns `None` when the body carries no `q`
/// introducer or describes an empty image.
pub fn decode(body: &str) -> Option<Image> {
    // The sixel data proper begins after the `q` that closes the DCS macro
    // parameters (P1;P2;P3). Anything before it is ignored: the default 1:1
    // pixel aspect ratio is assumed.
    let data_start = body.find('q')? + 1;
    let chars: Vec<char> = body[data_start..].chars().collect();

    let mut decoder = Decoder::new();
    decoder.run(&chars);
    decoder.into_image()
}

struct Decoder {
    palette: Vec<RGB8>,
    color: RGB8,
    /// Current pixel column.
    x: usize,
    /// Top pixel row of the current six-pixel band.
    band: usize,
    /// Pixel rows, each a tightly packed slice of columns. Grown lazily.
    rows: Vec<Vec<RGBA8>>,
    /// Extents reached so far, including columns advanced over empty data.
    max_width: usize,
    max_height: usize,
    /// Canvas size declared via the raster-attributes command, if any.
    declared_width: usize,
    declared_height: usize,
}

impl Decoder {
    fn new() -> Self {
        let palette = default_palette();
        let color = palette[0];

        Self {
            palette,
            color,
            x: 0,
            band: 0,
            rows: Vec::new(),
            max_width: 0,
            max_height: 0,
            declared_width: 0,
            declared_height: 0,
        }
    }

    fn run(&mut self, chars: &[char]) {
        let mut i = 0;

        while i < chars.len() {
            match chars[i] {
                '#' => i = self.handle_color(chars, i + 1),
                '"' => i = self.handle_raster(chars, i + 1),
                '!' => i = self.handle_repeat(chars, i + 1),
                '$' => {
                    self.x = 0;
                    i += 1;
                }
                '-' => {
                    self.x = 0;
                    self.band += 6;
                    i += 1;
                }
                c @ '?'..='~' => {
                    self.put_sixel(c as u8 - 0x3f, 1);
                    i += 1;
                }
                // Whitespace and any unrecognized control characters are
                // ignored, matching how real decoders skip stream padding.
                _ => i += 1,
            }
        }
    }

    /// `#Pc` selects color register `Pc`; `#Pc;Pu;Px;Py;Pz` defines it. `Pu` is
    /// the color space: `2` for RGB, `1` for HLS, both with components scaled
    /// to 0..=100 (hue is 0..360).
    fn handle_color(&mut self, chars: &[char], start: usize) -> usize {
        let (pc, mut i) = parse_number(chars, start);

        if !matches!(chars.get(i), Some(';')) {
            self.color = self.palette.get(pc).copied().unwrap_or(self.palette[0]);
            return i;
        }

        let pu;
        let px;
        let py;
        let pz;
        (pu, i) = parse_number(chars, i + 1);
        (px, i) = parse_param(chars, i);
        (py, i) = parse_param(chars, i);
        (pz, i) = parse_param(chars, i);

        let color = match pu {
            1 => hls_to_rgb(px, py, pz),
            _ => rgb_from_percent(px, py, pz),
        };

        if pc < self.palette.len() {
            self.palette[pc] = color;
        }

        self.color = color;
        i
    }

    /// `"Pan;Pad;Ph;Pv` declares the pixel aspect ratio (ignored) and the
    /// raster width `Ph` and height `Pv`, which size the canvas even where the
    /// stream leaves trailing rows or columns blank.
    fn handle_raster(&mut self, chars: &[char], start: usize) -> usize {
        let mut i = start;
        let _pan;
        let _pad;
        let ph;
        let pv;
        (_pan, i) = parse_number(chars, i);
        (_pad, i) = parse_param(chars, i);
        (ph, i) = parse_param(chars, i);
        (pv, i) = parse_param(chars, i);

        self.declared_width = self.declared_width.max(ph);
        self.declared_height = self.declared_height.max(pv);
        i
    }

    /// `!Pn<c>` repeats the sixel data character `c` `Pn` times.
    fn handle_repeat(&mut self, chars: &[char], start: usize) -> usize {
        let (count, i) = parse_number(chars, start);

        match chars.get(i) {
            Some(&c @ '?'..='~') => {
                self.put_sixel(c as u8 - 0x3f, count.max(1));
                i + 1
            }
            _ => i,
        }
    }

    /// Paint `count` consecutive columns from one six-bit sixel value, the
    /// least-significant bit being the topmost pixel of the current band.
    fn put_sixel(&mut self, value: u8, count: usize) {
        for _ in 0..count {
            for bit in 0..6 {
                if value & (1 << bit) != 0 {
                    self.plot(self.x, self.band + bit);
                }
            }

            self.x += 1;
            self.max_width = self.max_width.max(self.x);
        }

        // A data character occupies the full six-pixel band even when only
        // some bits are set, so the canvas is band-aligned in height.
        self.max_height = self.max_height.max(self.band + 6);
    }

    fn plot(&mut self, x: usize, y: usize) {
        if self.rows.len() <= y {
            self.rows.resize(y + 1, Vec::new());
        }

        let row = &mut self.rows[y];

        if row.len() <= x {
            row.resize(x + 1, TRANSPARENT);
        }

        row[x] = self.color.with_alpha(255);
        self.max_height = self.max_height.max(y + 1);
    }

    fn into_image(self) -> Option<Image> {
        let width = self.max_width.max(self.declared_width);
        let height = self.max_height.max(self.declared_height);

        if width == 0 || height == 0 {
            return None;
        }

        let mut pixels = vec![TRANSPARENT; width * height];

        for (y, row) in self.rows.iter().enumerate() {
            for (x, &px) in row.iter().enumerate() {
                pixels[y * width + x] = px;
            }
        }

        Some(Image {
            width,
            height,
            pixels,
        })
    }
}

/// Parse an optional decimal number at `start`, returning its value (`0` when
/// absent) and the index of the first non-digit character.
fn parse_number(chars: &[char], start: usize) -> (usize, usize) {
    let mut value = 0usize;
    let mut i = start;

    while let Some(d) = chars.get(i).and_then(|c| c.to_digit(10)) {
        value = value.saturating_mul(10).saturating_add(d as usize);
        i += 1;
    }

    (value, i)
}

/// Parse a `;`-prefixed parameter, as used by the color and raster commands.
/// A missing separator or value yields `0` and leaves the index unchanged past
/// any consumed separator.
fn parse_param(chars: &[char], i: usize) -> (usize, usize) {
    match chars.get(i) {
        Some(';') => parse_number(chars, i + 1),
        _ => (0, i),
    }
}

fn rgb_from_percent(r: usize, g: usize, b: usize) -> RGB8 {
    RGB8::new(percent_to_u8(r), percent_to_u8(g), percent_to_u8(b))
}

fn percent_to_u8(v: usize) -> u8 {
    ((v.min(100) * 255 + 50) / 100) as u8
}

/// Convert DEC sixel HLS to RGB. Sixel hue is measured so that 0° is blue,
/// 120° red and 240° green, a 240° rotation of the conventional HSL wheel.
/// Lightness and saturation are percentages.
fn hls_to_rgb(h: usize, l: usize, s: usize) -> RGB8 {
    let h = ((h % 360) as f64 + 240.0) % 360.0 / 360.0;
    let l = (l.min(100) as f64) / 100.0;
    let s = (s.min(100) as f64) / 100.0;

    if s == 0.0 {
        let v = (l * 255.0).round() as u8;
        return RGB8::new(v, v, v);
    }

    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let p = 2.0 * l - q;

    let r = hue_to_channel(p, q, h + 1.0 / 3.0);
    let g = hue_to_channel(p, q, h);
    let b = hue_to_channel(p, q, h - 1.0 / 3.0);

    RGB8::new(
        (r * 255.0).round() as u8,
        (g * 255.0).round() as u8,
        (b * 255.0).round() as u8,
    )
}

fn hue_to_channel(p: f64, q: f64, t: f64) -> f64 {
    let t = t.rem_euclid(1.0);

    if t < 1.0 / 6.0 {
        p + (q - p) * 6.0 * t
    } else if t < 1.0 / 2.0 {
        q
    } else if t < 2.0 / 3.0 {
        p + (q - p) * (2.0 / 3.0 - t) * 6.0
    } else {
        p
    }
}

/// The VT340 default 16-color palette, with the remaining registers black.
/// Components are the DEC-documented percentages scaled to 0..=255.
fn default_palette() -> Vec<RGB8> {
    const DEFAULTS: [(usize, usize, usize); 16] = [
        (0, 0, 0),
        (20, 20, 80),
        (80, 13, 13),
        (20, 80, 20),
        (80, 20, 80),
        (20, 80, 80),
        (80, 80, 20),
        (53, 53, 53),
        (26, 26, 26),
        (33, 33, 60),
        (60, 26, 26),
        (33, 60, 33),
        (60, 33, 60),
        (33, 60, 60),
        (60, 60, 33),
        (80, 80, 80),
    ];

    let mut palette = vec![RGB8::new(0, 0, 0); 256];

    for (i, (r, g, b)) in DEFAULTS.into_iter().enumerate() {
        palette[i] = rgb_from_percent(r, g, b);
    }

    palette
}

#[cfg(test)]
mod tests {
    use super::*;

    // A red pixel defined in register 0 then plotted once. `@` is 0x40, i.e.
    // value 1 — only the topmost pixel of the band is set.
    const RED: RGB8 = RGB8::new(255, 0, 0);

    fn opaque(c: RGB8) -> RGBA8 {
        c.with_alpha(255)
    }

    #[test]
    fn decodes_single_red_pixel() {
        let image = decode("q#0;2;100;0;0@").unwrap();

        assert_eq!(image.width, 1);
        assert_eq!(image.height, 6);
        assert_eq!(image.pixels[0], opaque(RED));
        // Only the topmost pixel of the six-pixel band is set; the rest stay
        // transparent.
        assert!(image.pixels[1..].iter().all(|p| p.a == 0));
    }

    #[test]
    fn honors_raster_dimensions_for_blank_canvas() {
        // Raster attributes declare a 4x12 canvas; a single top-left pixel is
        // plotted, the rest remain transparent but contribute to the size.
        let image = decode("q\"1;1;4;12#0;2;100;100;100@").unwrap();

        assert_eq!(image.width, 4);
        assert_eq!(image.height, 12);
        assert_eq!(image.pixels.len(), 48);
        assert_eq!(image.pixels[0], opaque(RGB8::new(255, 255, 255)));
    }

    #[test]
    fn full_band_sets_six_stacked_pixels() {
        // `~` is 0x7e -> value 63 -> all six bits set.
        let image = decode("q#0;2;0;100;0~").unwrap();

        assert_eq!(image.width, 1);
        assert_eq!(image.height, 6);
        assert!(image
            .pixels
            .iter()
            .all(|p| *p == opaque(RGB8::new(0, 255, 0))));
    }

    #[test]
    fn run_length_repeats_columns() {
        // Repeat a full-height column five times.
        let image = decode("q#0;2;0;0;100!5~").unwrap();

        assert_eq!(image.width, 5);
        assert_eq!(image.height, 6);
        assert!(image
            .pixels
            .iter()
            .all(|p| *p == opaque(RGB8::new(0, 0, 255))));
    }

    #[test]
    fn graphics_newline_starts_a_new_band() {
        // Top band fully set, newline, bottom band fully set: 12 rows tall.
        let image = decode("q#0;2;100;100;100~-~").unwrap();

        assert_eq!(image.width, 1);
        assert_eq!(image.height, 12);
        assert!(image.pixels.iter().all(|p| p.a == 255));
    }

    #[test]
    fn carriage_return_overwrites_same_band() {
        // Two columns, return to start, overwrite the first column's color.
        let image = decode("q#0;2;100;0;0~~$#1;2;0;100;0~").unwrap();

        assert_eq!(image.width, 2);
        assert_eq!(image.pixels[0], opaque(RGB8::new(0, 255, 0)));
        assert_eq!(image.pixels[1], opaque(RED));
    }

    #[test]
    fn selecting_a_predefined_register_uses_default_palette() {
        // Register 1 is the default blue without being redefined.
        let image = decode("q#1~").unwrap();

        assert_eq!(image.pixels[0], opaque(rgb_from_percent(20, 20, 80)));
    }

    #[test]
    fn rgb_percentages_scale_to_full_range() {
        assert_eq!(rgb_from_percent(100, 0, 0), RGB8::new(255, 0, 0));
        assert_eq!(rgb_from_percent(0, 100, 0), RGB8::new(0, 255, 0));
        assert_eq!(percent_to_u8(50), 128);
    }

    #[test]
    fn hls_primaries_match_dec_hue_wheel() {
        // Sixel hue 0° is blue, 120° red, 240° green, all at 50% lightness,
        // full saturation.
        assert_eq!(hls_to_rgb(0, 50, 100), RGB8::new(0, 0, 255));
        assert_eq!(hls_to_rgb(120, 50, 100), RGB8::new(255, 0, 0));
        assert_eq!(hls_to_rgb(240, 50, 100), RGB8::new(0, 255, 0));
        // Zero saturation collapses to a gray determined by lightness.
        assert_eq!(hls_to_rgb(0, 100, 0), RGB8::new(255, 255, 255));
    }

    #[test]
    fn returns_none_without_introducer() {
        assert!(decode("#0;2;100;0;0@").is_none());
    }

    #[test]
    fn returns_none_for_empty_data() {
        assert!(decode("q").is_none());
    }

    #[test]
    fn ignores_whitespace_between_commands() {
        let image = decode("q\n#0;2;100;0;0\n@\n").unwrap();

        assert_eq!(image.width, 1);
        assert_eq!(image.pixels[0], opaque(RED));
    }
}
