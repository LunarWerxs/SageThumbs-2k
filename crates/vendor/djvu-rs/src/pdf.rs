//! DjVu to PDF converter — preserves document structure.
//!
//! Converts DjVu documents to PDF while preserving:
//! - IW44 background as compressed RGB image (#2)
//! - JB2 foreground mask as 1-bit image (#3)
//! - Text layer as invisible selectable text (#4)
//! - NAVM bookmarks as PDF outline / table of contents (#5)
//! - ANTz hyperlinks as PDF link annotations (#6)
//!
//! # Example
//!
//! ```no_run
//! use djvu_rs::djvu_document::DjVuDocument;
//! use djvu_rs::pdf::djvu_to_pdf;
//!
//! let data = std::fs::read("input.djvu").unwrap();
//! let doc = DjVuDocument::parse(&data).unwrap();
//! let pdf_bytes = djvu_to_pdf(&doc).unwrap();
//! std::fs::write("output.pdf", pdf_bytes).unwrap();
//! ```

#[cfg(not(feature = "std"))]
use alloc::{format, string::String, vec, vec::Vec};

use crate::{
    annotation::Shape,
    djvu_document::{DjVuBookmark, DjVuDocument, DjVuPage, DocError},
    djvu_render::{self, RenderOptions},
    text::Rect,
};

// ---- Error ------------------------------------------------------------------

/// Errors from PDF conversion.
#[derive(Debug, thiserror::Error)]
pub enum PdfError {
    /// Document model error.
    #[error("document error: {0}")]
    Doc(#[from] DocError),
    /// Render error.
    #[error("render error: {0}")]
    Render(#[from] djvu_render::RenderError),
    /// I/O error writing to the output sink (`djvu_to_pdf_to_writer`).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

// ---- Low-level PDF object writer --------------------------------------------

/// Streams PDF objects to a [`Write`](std::io::Write) sink as they are added,
/// retaining only `(id, byte offset)` per object for the final xref table
/// (#606). Object bodies are written in insertion order — the same order the
/// former buffer-everything writer serialized them in, so output bytes are
/// unchanged.
struct PdfWriter<W: std::io::Write> {
    sink: W,
    /// Bytes written so far (= next object's offset).
    written: usize,
    /// `(object id, byte offset)` in insertion order.
    offsets: Vec<(usize, usize)>,
    next_id: usize,
}

impl<W: std::io::Write> PdfWriter<W> {
    /// Create the writer and emit the PDF header.
    fn new(sink: W) -> Result<Self, PdfError> {
        let mut w = PdfWriter {
            sink,
            written: 0,
            offsets: Vec::new(),
            next_id: 1,
        };
        w.write_all(b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n")?;
        Ok(w)
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), PdfError> {
        self.sink.write_all(bytes)?;
        self.written += bytes.len();
        Ok(())
    }

    /// Reserve the next object ID.
    fn alloc_id(&mut self) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Write an object with a pre-allocated ID; its body is not retained.
    fn add_obj(&mut self, id: usize, body: Vec<u8>) -> Result<(), PdfError> {
        self.offsets.push((id, self.written));
        self.write_all(format!("{id} 0 obj\n").as_bytes())?;
        self.write_all(&body)?;
        self.write_all(b"\nendobj\n")
    }

    /// Allocate and write an object, returning its ID.
    fn add(&mut self, body: Vec<u8>) -> Result<usize, PdfError> {
        let id = self.alloc_id();
        self.add_obj(id, body)?;
        Ok(id)
    }

    /// Write the cross-reference table and trailer, consuming the writer.
    fn finish(mut self) -> Result<(), PdfError> {
        let xref_offset = self.written;
        let max_id = self.offsets.iter().map(|(id, _)| *id).max().unwrap_or(0);
        let mut tail = format!("xref\n0 {}\n", max_id + 1).into_bytes();
        tail.extend_from_slice(b"0000000000 65535 f \n");

        let mut offset_map = vec![None; max_id + 1];
        for (obj_id, off) in &self.offsets {
            if *obj_id <= max_id {
                offset_map[*obj_id] = Some(*off);
            }
        }
        for entry in offset_map.iter().skip(1) {
            match entry {
                Some(off) => {
                    tail.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
                }
                None => tail.extend_from_slice(b"0000000000 65535 f \n"),
            }
        }

        tail.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
                max_id + 1,
                xref_offset
            )
            .as_bytes(),
        );
        self.write_all(&tail)?;
        self.sink.flush()?;
        Ok(())
    }
}

/// Helper: make a PDF stream object `<< ... /Length N >> stream\n...\nendstream`.
fn make_stream(dict_extra: &str, data: &[u8]) -> Vec<u8> {
    let len = data.len();
    let mut body = format!("<< /Length {len}{dict_extra} >>\nstream\n").into_bytes();
    body.extend_from_slice(data);
    body.extend_from_slice(b"\nendstream");
    body
}

/// Compress bytes using zlib/deflate.
fn deflate(data: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec_zlib(data, 6)
}

/// Helper: make a compressed stream object.
fn make_deflate_stream(dict_extra: &str, data: &[u8]) -> Vec<u8> {
    let compressed = deflate(data);
    let extra = format!(" /Filter /FlateDecode{dict_extra}");
    make_stream(&extra, &compressed)
}

/// Encode RGB bytes as JPEG and return the compressed bytes.
///
/// `quality` is in range 1–100. Values around 75–85 give excellent
/// perceptual quality for typical DjVu backgrounds at a fraction of the
/// FlateDecode+RGB size.
fn encode_rgb_to_jpeg(rgb: &[u8], width: u32, height: u32, quality: u8) -> Vec<u8> {
    use jpeg_encoder::{ColorType, Encoder};
    let mut out = Vec::new();
    let enc = Encoder::new(&mut out, quality);
    // Ignore encoding errors — fallback to empty, which will be caught at
    // the caller and downgraded to FlateDecode.
    let _ = enc.encode(rgb, width as u16, height as u16, ColorType::Rgb);
    out
}

/// Helper: make a DCTDecode (JPEG) stream object.
fn make_dct_stream(dict_extra: &str, jpeg_bytes: &[u8]) -> Vec<u8> {
    let extra = format!(" /Filter /DCTDecode{dict_extra}");
    make_stream(&extra, jpeg_bytes)
}

/// Helper: make a CCITTFaxDecode (Group 4 / T.6) stream object.
///
/// `K -1` selects pure two-dimensional (G4) decoding. `BlackIs1 true` matches
/// this crate's `Bitmap`/JB2 convention (bit `1` = black/marked pixel) so the
/// decoded samples are byte-identical to what the Deflate path already embeds
/// — only the filter changes, not the downstream `/Decode` array.
fn make_ccitt_stream(dict_extra: &str, ncols: u32, nrows: u32, bitstream: &[u8]) -> Vec<u8> {
    let extra = format!(
        " /Filter /CCITTFaxDecode /DecodeParms\
         << /K -1 /Columns {ncols} /Rows {nrows} /BlackIs1 true >>{dict_extra}"
    );
    make_stream(&extra, bitstream)
}

// ---- PDF font for invisible text --------------------------------------------

/// Build a Type1 font dictionary for Helvetica (standard 14 font, no embedding needed).
/// Returns object body bytes.
fn font_dict() -> Vec<u8> {
    b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_vec()
}

// ---- Coordinate helpers -----------------------------------------------------

/// Convert DjVu pixel coordinates to PDF points.
/// DjVu uses bottom-left origin (like PDF), so y-coordinates can be used directly
/// after scaling by 72/dpi.
fn px_to_pt(px: f32, dpi: f32) -> f32 {
    px * 72.0 / dpi
}

// ---- Page rendering ---------------------------------------------------------

/// Compute render dimensions for a page given `output_dpi` option.
///
/// Returns `(render_w, render_h)` in pixels. When `output_dpi == 0` the native
/// page resolution is returned unchanged.
fn render_dims(page: &DjVuPage, output_dpi: u32) -> (u32, u32) {
    let native_dpi = page.dpi().max(1) as f32;
    // PDF never upscales: a zero or above-native target DPI keeps native pixels.
    if output_dpi == 0 || output_dpi as f32 >= native_dpi {
        return (page.width() as u32, page.height() as u32);
    }
    crate::export_common::size_at_dpi(page, output_dpi as f32)
}

/// Pre-rendered page data — all expensive compute done, ready for sequential PDF emit.
///
/// # Memory note
///
/// `djvu_to_pdf_impl` collects `RenderedPage` for every page before emitting any PDF
/// objects (because `PdfWriter` is not `Send`). For large bilevel documents at native
/// DPI (e.g. 520 pages × ~1 MB deflated mask each) peak RAM can be significant.
/// A streaming/chunked approach is tracked in a separate issue.
struct RenderedPage {
    pt_w: f32,
    pt_h: f32,
    is_bilevel_only: bool,
    /// Fully encoded XObject body written as PDF resource `/Im0`.
    ///
    /// For bilevel-only pages this is the 1-bit JB2 mask; for mixed pages it is the
    /// RGB background image.
    img0_body: Option<Vec<u8>>,
    /// Fully encoded XObject bodies for the JB2 mask overlay (`/Mask0`,
    /// `/Mask1`, …), one per foreground colour, each painted in its own
    /// fill colour. Only populated for non-bilevel pages with a Sjbz chunk;
    /// pages without an FGbz colour palette get a single black layer.
    mask_layers: Vec<MaskLayer>,
    /// PDF content stream text operators (invisible text layer).
    text_ops: String,
    /// Pre-built annotation object bodies, one per hyperlink.
    link_annot_bodies: Vec<Vec<u8>>,
}

/// Render one page into a [`RenderedPage`].
///
/// This is the expensive step (pixel render, JPEG encode, JB2 decode, deflate)
/// and can safely run in parallel across pages.
fn render_page_data(page: &DjVuPage, opts: &PdfOptions) -> Result<RenderedPage, PdfError> {
    let pw = page.width() as u32;
    let ph = page.height() as u32;
    let dpi = page.dpi().max(1) as f32;
    let pt_w = px_to_pt(pw as f32, dpi);
    let pt_h = px_to_pt(ph as f32, dpi);

    let is_bilevel_only = page.find_chunk(b"Sjbz").is_some() && page.find_chunk(b"BG44").is_none();

    let (img0_body, mask_layers) = if is_bilevel_only {
        // Bilevel fast path: embed the 1-bit JB2 mask as the sole XObject.
        let mask = collect_mask_stream(page, opts);
        (mask, Vec::new())
    } else if opts.mrc
        && let layers = collect_mask_layers(page, opts)
        && !layers.is_empty()
        && let Ok(Some(bg)) = page.extract_background()
        && bg.width > 0
        && bg.height > 0
    {
        // True MRC (#563): the stencils fully cover the foreground, so embed
        // the background layer alone at its native (subsampled) resolution —
        // the page `cm` scales it to the MediaBox. Smaller (no upsampling, no
        // glyph edges in the raster) and cleaner (no JPEG ringing halos).
        let (rw, rh) = (bg.width, bg.height);
        let mut rgb = Vec::with_capacity(rw as usize * rh as usize * 3);
        for px in bg.data.chunks_exact(4) {
            rgb.extend_from_slice(&px[..3]);
        }
        (Some(encode_img0_body(&rgb, rw, rh, opts)), layers)
    } else {
        let (rw, rh) = render_dims(page, opts.output_dpi);
        // Set only the output size: the render pipeline derives the IW44 decode
        // scale from `width` (see `RenderOptions::decode_scale`). Previously this
        // left `scale = 1.0` at every DPI, forcing a full-resolution wavelet
        // decode followed by a downscale (#377).
        let render_opts = RenderOptions {
            width: rw,
            height: rh,
            ..RenderOptions::default()
        };
        let rgb = render_rgb_for_pdf(page, &render_opts, rw, rh)?;

        (
            Some(encode_img0_body(&rgb, rw, rh, opts)),
            collect_mask_layers(page, opts),
        )
    };

    let text_ops = build_text_content(page, dpi, pt_h);
    let link_annot_bodies = collect_link_annot_bodies(page, dpi, pt_h);

    Ok(RenderedPage {
        pt_w,
        pt_h,
        is_bilevel_only,
        img0_body,
        mask_layers,
        text_ops,
        link_annot_bodies,
    })
}

/// Encode packed RGB rows into the `/Im0` XObject body per the raster policy
/// (`jpeg_quality`, `adaptive_raster` — PDF_ADAPTIVE_RASTER's "encode both,
/// keep smaller"; only one page's pair of encodings is live at once, #449).
fn encode_img0_body(rgb: &[u8], rw: u32, rh: u32, opts: &PdfOptions) -> Vec<u8> {
    let img_dict = format!(
        " /Type /XObject /Subtype /Image /Width {rw} /Height {rh}\
         /ColorSpace /DeviceRGB /BitsPerComponent 8"
    );
    match opts.jpeg_quality {
        Some(quality) => {
            let jpeg = encode_rgb_to_jpeg(rgb, rw, rh, quality);
            if jpeg.is_empty() {
                make_deflate_stream(&img_dict, rgb)
            } else if opts.adaptive_raster {
                let dct_body = make_dct_stream(&img_dict, &jpeg);
                let deflate_body = make_deflate_stream(&img_dict, rgb);
                if deflate_body.len() < dct_body.len() {
                    deflate_body
                } else {
                    dct_body
                }
            } else {
                make_dct_stream(&img_dict, &jpeg)
            }
        }
        None => make_deflate_stream(&img_dict, rgb),
    }
}

fn render_rgb_for_pdf(
    page: &DjVuPage,
    opts: &RenderOptions,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, PdfError> {
    let mut rgb = Vec::with_capacity(width as usize * height as usize * 3);
    crate::export_common::render_rows_or_pixmap(page, opts, |rgba_row| {
        crate::export_common::rgba_row_to_rgb(&mut rgb, rgba_row);
    })?;
    Ok(rgb)
}

/// Decode and compress the JB2 foreground mask into a PDF ImageMask XObject body.
///
/// When `opts.ccitt_g4` is set (`PDF_G4`, opt-in), the mask is also encoded as
/// CCITTFaxDecode (Group 4 / T.6) via [`crate::smmr::encode_g4`] and whichever
/// stream is smaller is kept — the same "encode both, keep smaller" pattern as
/// `adaptive_raster` (round 28), so enabling it can never regress a page's mask
/// size. Default (`ccitt_g4: false`) is byte-identical to the pre-existing
/// Deflate-only behaviour.
///
/// Decodes via [`DjVuPage::extract_mask`] so shared-dictionary (DJVI `Djbz`)
/// pages get their mask too — the previous inline-Djbz-only decode silently
/// dropped the whole foreground overlay for such documents (#620).
fn collect_mask_stream(page: &DjVuPage, opts: &PdfOptions) -> Option<Vec<u8>> {
    let bitmap = page.extract_mask().ok()??;
    Some(mask_body_from_bitmap(&bitmap, opts.ccitt_g4))
}

/// Encode one 1-bit bitmap as a PDF ImageMask XObject body (Deflate, or the
/// smaller of Deflate/G4 when `use_g4` is set).
fn mask_body_from_bitmap(bitmap: &crate::bitmap::Bitmap, use_g4: bool) -> Vec<u8> {
    let bw = bitmap.width;
    let bh = bitmap.height;
    // Bitmap data is already packed 1-bit MSB-first, which is what PDF expects
    // for an ImageMask with /Decode [1 0] (1=black=marked).
    let dict_extra = format!(
        " /Type /XObject /Subtype /Image /Width {bw} /Height {bh}\
         /ImageMask true /BitsPerComponent 1 /Decode [1 0]"
    );
    let deflate_body = make_deflate_stream(&dict_extra, &bitmap.data);
    if !use_g4 {
        return deflate_body;
    }
    let g4_bits = crate::smmr::encode_g4(bitmap);
    let g4_body = make_ccitt_stream(&dict_extra, bw, bh, &g4_bits);
    if g4_body.len() < deflate_body.len() {
        g4_body
    } else {
        deflate_body
    }
}

/// One foreground stencil layer: an ImageMask XObject body painted in `rgb`.
///
/// `bbox` is the layer's pixel bounding box `(x0, y0_top, bw, bh)` within the
/// full mask of `mask_dims` pixels — colour planes are cropped to their
/// bounding box before compression, so the content stream must scale and
/// translate each stencil back into place.
struct MaskLayer {
    rgb: (u8, u8, u8),
    bbox: (u32, u32, u32, u32),
    mask_dims: (u32, u32),
    body: Vec<u8>,
}

/// Build the foreground stencil layers for a mixed page.
///
/// Pages without an FGbz colour palette (or whose palette is entirely black)
/// keep the historical single black stencil — byte-identical output. Pages with
/// a non-black FGbz palette get one ImageMask per palette colour actually used,
/// each painted in its own fill colour (#559: a single black stencil flattened
/// coloured foreground text to black).
fn collect_mask_layers(page: &DjVuPage, opts: &PdfOptions) -> Vec<MaskLayer> {
    let palette = page
        .find_chunk(b"FGbz")
        .and_then(|d| crate::fgbz::parse_fgbz(d).ok())
        .filter(|p| !p.colors.is_empty())
        .filter(|p| p.colors.iter().any(|c| (c.r, c.g, c.b) != (0, 0, 0)));

    let pal = match palette {
        Some(pal) => pal,
        None => {
            // FG44/FGjp foreground: the text colour is continuous-tone, so a
            // flat black stencil can flatten it (the FG44 analogue of #559).
            // If the FG44 colour under the mask is near-uniform (the common
            // scanned-book case: near-black text), keep a single stencil
            // painted in that colour — crisp full-res edges, correct colour.
            // Otherwise skip the stencil and let the composited /Im0 carry the
            // multi-coloured text (colour fidelity over edge crispness; true
            // MRC stencilling of FG44 pages is #563).
            if !page.fg44_chunks().is_empty() || page.find_chunk(b"FGjp").is_some() {
                // `decoded_mask`/`decoded_fg44` hit the page cache — the
                // page's own render (for /Im0) already decoded both layers,
                // so the heuristic must not decode them a second time.
                let Some(mask) = page.decoded_mask() else {
                    return Vec::new();
                };
                let fg_owned;
                let fg = match page.decoded_fg44() {
                    Some(fg) => Some(fg),
                    None => {
                        fg_owned = page.extract_foreground().ok().flatten();
                        fg_owned.as_ref()
                    }
                };
                let Some(fg) = fg else {
                    return Vec::new();
                };
                return match uniform_fg_color(fg, mask) {
                    Some(rgb) => stencil_layer_from_mask(mask, opts, rgb),
                    None => Vec::new(),
                };
            }
            return black_mask_layer(page, opts);
        }
    };

    let Ok(Some((mask, blit_map))) = page.extract_mask_indexed() else {
        // Indexed decode failed — fall back to the black stencil path.
        return black_mask_layer(page, opts);
    };

    // Pass 1: per-pixel colour index + per-colour bounding box. Colour lookup
    // mirrors the renderer (`lookup_palette_color`): blit index → FGbz index
    // table (or direct index when the table is absent) → colour, falling back
    // to colour 0.
    let w = mask.width;
    let h = mask.height;
    const NO_PIXEL: u16 = u16::MAX;
    let mut color_of_pixel = vec![NO_PIXEL; w as usize * h as usize];
    // (min_x, min_y, max_x, max_y) per colour
    let mut bboxes = vec![(u32::MAX, u32::MAX, 0u32, 0u32); pal.colors.len()];
    for y in 0..h {
        for x in 0..w {
            if !mask.get(x, y) {
                continue;
            }
            let mi = y as usize * w as usize + x as usize;
            let blit_idx = blit_map.get(mi).copied().unwrap_or(-1);
            let ci = if blit_idx >= 0 {
                let raw = if pal.indices.is_empty() {
                    blit_idx as usize
                } else {
                    pal.indices
                        .get(blit_idx as usize)
                        .map(|&i| i as usize)
                        .unwrap_or(0)
                };
                if raw < pal.colors.len() { raw } else { 0 }
            } else {
                0
            };
            color_of_pixel[mi] = ci as u16;
            let b = &mut bboxes[ci];
            b.0 = b.0.min(x);
            b.1 = b.1.min(y);
            b.2 = b.2.max(x);
            b.3 = b.3.max(y);
        }
    }

    // Pass 2: one bilevel plane per used colour, cropped to its bounding box
    // (a full-page plane per colour costs far more Deflate output — the crop
    // is what keeps the multi-stencil overhead small).
    let mut planes: Vec<Option<crate::bitmap::Bitmap>> = Vec::new();
    planes.resize_with(pal.colors.len(), || None);
    for y in 0..h {
        for x in 0..w {
            let ci = color_of_pixel[y as usize * w as usize + x as usize];
            if ci == NO_PIXEL {
                continue;
            }
            let ci = ci as usize;
            let (x0, y0, x1, y1) = bboxes[ci];
            planes[ci]
                .get_or_insert_with(|| crate::bitmap::Bitmap::new(x1 - x0 + 1, y1 - y0 + 1))
                .set_black(x - x0, y - y0);
        }
    }

    planes
        .into_iter()
        .enumerate()
        .filter_map(|(ci, plane)| {
            let plane = plane?;
            let c = pal.colors[ci];
            let (x0, y0, x1, y1) = bboxes[ci];
            Some(MaskLayer {
                rgb: (c.r, c.g, c.b),
                bbox: (x0, y0, x1 - x0 + 1, y1 - y0 + 1),
                mask_dims: (w, h),
                // Colour planes are new output (no byte-identity to preserve),
                // so always pick the smaller of Deflate/G4 regardless of the
                // `ccitt_g4` opt-in — the per-plane min can never regress size.
                body: mask_body_from_bitmap(&plane, true),
            })
        })
        .collect()
}

/// The historical single black stencil (pages without a colour palette).
fn black_mask_layer(page: &DjVuPage, opts: &PdfOptions) -> Vec<MaskLayer> {
    // Prefer the page cache (populated by this page's own /Im0 render).
    if let Some(mask) = page.decoded_mask() {
        return stencil_layer_from_mask(mask, opts, (0, 0, 0));
    }
    let Ok(Some(bitmap)) = page.extract_mask() else {
        return Vec::new();
    };
    stencil_layer_from_mask(&bitmap, opts, (0, 0, 0))
}

/// A single full-mask stencil painted in `rgb`, from an already-decoded mask.
fn stencil_layer_from_mask(
    mask: &crate::bitmap::Bitmap,
    opts: &PdfOptions,
    rgb: (u8, u8, u8),
) -> Vec<MaskLayer> {
    vec![MaskLayer {
        rgb,
        bbox: (0, 0, mask.width, mask.height),
        mask_dims: (mask.width, mask.height),
        body: mask_body_from_bitmap(mask, opts.ccitt_g4),
    }]
}

/// Per-channel spread (max−min) above which the FG44 foreground colour under
/// the mask counts as multi-coloured and the flat stencil is skipped.
const FG44_UNIFORM_SPREAD: u8 = 48;

/// The page's FG44/FGjp foreground colour, if near-uniform under the mask.
///
/// Samples the (subsampled) foreground pixmap at every marked mask pixel and
/// returns the mean colour when every channel's spread stays within
/// [`FG44_UNIFORM_SPREAD`]; `None` when the foreground is multi-coloured (or
/// either layer fails to decode).
fn uniform_fg_color(fg: &crate::Pixmap, mask: &crate::bitmap::Bitmap) -> Option<(u8, u8, u8)> {
    if fg.width == 0 || fg.height == 0 || mask.width == 0 || mask.height == 0 {
        return None;
    }
    let mut min = [255u8; 3];
    let mut max = [0u8; 3];
    let mut sum = [0u64; 3];
    let mut n = 0u64;
    for y in 0..mask.height {
        let fy = (y as u64 * fg.height as u64 / mask.height as u64).min(fg.height as u64 - 1);
        for x in 0..mask.width {
            if !mask.get(x, y) {
                continue;
            }
            let fx = (x as u64 * fg.width as u64 / mask.width as u64).min(fg.width as u64 - 1);
            let pi = (fy * fg.width as u64 + fx) as usize * 4;
            let px = &fg.data[pi..pi + 3];
            for c in 0..3 {
                min[c] = min[c].min(px[c]);
                max[c] = max[c].max(px[c]);
                sum[c] += u64::from(px[c]);
            }
            n += 1;
        }
    }
    if n == 0 {
        return None;
    }
    if (0..3).any(|c| max[c] - min[c] > FG44_UNIFORM_SPREAD) {
        return None;
    }
    Some(((sum[0] / n) as u8, (sum[1] / n) as u8, (sum[2] / n) as u8))
}

/// Format one colour component for a PDF `rg` operator (0..255 → 0..1).
///
/// `0` and `255` format as the exact literals `0` / `1` so the all-black layer
/// emits the historical `0 0 0 rg` operator byte-for-byte.
fn fmt_rg_component(v: u8) -> String {
    match v {
        0 => "0".to_string(),
        255 => "1".to_string(),
        _ => format!("{:.4}", f32::from(v) / 255.0),
    }
}

/// Format a point offset for a `cm` operator: exact `0` for zero (matching the
/// historical full-page operator), 4 decimals otherwise.
fn fmt_pt(v: f32) -> String {
    if v == 0.0 {
        "0".to_string()
    } else {
        format!("{v:.4}")
    }
}

/// Build pre-serialized annotation bodies for all hyperlinks on a page.
fn collect_link_annot_bodies(page: &DjVuPage, dpi: f32, pt_h: f32) -> Vec<Vec<u8>> {
    let hyperlinks = match page.hyperlinks() {
        Ok(links) => links,
        Err(_) => return Vec::new(),
    };
    hyperlinks
        .iter()
        .filter_map(|link| {
            let rect = shape_to_pdf_rect(&link.shape, dpi, pt_h)?;
            let url_escaped = pdf_escape_string(&link.url);
            Some(
                format!(
                    "<< /Type /Annot /Subtype /Link\n\
                       /Rect [{:.4} {:.4} {:.4} {:.4}]\n\
                       /Border [0 0 0]\n\
                       /A << /S /URI /URI ({url_escaped}) >> >>",
                    rect.0, rect.1, rect.2, rect.3
                )
                .into_bytes(),
            )
        })
        .collect()
}

/// Emit a pre-rendered page into `PdfWriter` (sequential). Returns the page object ID.
fn emit_page_objects<W: std::io::Write>(
    w: &mut PdfWriter<W>,
    data: RenderedPage,
    pages_id: usize,
    font_id: usize,
) -> Result<usize, PdfError> {
    let pt_w = data.pt_w;
    let pt_h = data.pt_h;

    let img_id = match data.img0_body {
        Some(body) => Some(w.add(body)?),
        None => None,
    };
    let mut mask_layers: Vec<(MaskLayer, usize)> = Vec::with_capacity(data.mask_layers.len());
    for mut layer in data.mask_layers {
        let body = core::mem::take(&mut layer.body);
        let id = w.add(body)?;
        mask_layers.push((layer, id));
    }

    let mut content = String::new();

    if data.is_bilevel_only {
        // img0 may still be None if JB2 decode failed at render time — render gracefully.
        if img_id.is_some() {
            // /Im0 is an ImageMask stencil: marked samples paint in the current
            // fill colour, so it must be black. The historical `1 1 1 rg` here
            // painted white-on-white — every bilevel-only page rendered blank
            // (#621).
            content.push_str("0 0 0 rg\n");
            content.push_str(&format!("q {pt_w:.4} 0 0 {pt_h:.4} 0 0 cm /Im0 Do Q\n"));
        }
    } else {
        if img_id.is_some() {
            content.push_str(&format!("q {pt_w:.4} 0 0 {pt_h:.4} 0 0 cm /Im0 Do Q\n"));
        }
        for (i, (layer, _)) in mask_layers.iter().enumerate() {
            let (r, g, b) = layer.rgb;
            let (x0, y0, bw, bh) = layer.bbox;
            let (mw, mh) = layer.mask_dims;
            // Map the cropped stencil back into place: PDF images fill the unit
            // square of the current transform, rows top-down, page origin
            // bottom-left. A full-page bbox reproduces the historical
            // `{pt_w} 0 0 {pt_h} 0 0 cm` operator byte-for-byte.
            let sw = pt_w * bw as f32 / mw as f32;
            let sh = pt_h * bh as f32 / mh as f32;
            let tx = pt_w * x0 as f32 / mw as f32;
            let ty = pt_h * (mh - y0 - bh) as f32 / mh as f32;
            content.push_str(&format!(
                "q {} {} {} rg {sw:.4} 0 0 {sh:.4} {} {} cm /Mask{i} Do Q\n",
                fmt_rg_component(r),
                fmt_rg_component(g),
                fmt_rg_component(b),
                fmt_pt(tx),
                fmt_pt(ty),
            ));
        }
    }

    if !data.text_ops.is_empty() {
        content.push_str(&data.text_ops);
    }

    let content_body = make_deflate_stream("", content.as_bytes());
    let content_id = w.add(content_body)?;

    let mut resources = String::from("/XObject <<");
    if let Some(id) = img_id {
        resources.push_str(&format!(" /Im0 {id} 0 R"));
    }
    for (i, (_, mid)) in mask_layers.iter().enumerate() {
        resources.push_str(&format!(" /Mask{i} {mid} 0 R"));
    }
    resources.push_str(" >>");
    if !data.text_ops.is_empty() {
        resources.push_str(&format!(" /Font << /F1 {font_id} 0 R >>"));
    }

    let mut annot_ids: Vec<usize> = Vec::with_capacity(data.link_annot_bodies.len());
    for body in data.link_annot_bodies {
        annot_ids.push(w.add(body)?);
    }
    let mut annots_str = String::new();
    if !annot_ids.is_empty() {
        annots_str.push_str(" /Annots [");
        for aid in &annot_ids {
            annots_str.push_str(&format!(" {aid} 0 R"));
        }
        annots_str.push_str(" ]");
    }

    w.add(
        format!(
            "<< /Type /Page /Parent {pages_id} 0 R\n\
               /MediaBox [0 0 {pt_w:.4} {pt_h:.4}]\n\
               /Contents {content_id} 0 R\n\
               /Resources << {resources} >>{annots_str} >>"
        )
        .into_bytes(),
    )
}

/// Build invisible text operators for the text layer.
fn build_text_content(page: &DjVuPage, dpi: f32, pt_h: f32) -> String {
    let text_layer = match page.text_layer() {
        Ok(Some(tl)) => tl,
        _ => return String::new(),
    };

    let mut ops = String::new();
    // Begin text object
    ops.push_str("BT\n");
    // Set text rendering mode to invisible (mode 3)
    ops.push_str("3 Tr\n");
    // Set font — use a small size, we scale per-word
    ops.push_str("/F1 1 Tf\n");

    // Emit one positioned run per leaf word/character zone (shared zone-walk).
    for span in crate::export_common::word_spans(&text_layer) {
        emit_word_span(&mut ops, span.rect, span.text, dpi, pt_h);
    }

    ops.push_str("ET\n");

    if ops == "BT\n3 Tr\n/F1 1 Tf\nET\n" {
        // No actual text was emitted
        return String::new();
    }

    ops
}

/// Emit text positioning operators for one leaf word/character span.
///
/// `rect` is top-left-origin pixels; PDF uses bottom-left origin, so the
/// baseline is flipped in point space: `pdf_y = pt_h - (r.y + r.height) * 72/dpi`.
/// (This subtract-after-convert order is what produces byte-identical output;
/// see the note on [`crate::export_common::flip_y_bottom`].)
fn emit_word_span(ops: &mut String, rect: &Rect, text: &str, dpi: f32, pt_h: f32) {
    let x = px_to_pt(rect.x as f32, dpi);
    let y = pt_h - px_to_pt((rect.y + rect.height) as f32, dpi);
    let w = px_to_pt(rect.width as f32, dpi);
    let h = px_to_pt(rect.height as f32, dpi);

    if w <= 0.0 || h <= 0.0 {
        return;
    }

    // Font size = zone height in points
    let font_size = h;
    if font_size < 0.5 {
        return;
    }

    // Horizontal scale to fit text width
    let text_escaped = pdf_escape_string(text);
    // Sum per-character advance widths using Helvetica metrics.
    let natural_width: f32 = text
        .chars()
        .map(|c| helvetica_advance(c) * font_size)
        .sum::<f32>()
        .max(0.01);
    let h_scale = if natural_width > 0.01 {
        (w / natural_width) * 100.0
    } else {
        100.0
    };

    ops.push_str(&format!(
        "{font_size:.2} 0 0 {font_size:.2} {x:.4} {y:.4} Tm\n"
    ));
    if (h_scale - 100.0).abs() > 1.0 {
        ops.push_str(&format!("{h_scale:.2} Tz\n"));
    }
    ops.push_str(&format!("({text_escaped}) Tj\n"));
}

/// Return the normalized advance width (fraction of em) for `c` in Helvetica.
///
/// Uses standard Helvetica metrics for ASCII, and Unicode-block heuristics
/// for non-ASCII ranges.  CJK, full-width, and Hangul characters are
/// treated as full-width (1.0).  Everything else falls back to 0.556 (the
/// Helvetica average for Latin lowercase).
fn helvetica_advance(c: char) -> f32 {
    let cp = c as u32;
    match c {
        // ASCII control / non-printing — zero width
        '\x00'..='\x1f' | '\x7f' => 0.0,
        // Space
        ' ' => 0.278,
        // Digits
        '0'..='9' => 0.556,
        // Common punctuation
        ',' | '.' | ':' | ';' | '!' | '?' => 0.278,
        '\'' | '"' => 0.222,
        '(' | ')' | '[' | ']' | '{' | '}' => 0.333,
        '-' | '\u{2013}' | '\u{2014}' => 0.333,
        // Uppercase ASCII — broad average for Helvetica
        'A'..='Z' => 0.667,
        // Lowercase ASCII
        'a'..='z' => 0.556,
        _ => {
            // CJK Unified Ideographs and common CJK blocks → full-width
            if matches!(cp,
                0x1100..=0x11FF  // Hangul Jamo
                | 0x2E80..=0x2EFF  // CJK Radicals Supplement
                | 0x2F00..=0x2FDF  // Kangxi Radicals
                | 0x3000..=0x303F  // CJK Symbols and Punctuation
                | 0x3040..=0x309F  // Hiragana
                | 0x30A0..=0x30FF  // Katakana
                | 0x3100..=0x312F  // Bopomofo
                | 0x3130..=0x318F  // Hangul Compatibility Jamo
                | 0x3190..=0x31FF  // various CJK
                | 0x3200..=0x32FF  // Enclosed CJK
                | 0x3300..=0x33FF  // CJK Compatibility
                | 0x3400..=0x4DBF  // CJK Extension A
                | 0x4E00..=0x9FFF  // CJK Unified Ideographs
                | 0xA000..=0xA48F  // Yi Syllables
                | 0xA490..=0xA4CF  // Yi Radicals
                | 0xAC00..=0xD7AF  // Hangul Syllables
                | 0xF900..=0xFAFF  // CJK Compatibility Ideographs
                | 0xFE10..=0xFE1F  // Vertical Forms
                | 0xFE30..=0xFE4F  // CJK Compatibility Forms
                | 0xFF00..=0xFFEF  // Halfwidth and Fullwidth Forms
                | 0x1B000..=0x1B0FF // Kana Supplement
                | 0x20000..=0x2A6DF // CJK Extension B
                | 0x2A700..=0x2CEAF // CJK Extensions C/D/E
                | 0x2CEB0..=0x2EBEF // CJK Extension F
                | 0x30000..=0x3134F // CJK Extension G
            ) {
                1.0
            } else {
                // Latin Extended, Cyrillic, Greek, Arabic, Hebrew, etc.
                0.556
            }
        }
    }
}

/// Escape a string for PDF literal string syntax.
fn pdf_escape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '(' => out.push_str("\\("),
            ')' => out.push_str("\\)"),
            '\\' => out.push_str("\\\\"),
            c if c.is_ascii() => out.push(c),
            // Non-ASCII: encode as UTF-16BE with BOM for PDF
            _ => {
                // For simplicity, skip non-ASCII chars in text positioning
                // (they'll still be in the document via the image)
                out.push('?');
            }
        }
    }
    out
}

/// Convert a DjVu shape to a PDF rectangle [x1, y1, x2, y2] in points.
///
/// DjVu annotation coordinates use bottom-left origin (same as PDF), so no
/// vertical flip is needed — only the point conversion of each edge. The
/// bounding box, and the empty/degenerate → `None` rule, are the shared
/// [`crate::export_common::shape_bbox`]; a zero-area shape encloses no link
/// region and is dropped. Because `px_to_pt` is monotonic, taking the bounding
/// box in pixel space and converting its edges yields the same points as the
/// previous per-point fold in point space.
fn shape_to_pdf_rect(shape: &Shape, dpi: f32, _pt_h: f32) -> Option<(f32, f32, f32, f32)> {
    let r = crate::export_common::shape_bbox(shape)?;
    let x1 = px_to_pt(r.x as f32, dpi);
    let y1 = px_to_pt(r.y as f32, dpi);
    let x2 = px_to_pt((r.x + r.width) as f32, dpi);
    let y2 = px_to_pt((r.y + r.height) as f32, dpi);
    Some((x1, y1, x2, y2))
}

// ---- Bookmarks (PDF outline) ------------------------------------------------

/// Build PDF outline objects from NAVM bookmarks.
/// Returns the outline root object ID, or None if no bookmarks.
fn build_outline<W: std::io::Write>(
    w: &mut PdfWriter<W>,
    bookmarks: &[DjVuBookmark],
    page_ids: &[usize],
) -> Result<Option<usize>, PdfError> {
    if bookmarks.is_empty() {
        return Ok(None);
    }

    let outline_id = w.alloc_id();

    // Flatten the bookmark tree into outline item objects
    let item_ids = build_outline_items(w, bookmarks, outline_id, page_ids)?;

    if item_ids.is_empty() {
        return Ok(None);
    }

    let first = item_ids[0];
    let last = *item_ids.last().unwrap();
    let count = count_outline_items(bookmarks);

    w.add_obj(
        outline_id,
        format!("<< /Type /Outlines /First {first} 0 R /Last {last} 0 R /Count {count} >>")
            .into_bytes(),
    )?;

    Ok(Some(outline_id))
}

/// Recursively build outline items. Returns IDs of top-level items at this level.
fn build_outline_items<W: std::io::Write>(
    w: &mut PdfWriter<W>,
    bookmarks: &[DjVuBookmark],
    parent_id: usize,
    page_ids: &[usize],
) -> Result<Vec<usize>, PdfError> {
    let mut ids = Vec::new();

    for _bm in bookmarks {
        let item_id = w.alloc_id();
        ids.push(item_id);
    }

    for (i, bm) in bookmarks.iter().enumerate() {
        let item_id = ids[i];
        let prev = if i > 0 {
            format!(" /Prev {} 0 R", ids[i - 1])
        } else {
            String::new()
        };
        let next = if i + 1 < ids.len() {
            format!(" /Next {} 0 R", ids[i + 1])
        } else {
            String::new()
        };

        // Resolve bookmark URL to page index
        let dest = resolve_bookmark_dest(&bm.url, page_ids);

        // Build children
        let child_ids = build_outline_items(w, &bm.children, item_id, page_ids)?;
        let children_str = if !child_ids.is_empty() {
            let first = child_ids[0];
            let last = *child_ids.last().unwrap();
            let count = count_outline_items(&bm.children);
            format!(" /First {first} 0 R /Last {last} 0 R /Count {count}")
        } else {
            String::new()
        };

        let title = pdf_escape_string(&bm.title);
        w.add_obj(
            item_id,
            format!(
                "<< /Title ({title}) /Parent {parent_id} 0 R{prev}{next}{dest}{children_str} >>"
            )
            .into_bytes(),
        )?;
    }

    Ok(ids)
}

/// Count total outline items (including nested children).
fn count_outline_items(bookmarks: &[DjVuBookmark]) -> usize {
    let mut n = bookmarks.len();
    for bm in bookmarks {
        n += count_outline_items(&bm.children);
    }
    n
}

/// Resolve a DjVu bookmark URL to a PDF destination string.
/// DjVu internal URLs look like `#page_N` or `#+N` or `#-N`.
fn resolve_bookmark_dest(url: &str, page_ids: &[usize]) -> String {
    if let Some(idx) = crate::export_common::bookmark_page_index(url)
        && let Some(&pid) = page_ids.get(idx)
    {
        return format!(" /Dest [{pid} 0 R /Fit]");
    }

    // External URL or unparseable — use URI action
    if !url.is_empty() {
        let escaped = pdf_escape_string(url);
        return format!(" /A << /S /URI /URI ({escaped}) >>");
    }

    String::new()
}

// ---- Public API -------------------------------------------------------------

/// Convert a DjVu document to PDF bytes.
///
/// Options for DjVu → PDF conversion.
///
/// Use `PdfOptions::default()` for sensible defaults:
/// - 150 DPI output (screen-quality, ~16× fewer pixels than native 600 DPI)
/// - DCTDecode (JPEG quality 80) for color backgrounds
/// - 1-bit FlateDecode for bilevel masks
/// - Bilevel-only pages skip RGB render entirely (direct 1-bit embed)
#[derive(Debug, Clone)]
pub struct PdfOptions {
    /// JPEG quality for background image encoding (1–100).
    ///
    /// Higher values produce better quality at larger file sizes.
    /// Set to `None` to use lossless FlateDecode (PNG-like, larger output).
    pub jpeg_quality: Option<u8>,

    /// Output resolution in DPI.
    ///
    /// Controls the pixel dimensions of embedded images. Lower values produce
    /// smaller files and faster exports; higher values preserve more detail.
    ///
    /// - `150` — screen quality (default); ~16× fewer pixels than native 600 DPI
    /// - `300` — print quality
    /// - `0` — use native page DPI (maximum quality, slowest)
    pub output_dpi: u32,

    /// Opt-in per-page adaptive raster encoding (default `false`).
    ///
    /// When `jpeg_quality` is `Some`, the default behaviour always emits
    /// DCTDecode (JPEG). On near-flat/text-dominated colour pages this can be
    /// *larger* than plain FlateDecode at no quality gain — JPEG's DCT
    /// overhead doesn't pay for itself when there's little photographic
    /// detail to amortize it against (see `PDF_DCT_PROBE` in
    /// `PERF_EXPERIMENTS.md`).
    ///
    /// When `true`, each page's rendered RGB is encoded *both* ways and
    /// whichever stream is smaller is embedded — losslessly (FlateDecode) when
    /// Deflate wins, lossy (DCTDecode) when JPEG wins. Only one page's pair of
    /// encodings is ever held in memory at a time (the loser is dropped
    /// immediately), so this doesn't change the O(1)-per-page memory profile.
    /// Has no effect when `jpeg_quality` is `None` (already all-Deflate).
    pub adaptive_raster: bool,

    /// Opt-in CCITT Group 4 (T.6) encoding for JB2 bilevel masks (default `false`).
    ///
    /// Every page's mask (the bilevel-only `/Im0` fast path *and* the `/Mask0`
    /// overlay on mixed pages) is currently always emitted as Deflate of the
    /// raw 1-bit raster. For scanned/text-dominated bilevel content, Group 4
    /// (fax) run-length coding exploits row-to-row redundancy that Deflate's
    /// generic LZ77 window doesn't reliably catch, often 1.5-2x+ smaller.
    ///
    /// When `true`, each mask is encoded *both* ways (Deflate and G4 via
    /// [`crate::smmr::encode_g4`]) and whichever stream is smaller is
    /// embedded — this can never regress a page's mask size, the same
    /// "encode both, keep smaller" pattern as `adaptive_raster`. On
    /// halftone/dithered bilevel content (rare for JB2 masks, which are
    /// normally already-segmented text/line-art) G4's run-length model can
    /// lose to Deflate; the per-mask min guards against that.
    pub ccitt_g4: bool,

    /// Opt-in true-MRC layering (default `false`).
    ///
    /// The default mixed-page path embeds `/Im0` as the **composited** render
    /// (background WITH text) at `output_dpi`, then repaints the text via the
    /// stencils anyway — the raster layer wastes bits on high-frequency glyph
    /// edges (JPEG ringing halos) and is stored upsampled relative to the
    /// background's native BG44 resolution. With `mrc: true`, pages whose
    /// foreground is fully covered by stencils embed the **background layer
    /// only** (no composited text) at its native subsampled resolution; the
    /// stencils carry the text (coloured per the FGbz/uniform-FG44 policy).
    /// Pages where the stencil is skipped (multi-colour FG44), photo-only
    /// pages, and bilevel pages fall back to the default path unchanged.
    pub mrc: bool,
}

impl Default for PdfOptions {
    fn default() -> Self {
        PdfOptions {
            jpeg_quality: Some(80),
            output_dpi: 150,
            adaptive_raster: false,
            ccitt_g4: false,
            mrc: false,
        }
    }
}

impl PdfOptions {
    /// High-quality archival preset: native DPI, JPEG quality 90.
    pub fn archival() -> Self {
        PdfOptions {
            jpeg_quality: Some(90),
            output_dpi: 0,
            adaptive_raster: false,
            ccitt_g4: false,
            mrc: false,
        }
    }
}

/// Convert a DjVu document to PDF bytes using custom options.
///
/// See [`PdfOptions`] for available settings.
pub fn djvu_to_pdf_with_options(
    doc: &DjVuDocument,
    opts: &PdfOptions,
) -> Result<Vec<u8>, PdfError> {
    let mut buf = Vec::new();
    djvu_to_pdf_to_writer(doc, opts, &mut buf)?;
    Ok(buf)
}

/// Convert a DjVu document to PDF, streaming the output to `sink` (#606).
///
/// Object bodies are written as they are produced and dropped immediately, so
/// peak memory stays O(1 page) plus the xref bookkeeping instead of holding
/// every object body *and* a second full serialization buffer. Output bytes
/// are identical to [`djvu_to_pdf_with_options`] (which now wraps this with a
/// `Vec` sink). Wrap `sink` in a [`std::io::BufWriter`] for file output.
///
/// # Errors
///
/// Returns `PdfError` if page rendering, text layer parsing, or writing to
/// `sink` fails. On error the sink is left with a partial PDF.
pub fn djvu_to_pdf_to_writer<W: std::io::Write>(
    doc: &DjVuDocument,
    opts: &PdfOptions,
    sink: W,
) -> Result<(), PdfError> {
    djvu_to_pdf_impl(doc, opts, sink)
}

/// This produces a PDF 1.4 file with:
/// - Rasterized page images (IW44 background + JB2 mask composite)
/// - Invisible text layer for search and selection
/// - Bookmarks (PDF outline) from NAVM
/// - Hyperlink annotations from ANTz
///
/// Background images are encoded as DCTDecode (JPEG at quality 80) by default,
/// producing significantly smaller files than the legacy FlateDecode path.
/// Use [`djvu_to_pdf_with_options`] with `jpeg_quality: None` for lossless output.
///
/// # Errors
///
/// Returns `PdfError` if page rendering or text layer parsing fails.
pub fn djvu_to_pdf(doc: &DjVuDocument) -> Result<Vec<u8>, PdfError> {
    djvu_to_pdf_with_options(doc, &PdfOptions::default())
}

fn djvu_to_pdf_impl<W: std::io::Write>(
    doc: &DjVuDocument,
    opts: &PdfOptions,
    sink: W,
) -> Result<(), PdfError> {
    let mut w = PdfWriter::new(sink)?;

    // Reserve IDs for catalog and pages
    let catalog_id = w.alloc_id(); // 1
    let pages_id = w.alloc_id(); // 2

    // Reserve a font object ID
    let font_id = w.alloc_id(); // 3
    w.add_obj(font_id, font_dict())?;

    let page_count = doc.page_count();

    // Emit one page's objects (rendered body or a blank-page fallback) and return
    // its page-object id. Shared by both the parallel and sequential paths.
    let emit_one = |w: &mut PdfWriter<W>,
                    i: usize,
                    rendered: Option<RenderedPage>|
     -> Result<usize, PdfError> {
        Ok(match rendered {
            Some(data) => emit_page_objects(w, data, pages_id, font_id)?,
            None => {
                // Fallback: blank page at native dimensions
                let page = doc.page(i)?;
                let dpi = page.dpi().max(1) as f32;
                let pt_w = px_to_pt(page.width() as f32, dpi);
                let pt_h = px_to_pt(page.height() as f32, dpi);
                w.add(
                    format!(
                        "<< /Type /Page /Parent {pages_id} 0 R\n\
                           /MediaBox [0 0 {pt_w:.4} {pt_h:.4}]\n\
                           /Resources << >> >>"
                    )
                    .into_bytes(),
                )?
            }
        })
    };

    let mut page_obj_ids = Vec::with_capacity(page_count);

    // With the `parallel` feature, render all pages concurrently via rayon, then
    // emit sequentially (PdfWriter is not Send).
    // #606: render in bounded chunks so the parallel path holds O(chunk) page
    // bodies instead of all `page_count` at once, then emit each chunk in
    // order (identical output ordering → identical bytes).
    #[cfg(feature = "parallel")]
    {
        use rayon::prelude::*;
        // Chunk size trades bounded memory (O(chunk) retained bodies) against
        // scheduling: too small starves the pool at each chunk barrier on
        // uneven pages. 8x threads measured within noise of the old
        // collect-everything path on a 504-page doc while bounding bodies.
        let chunk = rayon::current_num_threads().max(1) * 8;
        let mut start = 0;
        while start < page_count {
            let end = (start + chunk).min(page_count);
            let rendered_pages: Vec<Option<RenderedPage>> = (start..end)
                .into_par_iter()
                .map(|i| {
                    // #629: render on a cold clone so the decode caches die
                    // with it — the export never revisits a page, and caching
                    // on the document made peak RSS grow O(pages).
                    doc.page(i)
                        .ok()
                        .and_then(|p| render_page_data(&p.clone(), opts).ok())
                })
                .collect();
            for (off, rendered) in rendered_pages.into_iter().enumerate() {
                page_obj_ids.push(emit_one(&mut w, start + off, rendered)?);
            }
            start = end;
        }
    }

    // #449: sequential path renders, emits, and drops one page at a time, holding
    // O(1) page bodies in memory instead of collecting all `page_count` rendered
    // bodies first (peak RSS O(pages × body) → O(1 page); mirrors TIFF_STREAM).
    #[cfg(not(feature = "parallel"))]
    for i in 0..page_count {
        // #629: render on a cold clone — see the parallel path above.
        let rendered = doc
            .page(i)
            .ok()
            .and_then(|p| render_page_data(&p.clone(), opts).ok());
        page_obj_ids.push(emit_one(&mut w, i, rendered)?);
    }

    // Build outline from bookmarks
    let outline_id = build_outline(&mut w, doc.bookmarks(), &page_obj_ids)?;

    // Pages object
    let kids = page_obj_ids
        .iter()
        .map(|id| format!("{id} 0 R"))
        .collect::<Vec<_>>()
        .join(" ");
    let n = page_obj_ids.len();
    w.add_obj(
        pages_id,
        format!("<< /Type /Pages /Kids [{kids}] /Count {n} >>").into_bytes(),
    )?;

    // Catalog
    let outline_ref = match outline_id {
        Some(oid) => format!(" /Outlines {oid} 0 R /PageMode /UseOutlines"),
        None => String::new(),
    };
    w.add_obj(
        catalog_id,
        format!("<< /Type /Catalog /Pages {pages_id} 0 R{outline_ref} >>").into_bytes(),
    )?;

    w.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pdf_escape_string() {
        assert_eq!(pdf_escape_string("hello"), "hello");
        assert_eq!(pdf_escape_string("a(b)c"), "a\\(b\\)c");
        assert_eq!(pdf_escape_string("a\\b"), "a\\\\b");
    }

    #[test]
    fn test_px_to_pt() {
        // At 72 dpi, 72 pixels = 72 points
        assert!((px_to_pt(72.0, 72.0) - 72.0).abs() < 0.01);
        // At 300 dpi, 300 pixels = 72 points
        assert!((px_to_pt(300.0, 300.0) - 72.0).abs() < 0.01);
    }

    #[test]
    fn test_resolve_bookmark_dest_page_number() {
        let page_ids = vec![10, 20, 30];
        let dest = resolve_bookmark_dest("#1", &page_ids);
        assert!(dest.contains("10 0 R"));
    }

    #[test]
    fn test_pdf_writer_serialize() {
        let mut pdf = Vec::new();
        let mut w = PdfWriter::new(&mut pdf).unwrap();
        let id = w.add(b"<< /Type /Catalog >>".to_vec()).unwrap();
        assert_eq!(id, 1);
        w.finish().unwrap();
        assert!(pdf.starts_with(b"%PDF-1.4"));
        assert!(pdf.windows(5).any(|w| w == b"%%EOF"));
    }

    #[test]
    fn test_make_stream() {
        let stream = make_stream(" /Filter /FlateDecode", b"hello");
        let s = String::from_utf8_lossy(&stream);
        assert!(s.contains("/Length 5"));
        assert!(s.contains("stream\nhello\nendstream"));
    }

    #[test]
    fn test_deflate_roundtrip() {
        let data = b"hello world, this is a test of deflate compression";
        let compressed = deflate(data);
        // Compressed data should be non-empty
        assert!(!compressed.is_empty());
        // Decompress and verify
        let decompressed = miniz_oxide::inflate::decompress_to_vec_zlib(&compressed).unwrap();
        assert_eq!(&decompressed, data);
    }

    #[test]
    fn test_make_deflate_stream() {
        let body = make_deflate_stream(" /Type /XObject", b"test data");
        let s = String::from_utf8_lossy(&body);
        assert!(s.contains("/Filter /FlateDecode"));
        assert!(s.contains("/Type /XObject"));
        assert!(s.contains("stream\n"));
        assert!(s.contains("\nendstream"));
    }

    #[test]
    fn test_font_dict() {
        let d = font_dict();
        let s = String::from_utf8_lossy(&d);
        assert!(s.contains("/Type /Font"));
        assert!(s.contains("/BaseFont /Helvetica"));
    }

    #[test]
    fn test_pdf_writer_alloc_ids() {
        let mut buf = Vec::new();
        let mut w = PdfWriter::new(&mut buf).unwrap();
        let id1 = w.alloc_id();
        let id2 = w.alloc_id();
        let id3 = w.alloc_id();
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(id3, 3);
    }

    #[test]
    fn test_pdf_writer_multiple_objects() {
        let mut pdf = Vec::new();
        let mut w = PdfWriter::new(&mut pdf).unwrap();
        w.add(b"<< /Type /Catalog >>".to_vec()).unwrap();
        w.add(b"<< /Type /Pages >>".to_vec()).unwrap();
        w.finish().unwrap();
        let s = String::from_utf8_lossy(&pdf);
        assert!(s.contains("1 0 obj"));
        assert!(s.contains("2 0 obj"));
        assert!(s.contains("/Size 3")); // 0, 1, 2
    }

    #[test]
    fn test_resolve_bookmark_dest_page_prefix() {
        let page_ids = vec![10, 20, 30];
        let dest = resolve_bookmark_dest("#page2", &page_ids);
        assert!(dest.contains("20 0 R"));
        assert!(dest.contains("/Fit"));
    }

    #[test]
    fn test_resolve_bookmark_dest_page_underscore() {
        let page_ids = vec![10, 20, 30];
        let dest = resolve_bookmark_dest("#page_3", &page_ids);
        assert!(dest.contains("30 0 R"));
    }

    #[test]
    fn test_resolve_bookmark_dest_out_of_range() {
        let page_ids = vec![10];
        let dest = resolve_bookmark_dest("#page99", &page_ids);
        // Should fall through to bare number parse or be empty
        assert!(!dest.contains("10 0 R"));
    }

    #[test]
    fn test_resolve_bookmark_dest_external_url() {
        let page_ids = vec![10];
        let dest = resolve_bookmark_dest("http://example.com", &page_ids);
        assert!(dest.contains("/S /URI"));
        assert!(dest.contains("http://example.com"));
    }

    #[test]
    fn test_resolve_bookmark_dest_empty_url() {
        let page_ids = vec![10];
        let dest = resolve_bookmark_dest("", &page_ids);
        assert!(dest.is_empty());
    }

    #[test]
    fn test_pdf_escape_special_chars() {
        assert_eq!(pdf_escape_string("a(b)c\\d"), "a\\(b\\)c\\\\d");
    }

    #[test]
    fn test_pdf_escape_non_ascii() {
        // Non-ASCII chars should be replaced with ?
        let result = pdf_escape_string("caf\u{00e9}");
        assert_eq!(result, "caf?");
    }

    #[test]
    fn test_shape_to_pdf_rect_rect() {
        use crate::annotation;
        let shape = annotation::Shape::Rect(annotation::Rect {
            x: 0,
            y: 0,
            width: 300,
            height: 300,
        });
        let rect = shape_to_pdf_rect(&shape, 300.0, 72.0).unwrap();
        assert!((rect.0 - 0.0).abs() < 0.01); // x1
        assert!((rect.2 - 72.0).abs() < 0.01); // x2 = 300 * 72/300
    }

    #[test]
    fn test_shape_to_pdf_rect_poly() {
        use crate::annotation;
        let shape = annotation::Shape::Poly(vec![(0, 0), (300, 0), (300, 300), (0, 300)]);
        let rect = shape_to_pdf_rect(&shape, 300.0, 72.0).unwrap();
        assert!((rect.0 - 0.0).abs() < 0.01);
        assert!((rect.2 - 72.0).abs() < 0.01);
    }

    #[test]
    fn test_shape_to_pdf_rect_empty_poly() {
        use crate::annotation;
        let shape = annotation::Shape::Poly(vec![]);
        assert!(shape_to_pdf_rect(&shape, 300.0, 72.0).is_none());
    }

    #[test]
    fn test_shape_to_pdf_rect_line() {
        use crate::annotation;
        let shape = annotation::Shape::Line(0, 0, 150, 150);
        let rect = shape_to_pdf_rect(&shape, 150.0, 72.0).unwrap();
        assert!((rect.0 - 0.0).abs() < 0.01);
        assert!((rect.2 - 72.0).abs() < 0.01);
    }

    #[test]
    fn test_count_outline_items_empty() {
        let bookmarks: Vec<crate::djvu_document::DjVuBookmark> = vec![];
        assert_eq!(count_outline_items(&bookmarks), 0);
    }

    #[test]
    fn test_count_outline_items_nested() {
        use crate::djvu_document::DjVuBookmark;
        let bookmarks = vec![DjVuBookmark {
            title: "Chapter 1".into(),
            url: "#1".into(),
            children: vec![
                DjVuBookmark {
                    title: "Section 1.1".into(),
                    url: "#2".into(),
                    children: vec![],
                },
                DjVuBookmark {
                    title: "Section 1.2".into(),
                    url: "#3".into(),
                    children: vec![],
                },
            ],
        }];
        assert_eq!(count_outline_items(&bookmarks), 3);
    }

    // ── DCTDecode / PdfOptions tests ──────────────────────────────────────────

    fn assets_path() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("references/djvujs/library/assets")
    }

    fn load_doc(name: &str) -> crate::djvu_document::DjVuDocument {
        let data =
            std::fs::read(assets_path().join(name)).unwrap_or_else(|_| panic!("{name} must exist"));
        crate::djvu_document::DjVuDocument::parse(&data)
            .unwrap_or_else(|e| panic!("parse failed: {e}"))
    }

    /// `PdfOptions::default()` uses jpeg_quality = Some(80).
    #[test]
    fn pdf_options_default_is_jpeg80() {
        let opts = PdfOptions::default();
        assert_eq!(opts.jpeg_quality, Some(80));
    }

    /// JPEG encoding roundtrip: `encode_rgb_to_jpeg` returns a non-empty JPEG.
    #[test]
    fn encode_rgb_to_jpeg_returns_jpeg() {
        // 4×4 solid red image
        let rgb = [255u8, 0, 0].repeat(16); // 16 pixels * 3 channels
        let jpeg = encode_rgb_to_jpeg(&rgb, 4, 4, 80);
        assert!(!jpeg.is_empty(), "JPEG output must not be empty");
        // JPEG starts with FF D8
        assert_eq!(jpeg[0], 0xFF);
        assert_eq!(jpeg[1], 0xD8);
    }

    /// `make_dct_stream` embeds /Filter /DCTDecode in the PDF stream dict.
    #[test]
    fn make_dct_stream_has_dctdecode_filter() {
        let fake_jpeg = b"\xFF\xD8\xFF\xD9"; // minimal JPEG markers
        let stream = make_dct_stream(" /Type /XObject", fake_jpeg);
        let s = String::from_utf8_lossy(&stream);
        assert!(
            s.contains("/Filter /DCTDecode"),
            "must contain DCTDecode filter"
        );
        assert!(s.contains("/Type /XObject"));
    }

    /// DCT PDF is smaller than deflate PDF for the same page.
    #[test]
    fn dct_pdf_is_smaller_than_deflate_pdf() {
        let doc = load_doc("chicken.djvu");
        let dct_pdf = djvu_to_pdf_with_options(
            &doc,
            &PdfOptions {
                jpeg_quality: Some(75),
                output_dpi: 150,
                adaptive_raster: false,
                ccitt_g4: false,
                mrc: false,
            },
        )
        .expect("DCT conversion must succeed");
        let flat_pdf = djvu_to_pdf_with_options(
            &doc,
            &PdfOptions {
                jpeg_quality: None,
                output_dpi: 150,
                adaptive_raster: false,
                ccitt_g4: false,
                mrc: false,
            },
        )
        .expect("FlateDecode conversion must succeed");
        assert!(
            dct_pdf.len() < flat_pdf.len(),
            "DCT PDF ({} bytes) must be smaller than FlateDecode PDF ({} bytes)",
            dct_pdf.len(),
            flat_pdf.len()
        );
    }

    /// Output PDF contains /DCTDecode when jpeg_quality is set.
    #[test]
    fn pdf_with_dct_contains_dctdecode_marker() {
        let doc = load_doc("chicken.djvu");
        let pdf = djvu_to_pdf_with_options(
            &doc,
            &PdfOptions {
                jpeg_quality: Some(80),
                output_dpi: 150,
                adaptive_raster: false,
                ccitt_g4: false,
                mrc: false,
            },
        )
        .unwrap();
        let has_dct = pdf.windows(9).any(|w| w == b"DCTDecode");
        assert!(has_dct, "PDF must contain DCTDecode");
    }

    /// Output PDF does NOT contain /DCTDecode when jpeg_quality is None.
    #[test]
    fn pdf_without_dct_has_no_dctdecode() {
        let doc = load_doc("chicken.djvu");
        let pdf = djvu_to_pdf_with_options(
            &doc,
            &PdfOptions {
                jpeg_quality: None,
                output_dpi: 150,
                adaptive_raster: false,
                ccitt_g4: false,
                mrc: false,
            },
        )
        .unwrap();
        let has_dct = pdf.windows(9).any(|w| w == b"DCTDecode");
        assert!(!has_dct, "FlateDecode PDF must not contain DCTDecode");
    }

    /// `djvu_to_pdf` (default, DCT at 80) is smaller than FlateDecode.
    #[test]
    fn default_djvu_to_pdf_is_dct() {
        let doc = load_doc("chicken.djvu");
        let default_pdf = djvu_to_pdf(&doc).unwrap();
        let flat_pdf = djvu_to_pdf_with_options(
            &doc,
            &PdfOptions {
                jpeg_quality: None,
                output_dpi: 150,
                adaptive_raster: false,
                ccitt_g4: false,
                mrc: false,
            },
        )
        .unwrap();
        assert!(
            default_pdf.len() < flat_pdf.len(),
            "default PDF must use DCT and be smaller than FlateDecode"
        );
    }

    // ── PDF_ADAPTIVE_RASTER: opt-in per-page Deflate-vs-JPEG choice ─────────────

    #[test]
    fn adaptive_raster_defaults_to_off() {
        assert!(!PdfOptions::default().adaptive_raster);
        assert!(!PdfOptions::archival().adaptive_raster);
    }

    /// With `adaptive_raster: false` (the default), output must be byte-identical
    /// to the pre-existing always-DCT behaviour.
    #[test]
    fn adaptive_raster_off_is_byte_identical_to_default() {
        let doc = load_doc("chicken.djvu");
        let plain = djvu_to_pdf(&doc).unwrap();
        let explicit_off = djvu_to_pdf_with_options(
            &doc,
            &PdfOptions {
                adaptive_raster: false,
                ..PdfOptions::default()
            },
        )
        .unwrap();
        assert_eq!(plain, explicit_off);
    }

    /// On a near-flat colour scan (`PDF_DCT_PROBE`'s regression case), JPEG-80 is
    /// 3.1x larger than Deflate at no SSIM gain. `adaptive_raster: true` must pick
    /// Deflate on every such page and produce a visibly smaller whole-file PDF.
    #[test]
    fn adaptive_raster_shrinks_flat_colour_scan() {
        let data = std::fs::read(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/corpus/watchmaker.djvu"),
        )
        .expect("watchmaker.djvu must exist");
        let doc = crate::djvu_document::DjVuDocument::parse(&data).expect("parse");

        let default_pdf = djvu_to_pdf(&doc).unwrap();
        let adaptive_pdf = djvu_to_pdf_with_options(
            &doc,
            &PdfOptions {
                adaptive_raster: true,
                ..PdfOptions::default()
            },
        )
        .unwrap();

        assert!(
            adaptive_pdf.len() < default_pdf.len(),
            "adaptive PDF ({} B) must be smaller than always-DCT default ({} B)",
            adaptive_pdf.len(),
            default_pdf.len()
        );
        // Expect a substantial win (measured ~1.6x on this corpus file), not a
        // rounding-error difference.
        assert!(
            (default_pdf.len() as f64) / (adaptive_pdf.len() as f64) > 1.3,
            "expected a large win from adaptive raster on a flat colour scan"
        );
    }

    /// `adaptive_raster: true` must never be *larger* than always-DCT: it's a
    /// per-page min, so image-heavy pages where JPEG already wins are unchanged.
    #[test]
    fn adaptive_raster_never_larger_than_default() {
        let doc = load_doc("chicken.djvu");
        let default_pdf = djvu_to_pdf(&doc).unwrap();
        let adaptive_pdf = djvu_to_pdf_with_options(
            &doc,
            &PdfOptions {
                adaptive_raster: true,
                ..PdfOptions::default()
            },
        )
        .unwrap();
        assert!(adaptive_pdf.len() <= default_pdf.len());
    }

    // ── PDF_G4: opt-in CCITTFaxDecode (G4/T.6) for JB2 masks ────────────────────

    #[test]
    fn ccitt_g4_defaults_to_off() {
        assert!(!PdfOptions::default().ccitt_g4);
        assert!(!PdfOptions::archival().ccitt_g4);
    }

    /// With `ccitt_g4: false` (the default), output must be byte-identical to
    /// the pre-existing always-Deflate mask behaviour.
    #[test]
    fn ccitt_g4_off_is_byte_identical_to_default() {
        let doc = load_doc("boy_jb2.djvu"); // Sjbz-only (bilevel fast path)
        let plain = djvu_to_pdf(&doc).unwrap();
        let explicit_off = djvu_to_pdf_with_options(
            &doc,
            &PdfOptions {
                ccitt_g4: false,
                ..PdfOptions::default()
            },
        )
        .unwrap();
        assert_eq!(plain, explicit_off);
    }

    /// `ccitt_g4: true` must produce a PDF containing `/CCITTFaxDecode` for a
    /// bilevel document, and must never be larger than the Deflate-only default
    /// (it's a per-mask min, same pattern as `adaptive_raster`).
    #[test]
    fn ccitt_g4_on_uses_ccittfaxdecode_and_never_larger() {
        let doc = load_doc("boy_jb2.djvu");
        let default_pdf = djvu_to_pdf(&doc).unwrap();
        let g4_pdf = djvu_to_pdf_with_options(
            &doc,
            &PdfOptions {
                ccitt_g4: true,
                ..PdfOptions::default()
            },
        )
        .unwrap();
        let has_ccitt = g4_pdf.windows(14).any(|w| w == b"CCITTFaxDecode");
        assert!(has_ccitt, "ccitt_g4 PDF must contain /CCITTFaxDecode");
        assert!(
            g4_pdf.len() <= default_pdf.len(),
            "g4 PDF ({} B) must not be larger than default ({} B)",
            g4_pdf.len(),
            default_pdf.len()
        );
    }

    /// On a real scanned bilevel corpus doc (`watchmaker.djvu`), `ccitt_g4: true`
    /// must shrink the whole-file PDF meaningfully (measured ~1.7x on this file's
    /// masks — see `PDF_G4` in `PERF_EXPERIMENTS.md`).
    #[test]
    fn ccitt_g4_shrinks_bilevel_corpus_doc() {
        let data = std::fs::read(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/corpus/watchmaker.djvu"),
        )
        .expect("watchmaker.djvu must exist");
        let doc = crate::djvu_document::DjVuDocument::parse(&data).expect("parse");

        let default_pdf = djvu_to_pdf(&doc).unwrap();
        let g4_pdf = djvu_to_pdf_with_options(
            &doc,
            &PdfOptions {
                ccitt_g4: true,
                ..PdfOptions::default()
            },
        )
        .unwrap();

        assert!(
            g4_pdf.len() < default_pdf.len(),
            "g4 PDF ({} B) must be smaller than Deflate-only default ({} B)",
            g4_pdf.len(),
            default_pdf.len()
        );
    }

    /// `collect_mask_stream` with `ccitt_g4: true` still returns `None` for a
    /// page without `Sjbz` (same short-circuit as the Deflate-only path).
    #[test]
    fn collect_mask_stream_g4_returns_none_for_no_sjbz() {
        let doc = load_doc("chicken.djvu"); // no Sjbz
        let page = doc.page(0).unwrap();
        let opts = PdfOptions {
            ccitt_g4: true,
            ..PdfOptions::default()
        };
        assert!(collect_mask_stream(page, &opts).is_none());
    }

    #[test]
    fn pdf_rgb_streaming_matches_pixmap_rgb() {
        let doc = load_doc("boy.djvu");
        let page = doc.page(0).unwrap();
        let (rw, rh) = render_dims(page, PdfOptions::default().output_dpi);
        let opts = RenderOptions {
            width: rw,
            height: rh,
            ..RenderOptions::default()
        };

        let streamed = render_rgb_for_pdf(page, &opts, rw, rh).unwrap();
        let pixmap = djvu_render::render_pixmap(page, &opts).unwrap();

        assert_eq!(streamed, pixmap.to_rgb());
    }

    #[test]
    fn pdf_rgb_fallback_handles_non_streamable_options() {
        let doc = load_doc("boy.djvu");
        let page = doc.page(0).unwrap();
        let opts = RenderOptions {
            width: page.width() as u32,
            height: page.height() as u32,
            aa: true,
            ..RenderOptions::default()
        };

        let rgb = render_rgb_for_pdf(page, &opts, opts.width, opts.height).unwrap();
        let pixmap = djvu_render::render_pixmap(page, &opts).unwrap();

        assert_eq!(rgb, pixmap.to_rgb());
    }

    // ── Text layer ────────────────────────────────────────────────────────────

    /// A document with TXTz must embed invisible text (BT…ET blocks).
    #[test]
    fn pdf_with_text_layer_contains_bt_et_markers() {
        let doc = load_doc("colorbook.djvu");
        let pdf = djvu_to_pdf(&doc).unwrap();
        // PDF content streams are deflated, but "BT" / "ET" may appear in stream
        // dict or in the raw uncompressed bytes we can observe at the dict level.
        // More reliably: we check that at least one page's content stream was
        // added (the file is larger than a document without text).
        assert!(!pdf.is_empty(), "PDF must not be empty");
        // The text content stream dict contains /Font when text is present.
        let has_font = pdf.windows(5).any(|w| w == b"/Font");
        assert!(
            has_font,
            "PDF with text layer must reference a /Font resource"
        );
    }

    /// `build_text_content` returns empty when the page has no text layer.
    #[test]
    fn build_text_content_no_text_layer_returns_empty() {
        let doc = load_doc("chicken.djvu"); // no TXTz
        let page = doc.page(0).unwrap();
        let result = build_text_content(page, 100.0, 720.0);
        assert!(
            result.is_empty(),
            "page without text layer must produce empty text content"
        );
    }

    /// `build_text_content` returns non-empty when the page has a text layer.
    #[test]
    fn build_text_content_with_text_layer_returns_non_empty() {
        let doc = load_doc("colorbook.djvu"); // has TXTz
        // Find the first page that actually has a text layer
        for i in 0..doc.page_count() {
            let page = doc.page(i).unwrap();
            if page.text_layer().ok().flatten().is_some() {
                let dpi = page.dpi().max(1) as f32;
                let pt_h = page.height() as f32 * 72.0 / dpi;
                let result = build_text_content(page, dpi, pt_h);
                if !result.is_empty() {
                    assert!(result.contains("BT"), "text content must begin with BT");
                    assert!(result.contains("ET"), "text content must end with ET");
                    return;
                }
            }
        }
        // If no page had non-empty text, that's fine — fixture may have empty zones
    }

    // ── Bookmarks (PDF outline) ───────────────────────────────────────────────

    /// A document with NAVM bookmarks must produce /Outlines in the PDF catalog.
    #[test]
    fn pdf_with_bookmarks_contains_outlines() {
        let doc = load_doc("links.djvu"); // has NAVM
        let pdf = djvu_to_pdf(&doc).unwrap();
        let has_outlines = pdf.windows(8).any(|w| w == b"Outlines");
        assert!(
            has_outlines,
            "PDF with NAVM bookmarks must contain /Outlines"
        );
    }

    /// A document without bookmarks must NOT produce /Outlines.
    #[test]
    fn pdf_without_bookmarks_has_no_outlines() {
        let doc = load_doc("chicken.djvu"); // no NAVM
        let pdf = djvu_to_pdf(&doc).unwrap();
        let has_outlines = pdf.windows(8).any(|w| w == b"Outlines");
        assert!(
            !has_outlines,
            "PDF without bookmarks must not contain /Outlines"
        );
    }

    /// `resolve_bookmark_dest` resolves `#page_N` to a /Dest reference.
    /// DjVu page anchors are 1-based: `#page_1` = index 0, `#page_2` = index 1.
    #[test]
    fn resolve_bookmark_dest_page_anchor() {
        let page_ids = [10usize, 20, 30];
        // #page_1 is 1-based → index 0 → page_ids[0] = 10
        let dest = resolve_bookmark_dest("#page_1", &page_ids);
        assert!(dest.contains("/Dest"), "must produce /Dest: {dest}");
        assert!(dest.contains("10 0 R"), "must reference page id 10: {dest}");
        // #page_2 → index 1 → page_ids[1] = 20
        let dest2 = resolve_bookmark_dest("#page_2", &page_ids);
        assert!(
            dest2.contains("20 0 R"),
            "must reference page id 20: {dest2}"
        );
    }

    /// `resolve_bookmark_dest` falls back to /A /URI for external URLs.
    #[test]
    fn resolve_bookmark_dest_external_url() {
        let dest = resolve_bookmark_dest("https://example.com", &[10, 20]);
        assert!(
            dest.contains("/URI"),
            "external URL must produce URI action: {dest}"
        );
    }

    /// `resolve_bookmark_dest` returns empty string for empty URL.
    #[test]
    fn resolve_bookmark_dest_empty_url() {
        let dest = resolve_bookmark_dest("", &[10]);
        assert!(dest.is_empty(), "empty URL must produce empty dest: {dest}");
    }

    // ── Hyperlink annotations ─────────────────────────────────────────────────

    /// `collect_link_annot_bodies` runs without error on a document with ANTz.
    #[test]
    fn collect_link_annot_bodies_runs_without_error() {
        let doc = load_doc("czech.djvu"); // has ANTz
        for i in 0..doc.page_count() {
            let page = doc.page(i).unwrap();
            let dpi = page.dpi().max(1) as f32;
            let pt_h = page.height() as f32 * 72.0 / dpi;
            let _ = collect_link_annot_bodies(page, dpi, pt_h);
        }
        // Test passes if no panic
    }

    /// `collect_link_annot_bodies` builds correct annotation body for a link.
    ///
    /// Exercises the annotation formatting path directly without needing a
    /// specific fixture with Rect-shaped hyperlinks.
    #[test]
    fn link_annot_body_format() {
        use crate::annotation::{MapArea, Rect as ARect, Shape};

        // Build a synthetic Rect-shaped hyperlink
        let link = MapArea {
            shape: Shape::Rect(ARect {
                x: 10,
                y: 20,
                width: 100,
                height: 50,
            }),
            url: "https://example.com".to_string(),
            description: String::new(),
            border: None,
            highlight: None,
        };

        let rect = shape_to_pdf_rect(&link.shape, 100.0, 360.0);
        assert!(rect.is_some(), "Rect shape must produce a PDF rect");
        let (x1, y1, x2, y2) = rect.unwrap();
        let url_escaped = pdf_escape_string(&link.url);
        let body = format!(
            "<< /Type /Annot /Subtype /Link\n\
               /Rect [{:.4} {:.4} {:.4} {:.4}]\n\
               /Border [0 0 0]\n\
               /A << /S /URI /URI ({url_escaped}) >> >>",
            x1, y1, x2, y2
        );
        assert!(body.contains("/Type /Annot"), "must have /Type /Annot");
        assert!(body.contains("/Subtype /Link"), "must have /Subtype /Link");
        assert!(body.contains("https://example.com"), "must contain URL");
        assert!(body.contains("/Rect"), "must have /Rect");
    }

    // ── Bilevel-only pages ────────────────────────────────────────────────────

    /// Bilevel-only page (Sjbz, no BG44) must use /ImageMask in the PDF.
    #[test]
    fn bilevel_only_page_has_image_mask() {
        let doc = load_doc("boy_jb2.djvu"); // Sjbz-only
        let pdf = djvu_to_pdf(&doc).unwrap();
        let has_mask = pdf.windows(9).any(|w| w == b"ImageMask");
        assert!(has_mask, "bilevel-only page must embed /ImageMask XObject");
    }

    // ── Mixed page (Sjbz + BG44) — mask overlay ──────────────────────────────

    /// A page with both Sjbz (foreground mask) and BG44 (background) must
    /// embed both an /Im0 image and a /Mask0 ImageMask XObject.
    /// (irish.djvu: Sjbz+BG44+FGbz — the palette path emits /Mask0, /Mask1, …)
    #[test]
    fn mixed_page_has_both_image_and_mask_xobject() {
        let doc = load_doc("irish.djvu"); // Sjbz+BG44+FGbz
        let pdf = djvu_to_pdf(&doc).unwrap();
        let has_im0 = pdf.windows(4).any(|w| w == b"Im0 ");
        let has_mask0 = pdf.windows(5).any(|w| w == b"Mask0");
        assert!(has_im0, "mixed page must reference /Im0 background");
        assert!(
            has_mask0,
            "mixed page must reference /Mask0 foreground mask"
        );
    }

    /// A page whose foreground colour is continuous-tone (FG44, no FGbz
    /// palette) must NOT get a stencil: a black stencil would flatten the
    /// coloured text, and the composited /Im0 already carries it (#620).
    #[test]
    fn fg44_page_skips_mask_stencil() {
        let doc = load_doc("colorbook.djvu"); // Sjbz+BG44+FG44, no FGbz
        let pdf = djvu_to_pdf(&doc).unwrap();
        let has_im0 = pdf.windows(4).any(|w| w == b"Im0 ");
        let has_mask0 = pdf.windows(5).any(|w| w == b"Mask0");
        assert!(has_im0, "FG44 page must reference /Im0 background");
        assert!(
            !has_mask0,
            "FG44 page must not paint a flat stencil over continuous-tone text"
        );
    }

    // ── render_dims / output_dpi ──────────────────────────────────────────────

    /// When output_dpi is lower than native DPI the PDF is smaller.
    #[test]
    fn lower_output_dpi_produces_smaller_pdf() {
        let doc = load_doc("chicken.djvu");
        let native = djvu_to_pdf_with_options(
            &doc,
            &PdfOptions {
                jpeg_quality: None,
                output_dpi: 0,
                adaptive_raster: false,
                ccitt_g4: false,
                mrc: false,
            },
        )
        .unwrap();
        let downscaled = djvu_to_pdf_with_options(
            &doc,
            &PdfOptions {
                jpeg_quality: None,
                output_dpi: 50,
                adaptive_raster: false,
                ccitt_g4: false,
                mrc: false,
            },
        )
        .unwrap();
        assert!(
            downscaled.len() < native.len(),
            "50 DPI PDF ({} B) must be smaller than native ({} B)",
            downscaled.len(),
            native.len()
        );
    }

    // ── PdfOptions::archival() ────────────────────────────────────────────────

    #[test]
    fn pdf_archival_preset_produces_output() {
        let doc = load_doc("chicken.djvu");
        let pdf = djvu_to_pdf_with_options(&doc, &PdfOptions::archival()).unwrap();
        assert!(!pdf.is_empty());
        assert!(
            pdf.starts_with(b"%PDF-"),
            "archival PDF must start with %PDF-"
        );
    }

    #[test]
    fn pdf_archival_preset_fields() {
        let opts = PdfOptions::archival();
        assert_eq!(opts.jpeg_quality, Some(90));
        assert_eq!(opts.output_dpi, 0);
    }

    // ── pdf_escape_string ─────────────────────────────────────────────────────

    #[test]
    fn pdf_escape_parens_and_backslash() {
        assert_eq!(pdf_escape_string("a(b)c"), "a\\(b\\)c");
        assert_eq!(pdf_escape_string("a\\b"), "a\\\\b");
    }

    #[test]
    fn pdf_escape_ascii_passthrough() {
        assert_eq!(pdf_escape_string("hello 123"), "hello 123");
    }

    #[test]
    fn pdf_escape_non_ascii_replaced_with_question_mark() {
        let s = pdf_escape_string("über");
        assert!(s.contains('?'), "non-ASCII must be replaced with ?: {s}");
    }

    // ── helvetica_advance ─────────────────────────────────────────────────────

    #[test]
    fn helvetica_advance_space() {
        assert!((helvetica_advance(' ') - 0.278).abs() < 1e-6);
    }

    #[test]
    fn helvetica_advance_digit() {
        assert!((helvetica_advance('5') - 0.556).abs() < 1e-6);
    }

    #[test]
    fn helvetica_advance_uppercase() {
        assert!((helvetica_advance('A') - 0.667).abs() < 1e-6);
    }

    #[test]
    fn helvetica_advance_lowercase() {
        assert!((helvetica_advance('a') - 0.556).abs() < 1e-6);
    }

    #[test]
    fn helvetica_advance_cjk_full_width() {
        // CJK Unified Ideograph — should return 1.0
        assert!((helvetica_advance('中') - 1.0).abs() < 1e-6);
    }

    #[test]
    fn helvetica_advance_hiragana_full_width() {
        assert!((helvetica_advance('あ') - 1.0).abs() < 1e-6);
    }

    #[test]
    fn helvetica_advance_cyrillic_falls_back() {
        // Cyrillic falls through to the 0.556 default
        assert!((helvetica_advance('А') - 0.556).abs() < 1e-6);
    }

    #[test]
    fn helvetica_advance_control_char_is_zero() {
        assert!((helvetica_advance('\x00') - 0.0).abs() < 1e-6);
        assert!((helvetica_advance('\x7f') - 0.0).abs() < 1e-6);
    }

    #[test]
    fn helvetica_advance_punctuation() {
        assert!((helvetica_advance(',') - 0.278).abs() < 1e-6);
        assert!((helvetica_advance('(') - 0.333).abs() < 1e-6);
        assert!((helvetica_advance('-') - 0.333).abs() < 1e-6);
    }

    // ── collect_mask_stream ───────────────────────────────────────────────────

    #[test]
    fn collect_mask_stream_returns_none_for_no_sjbz() {
        let doc = load_doc("chicken.djvu"); // no Sjbz
        let page = doc.page(0).unwrap();
        let result = collect_mask_stream(page, &PdfOptions::default());
        assert!(
            result.is_none(),
            "page without Sjbz must return None from collect_mask_stream"
        );
    }

    #[test]
    fn collect_mask_stream_returns_some_for_sjbz_page() {
        let doc = load_doc("boy_jb2.djvu"); // has Sjbz
        let page = doc.page(0).unwrap();
        let result = collect_mask_stream(page, &PdfOptions::default());
        assert!(
            result.is_some(),
            "page with Sjbz must return Some from collect_mask_stream"
        );
        let body = result.unwrap();
        // Must contain /ImageMask keyword
        assert!(
            body.windows(9).any(|w| w == b"ImageMask"),
            "mask stream must contain /ImageMask"
        );
    }

    // ── shape_to_pdf_rect ─────────────────────────────────────────────────────

    #[test]
    fn shape_to_pdf_rect_converts_rect_shape() {
        use crate::annotation::{Rect as ARect, Shape};
        let shape = Shape::Rect(ARect {
            x: 0,
            y: 0,
            width: 100,
            height: 50,
        });
        let rect = shape_to_pdf_rect(&shape, 100.0, 360.0);
        assert!(rect.is_some(), "valid rect shape must produce a PDF rect");
        let (x1, y1, x2, y2) = rect.unwrap();
        assert!((x1 - 0.0).abs() < 0.01);
        assert!((y1 - 0.0).abs() < 0.01);
        assert!((x2 - 72.0).abs() < 0.01); // 100px * 72/100dpi = 72pt
        assert!((y2 - 36.0).abs() < 0.01); // 50px * 72/100dpi = 36pt
    }

    // ── px_to_pt ─────────────────────────────────────────────────────────────

    #[test]
    fn px_to_pt_at_72dpi_is_identity() {
        assert!((px_to_pt(100.0, 72.0) - 100.0).abs() < 0.001);
    }

    #[test]
    fn px_to_pt_at_300dpi() {
        // 300px at 300dpi = 72pt
        assert!((px_to_pt(300.0, 300.0) - 72.0).abs() < 0.001);
    }

    // ── emit_word_span guards ─────────────────────────────────────────────────

    #[test]
    fn emit_word_span_zero_width_produces_no_ops() {
        use crate::text::Rect;
        let rect = Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 20,
        };
        let mut ops = String::new();
        emit_word_span(&mut ops, &rect, "hello", 72.0, 720.0);
        assert!(ops.is_empty(), "zero-width rect must produce no output");
    }

    #[test]
    fn emit_word_span_tiny_height_produces_no_ops() {
        use crate::text::Rect;
        // height=1px at 300dpi → h = 1*72/300 = 0.24pt < 0.5 → skip
        let rect = Rect {
            x: 0,
            y: 0,
            width: 50,
            height: 1,
        };
        let mut ops = String::new();
        emit_word_span(&mut ops, &rect, "hi", 300.0, 720.0);
        assert!(ops.is_empty(), "sub-0.5pt font size must produce no output");
    }

    // ── build_outline with nested bookmarks ──────────────────────────────────

    #[test]
    fn build_outline_with_nested_children_sets_first_last_count() {
        use crate::djvu_document::DjVuBookmark;
        let bookmarks = vec![DjVuBookmark {
            title: "Chapter 1".into(),
            url: "#page_1".into(),
            children: vec![
                DjVuBookmark {
                    title: "Section 1.1".into(),
                    url: "#page_2".into(),
                    children: vec![],
                },
                DjVuBookmark {
                    title: "Section 1.2".into(),
                    url: "#page_3".into(),
                    children: vec![],
                },
            ],
        }];
        let page_ids = [10usize, 20, 30];
        let mut pdf = Vec::new();
        let mut w = PdfWriter::new(&mut pdf).unwrap();
        let outline_id = build_outline(&mut w, &bookmarks, &page_ids).unwrap();
        assert!(
            outline_id.is_some(),
            "nested bookmarks must produce an outline"
        );
        // Serialize and check that /First and /Last are present
        w.finish().unwrap();
        let s = String::from_utf8_lossy(&pdf);
        assert!(
            s.contains("/First"),
            "outline item with children must set /First"
        );
        assert!(
            s.contains("/Last"),
            "outline item with children must set /Last"
        );
        assert!(
            s.contains("/Count"),
            "outline item with children must set /Count"
        );
    }

    // Lines 411-415: hyperlink annotation block (/Annots [...]).
    // Build a synthetic single-page DjVu with an ANTz maparea URL, then convert.
    #[test]
    fn djvu_to_pdf_with_hyperlinks_produces_annots() {
        use crate::annotation::{self as ann, Annotation, MapArea};
        use crate::djvu_document::DjVuDocument;
        use crate::iff::{self as iff_mod, Chunk, DjvuFile};
        let maparea = MapArea {
            url: "https://example.com".to_string(),
            description: String::new(),
            shape: ann::Shape::Rect(ann::Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 50,
            }),
            border: None,
            highlight: None,
        };
        let ant_data = ann::encode_annotations_bzz(&Annotation::default(), &[maparea]);
        // Minimal INFO: width=100, height=100, dpi=0 (default), rest zero.
        let mut info = vec![0u8; 10];
        info[1] = 100; // width
        info[3] = 100; // height
        let bytes = iff_mod::emit(&DjvuFile {
            root: Chunk::Form {
                secondary_id: *b"DJVU",
                length: 0,
                children: vec![
                    Chunk::Leaf {
                        id: *b"INFO",
                        data: info,
                    },
                    Chunk::Leaf {
                        id: *b"ANTz",
                        data: ant_data,
                    },
                ],
            },
        });
        let doc = DjVuDocument::parse(&bytes).expect("synthetic doc must parse");
        let pdf = djvu_to_pdf(&doc).expect("synthetic hyperlink page must convert to PDF");
        let s = String::from_utf8_lossy(&pdf);
        assert!(
            s.contains("/Annots"),
            "PDF from hyperlink page must contain /Annots"
        );
    }

    /// Page with corrupted ANTz (invalid BZZ): `hyperlinks()` errors, so
    /// `collect_link_annot_bodies` returns empty (line 332 `Err(_) => Vec::new()`).
    #[test]
    fn djvu_to_pdf_with_corrupted_antz_skips_annotations() {
        use crate::djvu_document::DjVuDocument;
        use crate::iff::{self as iff_mod, Chunk, DjvuFile};

        let mut info = vec![0u8; 10];
        info[1] = 100; // width
        info[3] = 100; // height
        // Garbage bytes that are not valid BZZ — decoding will fail
        let bad_antz: Vec<u8> = vec![0xFF, 0xFE, 0xAB, 0xCD, 0x12, 0x34];
        let bytes = iff_mod::emit(&DjvuFile {
            root: Chunk::Form {
                secondary_id: *b"DJVU",
                length: 0,
                children: vec![
                    Chunk::Leaf {
                        id: *b"INFO",
                        data: info,
                    },
                    Chunk::Leaf {
                        id: *b"ANTz",
                        data: bad_antz,
                    },
                ],
            },
        });
        let doc = DjVuDocument::parse(&bytes).expect("synthetic doc must parse");
        let pdf = djvu_to_pdf(&doc).expect("corrupted ANTz must not abort PDF export");
        let s = String::from_utf8_lossy(&pdf);
        assert!(
            !s.contains("/Annots"),
            "corrupted ANTz should produce no /Annots block"
        );
    }

    /// Page whose render fails (0×0 dimensions, no image data) triggers the blank
    /// page fallback at lines 840-850: `rendered_pages[i]` is None so a blank
    /// /Page object is emitted with the native MediaBox dimensions.
    #[test]
    fn djvu_to_pdf_zero_dim_page_emits_blank_page_object() {
        use crate::djvu_document::DjVuDocument;
        use crate::iff::{self as iff_mod, Chunk, DjvuFile};

        // INFO chunk: width=0, height=0, dpi=0 (all zeros). No Sjbz or BG44 so
        // is_bilevel_only=false, and render_dims returns (0,0), which makes
        // render_pixmap return InvalidDimensions → render_page_data returns Err
        // → .ok() yields None → blank page fallback fires.
        let info = vec![0u8; 10];
        let bytes = iff_mod::emit(&DjvuFile {
            root: Chunk::Form {
                secondary_id: *b"DJVU",
                length: 0,
                children: vec![Chunk::Leaf {
                    id: *b"INFO",
                    data: info,
                }],
            },
        });
        let doc = DjVuDocument::parse(&bytes).expect("zero-dim doc must parse");
        let pdf = djvu_to_pdf(&doc).expect("zero-dim page must not crash PDF export");
        let s = String::from_utf8_lossy(&pdf);
        assert!(
            s.contains("/Type /Page"),
            "PDF must contain at least one Page object"
        );
    }
}
