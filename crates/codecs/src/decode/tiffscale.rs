//! A TIFF read a strip or tile at a time and shrunk as it is read: for the TIFF past the input
//! ceiling that Windows' own codec cannot open, which is BigTIFF - the variant that exists for
//! exactly the files too big for a classic TIFF's 32-bit offsets (the big-file gate's pixel
//! axis, 2026-09-23: a 313 MB BigTIFF scan got nothing on every surface).
//!
//! Only the chunks holding a row the smaller picture takes are read, each once; each row's
//! columns are box-averaged as it goes by. Chunky 8- and 16-bit grey, grey+alpha, RGB and RGBA,
//! strips or tiles, any compression the `tiff` crate reads. Anything else is `None`. An
//! embedded colour profile is applied, as the buffered tiers apply it.

use std::io::{Read, Seek};

use image::{DynamicImage, RgbaImage};
use tiff::decoder::{ChunkType, Decoder, DecodingResult};
use tiff::tags::Tag;
use tiff::ColorType;

/// The longest side the result is given: 256 MiB of RGBA.
const MAX_EDGE: u32 = 8192;

/// The longest side a TIFF may declare, as the other row-at-a-time readers bound theirs
/// (`rawraster`, `fits`). The header is file data: a 2,000,000,000-pixel ImageWidth sized the
/// row buffer at 8 GiB before a single chunk was read, and a failed allocation aborts the
/// shell under `panic = "abort"` (Dredd, 2026-09-23).
const MAX_SIDE: u32 = 1 << 20;

/// The most one band of chunks may hold at once. The cache keeps every chunk across a row
/// band, so a wide image in tall tiles would otherwise hold width x tile height x channels.
const MAX_BAND_BYTES: u64 = 256 << 20;

/// One chunk's samples as display bytes, and its size in pixels.
struct Chunk {
    index: u32,
    samples: Vec<u8>,
    width: usize,
}

/// A decoded chunk as one byte a sample (16-bit keeps its high byte).
fn to_bytes(r: DecodingResult) -> Option<Vec<u8>> {
    match r {
        DecodingResult::U8(v) => Some(v),
        DecodingResult::U16(v) => Some(v.into_iter().map(|s| (s >> 8) as u8).collect()),
        _ => None,
    }
}

/// Which source rows and columns make the smaller picture: a cell is `step` pixels square,
/// its columns averaged along the row through the middle of the band.
struct Grid {
    step: u32,
    tw: u32,
    th: u32,
    height: u32,
}

impl Grid {
    fn new(width: u32, height: u32, edge: u32) -> Self {
        let edge = edge.clamp(1, MAX_EDGE);
        // FLOOR, as `exrscale` and the XCF walk do: the caller resizes the grid to its target
        // with a real filter, so the grid must never come out SMALLER than asked for (ceil
        // handed a 300 px canvas asked for 256 a 150 px grid: the big-file gate's `.pdd`, a
        // blurrier tile than the same file under the input ceiling). The ceiling on the edge
        // still bounds the memory.
        let long = width.max(height);
        let step = (long / edge).max(long.div_ceil(MAX_EDGE)).max(1);
        Self {
            step,
            tw: width.div_ceil(step),
            th: height.div_ceil(step),
            height,
        }
    }

    fn row(&self, ty: u32) -> u32 {
        (ty * self.step + self.step / 2).min(self.height - 1)
    }
}

/// The picture of a TIFF (classic or BigTIFF), at most `target_edge` on its long side, read
/// from `r` without buffering the file. `None` for a layout this does not read.
pub(crate) fn decode_scaled<R: Read + Seek>(r: R, target_edge: u32) -> Option<DynamicImage> {
    let mut d = Decoder::new(r).ok()?;
    let (w, h) = d.dimensions().ok()?;
    if !(1..=MAX_SIDE).contains(&w) || !(1..=MAX_SIDE).contains(&h) {
        return None;
    }
    let plan = read_plan(&mut d, w, h, target_edge)?;
    let (tw, th) = (plan.grid.tw as usize, plan.grid.th as usize);
    let mut out = Vec::new();
    out.try_reserve_exact(tw.checked_mul(th)?.checked_mul(4)?)
        .ok()?;
    out.resize(tw * th * 4, 255);
    let mut cache: Vec<Chunk> = Vec::new();
    let mut row = vec![0u8; (w as usize).checked_mul(plan.channels)?];
    for ty in 0..plan.grid.th {
        read_row(&mut d, &mut cache, &plan, ty, &mut row)?;
        shrink_into(&row, (plan.channels, plan.invert), &plan.grid, ty, &mut out);
    }
    let img = RgbaImage::from_raw(plan.grid.tw, plan.grid.th, out)?;
    Some(super::color::apply_icc_to_srgb(
        DynamicImage::ImageRgba8(img),
        plan.icc,
    ))
}

/// A TIFF's read layout: its sample layout, chunk size, target grid and profile to apply.
struct Plan {
    grid: Grid,
    channels: usize,
    invert: bool,
    icc: Option<Vec<u8>>,
    cw: u32,
    ch: u32,
    across: u32,
}

/// Read a decoder's layout: sample channels, inversion, profile, chunk size and grid.
fn read_plan<R: Read + Seek>(d: &mut Decoder<R>, w: u32, h: u32, target_edge: u32) -> Option<Plan> {
    let channels = match d.colortype().ok()? {
        ColorType::Gray(8 | 16) => 1,
        ColorType::GrayA(8 | 16) => 2,
        ColorType::RGB(8 | 16) => 3,
        ColorType::RGBA(8 | 16) => 4,
        _ => return None,
    };
    let planar = d
        .find_tag(Tag::PlanarConfiguration)
        .ok()
        .flatten()
        .and_then(|v| v.into_u16().ok());
    if planar.is_some_and(|p| p != 1) {
        return None;
    }
    // WhiteIsZero greyscale stores ink, not light.
    let invert = channels <= 2
        && d.find_tag(Tag::PhotometricInterpretation)
            .ok()
            .flatten()
            .and_then(|v| v.into_u16().ok())
            == Some(0);
    // Read before the chunks, which borrow the decoder from here on. Profiles past 4 MiB are
    // ignored, as `tiff_icc` ignores them on the buffered path.
    let icc = d
        .get_tag_u8_vec(Tag::IccProfile)
        .ok()
        .filter(|p| p.len() <= 4 << 20);
    let (cw, ch) = d.chunk_dimensions();
    if cw == 0 || ch == 0 {
        return None;
    }
    if u64::from(w) * u64::from(ch) * channels as u64 > MAX_BAND_BYTES {
        return None;
    }
    let across = match d.get_chunk_type() {
        ChunkType::Strip => 1,
        ChunkType::Tile => w.div_ceil(cw),
    };
    Some(Plan {
        grid: Grid::new(w, h, target_edge),
        channels,
        invert,
        icc,
        cw,
        ch,
        across,
    })
}

/// Read the band of chunks holding output row `ty` into `row`, one chunk per column.
fn read_row<R: Read + Seek>(
    d: &mut Decoder<R>,
    cache: &mut Vec<Chunk>,
    plan: &Plan,
    ty: u32,
    row: &mut [u8],
) -> Option<()> {
    let y = plan.grid.row(ty);
    let band = y / plan.ch;
    for tx in 0..plan.across {
        let index = band * plan.across + tx;
        if !cache.iter().any(|c| c.index == index) {
            let (dw, _) = d.chunk_data_dimensions(index);
            let samples = to_bytes(d.read_chunk(index).ok()?)?;
            cache.retain(|c| c.index / plan.across == band);
            cache.push(Chunk {
                index,
                samples,
                width: dw as usize,
            });
        }
        let chunk = cache.iter().find(|c| c.index == index)?;
        let line = chunk.width * plan.channels;
        let at = (y % plan.ch) as usize * line;
        let src = chunk.samples.get(at..at + line)?;
        let x0 = (tx * plan.cw) as usize * plan.channels;
        row.get_mut(x0..x0 + line)?.copy_from_slice(src);
    }
    Some(())
}

/// Box-average one row of samples into output row `ty` as RGBA.
fn shrink_into(
    row: &[u8],
    (channels, invert): (usize, bool),
    grid: &Grid,
    ty: u32,
    out: &mut [u8],
) {
    let tw = grid.tw as usize;
    let line = &mut out[ty as usize * tw * 4..(ty as usize + 1) * tw * 4];
    for (cell, px) in line
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(row.chunks(grid.step as usize * channels))
    {
        let mut sum = [0u32; 4];
        let mut n = 0u32;
        for p in px.chunks_exact(channels) {
            let rgba = match channels {
                1 => [p[0], p[0], p[0], 255],
                2 => [p[0], p[0], p[0], p[1]],
                3 => [p[0], p[1], p[2], 255],
                _ => [p[0], p[1], p[2], p[3]],
            };
            sum.iter_mut()
                .zip(rgba)
                .for_each(|(s, v)| *s += u32::from(v));
            n += 1;
        }
        let n = n.max(1);
        cell.iter_mut()
            .zip(sum)
            .for_each(|(c, s)| *c = (s / n) as u8);
        if invert {
            cell[..3].iter_mut().for_each(|c| *c = 255 - *c);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn tiff_of(img: &DynamicImage) -> Vec<u8> {
        let mut bytes = Vec::new();
        img.write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Tiff)
            .expect("write");
        bytes
    }

    /// Whatever the `image` crate writes (RGB, RGBA, grey, 16-bit) reads back to its pixels.
    #[test]
    fn a_tiff_reads_back_to_what_was_written() {
        let rgba = RgbaImage::from_fn(9, 7, |x, y| {
            image::Rgba([x as u8 * 25, y as u8 * 30, 70, 200])
        });
        let base = DynamicImage::ImageRgba8(rgba);
        for img in [
            DynamicImage::ImageRgb8(base.to_rgb8()),
            base.clone(),
            DynamicImage::ImageLuma8(base.to_luma8()),
            DynamicImage::ImageRgb16(base.to_rgb16()),
        ] {
            let ours = decode_scaled(Cursor::new(tiff_of(&img)), 4096).expect("reads");
            assert_eq!(ours.to_rgba8(), img.to_rgba8(), "{:?}", img.color());
        }
    }

    /// Past the decoders' side limit, shrunk from the rows it samples.
    #[test]
    fn a_wide_tiff_is_shrunk() {
        let img = DynamicImage::ImageLuma8(image::GrayImage::from_fn(20000, 3, |x, _| {
            image::Luma([if x < 10000 { 0 } else { 255 }])
        }));
        let ours = decode_scaled(Cursor::new(tiff_of(&img)), 256)
            .expect("reads")
            .to_rgba8();
        assert!((256..512).contains(&ours.width()), "{}", ours.width());
        assert_eq!(ours.get_pixel(5, 0).0[0], 0);
        assert_eq!(ours.get_pixel(ours.width() - 5, 0).0[0], 255);
    }

    #[test]
    fn not_a_tiff_is_refused_without_panicking() {
        assert!(decode_scaled(Cursor::new(b"not a tiff at all".to_vec()), 64).is_none());
        let bytes = tiff_of(&DynamicImage::ImageRgb8(image::RgbImage::new(8, 8)));
        for cut in 0..bytes.len() {
            let _ = decode_scaled(Cursor::new(&bytes[..cut]), 64);
        }
    }

    /// A TIFF declaring an absurd width is refused before anything is sized by it: the row
    /// buffer used to be `width x channels` bytes straight from the header, 8 GiB here, and a
    /// failed allocation aborts the shell (Dredd, 2026-09-23). The `tiff` crate itself accepts
    /// the header, so the refusal is this reader's own side cap.
    #[test]
    fn a_tiff_declaring_an_absurd_width_is_refused_before_allocating() {
        let img = DynamicImage::ImageRgba8(RgbaImage::from_pixel(4, 1, image::Rgba([1, 2, 3, 4])));
        let mut bytes = tiff_of(&img);
        let ifd = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        let count = u16::from_le_bytes(bytes[ifd..ifd + 2].try_into().unwrap()) as usize;
        let entry = (0..count)
            .map(|i| ifd + 2 + i * 12)
            .find(|&e| u16::from_le_bytes(bytes[e..e + 2].try_into().unwrap()) == 256)
            .expect("an ImageWidth entry");
        bytes[entry + 2..entry + 4].copy_from_slice(&4u16.to_le_bytes()); // LONG
        bytes[entry + 4..entry + 8].copy_from_slice(&1u32.to_le_bytes());
        bytes[entry + 8..entry + 12].copy_from_slice(&0x7FFF_FFFFu32.to_le_bytes());
        let mut d = Decoder::new(Cursor::new(&bytes)).expect("the tiff crate opens it");
        assert_eq!(d.dimensions().expect("dimensions").0, 0x7FFF_FFFF);
        assert!(decode_scaled(Cursor::new(&bytes), 256).is_none());
    }
}
