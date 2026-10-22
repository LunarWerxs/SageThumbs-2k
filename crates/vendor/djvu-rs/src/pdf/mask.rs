//! The JB2 foreground as 1-bit image masks and per-colour stencil layers.

use super::*;

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
pub(super) fn collect_mask_stream(page: &DjVuPage, opts: &PdfOptions) -> Option<Vec<u8>> {
    let bitmap = page.extract_mask().ok()??;
    Some(mask_body_from_bitmap(&bitmap, opts.ccitt_g4))
}

/// Encode one 1-bit bitmap as a PDF ImageMask XObject body (Deflate, or the
/// smaller of Deflate/G4 when `use_g4` is set).
pub(super) fn mask_body_from_bitmap(bitmap: &crate::bitmap::Bitmap, use_g4: bool) -> Vec<u8> {
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
pub(super) struct MaskLayer {
    pub(super) rgb: (u8, u8, u8),
    pub(super) bbox: (u32, u32, u32, u32),
    pub(super) mask_dims: (u32, u32),
    pub(super) body: Vec<u8>,
}

/// Build the foreground stencil layers for a mixed page.
///
/// Pages without an FGbz colour palette (or whose palette is entirely black)
/// keep the historical single black stencil — byte-identical output. Pages with
/// a non-black FGbz palette get one ImageMask per palette colour actually used,
/// each painted in its own fill colour (#559: a single black stencil flattened
/// coloured foreground text to black).
pub(super) fn collect_mask_layers(page: &DjVuPage, opts: &PdfOptions) -> Vec<MaskLayer> {
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
                let fg = match page.decoded_fg44() {
                    Some(fg) => Some(fg),
                    None => page.extract_foreground().ok().flatten().map(Arc::new),
                };
                let Some(fg) = fg else {
                    return Vec::new();
                };
                return match uniform_fg_color(&fg, &mask) {
                    Some(rgb) => stencil_layer_from_mask(&mask, opts, rgb),
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
pub(super) fn black_mask_layer(page: &DjVuPage, opts: &PdfOptions) -> Vec<MaskLayer> {
    // Prefer the page cache (populated by this page's own /Im0 render).
    if let Some(mask) = page.decoded_mask() {
        return stencil_layer_from_mask(&mask, opts, (0, 0, 0));
    }
    let Ok(Some(bitmap)) = page.extract_mask() else {
        return Vec::new();
    };
    stencil_layer_from_mask(&bitmap, opts, (0, 0, 0))
}

/// A single full-mask stencil painted in `rgb`, from an already-decoded mask.
pub(super) fn stencil_layer_from_mask(
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
pub(super) const FG44_UNIFORM_SPREAD: u8 = 48;

/// The page's FG44/FGjp foreground colour, if near-uniform under the mask.
///
/// Samples the (subsampled) foreground pixmap at every marked mask pixel and
/// returns the mean colour when every channel's spread stays within
/// [`FG44_UNIFORM_SPREAD`]; `None` when the foreground is multi-coloured (or
/// either layer fails to decode).
pub(super) fn uniform_fg_color(
    fg: &crate::Pixmap,
    mask: &crate::bitmap::Bitmap,
) -> Option<(u8, u8, u8)> {
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
