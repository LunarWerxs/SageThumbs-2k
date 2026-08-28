//! Streaming, sub-sampled OpenEXR decode for thumbnails and previews.
//!
//! The `image` crate's OpenEXR tier is a full-resolution decode: it allocates one
//! `w * h * channels` f32 buffer inside the exr crate, copies it into a second
//! `DynamicImage` buffer of the same size, and needs the whole file in memory
//! first. A 12288x6480 render pass therefore wants ~445 MB of input plus ~2.5 GB
//! of float pixels to produce a 256 px tile — so it never even reaches the
//! decoder: the shared input ceiling ([`super::limits::MAX_INPUT_BYTES`]) and the
//! user's MaxSize both refuse the file, and Explorer shows the stock icon.
//!
//! This module decodes the same files the other way round: it reads from a
//! `Read + Seek` source (the shell `IStream` or a `File` — never a buffered
//! copy), asks the exr block reader for ONLY the chunks that contain a row the
//! downscale actually samples, and accumulates straight into the small target
//! grid. Peak memory is the target grid, not the source image, and the chunks
//! that carry no sampled row are never decompressed at all.
//!
//! Scope is deliberately identical to the `image` crate tier it front-runs —
//! non-deep, un-subsampled, RGB(A)-named channels, largest resolution level — so
//! anything exotic simply returns `Err` and the normal tiered decode runs
//! unchanged. Being a strict subset is what makes this safe to try first.

use std::io::{BufReader, Read, Seek};

use exr::block::reader::ChunksReader;
use exr::meta::header::Header;
use exr::prelude::*;
use image::DynamicImage;
use windows::core::{Error, Result};
use windows::Win32::Foundation::E_FAIL;

use super::limits::{MAX_DIM, MAX_PIXELS};

/// OpenEXR file signature.
const EXR_MAGIC: [u8; 4] = [0x76, 0x2f, 0x31, 0x01];

/// Hard ceiling on the requested edge, so the accumulator grid stays bounded no
/// matter what a caller asks for (at 2048 it is ~84 MB worst case). Matches the
/// largest edge any front end actually requests, [`super::EXR_PATH_EDGE`].
const MAX_TARGET_EDGE: u32 = 2048;

/// Hard ceiling on a layer's chunk count. This tier deliberately has no file-size
/// gate (that is the whole point), so it needs its own bound on the one structure
/// that scales with the file rather than with the output: `filter_chunks` reads the
/// WHOLE offset table (8 bytes per chunk) and walks every block before our row
/// filter can drop any of them. A degenerate 1x1-tile layer at [`MAX_DIM`] squared
/// declares ~268M chunks, i.e. a 2 GB table and 268M filter iterations.
///
/// 4 Mi chunks is far above anything real: the largest image we accept at all
/// (16384x16384) is 16 384 chunks as scan lines and 1 Mi chunks even at 16x16
/// tiles, and production EXRs use 32-256 px tiles. It bounds the table at 32 MB.
/// (`chunk_count` is COMPUTED from the data window and block description, never
/// trusted from the file, so this is a bound on geometry, not on a claimed number.)
const MAX_CHUNKS: usize = 1 << 22;

/// Does this head start with the OpenEXR magic? Used to route a stream/path into
/// [`decode_scaled`] before anything buffers it.
pub(crate) fn is_exr_magic(bytes: &[u8]) -> bool {
    bytes.starts_with(&EXR_MAGIC)
}

fn fail() -> Error {
    Error::from(E_FAIL)
}

/// Decode `src` to a linear-float image at most `target_edge` px on its long side,
/// point-sampling rows and box-averaging columns. The result is `Rgba32F` (still
/// linear HDR) — callers tone-map it exactly like the `image` tier's output.
///
/// Never materializes the full-resolution image and never buffers the file, so it
/// works on inputs far past every in-memory cap. Returns `Err` for anything
/// outside the supported subset (see the module docs), which is the caller's
/// signal to fall through to the ordinary tiers.
pub(crate) fn decode_scaled<R: Read + Seek>(src: R, target_edge: u32) -> Result<DynamicImage> {
    // The exr block reader is documented as assuming a buffered source; the shell
    // IStream in particular charges per marshaled read.
    let reader = exr::block::read(BufReader::new(src), false).map_err(|_| fail())?;

    // Same header choice as the `image` crate: the first non-deep part that has
    // R, G and B. Anything else (deep, luminance/chroma-only, subsampled) is left
    // to the existing tiers.
    let headers = reader.headers();
    let (layer, header) = resolve_rgb_layer(headers).ok_or_else(fail)?;

    // Channel slots. R/G/B are guaranteed by the search above; A is optional and
    // defaults to fully opaque, matching the `image` tier.
    let (ch_r, ch_g, ch_b, ch_a) = resolve_channels(header).ok_or_else(fail)?;
    let sample_types: Vec<SampleType> =
        header.channels.list.iter().map(|c| c.sample_type).collect();
    let bytes_per_pixel = header.channels.bytes_per_pixel;

    // The DISPLAY window is the image the viewer sees; the layer's own data window
    // may be offset from it (and may be smaller or larger).
    let display = header.shared_attributes.display_window;
    let (w, h) = validate_layer_dims(header).ok_or_else(fail)?;
    let offset = header.own_attributes.layer_position - display.position;
    let (off_x, off_y) = (offset.x() as i64, offset.y() as i64);

    let grid = Grid::new(w, h, target_edge);
    let (tw, th) = (grid.tw, grid.th);

    // One accumulator per target pixel. `count` is bumped on the RED channel only:
    // with subsampling rejected above, every channel of a kept row contributes the
    // same sample positions, so one counter describes all four sums.
    let cells = tw.checked_mul(th).ok_or_else(fail)?;
    let mut sums = vec![[0f32; 4]; cells];
    let mut counts = vec![0u32; cells];

    let grid_for_filter = grid;
    let filtered = reader
        .filter_chunks(false, move |_meta, _tile, block| {
            if block.layer != layer || block.level != Vec2(0, 0) {
                return false;
            }
            let y0 = block.pixel_position.y() as i64 + off_y;
            let y1 = y0.saturating_add(block.pixel_size.height() as i64);
            grid_for_filter.covers_sampled_row(y0, y1)
        })
        .map_err(|_| fail())?;

    filtered
        .decompress_sequential(false, |meta, block| {
            if block.index.layer != layer {
                return Ok(());
            }
            let Some(header) = meta.headers.get(layer) else {
                return Ok(());
            };
            // `UncompressedBlock::lines` slices `data` by computed byte ranges and
            // would panic on a block whose decompressed size disagrees with its
            // declared geometry. Validate first and skip a malformed block instead
            // (`panic = "abort"` makes any panic here fatal to the shell host).
            let expected = block
                .index
                .pixel_size
                .area()
                .saturating_mul(bytes_per_pixel);
            if block.data.len() != expected {
                return Ok(());
            }
            for line in block.lines(&header.channels) {
                let Some(slot) = channel_slot(line.location.channel, ch_r, ch_g, ch_b, ch_a) else {
                    continue;
                };
                let y = line.location.position.y() as i64 + off_y;
                let Some(ty) = grid.sampled_row(y) else {
                    continue;
                };
                let Some(&sample_type) = sample_types.get(line.location.channel) else {
                    continue;
                };
                let row = ty * tw;
                let x0 = line.location.position.x() as i64 + off_x;
                accumulate_line(
                    line.value,
                    sample_type,
                    x0,
                    &grid,
                    row,
                    slot,
                    &mut sums,
                    &mut counts,
                );
            }
            Ok(())
        })
        .map_err(|_| fail())?;

    Ok(image_from_accumulators(
        tw,
        th,
        ch_a.is_some(),
        &sums,
        &counts,
    ))
}

/// The non-deep part with R, G and B channels, un-subsampled: the same header choice
/// the `image` crate's OpenEXR tier makes. Anything else (deep, luminance/chroma-only,
/// subsampled) is left to the existing tiers.
fn resolve_rgb_layer(headers: &[Header]) -> Option<(usize, &Header)> {
    let layer = headers.iter().position(|h| {
        !h.deep
            && ["R", "G", "B"]
                .iter()
                .all(|c| h.channels.find_index_of_channel(&Text::from(*c)).is_some())
            && h.channels.list.iter().all(|c| c.sampling == Vec2(1, 1))
    })?;
    Some((layer, headers.get(layer)?))
}

/// R/G/B channel indices (guaranteed present by [`resolve_rgb_layer`]'s search) plus the
/// optional A channel, which defaults to fully opaque when absent, matching the `image`
/// tier.
fn resolve_channels(header: &Header) -> Option<(usize, usize, usize, Option<usize>)> {
    let idx_of = |name: &str| header.channels.find_index_of_channel(&Text::from(name));
    let (Some(ch_r), Some(ch_g), Some(ch_b)) = (idx_of("R"), idx_of("G"), idx_of("B")) else {
        return None;
    };
    Some((ch_r, ch_g, ch_b, idx_of("A")))
}

/// The DISPLAY window's `(width, height)`, bounds-checked against [`MAX_DIM`],
/// [`MAX_PIXELS`] and [`MAX_CHUNKS`]. The display window is the image the viewer sees;
/// the layer's own data window may be offset from it (and may be smaller or larger).
fn validate_layer_dims(header: &Header) -> Option<(usize, usize)> {
    let display = header.shared_attributes.display_window;
    let (w, h) = (display.size.width(), display.size.height());
    if w == 0 || h == 0 || w > MAX_DIM as usize || h > MAX_DIM as usize {
        return None;
    }
    if (w as u64).saturating_mul(h as u64) > MAX_PIXELS {
        return None;
    }
    if header.chunk_count > MAX_CHUNKS {
        return None;
    }
    Some((w, h))
}

/// Which accumulator slot (R=0, G=1, B=2, A=3) a decompressed line's channel index maps
/// to, or `None` for a channel this tier doesn't accumulate.
fn channel_slot(
    channel: usize,
    ch_r: usize,
    ch_g: usize,
    ch_b: usize,
    ch_a: Option<usize>,
) -> Option<usize> {
    if channel == ch_r {
        Some(0)
    } else if channel == ch_g {
        Some(1)
    } else if channel == ch_b {
        Some(2)
    } else if Some(channel) == ch_a {
        Some(3)
    } else {
        None
    }
}

/// Turn the per-cell sum/count accumulators into the final linear-float image: an
/// unsampled cell (outside the data window, or a chunk the file never stored) stays
/// zeroed, matching what the `image` tier leaves there. Alpha defaults to fully opaque
/// when the source had no A channel.
fn image_from_accumulators(
    tw: usize,
    th: usize,
    has_alpha: bool,
    sums: &[[f32; 4]],
    counts: &[u32],
) -> DynamicImage {
    let mut out = image::Rgba32FImage::new(tw as u32, th as u32);
    for (px, (sum, &n)) in out.pixels_mut().zip(sums.iter().zip(counts.iter())) {
        if n == 0 {
            continue;
        }
        let inv = 1.0 / n as f32;
        px.0 = [
            sum[0] * inv,
            sum[1] * inv,
            sum[2] * inv,
            if has_alpha { sum[3] * inv } else { 1.0 },
        ];
    }
    DynamicImage::ImageRgba32F(out)
}

/// Read one channel's row of samples and box-average it into the target row.
#[allow(clippy::too_many_arguments)]
fn accumulate_line(
    bytes: &[u8],
    sample_type: SampleType,
    x0: i64,
    grid: &Grid,
    row: usize,
    slot: usize,
    sums: &mut [[f32; 4]],
    counts: &mut [u32],
) {
    let mut add = |i: usize, value: f32| {
        let Some(tx) = grid.target_col(x0 + i as i64) else {
            return;
        };
        let Some(cell) = sums.get_mut(row + tx) else {
            return;
        };
        cell[slot] += if value.is_finite() { value } else { 0.0 };
        if slot == 0 {
            if let Some(n) = counts.get_mut(row + tx) {
                *n += 1;
            }
        }
    };
    match sample_type {
        SampleType::F16 => {
            for (i, b) in bytes.chunks_exact(2).enumerate() {
                add(i, f16::from_bits(u16::from_le_bytes([b[0], b[1]])).to_f32());
            }
        }
        SampleType::F32 => {
            for (i, b) in bytes.chunks_exact(4).enumerate() {
                add(i, f32::from_le_bytes([b[0], b[1], b[2], b[3]]));
            }
        }
        // UINT channels are ID/object passes rather than colour, but showing their
        // magnitude beats showing nothing.
        SampleType::U32 => {
            for (i, b) in bytes.chunks_exact(4).enumerate() {
                add(i, u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32);
            }
        }
    }
}

/// The source→target sampling grid: which source rows are kept, and which target
/// column a source column falls into. Rows are point-sampled (so whole chunks can
/// be skipped) while columns are box-averaged (they are free — the row is already
/// decompressed).
#[derive(Clone, Copy)]
struct Grid {
    w: i64,
    h: i64,
    step: usize,
    tw: usize,
    th: usize,
}

impl Grid {
    fn new(w: usize, h: usize, target_edge: u32) -> Self {
        let target = target_edge.clamp(1, MAX_TARGET_EDGE) as usize;
        let long = w.max(h);
        // FLOOR, not ceil: the caller resizes this grid down to its real target with
        // a proper filter, so the grid must never be SMALLER than what was asked for
        // (`step = ceil` would hand a 300 px source asked for a 256 px tile a 150 px
        // grid, i.e. a blurrier thumbnail than the full decode used to produce).
        // Flooring keeps `long / step >= target` for every input.
        let step = (long / target).max(1);
        Self {
            w: w as i64,
            h: h as i64,
            step,
            tw: w.div_ceil(step).max(1),
            th: h.div_ceil(step).max(1),
        }
    }

    /// The single source row that feeds target row `ty` — the middle of its band,
    /// clamped into the image so the last (partial) band still gets a row.
    fn representative_row(&self, ty: usize) -> i64 {
        let y = (ty * self.step + self.step / 2) as i64;
        y.min(self.h - 1)
    }

    /// The target row `y` feeds, or None when this source row isn't sampled.
    fn sampled_row(&self, y: i64) -> Option<usize> {
        if y < 0 || y >= self.h {
            return None;
        }
        let ty = (y as usize) / self.step;
        (ty < self.th && self.representative_row(ty) == y).then_some(ty)
    }

    /// Does the half-open source-row range `[y0, y1)` contain any sampled row?
    /// Used to drop whole chunks before they are decompressed.
    fn covers_sampled_row(&self, y0: i64, y1: i64) -> bool {
        let lo = y0.max(0);
        let hi = y1.min(self.h);
        if lo >= hi {
            return false;
        }
        let first = (lo as usize) / self.step;
        let last = ((hi - 1) as usize) / self.step;
        (first..=last.min(self.th.saturating_sub(1)))
            .any(|ty| (lo..hi).contains(&self.representative_row(ty)))
    }

    /// The target column a source column falls into, or None if it lies outside
    /// the display window.
    fn target_col(&self, x: i64) -> Option<usize> {
        if x < 0 || x >= self.w {
            return None;
        }
        let tx = (x as usize) / self.step;
        (tx < self.tw).then_some(tx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Write a synthetic EXR whose red channel encodes the column and whose green
    /// channel encodes the row, so a downscale can be checked positionally.
    fn ramp_exr(w: usize, h: usize, compression: Compression) -> Vec<u8> {
        let pixels =
            SpecificChannels::rgba(|p: Vec2<usize>| (p.x() as f32, p.y() as f32, 0.25f32, 1.0f32));
        let image = Image::from_encoded_channels(
            (w, h),
            Encoding {
                compression,
                ..Encoding::FAST_LOSSLESS
            },
            pixels,
        );
        let mut out = Cursor::new(Vec::new());
        image
            .write()
            .non_parallel()
            .to_buffered(&mut out)
            .expect("write test exr");
        out.into_inner()
    }

    #[test]
    fn magic_gate_only_matches_exr() {
        assert!(is_exr_magic(&[0x76, 0x2f, 0x31, 0x01, 0x02]));
        assert!(!is_exr_magic(b"\x89PNG"));
        assert!(!is_exr_magic(&[0x76, 0x2f]));
    }

    /// The grid must never be smaller than the edge the caller asked for — that is
    /// what keeps the final resize from working off a blurrier source than the old
    /// full-resolution decode did.
    #[test]
    fn the_grid_is_never_smaller_than_the_requested_edge() {
        for (w, h) in [(300usize, 200usize), (12288, 6480), (257, 257), (64, 4096)] {
            for edge in [32u32, 96, 256, 512, 1024, 2048] {
                let grid = Grid::new(w, h, edge);
                let long = grid.tw.max(grid.th) as u32;
                let requested = edge.min(w.max(h) as u32);
                assert!(
                    long >= requested,
                    "{w}x{h} @ {edge}: grid long edge {long} < requested {requested}"
                );
            }
        }
        // And an absurd request is clamped rather than allocating a giant grid.
        let grid = Grid::new(12288, 6480, u32::MAX);
        assert!(grid.tw <= MAX_TARGET_EDGE as usize && grid.th <= MAX_TARGET_EDGE as usize);
    }

    #[test]
    fn grid_samples_every_target_row_exactly_once() {
        for (h, edge) in [(6480usize, 256u32), (100, 7), (1, 64), (33, 32), (5000, 1)] {
            let grid = Grid::new(h, h, edge);
            let rows: Vec<usize> = (0..h as i64).filter_map(|y| grid.sampled_row(y)).collect();
            assert_eq!(
                rows.len(),
                grid.th,
                "every target row needs exactly one source row (h={h}, edge={edge})"
            );
            assert!(
                rows.windows(2).all(|p| p[1] == p[0] + 1),
                "target rows must be filled in order (h={h}, edge={edge})"
            );
        }
    }

    #[test]
    fn chunk_filter_keeps_exactly_the_blocks_holding_a_sampled_row() {
        // 12288x6480 downscaled to 256: PIZ stores 32 scan lines per chunk.
        let grid = Grid::new(12288, 6480, 256);
        let mut kept = 0usize;
        let mut sampled_in_kept = 0usize;
        let mut y = 0i64;
        while y < 6480 {
            let end = (y + 32).min(6480);
            if grid.covers_sampled_row(y, end) {
                kept += 1;
                sampled_in_kept += (y..end).filter(|&r| grid.sampled_row(r).is_some()).count();
            } else {
                assert!(
                    (y..end).all(|r| grid.sampled_row(r).is_none()),
                    "a skipped chunk must hold no sampled row"
                );
            }
            y = end;
        }
        assert_eq!(
            sampled_in_kept, grid.th,
            "kept chunks must carry every sampled row"
        );
        assert!(kept < 6480 / 32, "the filter must actually skip chunks");
    }

    #[test]
    fn scaled_decode_matches_the_source_ramp() {
        let bytes = ramp_exr(200, 100, Compression::PIZ);
        let img = decode_scaled(Cursor::new(&bytes), 20).expect("scaled decode");
        // step = ceil(200 / 20) = 10 -> 20x10 target.
        assert_eq!((img.width(), img.height()), (20, 10));
        let buf = img.to_rgba32f();
        // Red = column: target col 0 box-averages source columns 0..10 -> 4.5.
        assert!((buf.get_pixel(0, 0).0[0] - 4.5).abs() < 0.01);
        assert!((buf.get_pixel(19, 0).0[0] - 194.5).abs() < 0.01);
        // Green = row: target row 0 point-samples source row 5.
        assert!((buf.get_pixel(0, 0).0[1] - 5.0).abs() < 0.01);
        assert!((buf.get_pixel(0, 9).0[1] - 95.0).abs() < 0.01);
        // Blue is constant, alpha opaque.
        assert!((buf.get_pixel(7, 3).0[2] - 0.25).abs() < 0.01);
        assert!((buf.get_pixel(7, 3).0[3] - 1.0).abs() < 0.01);
    }

    #[test]
    fn a_target_larger_than_the_source_decodes_one_to_one() {
        let bytes = ramp_exr(16, 9, Compression::ZIP16);
        let img = decode_scaled(Cursor::new(&bytes), 512).expect("scaled decode");
        assert_eq!((img.width(), img.height()), (16, 9));
        let buf = img.to_rgba32f();
        assert!((buf.get_pixel(11, 4).0[0] - 11.0).abs() < 0.001);
        assert!((buf.get_pixel(11, 4).0[1] - 4.0).abs() < 0.001);
    }

    #[test]
    fn every_compression_the_writer_emits_round_trips_scaled() {
        for compression in [
            Compression::Uncompressed,
            Compression::RLE,
            Compression::ZIP1,
            Compression::ZIP16,
            Compression::PIZ,
            Compression::PXR24,
            Compression::B44,
            Compression::B44A,
        ] {
            let bytes = ramp_exr(64, 48, compression);
            let img = decode_scaled(Cursor::new(&bytes), 12)
                .unwrap_or_else(|_| panic!("scaled decode of {compression:?}"));
            // step = floor(64 / 12) = 5 -> ceil(64/5) x ceil(48/5) = 13x10.
            assert_eq!(
                (img.width(), img.height()),
                (13, 10),
                "{compression:?} dims"
            );
            let buf = img.to_rgba32f();
            // Green = row; target row 0 point-samples source row 5/2 = 2.
            assert!(
                (buf.get_pixel(0, 0).0[1] - 2.0).abs() < 0.01,
                "{compression:?} row sampling"
            );
            // Red = column; target col 0 box-averages source columns 0..5 -> 2.0.
            assert!(
                (buf.get_pixel(0, 0).0[0] - 2.0).abs() < 0.01,
                "{compression:?} column averaging"
            );
        }
    }

    /// The chunk-count ceiling has to sit ABOVE every image this decoder accepts,
    /// or it would silently reject legitimate files instead of pathological ones.
    #[test]
    fn the_chunk_ceiling_clears_every_acceptable_image() {
        let max = MAX_DIM as usize;
        // Worst legitimate cases: full-size scan lines (1 chunk per row), and full
        // size at the smallest tile size real encoders emit (16x16).
        assert!(max <= MAX_CHUNKS, "scan-line worst case must fit");
        assert!(
            max.div_ceil(16) * max.div_ceil(16) <= MAX_CHUNKS,
            "16x16-tiled worst case must fit"
        );
        // ...and BELOW the degenerate 1x1-tile layer it exists to stop.
        assert!(max * max > MAX_CHUNKS, "1x1 tiles must be refused");
    }

    #[test]
    fn non_exr_bytes_are_rejected_without_panicking() {
        assert!(decode_scaled(Cursor::new(vec![0u8; 4096]), 256).is_err());
        assert!(decode_scaled(Cursor::new(b"\x89PNG\r\n\x1a\n".to_vec()), 256).is_err());
        // Truncated EXR: the header parses, the chunks don't.
        let full = ramp_exr(32, 32, Compression::ZIP16);
        for cut in [8usize, 64, full.len() / 2] {
            let _ = decode_scaled(Cursor::new(full[..cut].to_vec()), 16);
        }
    }
}
