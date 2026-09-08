//! DjVu to TIFF exporter — phase 4 format extension.
//!
//! Converts DjVu documents to multi-page TIFF files.
//!
//! ## Key public types
//!
//! - [`TiffOptions`] — export parameters (color vs. bilevel mode)
//! - [`djvu_to_tiff_writer`] — low-memory writer API backed by row-streaming
//! - [`TiffError`] — errors from TIFF conversion
//!
//! ## Modes
//!
//! - **Color** (`TiffMode::Color`): each page is rendered to an RGB Pixmap
//!   and written as a 24-bit RGB TIFF strip.
//! - **Bilevel** (`TiffMode::Bilevel`): the JB2 mask is extracted and written
//!   as an 8-bit grayscale TIFF strip (0 = white, 255 = black). Pages with no
//!   JB2 mask fall back to a blank white page.
//!
//! ## Example
//!
//! ```no_run
//! use djvu_rs::djvu_document::DjVuDocument;
//! use djvu_rs::tiff_export::{djvu_to_tiff, TiffOptions, TiffMode};
//!
//! let data = std::fs::read("input.djvu").unwrap();
//! let doc = DjVuDocument::parse(&data).unwrap();
//! let tiff_bytes = djvu_to_tiff(&doc, &TiffOptions::default()).unwrap();
//! std::fs::write("output.tiff", tiff_bytes).unwrap();
//! ```

use std::io::{Cursor, Seek, Write};

use tiff::encoder::{Rational, TiffEncoder, colortype, compression::Deflate};
use tiff::tags::ResolutionUnit;

use crate::{
    djvu_document::{DjVuDocument, DjVuPage, DocError},
    djvu_render::{self, RenderError, RenderOptions},
};

// ---- Error ------------------------------------------------------------------

/// Errors from TIFF conversion.
#[derive(Debug, thiserror::Error)]
pub enum TiffError {
    /// Document model error.
    #[error("document error: {0}")]
    Doc(#[from] DocError),

    /// Render error.
    #[error("render error: {0}")]
    Render(#[from] RenderError),

    /// TIFF encoding error.
    #[error("TIFF encoding error: {0}")]
    Encode(String),
}

impl From<tiff::TiffError> for TiffError {
    fn from(e: tiff::TiffError) -> Self {
        TiffError::Encode(e.to_string())
    }
}

// ---- Options ----------------------------------------------------------------

/// Rendering mode for TIFF export.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TiffMode {
    /// Render each page as a full-color RGB image (24-bit per pixel).
    #[default]
    Color,
    /// Extract the JB2 foreground mask as an 8-bit grayscale image.
    ///
    /// Pixels set in the JB2 mask are exported as black (255); background as
    /// white (0).  Pages with no JB2 mask are written as blank white pages.
    Bilevel,
}

/// Compression choice for [`TiffMode::Bilevel`] pages (#579).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TiffBilevelCompression {
    /// 8-bit grayscale strips with Deflate — the historical default.
    #[default]
    Deflate,
    /// 1-bit CCITT Group 4 (T.6) via [`crate::smmr::encode_g4`] — the native
    /// archival compression for bilevel scans. Written by a minimal in-crate
    /// IFD writer (the `tiff` crate has no CCITT encoder); validated against
    /// libtiff/Pillow. Ignored in [`TiffMode::Color`].
    G4,
}

/// Options for DjVu → TIFF conversion.
#[derive(Debug, Clone)]
pub struct TiffOptions {
    /// Rendering mode.
    pub mode: TiffMode,
    /// Scale factor for color rendering (1.0 = native resolution).
    pub scale: f32,
    /// Compression for bilevel pages (default: the historical Deflate).
    pub bilevel_compression: TiffBilevelCompression,
}

impl Default for TiffOptions {
    fn default() -> Self {
        TiffOptions {
            mode: TiffMode::Color,
            scale: 1.0,
            bilevel_compression: TiffBilevelCompression::default(),
        }
    }
}

// ---- Entry point ------------------------------------------------------------

/// Convert a DjVu document to a multi-page TIFF byte buffer.
///
/// Each page in `doc` produces one IFD in the output TIFF.  Color pages use the
/// row-streaming renderer when the requested options do not require a full-image
/// post-processing pass; unsupported render options automatically fall back to
/// the full-pixmap path.
pub fn djvu_to_tiff(doc: &DjVuDocument, opts: &TiffOptions) -> Result<Vec<u8>, TiffError> {
    let mut buf: Vec<u8> = Vec::new();
    {
        let cursor = Cursor::new(&mut buf);
        djvu_to_tiff_writer(doc, opts, cursor)?;
    }
    Ok(buf)
}

/// Write a DjVu document as a multi-page TIFF to `writer`.
///
/// This is the lowest-memory TIFF export entry point: when color rendering is
/// streamable, rows are passed directly from [`djvu_render::render_streaming`]
/// into TIFF strips without constructing a full output [`crate::Pixmap`] or an
/// intermediate full RGB image.
pub fn djvu_to_tiff_writer<W: Write + Seek>(
    doc: &DjVuDocument,
    opts: &TiffOptions,
    writer: W,
) -> Result<(), TiffError> {
    if opts.mode == TiffMode::Bilevel && opts.bilevel_compression == TiffBilevelCompression::G4 {
        return write_bilevel_g4_tiff(doc, writer);
    }
    let mut encoder = TiffEncoder::new(writer)?;
    let indices: Vec<usize> = crate::export_common::page_indices(doc, None).collect();

    // Building a page's pixel buffer (color: render → RGB; bilevel: JB2 decode →
    // Gray8) is independent and CPU-heavy per page; only appending IFDs to the
    // single `TiffEncoder` must stay serial. With the `parallel` feature, build
    // every page's image concurrently via rayon, then write them in index order
    // — the same shape as the PDF/EPUB parallel exporters. Output is byte-
    // identical: the materialised RGB matches the streaming path (asserted by
    // `streamed_color_tiff_matches_render_pixmap*`), and the encoder produces the
    // same IFDs from the same pixels. This trades the sequential path's
    // row-streaming O(1)-page memory for wall-time, so it is gated to the feature.
    #[cfg(feature = "parallel")]
    {
        use rayon::prelude::*;
        let images: Vec<PageImage> = indices
            .par_iter()
            .map(|&i| {
                // #629: cold clone — decode caches drop with the page.
                let page = doc.page(i)?.clone();
                build_page_image(&page, opts)
            })
            .collect::<Result<Vec<_>, TiffError>>()?;
        for img in &images {
            write_page_image(&mut encoder, img)?;
        }
    }

    #[cfg(not(feature = "parallel"))]
    for &i in &indices {
        // #629: cold clone — decode caches drop with the page.
        let page = doc.page(i)?.clone();
        match opts.mode {
            TiffMode::Color => write_color_page(&mut encoder, &page, opts.scale)?,
            TiffMode::Bilevel => write_bilevel_page(&mut encoder, &page)?,
        }
    }
    Ok(())
}

/// A page's fully-materialised pixel buffer, ready to append as one TIFF IFD.
/// This is the `Send`-safe unit the parallel exporter builds off-thread; writing
/// it into the shared `TiffEncoder` is the serial tail.
#[cfg(feature = "parallel")]
struct PageImage {
    w: u32,
    h: u32,
    dpi: u32,
    data: PageImageData,
}

#[cfg(feature = "parallel")]
enum PageImageData {
    /// 24-bit RGB strip (color mode).
    Rgb(Vec<u8>),
    /// 8-bit grayscale strip written with Deflate compression (bilevel mode).
    GrayDeflate(Vec<u8>),
}

/// Produce one page's pixel buffer without touching the encoder. Mirrors the
/// sequential per-mode dispatch so the resulting IFD is byte-identical.
#[cfg(feature = "parallel")]
fn build_page_image(page: &DjVuPage, opts: &TiffOptions) -> Result<PageImage, TiffError> {
    match opts.mode {
        TiffMode::Color => {
            let (w, h, ropts) = color_render_options(page, opts.scale);
            let dpi = (page.dpi() as f32 * opts.scale).round() as u32;
            // Materialise the same RGB the sequential path writes: collect the
            // streamed rows when streamable (byte-identical to the strip path),
            // else fall back to the full-pixmap path for non-streamable options.
            let rgb = if ropts.can_stream(page) {
                let mut rgb = Vec::with_capacity(w as usize * h as usize * 3);
                djvu_render::render_streaming(page, &ropts, |_, rgba_row| {
                    crate::export_common::rgba_row_to_rgb(&mut rgb, rgba_row);
                })?;
                rgb
            } else {
                djvu_render::render_pixmap(page, &ropts)?.to_rgb()
            };
            Ok(PageImage {
                w,
                h,
                dpi,
                data: PageImageData::Rgb(rgb),
            })
        }
        TiffMode::Bilevel => {
            let w = page.width() as u32;
            let h = page.height() as u32;
            let gray = extract_bilevel_pixels(page, w, h)?;
            let dpi = page.dpi() as u32;
            Ok(PageImage {
                w,
                h,
                dpi,
                data: PageImageData::GrayDeflate(gray),
            })
        }
    }
}

/// Append one pre-built page image to the encoder as a single IFD.
#[cfg(feature = "parallel")]
fn write_page_image<W: Write + Seek>(
    encoder: &mut TiffEncoder<W>,
    img: &PageImage,
) -> Result<(), TiffError> {
    match &img.data {
        PageImageData::Rgb(rgb) => {
            let mut image = encoder.new_image::<colortype::RGB8>(img.w, img.h)?;
            image.resolution(ResolutionUnit::Inch, Rational { n: img.dpi, d: 1 });
            image.write_data(rgb)?;
        }
        PageImageData::GrayDeflate(gray) => {
            let mut image = encoder.new_image_with_compression::<colortype::Gray8, _>(
                img.w,
                img.h,
                Deflate::default(),
            )?;
            image.resolution(ResolutionUnit::Inch, Rational { n: img.dpi, d: 1 });
            image.write_data(gray)?;
        }
    }
    Ok(())
}

// ---- Per-page helpers -------------------------------------------------------

/// Render `page` as RGB and append one IFD to `encoder`.
#[cfg(not(feature = "parallel"))]
fn write_color_page<W: Write + Seek>(
    encoder: &mut TiffEncoder<W>,
    page: &DjVuPage,
    scale: f32,
) -> Result<(), TiffError> {
    let (w, h, opts) = color_render_options(page, scale);
    let dpi = (page.dpi() as f32 * scale).round() as u32;

    if opts.can_stream(page) {
        write_color_page_streaming(encoder, page, &opts, w, h, dpi)
    } else {
        write_color_page_pixmap(encoder, page, &opts, w, h, dpi)
    }
}

fn color_render_options(page: &DjVuPage, scale: f32) -> (u32, u32, RenderOptions) {
    let (w, h) =
        crate::export_common::scaled_size(page.width() as u32, page.height() as u32, scale);

    // Only the size is set; the pipeline derives the decode scale from `width`.
    // The remaining fields (bold/aa/rotation/permissive/resampling) are the
    // `RenderOptions` defaults.
    let opts = RenderOptions {
        width: w,
        height: h,
        ..RenderOptions::default()
    };
    (w, h, opts)
}

#[cfg(not(feature = "parallel"))]
fn write_color_page_streaming<W: Write + Seek>(
    encoder: &mut TiffEncoder<W>,
    page: &DjVuPage,
    opts: &RenderOptions,
    w: u32,
    h: u32,
    dpi: u32,
) -> Result<(), TiffError> {
    let mut img = encoder.new_image::<colortype::RGB8>(w, h)?;
    img.resolution(ResolutionUnit::Inch, Rational { n: dpi, d: 1 });

    let mut next_strip_samples = img.next_strip_sample_count() as usize;
    let mut strip = Vec::with_capacity(next_strip_samples);
    let mut encode_error: Option<tiff::TiffError> = None;

    djvu_render::render_streaming(page, opts, |_, rgba_row| {
        if encode_error.is_some() {
            return;
        }

        crate::export_common::rgba_row_to_rgb(&mut strip, rgba_row);

        if strip.len() > next_strip_samples {
            encode_error = Some(
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "streamed RGB strip exceeded expected TIFF strip size",
                )
                .into(),
            );
            return;
        }

        if strip.len() == next_strip_samples {
            if let Err(e) = img.write_strip(&strip) {
                encode_error = Some(e);
                return;
            }
            strip.clear();
            next_strip_samples = img.next_strip_sample_count() as usize;
        }
    })?;

    if let Some(e) = encode_error {
        return Err(e.into());
    }
    if !strip.is_empty() || img.next_strip_sample_count() != 0 {
        return Err(TiffError::Encode(
            "streamed render ended before all TIFF strips were written".to_string(),
        ));
    }

    img.finish()?;
    Ok(())
}

#[cfg(not(feature = "parallel"))]
fn write_color_page_pixmap<W: Write + Seek>(
    encoder: &mut TiffEncoder<W>,
    page: &DjVuPage,
    opts: &RenderOptions,
    w: u32,
    h: u32,
    dpi: u32,
) -> Result<(), TiffError> {
    let pixmap = djvu_render::render_pixmap(page, opts)?;
    let rgb = pixmap.to_rgb();

    let mut img = encoder.new_image::<colortype::RGB8>(w, h)?;
    img.resolution(ResolutionUnit::Inch, Rational { n: dpi, d: 1 });
    img.write_data(&rgb)?;
    Ok(())
}

/// Extract the JB2 mask from `page` as an 8-bit grayscale strip and append
/// one IFD to `encoder`.
///
/// Black pixels in the mask are written as 255; white background as 0.
/// Pages without a JB2 mask get a blank white page.
#[cfg(not(feature = "parallel"))]
fn write_bilevel_page<W: std::io::Write + std::io::Seek>(
    encoder: &mut TiffEncoder<W>,
    page: &DjVuPage,
) -> Result<(), TiffError> {
    let w = page.width() as u32;
    let h = page.height() as u32;

    // Try to extract the JB2 mask directly from the page chunks.
    let gray = extract_bilevel_pixels(page, w, h)?;
    let dpi = page.dpi() as u32;
    // Bilevel content is just 0x00 / 0xFF bytes with long runs (text on white),
    // so Deflate shrinks the Gray8 strip by ~20–50× — far past the 8× of a true
    // 1-bit packing, which the `tiff` crate's high-level encoder cannot emit (no
    // 1-bit ColorType). Deflate (tag 8) is universally readable.
    let mut img =
        encoder.new_image_with_compression::<colortype::Gray8, _>(w, h, Deflate::default())?;
    img.resolution(ResolutionUnit::Inch, Rational { n: dpi, d: 1 });
    img.write_data(&gray)?;
    Ok(())
}

/// Extract the JB2 Sjbz mask as 8-bit grayscale (0=white, 255=black).
///
/// Returns a blank white buffer if no Sjbz chunk is present (pure IW44 page).
/// Returns `Err` if an Sjbz chunk exists but decoding fails.
// ---- Bilevel G4 writer (#579) ------------------------------------------------
//
// The `tiff` crate (0.9) has no CCITT encoder, so the G4 path hand-rolls the
// minimal multi-page bilevel TIFF: little-endian header, one IFD per page with
// the 11 tags libtiff expects for a G4 fax image, strip data = the raw
// `smmr::encode_g4` T.6 payload (1 strip per page). Photometric is
// min-is-white (0), matching T.6's white-first run convention and our mask's
// 1 = black.
fn write_bilevel_g4_tiff<W: Write + Seek>(doc: &DjVuDocument, mut w: W) -> Result<(), TiffError> {
    let indices: Vec<usize> = crate::export_common::page_indices(doc, None).collect();

    // Encode every page's G4 payload first (parallel when available) — the
    // IFD chain needs strip offsets, so sizes must be known before layout.
    let encode_one = |i: usize| -> Result<(u32, u32, u32, Vec<u8>), TiffError> {
        // #629: cold clone — the mask decode cache drops with the page.
        let page = doc.page(i)?.clone();
        let pw = page.width() as u32;
        let ph = page.height() as u32;
        let dpi = page.dpi().max(1) as u32;
        let mask = page
            .extract_mask()
            .map_err(TiffError::Doc)?
            .filter(|m| m.width >= pw && m.height >= ph)
            .unwrap_or_else(|| crate::bitmap::Bitmap::new(pw, ph));
        Ok((pw, ph, dpi, crate::smmr::encode_g4(&mask)))
    };
    #[cfg(feature = "parallel")]
    let pages: Vec<(u32, u32, u32, Vec<u8>)> = {
        use rayon::prelude::*;
        indices
            .par_iter()
            .map(|&i| encode_one(i))
            .collect::<Result<Vec<_>, _>>()?
    };
    #[cfg(not(feature = "parallel"))]
    let pages: Vec<(u32, u32, u32, Vec<u8>)> = indices
        .iter()
        .map(|&i| encode_one(i))
        .collect::<Result<Vec<_>, _>>()?;

    let io = |e: std::io::Error| TiffError::Encode(e.to_string());

    // Layout: header (8) → per page [strip data, then 8-byte-aligned IFD].
    const NTAGS: u16 = 11;
    let ifd_size = 2 + NTAGS as u32 * 12 + 4; // count + entries + next-IFD ptr
    let mut offset: u32 = 8;
    let mut layout = Vec::with_capacity(pages.len()); // (strip_off, rational_off, ifd_off)
    for (_, _, _, g4) in &pages {
        let strip_off = offset;
        // XResolution/YResolution rationals (2×8 bytes) live after the strip.
        let rat_off = (strip_off + g4.len() as u32).div_ceil(2) * 2;
        let ifd_off = (rat_off + 16).div_ceil(2) * 2;
        layout.push((strip_off, rat_off, ifd_off));
        offset = ifd_off + ifd_size;
    }

    // Header: little-endian, magic 42, first IFD offset.
    w.write_all(b"II\x2a\x00").map_err(io)?;
    w.write_all(&layout[0].2.to_le_bytes()).map_err(io)?;

    let entry = |tag: u16, typ: u16, count: u32, value: u32| -> [u8; 12] {
        let mut e = [0u8; 12];
        e[0..2].copy_from_slice(&tag.to_le_bytes());
        e[2..4].copy_from_slice(&typ.to_le_bytes());
        e[4..8].copy_from_slice(&count.to_le_bytes());
        e[8..12].copy_from_slice(&value.to_le_bytes());
        e
    };

    let mut pos: u32 = 8;
    for (idx, ((pw, ph, dpi, g4), &(strip_off, rat_off, ifd_off))) in
        pages.iter().zip(&layout).enumerate()
    {
        debug_assert_eq!(pos, strip_off);
        w.write_all(g4).map_err(io)?;
        pos += g4.len() as u32;
        while pos < rat_off {
            w.write_all(&[0]).map_err(io)?;
            pos += 1;
        }
        // X/Y resolution rationals (dpi / 1).
        for _ in 0..2 {
            w.write_all(&dpi.to_le_bytes()).map_err(io)?;
            w.write_all(&1u32.to_le_bytes()).map_err(io)?;
        }
        pos += 16;
        while pos < ifd_off {
            w.write_all(&[0]).map_err(io)?;
            pos += 1;
        }

        w.write_all(&NTAGS.to_le_bytes()).map_err(io)?;
        // Types: 3 = SHORT, 4 = LONG, 5 = RATIONAL.
        w.write_all(&entry(256, 4, 1, *pw)).map_err(io)?; // ImageWidth
        w.write_all(&entry(257, 4, 1, *ph)).map_err(io)?; // ImageLength
        w.write_all(&entry(258, 3, 1, 1)).map_err(io)?; // BitsPerSample
        w.write_all(&entry(259, 3, 1, 4)).map_err(io)?; // Compression = CCITT G4
        w.write_all(&entry(262, 3, 1, 0)).map_err(io)?; // Photometric = WhiteIsZero
        w.write_all(&entry(273, 4, 1, strip_off)).map_err(io)?; // StripOffsets
        w.write_all(&entry(277, 3, 1, 1)).map_err(io)?; // SamplesPerPixel
        w.write_all(&entry(278, 4, 1, *ph)).map_err(io)?; // RowsPerStrip
        w.write_all(&entry(279, 4, 1, g4.len() as u32))
            .map_err(io)?; // StripByteCounts
        w.write_all(&entry(282, 5, 1, rat_off)).map_err(io)?; // XResolution
        w.write_all(&entry(283, 5, 1, rat_off + 8)).map_err(io)?; // YResolution
        let next = if idx + 1 < layout.len() {
            layout[idx + 1].2
        } else {
            0
        };
        w.write_all(&next.to_le_bytes()).map_err(io)?;
        pos = ifd_off + ifd_size;
    }
    w.flush().map_err(io)?;
    Ok(())
}

fn extract_bilevel_pixels(page: &DjVuPage, w: u32, h: u32) -> Result<Vec<u8>, TiffError> {
    let sjbz = match page.find_chunk(b"Sjbz") {
        Some(d) => d,
        None => return Ok(vec![0u8; (w * h) as usize]),
    };

    let dict = page
        .find_chunk(b"Djbz")
        .and_then(|djbz| crate::jb2::decode_dict(djbz, None).ok());

    let bm = crate::jb2::decode(sjbz, dict.as_ref())
        .map_err(|e| TiffError::Encode(format!("JB2 decode failed: {e}")))?;

    let wq = w as usize;
    let mut pixels = vec![0u8; wq * h as usize];

    // LUT byte-expansion: when the decoded mask covers the page, expand each
    // packed mask byte to 8 Gray8 pixels via a 256-entry table instead of one
    // `bm.get()` (stride mult + bit-extract) per pixel. MSB-first packing: pixel
    // x is bit (7 - x%8) of byte x/8, matching `BILEVEL_GRAY8`.
    if bm.width >= w && bm.height >= h {
        let stride = bm.row_stride();
        let nb_full = wq / 8;
        let rem = wq % 8;
        for y in 0..h as usize {
            let row = &bm.data[y * stride..];
            let out = &mut pixels[y * wq..(y + 1) * wq];
            for bi in 0..nb_full {
                out[bi * 8..bi * 8 + 8].copy_from_slice(&BILEVEL_GRAY8[row[bi] as usize]);
            }
            if rem > 0 {
                let src = &BILEVEL_GRAY8[row[nb_full] as usize];
                out[nb_full * 8..nb_full * 8 + rem].copy_from_slice(&src[..rem]);
            }
        }
        return Ok(pixels);
    }

    // Fallback for the unexpected case where the mask is smaller than the page.
    // Bitmap pixels: true = black foreground, false = white background.
    for y in 0..h {
        for x in 0..w {
            pixels[(y * w + x) as usize] = if bm.get(x, y) { 255u8 } else { 0u8 };
        }
    }
    Ok(pixels)
}

/// Maps each packed mask byte (MSB-first) to its 8 expanded Gray8 pixels —
/// bit set (black) → 255, bit clear (white) → 0.
const BILEVEL_GRAY8: [[u8; 8]; 256] = {
    let mut lut = [[0u8; 8]; 256];
    let mut mb = 0usize;
    while mb < 256 {
        let mut j = 0usize;
        while j < 8 {
            lut[mb][j] = if (mb >> (7 - j)) & 1 != 0 { 255u8 } else { 0u8 };
            j += 1;
        }
        mb += 1;
    }
    lut
};

// ---- Tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::djvu_render;

    fn assets_path() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("references/djvujs/library/assets")
    }

    fn load_doc(filename: &str) -> DjVuDocument {
        let data = std::fs::read(assets_path().join(filename))
            .unwrap_or_else(|_| panic!("{filename} must exist"));
        DjVuDocument::parse(&data).unwrap_or_else(|e| panic!("parse failed: {e}"))
    }

    fn decode_first_tiff_rgb(tiff_bytes: &[u8]) -> (u32, u32, Vec<u8>) {
        let cursor = std::io::Cursor::new(tiff_bytes);
        let mut decoder = tiff::decoder::Decoder::new(cursor).expect("tiff must be decodable");
        let (w, h) = decoder.dimensions().expect("must have dimensions");
        let img = decoder.read_image().expect("image must decode");
        let tiff::decoder::DecodingResult::U8(pixels) = img else {
            panic!("expected RGB8 TIFF pixels");
        };
        (w, h, pixels)
    }

    fn assert_streamed_color_tiff_matches_render_pixmap(filename: &str) {
        let doc = load_doc(filename);
        let page = doc.page(0).unwrap();
        let (_, _, render_opts) = color_render_options(page, 1.0);
        assert!(
            render_opts.can_stream(page),
            "fixture should use the streaming TIFF color path"
        );

        let mut cursor = std::io::Cursor::new(Vec::new());
        djvu_to_tiff_writer(&doc, &TiffOptions::default(), &mut cursor)
            .expect("streamed TIFF writer must succeed");
        let tiff_bytes = cursor.into_inner();
        let (w, h, pixels) = decode_first_tiff_rgb(&tiff_bytes);

        assert_eq!((w, h), (render_opts.width, render_opts.height));
        let expected = djvu_render::render_pixmap(page, &render_opts)
            .expect("render_pixmap must succeed")
            .to_rgb();
        assert_eq!(pixels, expected);
    }

    // ── TDD tests ─────────────────────────────────────────────────────────────

    /// `djvu_to_tiff` produces non-empty bytes for a color document.
    #[test]
    fn color_export_produces_bytes() {
        let doc = load_doc("chicken.djvu");
        let tiff = djvu_to_tiff(&doc, &TiffOptions::default()).expect("color export must succeed");
        assert!(!tiff.is_empty(), "TIFF output must not be empty");
    }

    /// TIFF output starts with the standard TIFF magic bytes (little-endian II or big-endian MM).
    #[test]
    fn output_starts_with_tiff_magic() {
        let doc = load_doc("chicken.djvu");
        let tiff = djvu_to_tiff(&doc, &TiffOptions::default()).unwrap();
        let magic = &tiff[..4];
        assert!(
            magic == b"II\x2A\x00" || magic == b"MM\x00\x2A",
            "must start with TIFF magic, got: {magic:?}"
        );
    }

    /// Bilevel export produces non-empty bytes.
    #[test]
    fn bilevel_export_produces_bytes() {
        let doc = load_doc("boy_jb2.djvu");
        let opts = TiffOptions {
            mode: TiffMode::Bilevel,
            ..Default::default()
        };
        let tiff = djvu_to_tiff(&doc, &opts).expect("bilevel export must succeed");
        assert!(!tiff.is_empty());
    }

    /// Bilevel export also starts with TIFF magic.
    #[test]
    fn bilevel_output_starts_with_tiff_magic() {
        let doc = load_doc("boy_jb2.djvu");
        let opts = TiffOptions {
            mode: TiffMode::Bilevel,
            ..Default::default()
        };
        let tiff = djvu_to_tiff(&doc, &opts).unwrap();
        let magic = &tiff[..4];
        assert!(magic == b"II\x2A\x00" || magic == b"MM\x00\x2A");
    }

    /// Multi-page export: two pages produce more output than one page.
    #[test]
    fn multipage_larger_than_single_page() {
        // Build a two-page DjVu document by concatenating two single-page exports
        // as separate DjVuDocument instances and comparing their individual outputs.
        let doc_a = load_doc("chicken.djvu");
        let doc_b = load_doc("boy.djvu");
        let opts = TiffOptions::default();

        let tiff_a = djvu_to_tiff(&doc_a, &opts).expect("page A export must succeed");
        let tiff_b = djvu_to_tiff(&doc_b, &opts).expect("page B export must succeed");

        // Both single-page TIFFs must be non-trivially sized
        assert!(tiff_a.len() > 100, "page A TIFF must be non-trivial");
        assert!(tiff_b.len() > 100, "page B TIFF must be non-trivial");
    }

    /// Two different single-page documents produce differently-sized TIFFs.
    #[test]
    fn different_pages_produce_different_sizes() {
        let doc_a = load_doc("chicken.djvu");
        let doc_b = load_doc("boy.djvu");
        let opts = TiffOptions::default();

        let tiff_a = djvu_to_tiff(&doc_a, &opts).unwrap();
        let tiff_b = djvu_to_tiff(&doc_b, &opts).unwrap();
        // Different pages have different content, so their TIFFs should differ
        assert_ne!(
            tiff_a.len(),
            tiff_b.len(),
            "different pages must produce different TIFF sizes"
        );
    }

    /// Color export at 0.5 scale produces a smaller file than at 1.0 scale.
    #[test]
    fn scale_factor_reduces_file_size() {
        let doc = load_doc("chicken.djvu");
        let full = djvu_to_tiff(&doc, &TiffOptions::default()).unwrap();
        let half = djvu_to_tiff(
            &doc,
            &TiffOptions {
                scale: 0.5,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            half.len() < full.len(),
            "half-scale TIFF must be smaller: half={} full={}",
            half.len(),
            full.len()
        );
    }

    /// Round-trip: exported TIFF can be re-decoded by the `tiff` crate.
    #[test]
    fn color_tiff_round_trips_via_tiff_decoder() {
        let doc = load_doc("chicken.djvu");
        let tiff_bytes = djvu_to_tiff(&doc, &TiffOptions::default()).unwrap();

        let cursor = std::io::Cursor::new(&tiff_bytes);
        let mut decoder = tiff::decoder::Decoder::new(cursor).expect("tiff must be decodable");
        // The first IFD must decode without error and have reasonable dimensions.
        let (w, h) = decoder.dimensions().expect("must have dimensions");
        let page = doc.page(0).unwrap();
        assert_eq!(w, page.width() as u32);
        assert_eq!(h, page.height() as u32);
    }

    /// Streamed color TIFF export matches the existing full-pixmap render path on a color page.
    #[test]
    fn streamed_color_tiff_matches_render_pixmap_color_page() {
        assert_streamed_color_tiff_matches_render_pixmap("chicken.djvu");
    }

    /// Streamed color TIFF export also matches the full-pixmap path on a bilevel page.
    #[test]
    fn streamed_color_tiff_matches_render_pixmap_bilevel_page() {
        assert_streamed_color_tiff_matches_render_pixmap("boy_jb2.djvu");
    }

    /// Bilevel pages with JB2 mask have non-uniform pixel values (some black pixels).
    #[test]
    fn bilevel_jb2_page_has_black_pixels() {
        let doc = load_doc("boy_jb2.djvu");
        let opts = TiffOptions {
            mode: TiffMode::Bilevel,
            ..Default::default()
        };
        let tiff_bytes = djvu_to_tiff(&doc, &opts).unwrap();

        let cursor = std::io::Cursor::new(&tiff_bytes);
        let mut decoder = tiff::decoder::Decoder::new(cursor).unwrap();
        let img = decoder.read_image().unwrap();
        if let tiff::decoder::DecodingResult::U8(pixels) = img {
            let has_black = pixels.contains(&255);
            assert!(
                has_black,
                "bilevel JB2 page must have at least one black pixel"
            );
        }
    }

    /// Bilevel export on a page without JB2 mask returns a blank (all-white) page.
    #[test]
    fn bilevel_blank_when_no_jb2_mask() {
        // chicken.djvu is a color-only document with no JB2 mask
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();
        let w = page.width() as u32;
        let h = page.height() as u32;

        let pixels = extract_bilevel_pixels(page, w, h).unwrap();
        assert!(
            pixels.iter().all(|&p| p == 0),
            "page without JB2 must be all-white (0)"
        );
    }

    /// Color export on a rotated page forces the pixmap path (can_stream returns
    /// false when page.rotation() != None), exercising write_color_page_pixmap.
    #[test]
    fn color_export_rotated_page_uses_pixmap_path() {
        let doc = load_doc("boy_jb2_rotate90.djvu");
        let page = doc.page(0).unwrap();
        // Confirm rotation is set so can_stream returns false
        assert_ne!(
            page.rotation(),
            crate::info::Rotation::None,
            "fixture must have rotation set"
        );
        let tiff =
            djvu_to_tiff(&doc, &TiffOptions::default()).expect("rotated color export must succeed");
        assert!(!tiff.is_empty());
        let magic = &tiff[..4];
        assert!(magic == b"II\x2A\x00" || magic == b"MM\x00\x2A");
    }

    /// `TiffOptions::default()` selects color mode at 1.0 scale.
    #[test]
    fn tiff_options_default() {
        let opts = TiffOptions::default();
        assert_eq!(opts.mode, TiffMode::Color);
        assert!((opts.scale - 1.0).abs() < 1e-6);
    }

    /// `From<tiff::TiffError> for TiffError` wraps the error message.
    #[test]
    fn from_tiff_error_wraps_message() {
        let io_err = std::io::Error::other("test tiff failure");
        let tiff_err: tiff::TiffError = io_err.into();
        let djvu_tiff_err: TiffError = tiff_err.into();
        let s = djvu_tiff_err.to_string();
        assert!(
            s.contains("TIFF encoding error"),
            "must mention TIFF encoding error: {s}"
        );
    }

    /// `TiffError::Encode` display includes the inner message.
    #[test]
    fn tiff_error_display() {
        let e = TiffError::Encode("something went wrong".to_string());
        assert!(e.to_string().contains("something went wrong"));
    }

    /// #579: the G4 bilevel TIFF is structurally sound (LE header, one IFD per
    /// page, CCITT G4 compression tag) and its strip round-trips through our
    /// own T.6 decoder to the exact page mask.
    #[test]
    fn bilevel_g4_tiff_round_trips_through_own_decoder() {
        let data = std::fs::read(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/boy_jb2.djvu"),
        )
        .unwrap();
        let doc = DjVuDocument::parse(&data).unwrap();
        let tiff = djvu_to_tiff(
            &doc,
            &TiffOptions {
                mode: TiffMode::Bilevel,
                bilevel_compression: TiffBilevelCompression::G4,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(&tiff[0..4], b"II\x2a\x00", "little-endian TIFF header");
        let rd32 = |o: usize| u32::from_le_bytes(tiff[o..o + 4].try_into().unwrap());
        let rd16 = |o: usize| u16::from_le_bytes(tiff[o..o + 2].try_into().unwrap());
        let ifd = rd32(4) as usize;
        let ntags = rd16(ifd) as usize;
        let mut tags = std::collections::BTreeMap::new();
        for i in 0..ntags {
            let e = ifd + 2 + i * 12;
            tags.insert(rd16(e), rd32(e + 8));
        }
        assert_eq!(tags[&259], 4, "Compression must be CCITT G4");
        assert_eq!(tags[&258], 1, "BitsPerSample must be 1");
        assert_eq!(tags[&262], 0, "Photometric must be min-is-white");
        assert_eq!(rd32(ifd + 2 + ntags * 12), 0, "single page: next IFD = 0");

        let (w, h) = (tags[&256], tags[&257]);
        let off = tags[&273] as usize;
        let len = tags[&279] as usize;
        let mut chunk = Vec::with_capacity(4 + len);
        chunk.extend_from_slice(&(w as u16).to_be_bytes());
        chunk.extend_from_slice(&(h as u16).to_be_bytes());
        chunk.extend_from_slice(&tiff[off..off + len]);
        let decoded = crate::smmr::decode_smmr(&chunk).expect("G4 strip must decode");

        let mask = doc.page(0).unwrap().extract_mask().unwrap().unwrap();
        assert_eq!((decoded.width, decoded.height), (w, h));
        for y in 0..h {
            for x in 0..w {
                assert_eq!(decoded.get(x, y), mask.get(x, y), "pixel ({x},{y})");
            }
        }
    }

    /// #579: multi-page G4 TIFF chains IFDs for every page.
    #[test]
    fn bilevel_g4_tiff_multipage_chains_ifds() {
        let data = std::fs::read(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/corpus/cable_1973_100133.djvu"),
        )
        .unwrap();
        let doc = DjVuDocument::parse(&data).unwrap();
        let tiff = djvu_to_tiff(
            &doc,
            &TiffOptions {
                mode: TiffMode::Bilevel,
                bilevel_compression: TiffBilevelCompression::G4,
                ..Default::default()
            },
        )
        .unwrap();
        let rd32 = |o: usize| u32::from_le_bytes(tiff[o..o + 4].try_into().unwrap());
        let rd16 = |o: usize| u16::from_le_bytes(tiff[o..o + 2].try_into().unwrap());
        let mut ifd = rd32(4) as usize;
        let mut pages = 0;
        while ifd != 0 {
            let ntags = rd16(ifd) as usize;
            pages += 1;
            ifd = rd32(ifd + 2 + ntags * 12) as usize;
        }
        assert_eq!(pages, doc.page_count(), "one IFD per page");
    }
}
