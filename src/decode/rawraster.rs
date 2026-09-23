//! The simple rasters - binary PNM (PBM, PGM, PPM), PAM, PFM, farbfeld and TGA - read a row at
//! a time and shrunk as they are read, whatever their size.
//!
//! These formats are nothing but a header and the pixels, so a big one is big pixels: an
//! 11000x9500 scan saved as PPM is 313 MB, past the 256 MiB input ceiling, and a 17000-pixel
//! panorama is past the decoders' 16384 side limit. The `image` crate reads them only whole,
//! and only under both limits, so either file got nothing (the big-file gate's pixel axis,
//! 2026-09-23). Here the rows a smaller picture takes are read - by offset for the raw
//! layouts, in one pass for run-length TGA - and each row's columns box-averaged as it goes by.
//! The cost is the output picture plus one row, whatever the file.
//!
//! Called for what the `image` tier declines (a picture past its limits) and for a file past
//! the input ceiling; everything smaller keeps the `image` crate's reading, which this matches.

use std::io::{Read, Seek, SeekFrom};

use image::{DynamicImage, RgbaImage};

/// The longest side accepted.
const MAX_SIDE: u64 = 1 << 20;
/// The longest side the result is given: 256 MiB of RGBA.
const MAX_EDGE: u32 = 8192;
/// Longest header read (PNM comments can make it long).
const MAX_HEADER: usize = 64 * 1024;

/// How the samples of a pixel are stored.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Sample {
    /// One byte, scaled from `0..=max`.
    Byte(u16),
    /// Two bytes big-endian, scaled from `0..=max`.
    Word(u16),
    /// A 32-bit float, little- or big-endian, shown clamped to 0..=1.
    Float { le: bool },
    /// One bit, eight pixels a byte, a set bit black (PBM).
    Bit,
}

/// Where the pixels are and how they are laid out.
#[derive(Clone, Copy, Debug)]
struct Layout {
    width: u64,
    height: u64,
    /// Offset of the first stored row.
    data: u64,
    /// Samples a pixel: 1 grey, 2 grey+alpha, 3 colour, 4 colour+alpha.
    channels: usize,
    sample: Sample,
    /// Colour stored blue first (TGA).
    bgr: bool,
    /// The first stored row is the bottom one.
    bottom_up: bool,
    /// TGA run-length packets rather than raw rows.
    rle: bool,
}

impl Layout {
    fn row_bytes(&self) -> u64 {
        match self.sample {
            Sample::Bit => self.width.div_ceil(8),
            Sample::Byte(_) => self.width * self.channels as u64,
            Sample::Word(_) => self.width * self.channels as u64 * 2,
            Sample::Float { .. } => self.width * self.channels as u64 * 4,
        }
    }

    fn pixel_bytes(&self) -> usize {
        match self.sample {
            Sample::Bit => 0,
            Sample::Byte(_) => self.channels,
            Sample::Word(_) => self.channels * 2,
            Sample::Float { .. } => self.channels * 4,
        }
    }
}

/// Is `head` one of the formats read here?
pub(crate) fn is_raw_raster(head: &[u8]) -> bool {
    matches!(
        head.get(..2),
        Some(b"P4" | b"P5" | b"P6" | b"P7" | b"PF" | b"Pf")
    ) || head.starts_with(b"farbfeld")
        || tga_layout(head).is_some()
}

/// The whitespace-separated tokens of a PNM header, comments skipped, and the offset after the
/// single whitespace byte that ends it (`count` tokens after the magic).
fn pnm_tokens(head: &[u8], count: usize) -> Option<(Vec<u64>, u64)> {
    let mut out = Vec::with_capacity(count);
    let mut i = 2;
    while out.len() < count {
        let (v, next) = pnm_token(head, i)?;
        if let Some(v) = v {
            out.push(v);
        }
        i = next;
    }
    head.get(i)?
        .is_ascii_whitespace()
        .then_some((out, i as u64 + 1))
}

/// One token of the header at `i`: its value (`None` for a comment or whitespace, which are no
/// token) and the index just past it.
fn pnm_token(head: &[u8], i: usize) -> Option<(Option<u64>, usize)> {
    match *head.get(i)? {
        b'#' => {
            let nl = head.get(i..)?.iter().position(|&c| c == b'\n')?;
            Some((None, i + nl))
        }
        c if c.is_ascii_whitespace() => Some((None, i + 1)),
        c if c.is_ascii_digit() || c == b'-' || c == b'.' => {
            let (v, end) = pnm_number(head, i)?;
            Some((Some(v), end))
        }
        _ => None,
    }
}

/// The number token starting at `i`, and the index after it.
fn pnm_number(head: &[u8], i: usize) -> Option<(u64, usize)> {
    let end = i + head
        .get(i..)?
        .iter()
        .position(|c| c.is_ascii_whitespace())?;
    let text = std::str::from_utf8(head.get(i..end)?).ok()?;
    // PFM's scale is a float; only its sign (the byte order) matters.
    let v = if text.starts_with('-') {
        1
    } else {
        text.parse::<f64>().ok()? as u64
    };
    Some((v, end))
}

/// A PNM (P4, P5, P6) or PFM (PF, Pf) layout.
fn pnm_layout(head: &[u8]) -> Option<Layout> {
    let magic = head.get(..2)?;
    let (channels, tokens) = match magic {
        b"P4" => (1, 2),
        b"P5" => (1, 3),
        b"P6" => (3, 3),
        b"PF" => (3, 3),
        b"Pf" => (1, 3),
        _ => return None,
    };
    let (t, data) = pnm_tokens(head, tokens)?;
    let (width, height) = (t[0], t[1]);
    let sample = match magic {
        b"P4" => Sample::Bit,
        b"PF" | b"Pf" => {
            // A negative scale is little-endian.
            let text = std::str::from_utf8(head.get(..data as usize)?).ok()?;
            Sample::Float {
                le: text.split_ascii_whitespace().nth(3)?.starts_with('-'),
            }
        }
        _ => sample_of_max(t[2])?,
    };
    Some(Layout {
        width,
        height,
        data,
        channels,
        sample,
        bgr: false,
        bottom_up: matches!(magic, b"PF" | b"Pf"),
        rle: false,
    })
}

/// A byte or a big-endian word, by the file's MAXVAL.
fn sample_of_max(max: u64) -> Option<Sample> {
    match max {
        1..=255 => Some(Sample::Byte(max as u16)),
        256..=65535 => Some(Sample::Word(max as u16)),
        _ => None,
    }
}

/// A PAM (P7) layout: `WIDTH`, `HEIGHT`, `DEPTH`, `MAXVAL`, then `ENDHDR`.
fn pam_layout(head: &[u8]) -> Option<Layout> {
    let end = head.windows(7).position(|w| w == b"ENDHDR\n")? + 7;
    let text = std::str::from_utf8(head.get(..end)?).ok()?;
    let field = |key: &str| -> Option<u64> {
        text.lines()
            .find_map(|l| l.trim().strip_prefix(key))
            .and_then(|v| v.trim().parse().ok())
    };
    let channels = usize::try_from(field("DEPTH")?)
        .ok()
        .filter(|c| (1..=4).contains(c))?;
    Some(Layout {
        width: field("WIDTH")?,
        height: field("HEIGHT")?,
        data: end as u64,
        channels,
        sample: sample_of_max(field("MAXVAL")?)?,
        bgr: false,
        bottom_up: false,
        rle: false,
    })
}

/// A farbfeld layout: `farbfeld`, width and height big-endian, then 16-bit RGBA.
fn farbfeld_layout(head: &[u8]) -> Option<Layout> {
    let dim = |at: usize| {
        head.get(at..at + 4)
            .map(|b| u64::from(u32::from_be_bytes([b[0], b[1], b[2], b[3]])))
    };
    Some(Layout {
        width: dim(8)?,
        height: dim(12)?,
        data: 16,
        channels: 4,
        sample: Sample::Word(65535),
        bgr: false,
        bottom_up: false,
        rle: false,
    })
}

/// A true-colour or greyscale TGA (types 2, 3, 10, 11) at 8, 24 or 32 bits a pixel. A
/// colour-mapped or 15/16-bit one is left to the other tiers; so is anything whose header does
/// not add up, since TGA has no signature and this must not claim other files.
fn tga_layout(head: &[u8]) -> Option<Layout> {
    let h = head.get(..18)?;
    let (id_len, cmap_type, kind) = (h[0], h[1], h[2]);
    let (width, height) = (
        u16::from_le_bytes([h[12], h[13]]),
        u16::from_le_bytes([h[14], h[15]]),
    );
    let (depth, descriptor) = (h[16], h[17]);
    let channels = match (kind, depth) {
        (3 | 11, 8) => 1,
        (2 | 10, 24) => 3,
        (2 | 10, 32) => 4,
        _ => return None,
    };
    if cmap_type != 0 || width == 0 || height == 0 || descriptor & 0xC0 != 0 {
        return None;
    }
    Some(Layout {
        width: u64::from(width),
        height: u64::from(height),
        data: 18 + u64::from(id_len),
        channels,
        sample: Sample::Byte(255),
        bgr: channels >= 3,
        bottom_up: descriptor & 0x20 == 0,
        rle: kind >= 9,
    })
}

fn layout(head: &[u8]) -> Option<Layout> {
    let l = match head.get(..2)? {
        b"P7" => pam_layout(head),
        b"P4" | b"P5" | b"P6" | b"PF" | b"Pf" => pnm_layout(head),
        _ if head.starts_with(b"farbfeld") => farbfeld_layout(head),
        _ => tga_layout(head),
    }?;
    let sane = (1..=MAX_SIDE).contains(&l.width) && (1..=MAX_SIDE).contains(&l.height);
    sane.then_some(l)
}

/// One sample as a display byte.
fn level(s: &[u8], sample: Sample) -> u8 {
    match sample {
        Sample::Byte(max) => (u32::from(s[0]) * 255 / u32::from(max.max(1))).min(255) as u8,
        Sample::Word(max) => {
            let v = u32::from(u16::from_be_bytes([s[0], s[1]]));
            (v * 255 / u32::from(max.max(1))).min(255) as u8
        }
        Sample::Float { le } => {
            let b = [s[0], s[1], s[2], s[3]];
            let v = if le {
                f32::from_le_bytes(b)
            } else {
                f32::from_be_bytes(b)
            };
            let v = if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) };
            (v * 255.0).round() as u8
        }
        Sample::Bit => 0,
    }
}

/// One stored row as RGBA.
fn row_rgba(row: &[u8], l: &Layout) -> Vec<[u8; 4]> {
    if l.sample == Sample::Bit {
        return (0..l.width as usize)
            .map(|x| {
                let set = row.get(x / 8).is_some_and(|&b| b & (0x80 >> (x % 8)) != 0);
                let v = if set { 0 } else { 255 };
                [v, v, v, 255]
            })
            .collect();
    }
    let size = l.pixel_bytes() / l.channels;
    row.chunks_exact(l.pixel_bytes())
        .map(|px| {
            let at = |c: usize| level(&px[c * size..], l.sample);
            match l.channels {
                1 => {
                    let v = at(0);
                    [v, v, v, 255]
                }
                2 => [at(0), at(0), at(0), at(1)],
                3 if l.bgr => [at(2), at(1), at(0), 255],
                3 => [at(0), at(1), at(2), 255],
                _ if l.bgr => [at(2), at(1), at(0), at(3)],
                _ => [at(0), at(1), at(2), at(3)],
            }
        })
        .collect()
}

/// Which source rows and columns make the smaller picture, as the FITS and Photoshop readers
/// take them: a cell is `step` pixels square, its columns averaged along the row through the
/// middle of the band.
struct Grid {
    step: u64,
    tw: u64,
    th: u64,
    height: u64,
}

impl Grid {
    fn new(l: &Layout, edge: u32) -> Self {
        let edge = u64::from(edge.clamp(1, MAX_EDGE));
        // FLOOR, as `exrscale` and the XCF walk do: the caller resizes the grid to its target
        // with a real filter, so the grid must never come out SMALLER than asked for (ceil
        // handed a 300 px canvas asked for 256 a 150 px grid: the big-file gate's `.pdd`, a
        // blurrier tile than the same file under the input ceiling). The ceiling on the edge
        // still bounds the memory.
        let long = l.width.max(l.height);
        let step = (long / edge).max(long.div_ceil(u64::from(MAX_EDGE))).max(1);
        Self {
            step,
            tw: l.width.div_ceil(step),
            th: l.height.div_ceil(step),
            height: l.height,
        }
    }

    /// The display row output row `ty` is taken from.
    fn row(&self, ty: u64) -> u64 {
        (ty * self.step + self.step / 2).min(self.height - 1)
    }
}

/// Box-average one RGBA row into output row `ty` of `out`.
fn shrink_into(px: &[[u8; 4]], grid: &Grid, ty: u64, out: &mut [u8]) {
    let tw = grid.tw as usize;
    let line = &mut out[ty as usize * tw * 4..(ty as usize + 1) * tw * 4];
    for (cell, run) in line
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(px.chunks(grid.step as usize))
    {
        let mut sum = [0u32; 4];
        for p in run {
            sum.iter_mut().zip(p).for_each(|(s, &v)| *s += u32::from(v));
        }
        let n = run.len().max(1) as u32;
        cell.iter_mut()
            .zip(sum)
            .for_each(|(c, s)| *c = (s / n) as u8);
    }
}

/// The stored row that display row `y` is.
fn stored_row(l: &Layout, y: u64) -> u64 {
    if l.bottom_up {
        l.height - 1 - y
    } else {
        y
    }
}

/// Raw rows, each read by its offset.
fn read_raw<R: Read + Seek>(r: &mut R, l: &Layout, grid: &Grid, out: &mut [u8]) -> Option<()> {
    let mut row = vec![0u8; usize::try_from(l.row_bytes()).ok()?];
    for ty in 0..grid.th {
        let at = l
            .data
            .checked_add(stored_row(l, grid.row(ty)).checked_mul(l.row_bytes())?)?;
        r.seek(SeekFrom::Start(at)).ok()?;
        r.read_exact(&mut row).ok()?;
        shrink_into(&row_rgba(&row, l), grid, ty, out);
    }
    Some(())
}

/// Run-length TGA, decoded front to back; the rows the picture takes are kept as they pass.
fn read_rle<R: Read>(r: R, l: &Layout, grid: &Grid, out: &mut [u8]) -> Option<()> {
    let mut r = std::io::BufReader::with_capacity(1 << 20, r);
    let pb = l.pixel_bytes();
    let mut row = Vec::with_capacity(usize::try_from(l.row_bytes()).ok()?);
    let mut px = [0u8; 4];
    let mut repeat = 0usize;
    let mut literal = 0usize;
    for stored in 0..l.height {
        row.clear();
        while row.len() < l.row_bytes() as usize {
            rle_step(&mut r, &mut row, &mut px, pb, &mut repeat, &mut literal)?;
        }
        let y = stored_row(l, stored);
        let ty = y / grid.step;
        if ty < grid.th && grid.row(ty) == y {
            shrink_into(&row_rgba(&row, l), grid, ty, out);
        }
    }
    Some(())
}

/// Adds one more pixel to `row` from the run-length stream `r`, reading the next packet header
/// when neither a repeat nor a literal run is pending.
fn rle_step<R: Read>(
    r: &mut R,
    row: &mut Vec<u8>,
    px: &mut [u8; 4],
    pb: usize,
    repeat: &mut usize,
    literal: &mut usize,
) -> Option<()> {
    if *repeat == 0 && *literal == 0 {
        let mut packet = [0u8; 1];
        r.read_exact(&mut packet).ok()?;
        let n = usize::from(packet[0] & 0x7F) + 1;
        if packet[0] & 0x80 != 0 {
            r.read_exact(&mut px[..pb]).ok()?;
            *repeat = n;
        } else {
            *literal = n;
        }
    }
    if *repeat > 0 {
        row.extend_from_slice(&px[..pb]);
        *repeat -= 1;
    } else {
        r.read_exact(&mut px[..pb]).ok()?;
        row.extend_from_slice(&px[..pb]);
        *literal -= 1;
    }
    Some(())
}

/// The picture of one of these files, at most `target_edge` on its long side, read from `r`
/// without buffering the file. `None` for a file this does not read.
pub(crate) fn decode_scaled<R: Read + Seek>(mut r: R, target_edge: u32) -> Option<DynamicImage> {
    let l = read_checked_layout(&mut r)?;
    let grid = Grid::new(&l, target_edge);
    let (tw, th) = (
        usize::try_from(grid.tw).ok()?,
        usize::try_from(grid.th).ok()?,
    );
    let mut out = Vec::new();
    out.try_reserve_exact(tw.checked_mul(th)?.checked_mul(4)?)
        .ok()?;
    out.resize(tw * th * 4, 0);
    if l.rle {
        r.seek(SeekFrom::Start(l.data)).ok()?;
        read_rle(&mut r, &l, &grid, &mut out)?;
    } else {
        read_raw(&mut r, &l, &grid, &mut out)?;
    }
    let img = RgbaImage::from_raw(tw as u32, th as u32, out)?;
    Some(DynamicImage::ImageRgba8(img))
}

/// Reads the header from the start of `r` and returns the layout, rejecting a raw layout whose
/// rows are not all in the file.
fn read_checked_layout<R: Read + Seek>(r: &mut R) -> Option<Layout> {
    r.seek(SeekFrom::Start(0)).ok()?;
    let mut head = Vec::with_capacity(MAX_HEADER);
    r.by_ref()
        .take(MAX_HEADER as u64)
        .read_to_end(&mut head)
        .ok()?;
    let l = layout(&head)?;
    // A raw layout's rows must all be in the file: TGA has no signature, and a header that
    // merely looks like one must not be drawn as noise.
    let len = r.seek(SeekFrom::End(0)).ok()?;
    let need = l.data.checked_add(l.row_bytes().checked_mul(l.height)?)?;
    if !l.rle && need > len {
        return None;
    }
    Some(l)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A 7x5 test picture: red follows the column, green the row, blue flat, alpha a ramp.
    fn px(x: u32, y: u32) -> [u8; 4] {
        [(x * 30) as u8, (y * 50) as u8, 90, (200 + x) as u8]
    }

    fn picture(channels: usize) -> image::DynamicImage {
        let rgba = RgbaImage::from_fn(7, 5, |x, y| image::Rgba(px(x, y)));
        let img = DynamicImage::ImageRgba8(rgba);
        match channels {
            1 => DynamicImage::ImageLuma8(img.to_luma8()),
            3 => DynamicImage::ImageRgb8(img.to_rgb8()),
            _ => img,
        }
    }

    fn ours(bytes: &[u8]) -> RgbaImage {
        decode_scaled(Cursor::new(bytes), 4096)
            .expect("reads")
            .to_rgba8()
    }

    /// Each format written by the `image` crate reads back here to the same pixels it does.
    #[test]
    fn what_the_image_crate_writes_reads_back_the_same() {
        use image::ImageFormat as F;
        for (fmt, channels) in [
            (F::Pnm, 1),
            (F::Pnm, 3),
            (F::Farbfeld, 4),
            (F::Tga, 3),
            (F::Tga, 4),
        ] {
            let mut bytes = Vec::new();
            let img = picture(channels);
            let img = if fmt == F::Farbfeld {
                DynamicImage::ImageRgba16(img.to_rgba16())
            } else {
                img
            };
            img.write_to(&mut Cursor::new(&mut bytes), fmt)
                .expect("write");
            let theirs = image::load_from_memory_with_format(&bytes, fmt)
                .expect("image reads it")
                .to_rgba8();
            assert_eq!(ours(&bytes), theirs, "{fmt:?} x{channels}");
        }
    }

    /// Bottom-up rows (TGA's default, PFM, BMP-style) come out the right way up.
    #[test]
    fn a_bottom_up_tga_and_a_pfm_are_the_right_way_up() {
        // A 2x2 TGA, type 2, 24 bits, descriptor 0: rows stored bottom first.
        let mut tga = vec![0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 2, 0, 24, 0];
        tga.extend_from_slice(&[0, 0, 255, 0, 0, 255]); // bottom row: red, red (BGR)
        tga.extend_from_slice(&[255, 0, 0, 255, 0, 0]); // top row: blue, blue
        let img = ours(&tga);
        assert_eq!(img.get_pixel(0, 0).0, [0, 0, 255, 255], "top is blue");
        assert_eq!(img.get_pixel(1, 1).0, [255, 0, 0, 255], "bottom is red");
        // A 1x2 grey PFM, little-endian, bottom row first: 1.0 then 0.25.
        let mut pfm = b"Pf\n1 2\n-1.0\n".to_vec();
        pfm.extend_from_slice(&1.0f32.to_le_bytes());
        pfm.extend_from_slice(&0.25f32.to_le_bytes());
        let img = ours(&pfm);
        assert_eq!(img.get_pixel(0, 0).0[0], 64, "top is the 0.25 row");
        assert_eq!(img.get_pixel(0, 1).0[0], 255, "bottom is the 1.0 row");
    }

    /// Run-length TGA: a repeat packet and a literal packet across rows.
    #[test]
    fn a_run_length_tga_decodes() {
        // 3x2, type 11 (RLE grey), top-down (descriptor bit 5).
        let mut tga = vec![0, 0, 11, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, 0, 2, 0, 8, 0x20];
        tga.extend_from_slice(&[0x83, 50]); // 4 x 50: the first row and one of the second
        tga.extend_from_slice(&[0x01, 100, 150]); // 2 literals
        let img = ours(&tga);
        assert_eq!(img.get_pixel(2, 0).0[0], 50);
        assert_eq!(img.get_pixel(0, 1).0[0], 50);
        assert_eq!(img.get_pixel(1, 1).0[0], 100);
        assert_eq!(img.get_pixel(2, 1).0[0], 150);
    }

    /// Past the decoders' 16384 side limit, which the `image` tier refuses, the picture is
    /// still read, shrunk from the rows it samples.
    #[test]
    fn a_picture_past_the_side_limit_is_read_and_shrunk() {
        let (w, h) = (20000u32, 4u32);
        let mut pgm = format!("P5\n{w} {h}\n255\n").into_bytes();
        for _y in 0..h {
            pgm.extend((0..w).map(|x| if x < w / 2 { 0 } else { 255 }));
        }
        let img = decode_scaled(Cursor::new(&pgm), 256)
            .expect("reads")
            .to_rgba8();
        // Whole-pixel cells of 78 source columns (the floor of 20000 / 256), so 257 across:
        // never fewer than asked for.
        assert_eq!(img.width(), 257);
        assert_eq!(img.get_pixel(10, 0).0[0], 0);
        assert_eq!(img.get_pixel(250, 0).0[0], 255);
    }

    /// A header that promises more rows than the file holds is refused, and no header that is
    /// merely TGA-shaped is drawn as noise; truncation never panics.
    #[test]
    fn a_short_or_lying_file_is_refused() {
        let mut ppm = b"P6\n4 4\n255\n".to_vec();
        ppm.extend_from_slice(&[7u8; 4 * 3 * 3]);
        assert!(
            decode_scaled(Cursor::new(&ppm), 64).is_none(),
            "a row short"
        );
        assert!(!is_raw_raster(
            b"not a picture at all, not even a tga header"
        ));
        let mut full = b"P6\n4 4\n255\n".to_vec();
        full.extend_from_slice(&[7u8; 48]);
        for cut in 0..full.len() {
            let _ = decode_scaled(Cursor::new(&full[..cut]), 64);
        }
    }
}
