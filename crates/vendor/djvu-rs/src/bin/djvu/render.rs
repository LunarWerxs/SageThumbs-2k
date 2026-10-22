//! `djvu render`: PNG, PDF, EPUB and CBZ output.

use super::*;

pub(super) fn to_user_rotation(r: &RotateArg) -> djvu_rs::djvu_render::UserRotation {
    use djvu_rs::djvu_render::UserRotation;
    match r {
        RotateArg::None => UserRotation::None,
        RotateArg::Cw90 => UserRotation::Cw90,
        RotateArg::Rot180 => UserRotation::Rot180,
        RotateArg::Ccw90 => UserRotation::Ccw90,
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn cmd_render(
    path: &Path,
    page: usize,
    all: bool,
    dpi: u32,
    format: Format,
    layer: Layer,
    rotate: RotateArg,
    output: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    // PDF uses the new DjVuDocument API directly (preserves text, bookmarks, links)
    if matches!(format, Format::Pdf) {
        return render_pdf_structured(path, output);
    }

    // EPUB uses the new DjVuDocument API directly
    #[cfg(feature = "epub")]
    if matches!(format, Format::Epub) {
        return render_epub_structured(path, output);
    }
    #[cfg(not(feature = "epub"))]
    if matches!(format, Format::Epub) {
        return Err("epub feature not enabled; rebuild with --features epub".into());
    }

    // Layer extraction uses the DjVuDocument API
    if !matches!(layer, Layer::Composite) {
        return render_layer(path, page, all, layer, output);
    }

    // When the `parallel` feature is enabled and --all is requested for PNG,
    // use rayon-based parallel rendering via the DjVuDocument API.
    #[cfg(feature = "parallel")]
    if all && matches!(format, Format::Png) {
        return render_png_parallel(path, dpi, to_user_rotation(&rotate), output);
    }

    let doc = open(path)?;
    let count = doc.page_count();
    let user_rot = to_user_rotation(&rotate);

    match format {
        Format::Png => render_png(&doc, page, all, dpi, count, user_rot, output),
        Format::Pdf | Format::Epub => unreachable!(),
        Format::Cbz => render_cbz(path, page, all, dpi, count, user_rot, output),
    }
}

pub(super) fn render_layer(
    path: &Path,
    page: usize,
    all: bool,
    layer: Layer,
    output: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read(path)?;
    let doc = djvu_rs::djvu_document::DjVuDocument::parse(&data)?;
    let count = doc.page_count();

    let pages: Vec<usize> = if all {
        (0..count).collect()
    } else {
        vec![page_idx(page, count)?]
    };

    if all {
        std::fs::create_dir_all(output)?;
    } else if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }

    for idx in pages {
        let pg = doc.page(idx)?;
        let out_path = if all {
            output.join(format!("page_{:04}.png", idx + 1))
        } else {
            output.to_path_buf()
        };

        match layer {
            Layer::Mask => {
                let bm = pg.extract_mask()?.ok_or("page has no JB2 mask layer")?;
                // Convert 1-bit bitmap to RGBA (black/white)
                let w = bm.width;
                let h = bm.height;
                let mut rgba = vec![255u8; (w * h * 4) as usize];
                for y in 0..h {
                    for x in 0..w {
                        if bm.get(x, y) {
                            let off = ((y * w + x) * 4) as usize;
                            rgba[off] = 0;
                            rgba[off + 1] = 0;
                            rgba[off + 2] = 0;
                        }
                    }
                }
                let file = std::fs::File::create(&out_path)?;
                let mut writer = std::io::BufWriter::new(file);
                encode_png(&mut writer, w, h, &rgba)?;
            }
            Layer::Foreground => {
                let pm = pg
                    .extract_foreground()?
                    .ok_or("page has no foreground layer")?;
                let rgba = pixmap_to_rgba(&pm);
                let file = std::fs::File::create(&out_path)?;
                let mut writer = std::io::BufWriter::new(file);
                encode_png(&mut writer, pm.width, pm.height, &rgba)?;
            }
            Layer::Background => {
                let pm = pg
                    .extract_background()?
                    .ok_or("page has no background layer")?;
                let rgba = pixmap_to_rgba(&pm);
                let file = std::fs::File::create(&out_path)?;
                let mut writer = std::io::BufWriter::new(file);
                encode_png(&mut writer, pm.width, pm.height, &rgba)?;
            }
            Layer::Composite => unreachable!(),
        }
    }
    Ok(())
}

/// Apply user-requested rotation to a rendered pixmap (post-render, on top of INFO rotation).
pub(super) fn apply_user_rotation(
    src: djvu_rs::Pixmap,
    rot: djvu_rs::djvu_render::UserRotation,
) -> djvu_rs::Pixmap {
    use djvu_rs::djvu_render::UserRotation;
    match rot {
        UserRotation::None => src,
        UserRotation::Cw90 => src.rotate_cw90(),
        UserRotation::Rot180 => src.rotate_180(),
        UserRotation::Ccw90 => src.rotate_ccw90(),
    }
}

/// Convert an RGB Pixmap to RGBA bytes.
pub(super) fn pixmap_to_rgba(pm: &djvu_rs::Pixmap) -> Vec<u8> {
    let mut rgba = Vec::with_capacity((pm.width * pm.height * 4) as usize);
    for y in 0..pm.height {
        for x in 0..pm.width {
            let (r, g, b) = pm.get_rgb(x, y);
            rgba.extend_from_slice(&[r, g, b, 255]);
        }
    }
    rgba
}

pub(super) fn render_png(
    doc: &Document,
    page: usize,
    all: bool,
    dpi: u32,
    count: usize,
    rotate: djvu_rs::djvu_render::UserRotation,
    output: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    if all {
        std::fs::create_dir_all(output)?;
        for i in 0..count {
            let out = output.join(format!("page_{:04}.png", i + 1));
            render_page_png(doc, i, dpi, rotate, &out)?;
        }
    } else {
        let idx = page_idx(page, count)?;
        if let Some(parent) = output.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        render_page_png(doc, idx, dpi, rotate, output)?;
    }
    Ok(())
}

pub(super) fn render_pdf_structured(
    path: &Path,
    output: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read(path)?;
    let doc = djvu_rs::djvu_document::DjVuDocument::parse(&data)?;
    // Stream straight to a sibling temp file (#606), then atomically commit
    // only a complete PDF — the whole document is never buffered.
    write_atomic_with(output, |file| {
        let mut writer = std::io::BufWriter::new(file);
        djvu_rs::pdf::djvu_to_pdf_to_writer(
            &doc,
            &djvu_rs::pdf::PdfOptions::default(),
            &mut writer,
        )?;
        use std::io::Write;
        writer.flush()?;
        writer
            .into_inner()
            .map_err(|error| error.into_error())?
            .sync_all()?;
        Ok(())
    })
}

#[cfg(feature = "epub")]
pub(super) fn render_epub_structured(
    path: &Path,
    output: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read(path)?;
    let doc = djvu_rs::djvu_document::DjVuDocument::parse(&data)?;
    // Stream straight to a sibling temp file, then atomically commit only a
    // complete EPUB — the whole document is never buffered.
    write_atomic_with(output, |file| {
        let mut writer = std::io::BufWriter::new(file);
        djvu_rs::epub::djvu_to_epub_writer(
            &doc,
            &djvu_rs::epub::EpubOptions::default(),
            &mut writer,
        )?;
        use std::io::Write;
        writer.flush()?;
        writer
            .into_inner()
            .map_err(|error| error.into_error())?
            .sync_all()?;
        Ok(())
    })
}

pub(super) fn render_cbz(
    path: &Path,
    page: usize,
    all: bool,
    dpi: u32,
    count: usize,
    rotate: djvu_rs::djvu_render::UserRotation,
    output: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let pages = if all {
        None
    } else {
        Some(vec![page_idx(page, count)?])
    };

    let data = std::fs::read(path)?;
    let doc = djvu_rs::djvu_document::DjVuDocument::parse(&data)?;
    let opts = djvu_rs::cbz::CbzOptions {
        dpi,
        rotation: rotate,
        pages,
    };

    write_atomic_with(output, |file| {
        let mut zip = zip::ZipWriter::new(file);
        djvu_rs::cbz::write_pages(&mut zip, &doc, &opts)?;
        zip.finish()?.sync_all()?;
        Ok(())
    })
}

/// Parallel PNG rendering: renders all pages concurrently using rayon, then
/// writes PNGs sequentially.
#[cfg(feature = "parallel")]
pub(super) fn render_png_parallel(
    path: &Path,
    dpi: u32,
    rotate: djvu_rs::djvu_render::UserRotation,
    output: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read(path)?;
    let doc = djvu_rs::djvu_document::DjVuDocument::parse(&data)?;
    std::fs::create_dir_all(output)?;

    let pixmaps = djvu_rs::djvu_render::render_pages_parallel(&doc, dpi);

    for (i, result) in pixmaps.into_iter().enumerate() {
        let pixmap = apply_user_rotation(result?, rotate);
        let out = output.join(format!("page_{:04}.png", i + 1));
        let file = std::fs::File::create(&out)?;
        let mut writer = std::io::BufWriter::new(file);
        encode_png(&mut writer, pixmap.width, pixmap.height, &pixmap.data)?;
    }

    Ok(())
}

pub(super) fn render_page_png(
    doc: &Document,
    idx: usize,
    dpi: u32,
    rotate: djvu_rs::djvu_render::UserRotation,
    out: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let page = doc.page(idx)?;
    let (w, h) = page.size_at_dpi(dpi as f32);
    let pixmap = page.render_to_size(w, h)?;
    let pixmap = apply_user_rotation(pixmap, rotate);
    let file = std::fs::File::create(out)?;
    let mut writer = std::io::BufWriter::new(file);
    encode_png(&mut writer, pixmap.width, pixmap.height, &pixmap.data)?;
    Ok(())
}

pub(super) fn encode_png(
    out: &mut impl std::io::Write,
    width: u32,
    height: u32,
    rgba: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut encoder = png::Encoder::new(out, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(rgba)?;
    Ok(())
}
