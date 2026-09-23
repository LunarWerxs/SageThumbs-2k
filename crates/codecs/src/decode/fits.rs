//! FITS (Flexible Image Transport System), read natively: the file's first image, at the size
//! asked for, reading only the rows it draws.
//!
//! ImageMagick was the only reader until 2026-09-23, and the big-file gate found it failing
//! three ways. A file whose image sits in an extension after an empty primary header - the
//! usual shape of telescope data - drew nothing; a file past the 256 MiB input ceiling drew
//! nothing; and a 16-bit image came out nearly black, because the stored values are mapped
//! straight onto a 16-bit range (the corpus's M13 exposure, 109..3618 of 65535, filled 0..14 of
//! 255).
//!
//! Here the first header-data unit holding a 2-D (or deeper) image is read wherever it is in the
//! file, and only the rows the smaller picture takes. FITS stores rows bottom-up, so they are
//! flipped; three planes are RGB, otherwise the first plane is grey. An 8-bit image with no
//! scaling is shown as stored (as ImageMagick shows it); anything else is scaled between the
//! 0.5th and 99.5th percentiles of its values and drawn through an asinh curve, the automatic
//! display astronomy viewers use: a linear scale leaves a star field a few dots on black at
//! tile size, this keeps the sky dark and the faint stars visible without burning out a
//! cluster's core. BLANK and NaN pixels are black and take no part in the scale.

use std::io::{Read, Seek, SeekFrom};

use image::{DynamicImage, RgbaImage};

/// Every header and data block is this long.
const BLOCK: u64 = 2880;
/// Header cards a header-data unit may run to before its END is taken as missing.
const MAX_CARDS: usize = 36 * 1000;
/// Header-data units walked in search of an image.
const MAX_HDUS: usize = 256;
/// The longest side accepted on either axis.
const MAX_SIDE: u64 = 1 << 20;
/// The longest side the result is given: 256 MiB of RGBA.
const MAX_EDGE: u32 = 8192;
/// The side of the grid the percentiles are measured on.
const STATS_EDGE: u32 = 512;
/// The asinh curve's softening: the level, as a share of the scale, below which it is nearly
/// linear.
const SOFTENING: f64 = 0.1;

/// Does `head` open with the primary header's `SIMPLE = T` card?
pub(crate) fn is_fits(head: &[u8]) -> bool {
    head.len() >= 30 && head.starts_with(b"SIMPLE  =") && value(&head[..80.min(head.len())]) == "T"
}

/// One header-data unit that holds an image.
#[derive(Clone, Debug, Default)]
struct Image {
    bitpix: i32,
    width: u64,
    height: u64,
    planes: u64,
    bzero: f64,
    bscale: f64,
    blank: Option<i64>,
    data: u64,
}

impl Image {
    fn bytes_per_sample(&self) -> u64 {
        u64::from(self.bitpix.unsigned_abs() / 8)
    }

    /// Shown as stored: 8 bits and no scaling.
    fn direct(&self) -> bool {
        self.bitpix == 8 && self.bzero == 0.0 && self.bscale == 1.0
    }
}

/// The value field of a card: columns 11 to 80, the comment after a `/` cut off, quotes and
/// padding stripped.
fn value(card: &[u8]) -> String {
    let field = String::from_utf8_lossy(card.get(10..).unwrap_or_default()).into_owned();
    let field = field.trim();
    if let Some(quoted) = field.strip_prefix('\'') {
        return quoted.split('\'').next().unwrap_or("").trim().to_string();
    }
    field.split('/').next().unwrap_or("").trim().to_string()
}

/// A header's keywords, read card by card from `at` to its END card: `(keyword, value)` for
/// the cards that assign one, and the offset of the data after the header's last block.
fn read_header<R: Read + Seek>(r: &mut R, at: u64) -> Option<(Vec<(String, String)>, u64)> {
    r.seek(SeekFrom::Start(at)).ok()?;
    let mut cards = Vec::new();
    let mut block = [0u8; BLOCK as usize];
    let mut blocks = 0u64;
    while cards.len() < MAX_CARDS {
        r.read_exact(&mut block).ok()?;
        blocks += 1;
        if read_block(&block, &mut cards) {
            return Some((cards, at.checked_add(blocks * BLOCK)?));
        }
    }
    None
}

/// Scan one block's cards into `cards`; true once the END card is reached.
fn read_block(block: &[u8], cards: &mut Vec<(String, String)>) -> bool {
    for card in block.chunks(80) {
        let key = String::from_utf8_lossy(&card[..8]).trim_end().to_string();
        if key == "END" {
            return true;
        }
        if &card[8..10] == b"= " {
            cards.push((key, value(card)));
        }
    }
    false
}

fn find<'a>(cards: &'a [(String, String)], key: &str) -> Option<&'a str> {
    cards
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn int(cards: &[(String, String)], key: &str) -> Option<i64> {
    find(cards, key)?.parse().ok()
}

fn float(cards: &[(String, String)], key: &str, default: f64) -> f64 {
    find(cards, key)
        .and_then(|v| v.replace(['D', 'd'], "E").parse().ok())
        .filter(|v: &f64| v.is_finite())
        .unwrap_or(default)
}

/// The data length of a header-data unit, and the image in it when it is one: the primary
/// array or an `IMAGE` extension, at least two axes, a sample size FITS defines.
fn unit(cards: &[(String, String)], data: u64) -> Option<(u64, Option<Image>)> {
    let bitpix = i32::try_from(int(cards, "BITPIX")?).ok()?;
    if !matches!(bitpix, 8 | 16 | 32 | 64 | -32 | -64) {
        return None;
    }
    let naxis = usize::try_from(int(cards, "NAXIS")?)
        .ok()
        .filter(|&n| n <= 999)?;
    let dims: Vec<u64> = (1..=naxis)
        .map(|i| int(cards, &format!("NAXIS{i}")).and_then(|v| u64::try_from(v).ok()))
        .collect::<Option<_>>()?;
    let pcount = int(cards, "PCOUNT").unwrap_or(0).max(0) as u64;
    let gcount = int(cards, "GCOUNT").unwrap_or(1).max(1) as u64;
    let bytes = data_bytes(bitpix, naxis, &dims, pcount, gcount)?;
    let is_image = find(cards, "SIMPLE").is_some() || find(cards, "XTENSION") == Some("IMAGE");
    let shaped = naxis >= 2 && dims[..2].iter().all(|&d| (1..=MAX_SIDE).contains(&d));
    let image = (is_image && shaped).then(|| Image {
        bitpix,
        width: dims[0],
        height: dims[1],
        planes: dims.get(2).copied().unwrap_or(1).max(1),
        bzero: float(cards, "BZERO", 0.0),
        bscale: float(cards, "BSCALE", 1.0),
        blank: int(cards, "BLANK"),
        data,
    });
    Some((bytes, image))
}

/// The header-data unit's data length in bytes: its samples times the sample size, checked.
fn data_bytes(bitpix: i32, naxis: usize, dims: &[u64], pcount: u64, gcount: u64) -> Option<u64> {
    let elements = if naxis == 0 {
        0
    } else {
        dims.iter().try_fold(1u64, |a, &d| a.checked_mul(d))?
    };
    let bytes = u64::from(bitpix.unsigned_abs() / 8)
        .checked_mul(gcount)?
        .checked_mul(pcount.checked_add(elements)?)?;
    Some(bytes)
}

/// The first image in the file, walking the header-data units from the start.
fn first_image<R: Read + Seek>(r: &mut R) -> Option<Image> {
    let mut at = 0u64;
    for _ in 0..MAX_HDUS {
        let (cards, data) = read_header(r, at)?;
        let (bytes, image) = unit(&cards, data)?;
        if image.is_some() {
            return image;
        }
        at = data.checked_add(bytes.div_ceil(BLOCK).checked_mul(BLOCK)?)?;
    }
    None
}

/// One stored sample as its physical value, or NaN for BLANK and NaN.
fn sample(img: &Image, s: &[u8]) -> f64 {
    let stored = match (img.bitpix, s.len()) {
        (8, 1) => f64::from(s[0]),
        (16, 2) => f64::from(i16::from_be_bytes([s[0], s[1]])),
        (32, 4) => f64::from(i32::from_be_bytes([s[0], s[1], s[2], s[3]])),
        (64, 8) => i64::from_be_bytes(s.try_into().unwrap_or_default()) as f64,
        (-32, 4) => f64::from(f32::from_be_bytes([s[0], s[1], s[2], s[3]])),
        (-64, 8) => f64::from_be_bytes(s.try_into().unwrap_or_default()),
        _ => f64::NAN,
    };
    let blank = img.bitpix > 0 && img.blank.is_some_and(|b| b as f64 == stored);
    if blank {
        return f64::NAN;
    }
    img.bzero + img.bscale * stored
}

/// Which source rows and columns make a smaller picture, as in the Photoshop reader: a cell is
/// `step` pixels square, its columns averaged along the row through the middle of the band.
#[derive(Clone, Copy, Debug)]
struct Grid {
    step: u64,
    tw: u64,
    th: u64,
    height: u64,
}

impl Grid {
    fn new(img: &Image, edge: u32) -> Self {
        let edge = u64::from(edge.clamp(1, MAX_EDGE));
        // FLOOR, as `exrscale` and the XCF walk do: the caller resizes the grid to its target
        // with a real filter, so the grid must never come out SMALLER than asked for (ceil
        // handed a 300 px canvas asked for 256 a 150 px grid: the big-file gate's `.pdd`, a
        // blurrier tile than the same file under the input ceiling). The ceiling on the edge
        // still bounds the memory.
        let long = img.width.max(img.height);
        let step = (long / edge).max(long.div_ceil(u64::from(MAX_EDGE))).max(1);
        Self {
            step,
            tw: img.width.div_ceil(step),
            th: img.height.div_ceil(step),
            height: img.height,
        }
    }

    fn row(&self, ty: u64) -> u64 {
        (ty * self.step + self.step / 2).min(self.height - 1)
    }
}

/// The cells of one sampled row of `plane`: each cell's mean over its valid pixels, NaN when
/// it has none.
fn cells<R: Read + Seek>(
    r: &mut R,
    img: &Image,
    grid: &Grid,
    (plane, ty): (u64, u64),
) -> Option<Vec<f64>> {
    let bps = img.bytes_per_sample();
    let row_bytes = img.width.checked_mul(bps)?;
    let index = plane.checked_mul(img.height)?.checked_add(grid.row(ty))?;
    r.seek(SeekFrom::Start(
        img.data.checked_add(index.checked_mul(row_bytes)?)?,
    ))
    .ok()?;
    let mut row = vec![0u8; usize::try_from(row_bytes).ok()?];
    r.read_exact(&mut row).ok()?;
    let per_cell = usize::try_from(grid.step.checked_mul(bps)?).ok()?;
    Some(
        row.chunks(per_cell)
            .map(|cell| {
                let (sum, n) = cell
                    .chunks_exact(bps as usize)
                    .map(|s| sample(img, s))
                    .filter(|v| v.is_finite())
                    .fold((0.0, 0u32), |(s, n), v| (s + v, n + 1));
                if n == 0 {
                    f64::NAN
                } else {
                    sum / f64::from(n)
                }
            })
            .collect(),
    )
}

/// The planes drawn: three as red, green and blue, otherwise the first as grey.
fn planes(img: &Image) -> u64 {
    if img.planes == 3 {
        3
    } else {
        1
    }
}

/// The value range mapped onto 0..=255: the 0.5th to 99.5th percentile of the values on a
/// coarse grid, the whole range when that is flat, `None` when there is no valid pixel at all.
fn scale<R: Read + Seek>(r: &mut R, img: &Image) -> Option<(f64, f64)> {
    let grid = Grid::new(img, STATS_EDGE);
    let mut values = Vec::new();
    for plane in 0..planes(img) {
        for ty in 0..grid.th {
            values.extend(
                cells(r, img, &grid, (plane, ty))?
                    .into_iter()
                    .filter(|v| v.is_finite()),
            );
        }
    }
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let at = |p: f64| values[((values.len() - 1) as f64 * p).round() as usize];
    let (lo, hi) = (at(0.005), at(0.995));
    if hi > lo {
        return Some((lo, hi));
    }
    Some((values[0], values[values.len() - 1]))
}

/// A value's place on the scale (0..=1 inside it) as a display level, through the asinh curve
/// when `curve`.
fn display(t: f64, curve: bool) -> f64 {
    let t = t.clamp(0.0, 1.0);
    if curve {
        (t / SOFTENING).asinh() / (1.0 / SOFTENING).asinh()
    } else {
        t
    }
}

/// The first image of a FITS file, at most `target_edge` on its long side, read from `r`
/// without buffering the file. `None` for a file with no image this reads.
pub(crate) fn decode_scaled<R: Read + Seek>(mut r: R, target_edge: u32) -> Option<DynamicImage> {
    let img = first_image(&mut r)?;
    let range = if img.direct() {
        (0.0, 255.0)
    } else {
        scale(&mut r, &img)?
    };
    let grid = Grid::new(&img, target_edge);
    let (tw, th) = (
        usize::try_from(grid.tw).ok()?,
        usize::try_from(grid.th).ok()?,
    );
    let mut rgba = Vec::new();
    rgba.try_reserve_exact(tw.checked_mul(th)?.checked_mul(4)?)
        .ok()?;
    rgba.resize(tw * th * 4, 255);
    let levels = Levels {
        lo: range.0,
        span: (range.1 - range.0).max(f64::MIN_POSITIVE),
        curve: !img.direct(),
    };
    for plane in 0..planes(&img) {
        paint_plane(&mut r, &img, &grid, plane, levels, &mut rgba)?;
    }
    let image = RgbaImage::from_raw(tw as u32, th as u32, rgba)?;
    Some(DynamicImage::ImageRgba8(image))
}

/// How sample values become display levels: where the shown range starts, how wide it is,
/// and whether through the asinh curve.
#[derive(Clone, Copy)]
struct Levels {
    lo: f64,
    span: f64,
    curve: bool,
}

impl Levels {
    /// The display level of value `v`; a blank (NaN) sample is black.
    fn level(&self, v: f64) -> u8 {
        if v.is_finite() {
            (display((v - self.lo) / self.span, self.curve) * 255.0).round() as u8
        } else {
            0
        }
    }
}

/// Paint one plane's sampled rows into `rgba`, flipping the rows FITS counts from the bottom.
fn paint_plane<R: Read + Seek>(
    r: &mut R,
    img: &Image,
    grid: &Grid,
    plane: u64,
    levels: Levels,
    rgba: &mut [u8],
) -> Option<()> {
    let (tw, th) = (grid.tw as usize, grid.th as usize);
    for ty in 0..grid.th {
        let row = cells(r, img, grid, (plane, ty))?;
        // FITS counts rows from the bottom.
        let out = (th - 1 - ty as usize) * tw * 4;
        for (x, v) in row.iter().enumerate() {
            let level = levels.level(*v);
            let px = &mut rgba[out + x * 4..out + x * 4 + 3];
            if planes(img) == 3 {
                px[plane as usize] = level;
            } else {
                px.fill(level);
            }
        }
    }
    Some(())
}

/// The fuzz seed: an empty primary header, then a 16-bit IMAGE extension with a zero point, a
/// scale and a BLANK value - every card the reader acts on, and the extension walk.
#[cfg(test)]
pub(crate) fn fuzz_seed() -> Vec<u8> {
    let block = |cards: &[String]| {
        let mut h: Vec<u8> = cards
            .iter()
            .map(String::as_str)
            .chain(["END"])
            .flat_map(|c| format!("{c:<80}").into_bytes())
            .collect();
        h.resize(h.len().div_ceil(2880) * 2880, b' ');
        h
    };
    let card = |k: &str, v: &str| format!("{k:<8}= {v:>20}");
    let mut f = block(&[card("SIMPLE", "T"), card("BITPIX", "8"), card("NAXIS", "0")]);
    f.extend(block(&[
        format!("{:<8}= {:<20}", "XTENSION", "'IMAGE   '"),
        card("BITPIX", "16"),
        card("NAXIS", "2"),
        card("NAXIS1", "12"),
        card("NAXIS2", "9"),
        card("PCOUNT", "0"),
        card("GCOUNT", "1"),
        card("BZERO", "32768"),
        card("BSCALE", "1.5"),
        card("BLANK", "-32768"),
    ]));
    let mut data: Vec<u8> = (0..12 * 9)
        .flat_map(|i: i16| (i * 97 - 5000).to_be_bytes())
        .collect();
    data.resize(2880, 0);
    f.extend(data);
    f
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A header block of `cards`, padded with blank cards and ended.
    fn header(cards: &[&str]) -> Vec<u8> {
        let mut h = Vec::new();
        for c in cards.iter().copied().chain(["END"]) {
            let mut card = c.as_bytes().to_vec();
            card.resize(80, b' ');
            h.extend_from_slice(&card);
        }
        h.resize(h.len().div_ceil(2880) * 2880, b' ');
        h
    }

    fn card(key: &str, value: &str) -> String {
        format!("{key:<8}= {value:>20}")
    }

    /// A primary image (`primary`), or an empty primary and the image in an IMAGE extension,
    /// of `w` x `h` 16-bit samples `px(x, y)` counted from the bottom row.
    fn fits16(w: u32, h: u32, primary: bool, px: impl Fn(u32, u32) -> i16) -> Vec<u8> {
        fits16_with(w, h, primary, &[], px)
    }

    /// [`fits16`] with `extra` cards in the image's header.
    fn fits16_with(
        w: u32,
        h: u32,
        primary: bool,
        extra: &[String],
        px: impl Fn(u32, u32) -> i16,
    ) -> Vec<u8> {
        let mut dims = vec![
            card("BITPIX", "16"),
            card("NAXIS", "2"),
            card("NAXIS1", &w.to_string()),
            card("NAXIS2", &h.to_string()),
        ];
        dims.extend(extra.iter().cloned());
        let mut f = if primary {
            let mut c = vec![card("SIMPLE", "T")];
            c.extend(dims.iter().cloned());
            header(&c.iter().map(String::as_str).collect::<Vec<_>>())
        } else {
            let mut f = header(&[
                &card("SIMPLE", "T"),
                &card("BITPIX", "8"),
                &card("NAXIS", "0"),
            ]);
            let mut c = vec![format!("{:<8}= {:<20}", "XTENSION", "'IMAGE   '")];
            c.extend(dims.iter().cloned());
            c.push(card("PCOUNT", "0"));
            c.push(card("GCOUNT", "1"));
            f.extend(header(&c.iter().map(String::as_str).collect::<Vec<_>>()));
            f
        };
        let mut data = Vec::new();
        for y in 0..h {
            for x in 0..w {
                data.extend_from_slice(&px(x, y).to_be_bytes());
            }
        }
        data.resize(data.len().div_ceil(2880) * 2880, 0);
        f.extend(data);
        f
    }

    fn decode(f: &[u8], edge: u32) -> Option<RgbaImage> {
        decode_scaled(Cursor::new(f), edge).map(|i| i.to_rgba8())
    }

    #[test]
    fn the_signature_is_the_simple_card() {
        assert!(is_fits(&fits16(4, 4, true, |_, _| 0)));
        assert!(!is_fits(b"SIMPLE  =                    F"));
        assert!(!is_fits(b"not a fits file at all, not even close to one"));
    }

    #[test]
    fn rows_are_flipped_and_sixteen_bits_are_stretched() {
        // A vertical ramp, 0 at the bottom row: the top of the picture is the brightest.
        let f = fits16(8, 200, true, |_, y| (y * 10) as i16);
        let img = decode(&f, 4096).expect("fits");
        assert_eq!(img.dimensions(), (8, 200));
        assert!(img.get_pixel(3, 0).0[0] >= 250, "{:?}", img.get_pixel(3, 0));
        assert!(
            img.get_pixel(3, 199).0[0] <= 5,
            "{:?}",
            img.get_pixel(3, 199)
        );
    }

    #[test]
    fn an_image_in_an_extension_after_an_empty_primary_is_found() {
        let f = fits16(10, 6, false, |x, _| (x * 100) as i16);
        let img = decode(&f, 4096).expect("image extension");
        assert_eq!(img.dimensions(), (10, 6));
        assert!(img.get_pixel(0, 2).0[0] < img.get_pixel(9, 2).0[0]);
    }

    #[test]
    fn blank_pixels_are_black_and_take_no_part_in_the_scale() {
        let blank = [card("BLANK", "-32768")];
        let f = fits16_with(4, 4, true, &blank, |x, y| {
            if (x, y) == (0, 0) {
                -32768
            } else {
                500 + x as i16
            }
        });
        let img = decode(&f, 64).expect("fits");
        assert_eq!(
            img.get_pixel(0, 3).0,
            [0, 0, 0, 255],
            "the blank pixel, bottom left"
        );
        assert!(img.get_pixel(3, 1).0[0] >= 250);
    }

    #[test]
    fn a_big_image_is_shrunk_from_the_rows_it_samples() {
        let f = fits16(600, 300, true, |x, _| x as i16);
        let img = decode(&f, 100).expect("fits");
        assert_eq!(img.dimensions(), (100, 50));
    }

    #[test]
    fn a_cut_short_or_lying_file_is_refused_not_misread() {
        let f = fits16(16, 16, true, |x, y| (x * y) as i16);
        for cut in (0..f.len()).step_by(97) {
            let _ = decode(&f[..cut], 64);
        }
        assert!(
            decode(&f[..2880 + 100], 64).is_none(),
            "cut inside the data"
        );
        assert!(decode(&f[..2000], 64).is_none(), "cut inside the header");
    }

    /// The real files in the corpus decode, and the 16-bit M13 exposure is no longer the black
    /// square ImageMagick drew (mean level 0.4 of 255).
    #[test]
    fn real_files_are_not_black() {
        for name in ["real.fits", "real.fts", "sample.fits", "sample.fts"] {
            let Some(bytes) = st2k_base::testcorpus::read(name) else {
                eprintln!("NOT MEASURED: {name} absent");
                continue;
            };
            let img = decode(&bytes, 4096).unwrap_or_else(|| panic!("{name}"));
            let mean: f64 = img.pixels().map(|p| f64::from(p.0[0])).sum::<f64>()
                / f64::from(img.width() * img.height());
            assert!(mean > 20.0, "{name}: mean level {mean:.1}");
        }
    }
}
