//! DEC SIXEL graphics `.six` / `.sixel`: the terminal image format, decoded here.
//!
//! A SIXEL image is a device control string: `ESC P` (or the 8-bit `0x90`), parameters,
//! `q`, then a stream of commands, up to `ESC \`. Each data character from `?` to `~` is six
//! vertical pixels (bit 0 on top) in the current colour, one column wide; `!n` repeats the
//! next one `n` times, `$` returns to the left edge of the current six-pixel band, `-`
//! moves down a band, `#c` selects colour register `c` and `#c;u;x;y;z` also defines it
//! (`u` = 1 for HLS, whose hue 0 is BLUE, or 2 for RGB, both in percent), and `"a;b;w;h`
//! declares the picture's size. That is the whole format (VT330/VT340 Programmer Reference,
//! chapter 14), which is why it is decoded here rather than by a library.
//!
//! Native because ImageMagick 7.1.2's reader draws every real file this was checked against
//! as solid black (libsixel's `snake.six`, the sixel-testsuite's `snake.six` and its JWST
//! frame, VT340 captures from hackerb9's vt340test), 2026-09-22. Pixels no command paints
//! stay transparent, whatever the background parameter asks: a thumbnail has no terminal
//! background to show through.
//!
//! Real files often carry a little terminal chatter first (a newline, a `CSI` sequence), so
//! the control string may start anywhere in the first [`PREAMBLE`] bytes as long as what
//! comes before it is printable text and escapes: a binary file never passes that.

use image::{DynamicImage, RgbaImage};

use crate::decode::limits::MAX_DIM;

const PREAMBLE: usize = 512;
/// The same ceiling `pix.rs` uses: what one forged size can make this module allocate.
const MAX_PIXELS: u64 = 32 * 1024 * 1024;
/// A picture painted over and over (`$` and a repeat) can ask for unbounded work in a small
/// file; this many pixel writes per picture pixel is past any real encoder's output.
const WRITES_PER_PIXEL: u64 = 16;
/// Pixels the canvas may have per byte of file, and the canvas any file may have (4 Mpx):
/// see `extract`.
const PIXELS_PER_BYTE: u64 = 2048;
const MIN_CANVAS: u64 = 4 * 1024 * 1024;

/// Where the command stream starts (just past the `q`), and the background parameter.
fn body_start(b: &[u8]) -> Option<usize> {
    let window = &b[..b.len().min(PREAMBLE)];
    let mut i = 0;
    while i < window.len() {
        let params = match window[i] {
            0x90 => i + 1,
            0x1B if window.get(i + 1) == Some(&b'P') => i + 2,
            0x09 | 0x0A | 0x0D | 0x1B | 0x20..=0x7E => {
                i += 1;
                continue;
            }
            _ => return None,
        };
        let digits = window[params..]
            .iter()
            .take_while(|c| c.is_ascii_digit() || **c == b';')
            .count();
        if b.get(params + digits) == Some(&b'q') {
            return Some(params + digits + 1);
        }
        i = params;
    }
    None
}

pub fn looks_like_sixel(b: &[u8]) -> bool {
    body_start(b).is_some()
}

/// What the command stream does, in order.
enum Op {
    /// `"a;b;w;h`: the declared width and height.
    Raster(u32, u32),
    /// `#c` selects colour register `c`; `#c;u;x;y;z` defines it first.
    Colour(usize, Option<[u8; 3]>),
    /// Paint `bits` (bit 0 on top) in `count` columns from `x` in six-pixel `band`.
    Paint {
        x: u32,
        band: u32,
        bits: u8,
        count: u32,
    },
}

/// Read a decimal number at `*i`, advancing past it; saturates rather than overflowing.
fn number(b: &[u8], i: &mut usize) -> u32 {
    let mut n: u32 = 0;
    while let Some(d) = b.get(*i).filter(|c| c.is_ascii_digit()) {
        n = n.saturating_mul(10).saturating_add(u32::from(d - b'0'));
        *i += 1;
    }
    n
}

/// Up to five `;`-separated numbers at `*i`.
fn numbers(b: &[u8], i: &mut usize) -> ([u32; 5], usize) {
    let mut v = [0; 5];
    let mut n = 0;
    loop {
        let x = number(b, i);
        if n < 5 {
            v[n] = x;
        }
        n += 1;
        if b.get(*i) != Some(&b';') {
            return (v, n.min(5));
        }
        *i += 1;
    }
}

/// Where the command stream is: the next command's offset, and where the next sixel goes
/// (column `x` of six-pixel `band`).
struct Stream<'a> {
    b: &'a [u8],
    i: usize,
    x: u32,
    band: u32,
}

/// What one command came to.
enum Step {
    Op(Op),
    Skip,
    End,
}

impl Stream<'_> {
    /// Read one command. The string terminator (any `ESC`, or `0x9C`) and the end of the
    /// file end the stream, and so does a column or band past [`MAX_DIM`].
    fn step(&mut self) -> Step {
        let Some(&c) = self.b.get(self.i) else {
            return Step::End;
        };
        self.i += 1;
        match c {
            0x1B | 0x9C => Step::End,
            b'"' => {
                let (v, _) = numbers(self.b, &mut self.i);
                Step::Op(Op::Raster(v[2], v[3]))
            }
            b'#' => Step::Op(self.colour()),
            b'$' => {
                self.x = 0;
                Step::Skip
            }
            b'-' => {
                self.x = 0;
                self.band += 1;
                Step::Skip
            }
            b'!' => self.repeat(),
            c => self.sixel(c, 1),
        }
    }

    fn colour(&mut self) -> Op {
        let (v, n) = numbers(self.b, &mut self.i);
        let rgb = match (n, v[1]) {
            (5, 1) => Some(hls(v[2], v[3], v[4])),
            (5, 2) => Some([v[2], v[3], v[4]].map(percent)),
            _ => None,
        };
        Op::Colour(v[0] as usize % 256, rgb)
    }

    /// `!n`, then the sixel it repeats.
    fn repeat(&mut self) -> Step {
        let count = number(self.b, &mut self.i).clamp(1, MAX_DIM);
        let Some(&c) = self.b.get(self.i) else {
            return Step::End;
        };
        self.i += 1;
        self.sixel(c, count)
    }

    fn sixel(&mut self, c: u8, count: u32) -> Step {
        if !(0x3F..=0x7E).contains(&c) {
            return Step::Skip;
        }
        if self.x.saturating_add(count) > MAX_DIM || self.band >= MAX_DIM / 6 {
            return Step::End;
        }
        let op = Op::Paint {
            x: self.x,
            band: self.band,
            bits: c - 0x3F,
            count,
        };
        self.x += count;
        Step::Op(op)
    }
}

/// Walk the command stream from `start`, calling `f` for every operation.
fn run(b: &[u8], start: usize, mut f: impl FnMut(Op)) {
    let mut s = Stream {
        b,
        i: start,
        x: 0,
        band: 0,
    };
    loop {
        match s.step() {
            Step::Op(op) => f(op),
            Step::Skip => {}
            Step::End => return,
        }
    }
}

fn percent(v: u32) -> u8 {
    (v.min(100) * 255 / 100) as u8
}

/// DEC HLS (hue 0 = blue, 120 = red, 240 = green; lightness and saturation in percent)
/// to RGB.
fn hls(hue: u32, light: u32, sat: u32) -> [u8; 3] {
    let h = ((hue % 360 + 240) % 360) as f32 / 360.0;
    let l = light.min(100) as f32 / 100.0;
    let s = sat.min(100) as f32 / 100.0;
    if s == 0.0 {
        let v = (l * 255.0).round() as u8;
        return [v, v, v];
    }
    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let p = 2.0 * l - q;
    let channel = |t: f32| {
        let t = t.rem_euclid(1.0);
        let v = if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        };
        (v * 255.0).round().clamp(0.0, 255.0) as u8
    };
    [channel(h + 1.0 / 3.0), channel(h), channel(h - 1.0 / 3.0)]
}

/// The VT340's power-on palette, in percent (registers 16 and up start black).
const VT340: [[u32; 3]; 16] = [
    [0, 0, 0],
    [20, 20, 80],
    [80, 13, 13],
    [20, 80, 20],
    [80, 20, 80],
    [20, 80, 80],
    [80, 80, 20],
    [53, 53, 53],
    [26, 26, 26],
    [33, 33, 60],
    [60, 26, 26],
    [33, 60, 33],
    [60, 33, 60],
    [33, 60, 60],
    [60, 60, 33],
    [80, 80, 80],
];

/// Decode the picture, or `None` when it is not one or paints nothing.
pub fn extract(b: &[u8]) -> Option<DynamicImage> {
    let start = body_start(b)?;
    let (width, height) = measure(b, start)?;
    // The canvas is paid for by the file: a few bytes of raster attributes, or one long
    // repeat and a run of `-`, must not buy a 128 MB picture. Even a flat 4K frame, the most
    // compressible real SIXEL there is, stays under PIXELS_PER_BYTE.
    let allowed = (b.len() as u64)
        .saturating_mul(PIXELS_PER_BYTE)
        .clamp(MIN_CANVAS, MAX_PIXELS);
    if u64::from(width) * u64::from(height) > allowed {
        return None;
    }
    let mut canvas = Canvas::new(width, height);
    run(b, start, |op| canvas.apply(op));
    (canvas.budget > 0).then_some(DynamicImage::ImageRgba8(canvas.img))
}

/// Pass 1: the picture's size, allocating nothing. `None` when nothing is painted.
fn measure(b: &[u8], start: usize) -> Option<(u32, u32)> {
    let (mut declared, mut right, mut bands) = ((0, 0), 0u32, 0u32);
    run(b, start, |op| match op {
        Op::Raster(w, h) => declared = (w, h),
        Op::Paint {
            x,
            band,
            bits,
            count,
        } => {
            right = right.max(x + count);
            if bits != 0 {
                bands = bands.max(band + 1);
            }
        }
        Op::Colour(..) => {}
    });
    (bands > 0).then(|| canvas_size(declared, right, bands))
}

/// The painted extent, grown to a declared size; a declared height also trims the last
/// band's spare rows (371 rows is 62 bands of six), but one that would cut whole bands off
/// is not believed.
fn canvas_size(declared: (u32, u32), right: u32, bands: u32) -> (u32, u32) {
    let rows = bands * 6;
    let height = match declared.1 {
        h if h > 0 && h <= rows && rows - h < 6 => h,
        h => rows.max(h),
    };
    (right.max(declared.0).min(MAX_DIM), height.min(MAX_DIM))
}

/// Pass 2: the picture, the colour registers, the current colour, and the pixel writes left.
struct Canvas {
    img: RgbaImage,
    palette: [[u8; 3]; 256],
    colour: usize,
    budget: u64,
}

impl Canvas {
    fn new(width: u32, height: u32) -> Self {
        let mut palette = [[0u8; 3]; 256];
        for (slot, rgb) in palette.iter_mut().zip(VT340) {
            *slot = rgb.map(percent);
        }
        Canvas {
            img: RgbaImage::new(width, height),
            palette,
            colour: 0,
            budget: (u64::from(width) * u64::from(height)).saturating_mul(WRITES_PER_PIXEL),
        }
    }

    fn apply(&mut self, op: Op) {
        match op {
            Op::Colour(reg, rgb) => {
                if let Some(rgb) = rgb {
                    self.palette[reg] = rgb;
                }
                self.colour = reg;
            }
            Op::Paint {
                x,
                band,
                bits,
                count,
            } => self.paint(x, band, bits, count),
            Op::Raster(..) => {}
        }
    }

    fn paint(&mut self, x: u32, band: u32, bits: u8, count: u32) {
        let right = (x + count).min(self.img.width());
        if self.budget == 0 || x >= right {
            return;
        }
        let [r, g, b] = self.palette[self.colour];
        let height = self.img.height();
        let rows = (0..6u32).filter(|&k| bits & (1u8 << k) != 0);
        for y in rows.map(|k| band * 6 + k).filter(|&y| y < height) {
            self.budget = self.budget.saturating_sub(u64::from(right - x));
            for px in x..right {
                self.img.put_pixel(px, y, image::Rgba([r, g, b, 255]));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_small_picture_decodes() {
        // Two colours: register 1 red (RGB), register 2 pure blue in HLS (hue 0, 50%, 100%).
        // Column 0 is red top to bottom (`~` = all six bits), then three columns of blue on
        // the top half only (`F` = bits 0..2), then a new band with one red column.
        let v = b"\x1bPq\"1;1;4;12#1;2;100;0;0#2;1;0;50;100#1~#2!3F-#1~\x1b\\";
        let img = extract(v).expect("sixel decodes").to_rgba8();
        assert_eq!(img.dimensions(), (4, 12));
        assert_eq!(img.get_pixel(0, 5).0, [255, 0, 0, 255]);
        assert_eq!(img.get_pixel(3, 2).0, [0, 0, 255, 255]);
        assert_eq!(img.get_pixel(3, 3).0[3], 0, "unpainted stays transparent");
        assert_eq!(img.get_pixel(0, 11).0, [255, 0, 0, 255], "second band");
        assert_eq!(img.get_pixel(1, 11).0[3], 0);
    }

    #[test]
    fn dec_hue_zero_is_blue_and_120_is_red() {
        assert_eq!(hls(0, 50, 100), [0, 0, 255]);
        assert_eq!(hls(120, 50, 100), [255, 0, 0]);
        assert_eq!(hls(240, 50, 100), [0, 255, 0]);
        assert_eq!(hls(77, 40, 0), [102, 102, 102]);
    }

    #[test]
    fn a_short_preamble_is_allowed_and_binary_is_not() {
        let body = b"#1;2;0;100;0#1~~\x1b\\";
        let with = [b"\n\x1b[2 I\x1bP0;2;6q".as_slice(), body].concat();
        assert!(extract(&with).is_some());
        let binary = [b"\x89PNG\x1bPq".as_slice(), body].concat();
        assert!(!looks_like_sixel(&binary));
        assert!(extract(b"\x1bPq\x1b\\").is_none(), "paints nothing");
        assert!(
            !looks_like_sixel(b"\x1bP1$r\x1b\\"),
            "a DECRQSS reply is not SIXEL"
        );
    }

    #[test]
    fn hostile_sizes_and_overpainting_are_bounded() {
        // A repeat is clamped to MAX_DIM, and a column or band past it ends the stream, so
        // nothing a count or a run of `-` asks for can size the picture past the ceiling.
        let wide = extract(b"\x1bPq!99999~!99999~\x1b\\")
            .expect("clamped")
            .to_rgba8();
        assert_eq!(wide.dimensions(), (MAX_DIM, 6));
        let tall = [b"\x1bPq".as_slice(), &b"~-".repeat(5000)].concat();
        assert!(extract(&tall).expect("clipped").height() <= MAX_DIM);
        // Painting the same 16 columns over and over runs out of budget.
        let mut v = b"\x1bPq".to_vec();
        for _ in 0..200 {
            v.extend_from_slice(b"!16~$");
        }
        assert!(extract(&v).is_none());
    }

    /// Real files, where the corpus has them: every one ImageMagick drew black.
    #[test]
    fn real_files_decode_with_colour() {
        for (name, dims) in [("real.six", (600, 450)), ("real-vt340.six", (480, 480))] {
            let Some(bytes) = crate::testcorpus::read(name) else {
                eprintln!("NOT MEASURED: {name} absent");
                continue;
            };
            let img = extract(&bytes)
                .unwrap_or_else(|| panic!("{name}"))
                .to_rgba8();
            assert_eq!(img.dimensions(), dims, "{name}");
            let lit = img
                .pixels()
                .filter(|p| p.0[3] == 255 && p.0[..3] != [0, 0, 0])
                .count();
            assert!(
                lit > img.pixels().len() / 4,
                "{name} is mostly painted colour"
            );
        }
    }
}
