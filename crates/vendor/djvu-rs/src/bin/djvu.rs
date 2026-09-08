use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand, ValueEnum};
use djvu_rs::Document;

#[derive(Parser)]
#[command(name = "djvu", about = "DjVu file utility", version)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show document info: page count, dimensions, DPI.
    Info {
        /// Path to the DjVu file.
        file: PathBuf,
        /// Print only the page count as a plain integer (useful for scripting).
        #[arg(short, long, conflicts_with = "json")]
        count: bool,
        /// Output info as JSON.
        #[arg(short, long)]
        json: bool,
    },
    /// Render pages to PNG, PDF, CBZ, or EPUB.
    Render {
        /// Path to the DjVu file.
        file: PathBuf,
        /// Page number to render (1-based). Default: 1.
        #[arg(short, long, default_value = "1")]
        page: usize,
        /// Render all pages.
        #[arg(long, conflicts_with = "page")]
        all: bool,
        /// Output DPI. Default: 150.
        #[arg(short, long, default_value = "150")]
        dpi: u32,
        /// Output format.
        #[arg(short, long, default_value = "png", value_enum)]
        format: Format,
        /// Layer to extract: composite (default), mask, foreground, background.
        #[arg(short, long, default_value = "composite", value_enum)]
        layer: Layer,
        /// Additional rotation applied on top of the INFO chunk rotation.
        #[arg(short, long, default_value = "none", value_enum)]
        rotate: RotateArg,
        /// Output file (single page) or directory (--all, PNG only).
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Merge multiple DjVu files into one bundled DJVM.
    Merge {
        /// Input DjVu files to merge.
        files: Vec<PathBuf>,
        /// Output file path.
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Extract a range of pages from a DjVu document.
    Split {
        /// Path to the DjVu file.
        file: PathBuf,
        /// Page number to extract (1-based). Conflicts with --pages.
        #[arg(short, long)]
        page: Option<usize>,
        /// Page range to extract (e.g. "1-50", 1-based inclusive).
        #[arg(long, conflicts_with = "page")]
        pages: Option<String>,
        /// Output file path.
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Run OCR on pages and write the text layer back into the file.
    #[cfg(any(
        feature = "ocr-tesseract",
        feature = "ocr-onnx",
        feature = "ocr-neural"
    ))]
    Ocr {
        /// Path to the input DjVu file.
        file: PathBuf,
        /// OCR backend to use.
        #[arg(short, long, default_value = "tesseract", value_enum)]
        backend: OcrBackendChoice,
        /// Languages for recognition (e.g. "eng", "rus+eng").
        #[arg(short, long, default_value = "eng")]
        lang: String,
        /// Path to ONNX model file (required for --backend onnx).
        #[arg(long)]
        model: Option<PathBuf>,
        /// Output DjVu file with embedded OCR text layer.
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Compress a file using BZZ encoding.
    BzzEncode {
        /// Input file to compress.
        file: PathBuf,
        /// Output file path.
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Decompress a BZZ-encoded file.
    BzzDecode {
        /// BZZ-compressed input file.
        file: PathBuf,
        /// Output file path.
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Encode an image (PNG, JPEG, or TIFF) into a single-page DjVu file,
    /// or a directory of images into a multi-page DJVM bundle.
    ///
    /// Single-image input supports lossless bilevel JB2 plus layered
    /// quality/archival color profiles (`INFO + Sjbz + BG44 + FGbz`).
    /// Multi-page directory input supports the same profiles; both the
    /// lossless and layered paths share a Djbz dictionary across pages
    /// (see --shared-dict-pages).
    Encode {
        /// Input image path (PNG, JPEG, or TIFF), or a directory of images
        /// (sorted by file name) for multi-page encoding.
        input: PathBuf,
        /// Output DjVu file path.
        #[arg(short, long)]
        output: PathBuf,
        /// Page DPI stored in the INFO chunk. Default: 300.
        #[arg(short, long, default_value = "300")]
        dpi: u16,
        /// Encoding profile.
        #[arg(short, long, default_value = "lossless", value_enum)]
        quality: EncodeQualityArg,
        /// Mask binarization for layered quality/archival encodes.
        #[arg(long, default_value = "fixed", value_enum)]
        binarization: BinarizationArg,
        /// Sauvola local window size in pixels, used with --binarization sauvola.
        #[arg(long, default_value = "25")]
        sauvola_window: u32,
        /// Sauvola k factor, used with --binarization sauvola.
        #[arg(long, default_value = "0.34")]
        sauvola_k: f32,
        /// Inpaint fully masked background blocks for layered encodes.
        #[arg(long)]
        bg_inpaint: bool,
        /// IW44 background bits-per-pixel budget (quality/archival only).
        /// Encode BG44 slices until the cumulative payload reaches this many
        /// bits per pixel; overrides the default 100-slice schedule. A lower
        /// value means a smaller file at the cost of quality. Omit to use the
        /// default slice-based schedule.
        #[arg(long)]
        bg_bpp: Option<f32>,
        /// (Multi-page only.) Promote a connected component to the
        /// shared Djbz dictionary if it appears on at least this many
        /// distinct pages. Default: 2.
        #[arg(long, default_value = "2")]
        shared_dict_pages: usize,
        /// (Multi-page layered only.) Embed a TH44 colour thumbnail in each
        /// page — thumbnail grids decode 2–15× faster (TH44_GRID) at a small
        /// size cost.
        #[arg(long)]
        thumbnails: bool,
    },
    /// Extract the text layer from a DjVu document.
    Text {
        /// Path to the DjVu file.
        file: PathBuf,
        /// Page number to extract (1-based). Default: 1.
        #[arg(short, long, default_value = "1")]
        page: usize,
        /// Extract text from all pages.
        #[arg(long, conflicts_with = "page")]
        all: bool,
        /// Output format: plain (default), hocr, alto.
        #[arg(short, long, default_value = "plain", value_enum)]
        format: TextFormat,
        /// Output file path for hOCR/ALTO output. Default: stdout.
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
}

#[derive(Clone, ValueEnum)]
enum Format {
    Png,
    Pdf,
    Cbz,
    /// EPUB 3 (preserves text, bookmarks, hyperlinks).
    Epub,
}

#[derive(Clone, ValueEnum)]
enum TextFormat {
    /// Plain text (default).
    Plain,
    /// hOCR HTML format.
    Hocr,
    /// ALTO XML format.
    Alto,
}

#[cfg(any(
    feature = "ocr-tesseract",
    feature = "ocr-onnx",
    feature = "ocr-neural"
))]
#[derive(Clone, ValueEnum)]
enum OcrBackendChoice {
    /// Supported backend: system Tesseract via tesseract-rs.
    Tesseract,
    /// Experimental library-only ONNX scaffold; no stable CLI contract yet.
    Onnx,
    /// Experimental neural placeholder; no supported model implementation yet.
    Candle,
}

#[derive(Clone, ValueEnum)]
enum RotateArg {
    /// No additional rotation (only INFO chunk rotation applies).
    None,
    /// Rotate 90° clockwise.
    Cw90,
    /// Rotate 180°.
    Rot180,
    /// Rotate 90° counter-clockwise (270° clockwise).
    Ccw90,
}

#[derive(Clone, Debug, ValueEnum)]
enum EncodeQualityArg {
    /// Pixel-exact bilevel JB2 (`INFO + Sjbz`).
    Lossless,
    /// Layered FG/BG with lossy IW44 BG.
    Quality,
    /// Conservative layered profile with denser BG sampling and FGbz palette.
    Archival,
    /// Mask-less continuous-tone profile (DjVuPhoto): INFO + BG44 only.
    /// For photographs and grayscale scans.
    Photo,
    /// Detect the content type per input (bilevel text / layered document /
    /// photo) and pick the profile automatically (#570).
    Auto,
}

#[derive(Clone, Copy, ValueEnum, PartialEq, Eq)]
enum BinarizationArg {
    /// Fixed BT.601 luminance threshold.
    Fixed,
    /// Sauvola local adaptive threshold.
    Sauvola,
}

#[derive(Clone, ValueEnum)]
enum Layer {
    /// Full composite render (default).
    Composite,
    /// JB2 bilevel mask only.
    Mask,
    /// IW44 foreground layer only.
    Foreground,
    /// IW44 background layer only.
    Background,
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.command {
        Cmd::Info { file, count, json } => cmd_info(&file, count, json),
        Cmd::Render {
            file,
            page,
            all,
            dpi,
            format,
            layer,
            rotate,
            output,
        } => cmd_render(&file, page, all, dpi, format, layer, rotate, &output),
        #[cfg(any(
            feature = "ocr-tesseract",
            feature = "ocr-onnx",
            feature = "ocr-neural"
        ))]
        Cmd::Ocr {
            file,
            backend,
            lang,
            model,
            output,
        } => cmd_ocr(&file, backend, &lang, model.as_deref(), &output),
        Cmd::BzzEncode { file, output } => cmd_bzz_encode(&file, &output),
        Cmd::BzzDecode { file, output } => cmd_bzz_decode(&file, &output),
        Cmd::Merge { files, output } => cmd_merge(&files, &output),
        Cmd::Split {
            file,
            page,
            pages,
            output,
        } => cmd_split(&file, page, pages.as_deref(), &output),
        Cmd::Text {
            file,
            page,
            all,
            format,
            output,
        } => cmd_text(&file, page, all, format, output.as_deref()),
        Cmd::Encode {
            input,
            output,
            dpi,
            quality,
            binarization,
            sauvola_window,
            sauvola_k,
            bg_inpaint,
            bg_bpp,
            shared_dict_pages,
            thumbnails,
        } => cmd_encode(
            &input,
            &output,
            dpi,
            quality,
            EncodeSegmentArgs {
                binarization,
                sauvola_window,
                sauvola_k,
                bg_inpaint,
            },
            bg_bpp,
            EncodeBundleArgs {
                shared_dict_pages,
                thumbnails,
            },
        ),
    }
}

// ── merge ─────────────────────────────────────────────────────────────────────

fn cmd_merge(files: &[PathBuf], output: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if files.is_empty() {
        return Err("no input files".into());
    }

    let docs: Vec<Vec<u8>> = files
        .iter()
        .map(|f| std::fs::read(f).map_err(|e| format!("{}: {e}", f.display())))
        .collect::<Result<_, _>>()?;

    let refs: Vec<&[u8]> = docs.iter().map(|d| d.as_slice()).collect();
    let merged = djvu_rs::djvm::merge(&refs)?;

    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(output, merged)?;
    eprintln!("Merged {} files → {}", files.len(), output.display());
    Ok(())
}

// ── split ─────────────────────────────────────────────────────────────────────

fn cmd_split(
    path: &Path,
    page: Option<usize>,
    pages: Option<&str>,
    output: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read(path)?;

    let (start, end) = if let Some(p) = page {
        if p == 0 {
            return Err("page numbers are 1-based".into());
        }
        (p - 1, p)
    } else if let Some(range) = pages {
        parse_page_range(range)?
    } else {
        return Err("specify --page or --pages".into());
    };

    let result = djvu_rs::djvm::split(&data, start, end)?;

    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(output, result)?;
    eprintln!("Split pages {}–{} → {}", start + 1, end, output.display());
    Ok(())
}

/// Parse "1-50" into (0, 50) — 0-based start, exclusive end.
fn parse_page_range(s: &str) -> Result<(usize, usize), Box<dyn std::error::Error>> {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 2 {
        return Err(format!("invalid page range: {s} (expected N-M)").into());
    }
    let start: usize = parts[0].parse()?;
    let end: usize = parts[1].parse()?;
    if start == 0 || end == 0 || start > end {
        return Err(format!("invalid page range: {s}").into());
    }
    Ok((start - 1, end))
}

// ── info ──────────────────────────────────────────────────────────────────────

fn cmd_info(path: &Path, count_only: bool, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let doc = open(path)?;
    let count = doc.page_count();

    if count_only {
        println!("{count}");
        return Ok(());
    }

    if json {
        // All fields are numeric — no JSON string escaping needed.
        // If string fields (e.g. title, filename) are added in the future,
        // use a proper JSON library (e.g. serde_json) to avoid injection.
        let mut out = String::from("{\"pages\":[");
        for i in 0..count {
            let page = doc.page(i)?;
            if i > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                "{{\"page\":{},\"width\":{},\"height\":{},\"dpi\":{}}}",
                i + 1,
                page.width(),
                page.height(),
                page.dpi(),
            ));
        }
        out.push_str(&format!("],\"count\":{count}}}"));
        println!("{out}");
        return Ok(());
    }

    println!("Pages: {count}");
    for i in 0..count {
        let page = doc.page(i)?;
        println!(
            "  Page {:>4}: {} x {} px  {} dpi",
            i + 1,
            page.width(),
            page.height(),
            page.dpi(),
        );
    }
    Ok(())
}

// ── render ────────────────────────────────────────────────────────────────────

fn to_user_rotation(r: &RotateArg) -> djvu_rs::djvu_render::UserRotation {
    use djvu_rs::djvu_render::UserRotation;
    match r {
        RotateArg::None => UserRotation::None,
        RotateArg::Cw90 => UserRotation::Cw90,
        RotateArg::Rot180 => UserRotation::Rot180,
        RotateArg::Ccw90 => UserRotation::Ccw90,
    }
}

#[allow(clippy::too_many_arguments)]
fn cmd_render(
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
        return render_png_parallel(path, dpi, output);
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

fn render_layer(
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
fn apply_user_rotation(
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
fn pixmap_to_rgba(pm: &djvu_rs::Pixmap) -> Vec<u8> {
    let mut rgba = Vec::with_capacity((pm.width * pm.height * 4) as usize);
    for y in 0..pm.height {
        for x in 0..pm.width {
            let (r, g, b) = pm.get_rgb(x, y);
            rgba.extend_from_slice(&[r, g, b, 255]);
        }
    }
    rgba
}

fn render_png(
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

fn render_pdf_structured(path: &Path, output: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }

    let data = std::fs::read(path)?;
    let doc = djvu_rs::djvu_document::DjVuDocument::parse(&data)?;
    // Stream straight to the file (#606) — the whole PDF is never buffered.
    let file = std::fs::File::create(output)?;
    let mut writer = std::io::BufWriter::new(file);
    djvu_rs::pdf::djvu_to_pdf_to_writer(&doc, &djvu_rs::pdf::PdfOptions::default(), &mut writer)?;
    use std::io::Write;
    writer.flush()?;
    Ok(())
}

#[cfg(feature = "epub")]
fn render_epub_structured(path: &Path, output: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }

    let data = std::fs::read(path)?;
    let doc = djvu_rs::djvu_document::DjVuDocument::parse(&data)?;
    let epub = djvu_rs::epub::djvu_to_epub(&doc, &djvu_rs::epub::EpubOptions::default())?;
    std::fs::write(output, epub)?;
    Ok(())
}

fn render_cbz(
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

    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }

    let data = std::fs::read(path)?;
    let doc = djvu_rs::djvu_document::DjVuDocument::parse(&data)?;
    let opts = djvu_rs::cbz::CbzOptions {
        dpi,
        rotation: rotate,
        pages,
    };

    let file = std::fs::File::create(output)?;
    let mut zip = zip::ZipWriter::new(file);
    djvu_rs::cbz::write_pages(&mut zip, &doc, &opts)?;
    zip.finish()?;
    Ok(())
}

/// Parallel PNG rendering: renders all pages concurrently using rayon, then
/// writes PNGs sequentially.
#[cfg(feature = "parallel")]
fn render_png_parallel(
    path: &Path,
    dpi: u32,
    output: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read(path)?;
    let doc = djvu_rs::djvu_document::DjVuDocument::parse(&data)?;
    std::fs::create_dir_all(output)?;

    let pixmaps = djvu_rs::djvu_render::render_pages_parallel(&doc, dpi);

    for (i, result) in pixmaps.into_iter().enumerate() {
        let pixmap = result?;
        let out = output.join(format!("page_{:04}.png", i + 1));
        let file = std::fs::File::create(&out)?;
        let mut writer = std::io::BufWriter::new(file);
        encode_png(&mut writer, pixmap.width, pixmap.height, &pixmap.data)?;
    }

    Ok(())
}

// ── PNG helpers ───────────────────────────────────────────────────────────────

fn render_page_png(
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

fn encode_png(
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

// ── ocr ──────────────────────────────────────────────────────────────────────

#[cfg(any(
    feature = "ocr-tesseract",
    feature = "ocr-onnx",
    feature = "ocr-neural"
))]
fn cmd_ocr(
    path: &Path,
    backend: OcrBackendChoice,
    lang: &str,
    model_path: Option<&Path>,
    output: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    use djvu_rs::ocr::OcrOptions;

    // Fail early on a misconfigured backend before any per-page work.
    let ocr_backend = build_ocr_backend(backend.clone(), model_path)?;

    let data = std::fs::read(path)?;
    let mut doc_mut = djvu_rs::djvu_mut::DjVuDocumentMut::from_bytes(&data)?;
    let _ = doc_mut.page_mut(0)?;

    let doc = djvu_rs::djvu_document::DjVuDocument::parse(&data)?;

    // OCR each page and inject the recognized text layer. Pages are
    // independent and OCR dominates wall-clock, so with the `parallel`
    // feature the render+recognize fan out over rayon (#573) — one backend
    // instance per task (`recognize` builds a fresh Tesseract per call, so
    // instances never cross threads); text layers are injected sequentially
    // in page order afterwards, keeping the output bytes identical to the
    // sequential path.
    let count = doc.page_count();
    let ocr_one = |i: usize,
                   be: &dyn djvu_rs::ocr::OcrBackend|
     -> Result<djvu_rs::text::TextLayer, String> {
        let page = doc.page(i).map_err(|e| e.to_string())?;
        let w = page.width() as u32;
        let h = page.height() as u32;
        let opts = djvu_rs::djvu_render::RenderOptions {
            width: w,
            height: h,
            ..Default::default()
        };
        let pixmap = djvu_rs::djvu_render::render_pixmap(page, &opts).map_err(|e| e.to_string())?;
        // The render above is at the page's native resolution — tell the
        // recognizer the true dpi (#603: a hard-coded 300 mis-scaled OCR on
        // 400/600-dpi scans; Tesseract's segmentation is dpi-sensitive).
        let options = OcrOptions {
            languages: lang.to_string(),
            dpi: page.dpi() as u32,
        };
        be.recognize(&pixmap, &options).map_err(|e| e.to_string())
    };

    #[cfg(feature = "parallel")]
    let layers: Vec<djvu_rs::text::TextLayer> = {
        use rayon::prelude::*;
        drop(ocr_backend);
        let model_path = model_path.map(Path::to_path_buf);
        (0..count)
            .into_par_iter()
            .map(|i| {
                let be = build_ocr_backend(backend.clone(), model_path.as_deref())
                    .map_err(|e| e.to_string())?;
                ocr_one(i, be.as_ref())
            })
            .collect::<Result<Vec<_>, String>>()?
    };
    #[cfg(not(feature = "parallel"))]
    let layers: Vec<djvu_rs::text::TextLayer> = (0..count)
        .map(|i| ocr_one(i, ocr_backend.as_ref()))
        .collect::<Result<Vec<_>, String>>()?;

    for (i, text_layer) in layers.iter().enumerate() {
        eprintln!(
            "Page {}: {} chars, {} zones",
            i + 1,
            text_layer.text.len(),
            text_layer.zones.len()
        );
        doc_mut.page_mut(i)?.set_text_layer(text_layer)?;
    }

    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }

    std::fs::write(output, doc_mut.try_into_bytes()?)?;
    eprintln!(
        "OCR complete. Embedded text layers for {count} page(s) into {}",
        output.display()
    );

    Ok(())
}

#[cfg(any(
    feature = "ocr-tesseract",
    feature = "ocr-onnx",
    feature = "ocr-neural"
))]
fn build_ocr_backend(
    backend: OcrBackendChoice,
    model_path: Option<&Path>,
) -> Result<Box<dyn djvu_rs::ocr::OcrBackend>, Box<dyn std::error::Error>> {
    match backend {
        OcrBackendChoice::Tesseract => {
            let _ = model_path;
            #[cfg(feature = "ocr-tesseract")]
            {
                Ok(Box::new(djvu_rs::ocr_tesseract::TesseractBackend::new()))
            }
            #[cfg(not(feature = "ocr-tesseract"))]
            {
                Err(
                    "Tesseract OCR backend is not enabled; rebuild with --features ocr-tesseract"
                        .into(),
                )
            }
        }
        OcrBackendChoice::Onnx => {
            let _ = model_path;
            Err(
                "ONNX OCR backend is experimental library-only and has no stable CLI model \
                 contract yet; use --backend tesseract with --features ocr-tesseract"
                    .into(),
            )
        }
        OcrBackendChoice::Candle => {
            let _ = model_path;
            Err(
                "Candle OCR backend is experimental and has no supported model-specific \
                 implementation yet; use --backend tesseract with --features ocr-tesseract"
                    .into(),
            )
        }
    }
}

// ── text ──────────────────────────────────────────────────────────────────────

fn cmd_text(
    path: &Path,
    page: usize,
    all: bool,
    format: TextFormat,
    output: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    match format {
        TextFormat::Plain => {
            let doc = open(path)?;
            let count = doc.page_count();
            let mut text = String::new();
            if all {
                for i in 0..count {
                    text.push_str(&format!("--- Page {} ---\n", i + 1));
                    collect_page_text(&doc, i, &mut text)?;
                }
            } else {
                let idx = page_idx(page, count)?;
                collect_page_text(&doc, idx, &mut text)?;
            }
            write_or_print(output, &text)?;
        }
        TextFormat::Hocr => {
            let data = std::fs::read(path)?;
            let doc = djvu_rs::djvu_document::DjVuDocument::parse(&data)?;
            let opts = djvu_rs::text_serialize::HocrOptions {
                page_index: if all {
                    None
                } else {
                    Some(page_idx(page, doc.page_count())?)
                },
                dpi: None,
            };
            let hocr = djvu_rs::text_serialize::to_hocr(&doc, &opts)?;
            write_or_print(output, &hocr)?;
        }
        TextFormat::Alto => {
            let data = std::fs::read(path)?;
            let doc = djvu_rs::djvu_document::DjVuDocument::parse(&data)?;
            let opts = djvu_rs::text_serialize::AltoOptions {
                page_index: if all {
                    None
                } else {
                    Some(page_idx(page, doc.page_count())?)
                },
                dpi: None,
            };
            let alto = djvu_rs::text_serialize::to_alto(&doc, &opts)?;
            write_or_print(output, &alto)?;
        }
    }
    Ok(())
}

fn write_or_print(output: Option<&Path>, content: &str) -> Result<(), Box<dyn std::error::Error>> {
    match output {
        Some(path) => {
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, content)?;
        }
        None => print!("{content}"),
    }
    Ok(())
}

fn collect_page_text(
    doc: &Document,
    idx: usize,
    buf: &mut String,
) -> Result<(), Box<dyn std::error::Error>> {
    let page = doc.page(idx)?;
    match page.text()? {
        Some(text) if !text.trim().is_empty() => buf.push_str(&text),
        _ => buf.push_str("No text layer\n"),
    }
    Ok(())
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn open(path: &Path) -> Result<Document, Box<dyn std::error::Error>> {
    if !path.exists() {
        return Err(format!("{}: no such file", path.display()).into());
    }
    let data = std::fs::read(path)?;
    let doc = Document::from_bytes(data).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(doc)
}

/// Convert 1-based user page number to 0-based index, with bounds check.
fn page_idx(page: usize, count: usize) -> Result<usize, Box<dyn std::error::Error>> {
    if page == 0 || page > count {
        return Err(format!("page {page} out of range (document has {count} pages)").into());
    }
    Ok(page - 1)
}

// ── bzz encode/decode ────────────────────────────────────────────────────────

fn cmd_bzz_encode(file: &Path, output: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read(file)?;
    let compressed = djvu_rs::bzz_encode::bzz_encode(&data);
    std::fs::write(output, &compressed)?;
    eprintln!(
        "{}: {} → {} bytes ({:.1}%)",
        file.display(),
        data.len(),
        compressed.len(),
        if data.is_empty() {
            0.0
        } else {
            compressed.len() as f64 / data.len() as f64 * 100.0
        }
    );
    Ok(())
}

fn cmd_bzz_decode(file: &Path, output: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read(file)?;
    let decoded = djvu_rs::bzz::bzz_decode(&data)?;
    std::fs::write(output, &decoded)?;
    eprintln!(
        "{}: {} → {} bytes",
        file.display(),
        data.len(),
        decoded.len(),
    );
    Ok(())
}

// ── encode ───────────────────────────────────────────────────────────────────

fn cmd_encode(
    input: &Path,
    output: &Path,
    dpi: u16,
    quality: EncodeQualityArg,
    segment_args: EncodeSegmentArgs,
    bg_bpp: Option<f32>,
    bundle_args: EncodeBundleArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let EncodeBundleArgs {
        shared_dict_pages,
        thumbnails,
    } = bundle_args;
    use djvu_rs::djvu_encode::{EncodeQuality, PageEncoder};
    use djvu_rs::iw44_encode::{Iw44EncodeOptions, Iw44Target};
    use djvu_rs::jb2_encode::encode_djvm_bundle_jb2;
    use djvu_rs::segment::{SegmentOptions, segment_page};

    let q = match quality {
        EncodeQualityArg::Lossless => EncodeQuality::Lossless,
        EncodeQualityArg::Quality | EncodeQualityArg::Auto => EncodeQuality::Quality,
        EncodeQualityArg::Archival => EncodeQuality::Archival,
        EncodeQualityArg::Photo => EncodeQuality::Photo,
    };
    let segment_options = segment_args.to_options(q)?;

    if input.is_dir() {
        let entries = directory_image_entries(input)?;

        // --quality auto on a directory (#570): classify every page; the
        // bundle writer supports lossless-bilevel or layered bundles, so the
        // decision is bundle-wide — all pages bilevel → Lossless, anything
        // else → Quality (a Photo-classified page inside a bundle also goes
        // layered; per-page mixed bundles are the recorded follow-up).
        let quality = if matches!(quality, EncodeQualityArg::Auto) {
            let mut all_bilevel = true;
            for path in &entries {
                let pm = djvu_rs::png_io::decode_image_to_pixmap(path)?;
                if djvu_rs::djvu_encode::classify_content(&pm)
                    != djvu_rs::djvu_encode::EncodeQuality::Lossless
                {
                    all_bilevel = false;
                    break;
                }
            }
            let picked = if all_bilevel {
                EncodeQualityArg::Lossless
            } else {
                EncodeQualityArg::Quality
            };
            eprintln!("auto profile (bundle): {picked:?}");
            picked
        } else {
            quality
        };

        if matches!(quality, EncodeQualityArg::Lossless) {
            if thumbnails {
                eprintln!("--thumbnails is ignored for lossless (JB2-only) bundles");
            }
            let mut masks = Vec::with_capacity(entries.len());
            for path in &entries {
                let pixmap = djvu_rs::png_io::decode_image_to_pixmap(path)?;
                let seg = segment_page(&pixmap, &SegmentOptions::default());
                masks.push(seg.mask);
            }
            let bytes = encode_djvm_bundle_jb2(&masks, shared_dict_pages, dpi);
            std::fs::write(output, &bytes)?;
            eprintln!(
                "{} pages → {} ({} bytes, shared-dict threshold = {})",
                entries.len(),
                output.display(),
                bytes.len(),
                shared_dict_pages,
            );
            return Ok(());
        }

        // #452: layered multi-page now shares a Djbz dictionary across pages,
        // honoring --shared-dict-pages (was: per-page independent masks).
        let mut pixmaps = Vec::with_capacity(entries.len());
        for path in &entries {
            pixmaps.push(djvu_rs::png_io::decode_image_to_pixmap(path)?);
        }
        let bytes = djvu_rs::djvu_encode::encode_djvm_layered_shared_with_thumbnails(
            &pixmaps,
            q,
            dpi,
            segment_options,
            shared_dict_pages,
            thumbnails,
        )
        .map_err(|e| format!("layered encode: {e}"))?;
        std::fs::write(output, &bytes)?;
        eprintln!(
            "{} pages → {} ({} bytes, layered {:?}, shared-dict threshold = {}, thumbnails = {})",
            entries.len(),
            output.display(),
            bytes.len(),
            q,
            shared_dict_pages,
            thumbnails,
        );
        return Ok(());
    }

    if thumbnails {
        eprintln!("--thumbnails applies to multi-page bundles only — ignored");
    }
    let pixmap = djvu_rs::png_io::decode_image_to_pixmap(input)?;

    // --quality auto (#570): pick the profile from cheap pixel statistics.
    let q = if matches!(quality, EncodeQualityArg::Auto) {
        let detected = djvu_rs::djvu_encode::classify_content(&pixmap);
        eprintln!("auto profile: {detected:?}");
        detected
    } else {
        q
    };

    let bytes = match q {
        EncodeQuality::Lossless => {
            let seg = segment_page(&pixmap, &SegmentOptions::default());
            PageEncoder::from_bitmap(&seg.mask)
                .with_dpi(dpi)
                .with_quality(EncodeQuality::Lossless)
                .encode()
        }
        EncodeQuality::Quality | EncodeQuality::Archival | EncodeQuality::Photo => {
            let mut encoder = PageEncoder::from_pixmap(&pixmap)
                .with_dpi(dpi)
                .with_quality(q);
            if let Some(opts) = segment_options {
                encoder = encoder.with_segment_options(opts);
            }
            if let Some(bpp) = bg_bpp {
                let iw44_opts = Iw44EncodeOptions {
                    target: Iw44Target::Bpp(bpp),
                    ..Iw44EncodeOptions::default()
                };
                encoder = encoder.with_iw44_options(iw44_opts);
            }
            encoder.encode()
        }
    }
    .map_err(|e| format!("encode: {e}"))?;

    std::fs::write(output, &bytes)?;
    eprintln!(
        "{} → {} ({}×{} px, {} bytes)",
        input.display(),
        output.display(),
        pixmap.width,
        pixmap.height,
        bytes.len(),
    );
    Ok(())
}

/// Multi-page-bundle options of `djvu encode` (single-page paths ignore them).
#[derive(Clone, Copy)]
struct EncodeBundleArgs {
    shared_dict_pages: usize,
    thumbnails: bool,
}

#[derive(Clone, Copy)]
struct EncodeSegmentArgs {
    binarization: BinarizationArg,
    sauvola_window: u32,
    sauvola_k: f32,
    bg_inpaint: bool,
}

impl EncodeSegmentArgs {
    fn to_options(
        self,
        quality: djvu_rs::djvu_encode::EncodeQuality,
    ) -> Result<Option<djvu_rs::segment::SegmentOptions>, Box<dyn std::error::Error>> {
        use djvu_rs::djvu_encode::EncodeQuality;
        use djvu_rs::segment::Binarization;

        let has_segment_flags = self.binarization != BinarizationArg::Fixed || self.bg_inpaint;
        if !has_segment_flags {
            return Ok(None);
        }
        if matches!(quality, EncodeQuality::Lossless) {
            return Err(
                "--binarization and --bg-inpaint require --quality quality or --quality archival"
                    .into(),
            );
        }

        let mut opts = quality.default_segment_options();
        opts.binarization = match self.binarization {
            BinarizationArg::Fixed => Binarization::Fixed,
            BinarizationArg::Sauvola => Binarization::Sauvola {
                window: self.sauvola_window,
                k: self.sauvola_k,
            },
        };
        opts.bg_inpaint = self.bg_inpaint;
        if self.bg_inpaint {
            // `--bg-inpaint` explicitly selects the ring-average fill, so turn
            // off the colour profile's default harmonic diffusion (which would
            // otherwise take precedence and make the flag a no-op).
            opts.bg_diffuse = false;
        }
        Ok(Some(opts))
    }
}

fn directory_image_entries(dir: &Path) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                    matches!(
                        e.to_ascii_lowercase().as_str(),
                        "png" | "jpg" | "jpeg" | "tif" | "tiff"
                    )
                })
        })
        .collect();
    entries.sort();
    if entries.is_empty() {
        return Err(format!(
            "{}: no image files found in directory (supported: .png, .jpg, .jpeg, .tif, .tiff)",
            dir.display()
        )
        .into());
    }
    Ok(entries)
}
