//! Per-page output: geometry, the rendered background image, link annotations and page objects.

use super::*;

/// Convert DjVu pixel coordinates to PDF points.
/// DjVu uses bottom-left origin (like PDF), so y-coordinates can be used directly
/// after scaling by 72/dpi.
pub(super) fn px_to_pt(px: f32, dpi: f32) -> f32 {
    px * 72.0 / dpi
}

/// Raster size for a page given the `output_dpi` option.
///
/// When `output_dpi == 0` the native page resolution is kept. The raster is
/// embedded in the page's native orientation (render it with
/// [`RenderSize::native_options`]); the page's `/Rotate` entry turns it.
pub(super) fn render_size(page: &DjVuPage, output_dpi: u32) -> RenderSize {
    let native_dpi = page.dpi().max(1) as f32;
    // PDF never upscales: a zero or above-native target DPI keeps native pixels.
    if output_dpi == 0 || output_dpi as f32 >= native_dpi {
        return RenderSize::at_scale(page, 1.0);
    }
    RenderSize::at_dpi(page, output_dpi as f32)
}

/// The PDF `/Rotate` angle (clockwise degrees) for a page's INFO rotation.
pub(super) fn pdf_rotate(rotation: Rotation) -> u16 {
    match rotation {
        Rotation::None => 0,
        Rotation::Cw90 => 90,
        Rotation::Rot180 => 180,
        Rotation::Ccw90 => 270,
    }
}

/// ` /Rotate N` for a page dictionary, or nothing for an upright page.
pub(super) fn rotate_entry(rotate: u16) -> String {
    if rotate == 0 {
        String::new()
    } else {
        format!(" /Rotate {rotate}")
    }
}

/// Pre-rendered page data — all expensive compute done, ready for sequential PDF emit.
///
/// # Memory note
///
/// `djvu_to_pdf_impl` collects `RenderedPage` for every page before emitting any PDF
/// objects (because `PdfWriter` is not `Send`). For large bilevel documents at native
/// DPI (e.g. 520 pages × ~1 MB deflated mask each) peak RAM can be significant.
/// A streaming/chunked approach is tracked in a separate issue.
pub(super) struct RenderedPage {
    pub(super) pt_w: f32,
    pub(super) pt_h: f32,
    /// Clockwise page rotation for `/Rotate`: every layer (raster, masks,
    /// text, links) is laid out in the page's native orientation.
    pub(super) rotate: u16,
    pub(super) is_bilevel_only: bool,
    /// Fully encoded XObject body written as PDF resource `/Im0`.
    ///
    /// For bilevel-only pages this is the 1-bit JB2 mask; for mixed pages it is the
    /// RGB background image.
    pub(super) img0_body: Option<Vec<u8>>,
    /// Fully encoded XObject bodies for the JB2 mask overlay (`/Mask0`,
    /// `/Mask1`, …), one per foreground colour, each painted in its own
    /// fill colour. Only populated for non-bilevel pages with a Sjbz chunk;
    /// pages without an FGbz colour palette get a single black layer.
    pub(super) mask_layers: Vec<MaskLayer>,
    /// PDF content stream text operators (invisible text layer).
    pub(super) text_ops: String,
    /// Pre-built annotation object bodies, one per hyperlink.
    pub(super) link_annot_bodies: Vec<Vec<u8>>,
}

/// Render one page into a [`RenderedPage`].
///
/// This is the expensive step (pixel render, JPEG encode, JB2 decode, deflate)
/// and can safely run in parallel across pages.
pub(super) fn render_page_data(
    page: &DjVuPage,
    opts: &PdfOptions,
) -> Result<RenderedPage, PdfError> {
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
        for px in bg.data.as_chunks::<4>().0 {
            rgb.extend_from_slice(&px[..3]);
        }
        (Some(encode_img0_body(&rgb, rw, rh, opts)), layers)
    } else {
        let size = render_size(page, opts.output_dpi);
        let (rw, rh) = size.native;
        // The render pipeline derives the IW44 decode scale from `width` (see
        // `RenderOptions::decode_scale`) (#377). The raster stays in native
        // orientation to line up with the masks, text, and links.
        let render_opts = size.native_options(page);
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
        rotate: pdf_rotate(page.rotation()),
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
pub(super) fn encode_img0_body(rgb: &[u8], rw: u32, rh: u32, opts: &PdfOptions) -> Vec<u8> {
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

pub(super) fn render_rgb_for_pdf(
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

/// Build pre-serialized annotation bodies for all hyperlinks on a page.
pub(super) fn collect_link_annot_bodies(page: &DjVuPage, dpi: f32, pt_h: f32) -> Vec<Vec<u8>> {
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
pub(super) fn emit_page_objects<W: std::io::Write>(
    w: &mut PdfWriter<W>,
    data: RenderedPage,
    pages_id: usize,
    font_id: usize,
) -> Result<usize, PdfError> {
    let pt_w = data.pt_w;
    let pt_h = data.pt_h;
    let rotate = rotate_entry(data.rotate);

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
               /MediaBox [0 0 {pt_w:.4} {pt_h:.4}]{rotate}\n\
               /Contents {content_id} 0 R\n\
               /Resources << {resources} >>{annots_str} >>"
        )
        .into_bytes(),
    )
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
pub(super) fn shape_to_pdf_rect(
    shape: &Shape,
    dpi: f32,
    _pt_h: f32,
) -> Option<(f32, f32, f32, f32)> {
    let r = crate::export_common::shape_bbox(shape)?;
    let x1 = px_to_pt(r.x as f32, dpi);
    let y1 = px_to_pt(r.y as f32, dpi);
    let x2 = px_to_pt((r.x + r.width) as f32, dpi);
    let y2 = px_to_pt((r.y + r.height) as f32, dpi);
    Some((x1, y1, x2, y2))
}
