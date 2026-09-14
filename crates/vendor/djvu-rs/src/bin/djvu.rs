use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use clap::{Parser, Subcommand, ValueEnum};
use djvu_rs::{
    ComponentGraph, ComponentNodeKind, Document,
    iff::ChunkRecord,
    validate::{Layer as ValidationLayer, ResourceLimits, ValidateOptions, ValidationReport},
};
use serde_json::{Map, Value, json};

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
    /// Inspect the IFF chunk tree without decoding page content.
    ///
    /// With `--json`, emits a stable JSON object with `file`, `container`, and
    /// `chunks` keys. Every chunk object has `id`, `offset`, `length`, `depth`,
    /// and `path`; FORM objects additionally have `form_type`; embedded bundled
    /// component FORM objects additionally have `component_id` and `kind`.
    /// The shape is `{ "file": "book.djvu", "container": "DJVM", "chunks":
    /// [{ "id": "FORM", "form_type": "DJVM", "offset": 4, "length": 0,
    /// "depth": 0, "path": [] }], "components": [] }`.
    /// `path` is an array of zero-based child indexes from the root. When the
    /// bundled component graph parses, the object also has `components`, whose
    /// entries contain `id`, `kind`, `dirm_index`, `includes`, and `included_by`.
    /// The `components` key is omitted when graph parsing fails so inspection
    /// remains useful for diagnostically malformed documents.
    Inspect {
        /// Path to the DjVu file.
        file: PathBuf,
        /// Output stable machine-readable JSON.
        #[arg(short, long)]
        json: bool,
    },
    /// Validate document structure, dependencies, codec streams, and resource
    /// limits without rendering.
    ///
    /// Exits 0 when no errors are found; with --strict, warnings also exit 1.
    /// Exits 1 for validation findings and 2 when the input file (or a
    /// --limits file) cannot be read or parsed.
    ///
    /// A --limits JSON file may set any of `max_file_bytes`, `max_pages`,
    /// `max_components`, `max_page_pixels`, `max_total_pixels`, and
    /// `max_decoded_bytes` (all optional, unsigned integers). Exceeded limits
    /// are reported as resource-layer errors before any page decode, and a
    /// decode-cost limit additionally suppresses --decode-pages work.
    Validate {
        /// Path to the DjVu file.
        file: PathBuf,
        /// Treat warnings as a failing result for the process exit code.
        #[arg(long)]
        strict: bool,
        /// Output stable machine-readable JSON.
        #[arg(short, long)]
        json: bool,
        /// Decode IW44 coefficients and JB2 symbols, without RGB rendering.
        #[arg(long)]
        decode_pages: bool,
        /// Path to a JSON file of configured resource limits.
        #[arg(long)]
        limits: Option<PathBuf>,
    },
    /// Compare two documents semantically: page properties, text, annotations,
    /// metadata, bookmarks, and the component graph.
    ///
    /// Exits 0 when every compared plane matches, 1 when any plane diverges,
    /// and 2 when either input cannot be read or parsed.
    Diff {
        /// First DjVu file.
        a: PathBuf,
        /// Second DjVu file.
        b: PathBuf,
        /// Output stable machine-readable JSON.
        #[arg(short, long)]
        json: bool,
        /// Compare only the named planes (repeatable). Default: all planes.
        #[arg(long = "plane")]
        planes: Vec<String>,
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
    /// Plan or apply safe document-level optimization.
    Optimize {
        /// Path to the input DjVu file.
        file: PathBuf,
        /// Output file path. Required even for --dry-run so scripts can use
        /// one stable invocation shape; dry-run never creates it.
        #[arg(short, long)]
        output: PathBuf,
        /// Optimization policy.
        #[arg(short, long, default_value = "lossless-cleanup", value_enum)]
        preset: OptimizePresetArg,
        /// Maximum output size in bytes. Safe cleanup reports when it cannot
        /// meet the target without lossy re-encoding.
        #[arg(long)]
        target_size: Option<u64>,
        /// Maximum permitted SSIM loss.
        #[arg(long)]
        max_ssim_loss: Option<f32>,
        /// Print the machine-readable plan without writing the output.
        #[arg(long)]
        dry_run: bool,
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
        /// Unused: --backend onnx loads its models from the pinned manifest
        /// (fetch with scripts/fetch_ocr_models.sh; directory override via
        /// DJVU_OCR_MODELS_DIR). Kept for CLI-shape stability.
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
        /// Page DPI stored in the INFO chunk. Default: the input's TIFF
        /// X/YResolution tags when present, else 300.
        #[arg(short, long)]
        dpi: Option<u16>,
        /// Encoding profile.
        #[arg(short, long, default_value = "lossless", value_enum)]
        quality: EncodeQualityArg,
        /// Bilevel mask codec for single-image lossless encodes. Default: jb2.
        #[arg(long, default_value = "jb2", value_enum)]
        bilevel_codec: BilevelCodecArg,
        /// Composite transparent pixels onto this background colour at
        /// decode time (PNG/TIFF alpha; JPEG never has alpha). Accepts
        /// RRGGBB hex with optional '#', or 'white'/'black'. Default:
        /// preserve the alpha channel unchanged.
        #[arg(long, value_parser = parse_background_color)]
        background: Option<(u8, u8, u8)>,
        /// Embedded ICC colour profile handling. DjVu cannot store a
        /// profile and no colour management is applied, so 'ignore'
        /// (default) decodes pixel bytes as-is and drops the profile;
        /// 'reject' fails with an explicit error on profiled input.
        #[arg(long, default_value = "ignore", value_enum)]
        icc: IccArg,
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
        /// Classify 32x32 blocks as text vs photo/halftone and route photo
        /// blocks wholly to the background layer (quality/archival only).
        #[arg(long)]
        block_classify: bool,
        /// Content-adaptive background subsample: densify the BG grid when
        /// unmasked detail warrants it (pairs well with --block-classify).
        #[arg(long)]
        adaptive_bg_subsample: bool,
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

#[derive(Clone, ValueEnum)]
enum OptimizePresetArg {
    /// Remove semantically inert IFF FREE padding.
    LosslessCleanup,
    /// Prefer archival fidelity; this slice remains pixel-exact.
    Archival,
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
    /// Neural PP-OCR pipeline (DBNet detection + Cyrillic CTC recognition)
    /// using the pinned model manifest; fetch models first with
    /// scripts/fetch_ocr_models.sh.
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
    /// Pixel-exact bilevel JB2 (`INFO + Sjbz`), unless `--bilevel-codec smmr`.
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

#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
enum BilevelCodecArg {
    /// JB2 arithmetic-coded mask (`Sjbz`), the compatibility default.
    Jb2,
    /// G4/MMR mask (`Smmr`) for explicit single-page bilevel encoding.
    Smmr,
}

#[derive(Clone, Copy, ValueEnum, PartialEq, Eq)]
enum IccArg {
    /// Decode without colour management; the embedded profile is dropped.
    Ignore,
    /// Fail with an explicit error when the input embeds an ICC profile.
    Reject,
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
        if let Some(exit) = e.downcast_ref::<ValidateExit>() {
            if !exit.silent {
                eprintln!("error: {exit}");
            }
            std::process::exit(exit.code);
        }
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.command {
        Cmd::Info { file, count, json } => cmd_info(&file, count, json),
        Cmd::Inspect { file, json } => cmd_inspect(&file, json),
        Cmd::Validate {
            file,
            strict,
            json,
            decode_pages,
            limits,
        } => cmd_validate(&file, strict, json, decode_pages, limits.as_deref()),
        Cmd::Diff { a, b, json, planes } => cmd_diff(&a, &b, json, &planes),
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
        Cmd::Optimize {
            file,
            output,
            preset,
            target_size,
            max_ssim_loss,
            dry_run,
        } => cmd_optimize(&file, &output, preset, target_size, max_ssim_loss, dry_run),
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
            bilevel_codec,
            background,
            icc,
            binarization,
            sauvola_window,
            sauvola_k,
            bg_inpaint,
            block_classify,
            adaptive_bg_subsample,
            bg_bpp,
            shared_dict_pages,
            thumbnails,
        } => cmd_encode(
            &input,
            &output,
            dpi,
            EncodeProfileArgs {
                quality,
                bilevel_codec,
                background,
                icc,
            },
            EncodeSegmentArgs {
                binarization,
                sauvola_window,
                sauvola_k,
                bg_inpaint,
                block_classify,
                adaptive_bg_subsample,
            },
            bg_bpp,
            EncodeBundleArgs {
                shared_dict_pages,
                thumbnails,
            },
        ),
    }
}

#[derive(Debug)]
struct ValidateExit {
    code: i32,
    silent: bool,
    message: String,
}

impl std::fmt::Display for ValidateExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ValidateExit {}

// ── optimize ─────────────────────────────────────────────────────────────────

fn cmd_optimize(
    input: &Path,
    output: &Path,
    preset: OptimizePresetArg,
    target_size: Option<u64>,
    max_ssim_loss: Option<f32>,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let input_bytes = std::fs::read(input)?;
    if equivalent_paths(input, output)? {
        return Err(
            "optimizer refuses to replace the input file; choose a different --output".into(),
        );
    }

    let preset = match preset {
        OptimizePresetArg::LosslessCleanup => {
            djvu_rs::optimizer::OptimizationPreset::LosslessCleanup
        }
        OptimizePresetArg::Archival => djvu_rs::optimizer::OptimizationPreset::Archival,
    };
    let mut request = djvu_rs::optimizer::OptimizationRequest::new(preset);
    if let Some(target) = target_size {
        request = request.with_target_size(target);
    }
    if let Some(loss) = max_ssim_loss {
        request = request.with_max_ssim_loss(loss);
    }

    let optimizer = djvu_rs::optimizer::Optimizer::new(request);
    if dry_run {
        println!("{}", optimizer.plan(&input_bytes)?.to_json());
        return Ok(());
    }

    let result = optimizer.optimize(&input_bytes)?;
    write_atomic(output, &result.bytes)?;
    println!("{}", result.report.to_json());
    Ok(())
}

fn equivalent_paths(input: &Path, output: &Path) -> Result<bool, Box<dyn std::error::Error>> {
    let input = std::fs::canonicalize(input)?;
    let output = if output.exists() {
        std::fs::canonicalize(output)?
    } else {
        let parent = output.parent().unwrap_or_else(|| Path::new("."));
        std::fs::canonicalize(parent)?.join(output.file_name().ok_or("--output must name a file")?)
    };
    Ok(input == output)
}

fn write_atomic(output: &Path, bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    write_atomic_with(output, |mut file| {
        use std::io::Write;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })
}

/// Write an output through a sibling temporary path, committing it only after
/// `write` succeeds. The temporary path is always removed when `write` or the
/// final rename fails, so an existing destination is left untouched on error.
fn write_atomic_with<F>(output: &Path, write: F) -> Result<(), Box<dyn std::error::Error>>
where
    F: FnOnce(std::fs::File) -> Result<(), Box<dyn std::error::Error>>,
{
    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    let name = output
        .file_name()
        .ok_or("--output must name a file")?
        .to_string_lossy();
    let temp = parent.join(format!(".{name}.{}.tmp", std::process::id()));
    let file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)?;
    if let Err(error) = write(file) {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&temp, output) {
        let _ = std::fs::remove_file(&temp);
        return Err(error.into());
    }
    Ok(())
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
        let mut pages = Vec::with_capacity(count);
        for i in 0..count {
            let page = doc.page(i)?;
            pages.push(json!({
                "page": i + 1,
                "width": page.width(),
                "height": page.height(),
                "dpi": page.dpi(),
            }));
        }
        println!(
            "{}",
            serde_json::to_string(&json!({ "pages": pages, "count": count }))?
        );
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

// ── inspect ──────────────────────────────────────────────────────────────────

struct InspectComponents {
    by_form_offset: BTreeMap<usize, (String, String)>,
    json: Vec<Value>,
}

fn cmd_inspect(path: &Path, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read(path)?;
    let chunks = djvu_rs::iff::walk_chunks(&data)?;
    let container = chunks
        .first()
        .and_then(|chunk| chunk.form_type)
        .map(chunk_id_text)
        .ok_or("IFF walker did not return a root FORM")?;
    let components = inspect_components(&data, &chunks);

    if json {
        let mut output = Map::new();
        output.insert("file".into(), Value::String(path.display().to_string()));
        output.insert("container".into(), Value::String(container));
        output.insert(
            "chunks".into(),
            Value::Array(
                chunks
                    .iter()
                    .map(|chunk| inspect_chunk_json(chunk, components.as_ref()))
                    .collect(),
            ),
        );
        if let Some(components) = &components {
            output.insert("components".into(), Value::Array(components.json.clone()));
        }
        println!("{}", serde_json::to_string(&Value::Object(output))?);
    } else {
        print_inspect_human(&chunks, components.as_ref());
    }

    Ok(())
}

// ── validate ────────────────────────────────────────────────────────────────

fn cmd_validate(
    path: &Path,
    strict: bool,
    json: bool,
    decode_pages: bool,
    limits_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read(path).map_err(|error| ValidateExit {
        code: 2,
        silent: false,
        message: format!("cannot read {}: {error}", path.display()),
    })?;
    let limits = match limits_path {
        Some(limits_path) => Some(load_limits(limits_path)?),
        None => None,
    };
    let options = ValidateOptions {
        strict,
        decode_pages,
        limits,
    };
    let report = djvu_rs::validate::validate(&data, &options);
    let summary = report.summary();

    if json {
        println!("{}", serde_json::to_string(&validate_json(path, &report))?);
    } else {
        print_validate_human(&report);
    }

    if !report.is_valid() || (strict && summary.warnings > 0) {
        return Err(Box::new(ValidateExit {
            code: 1,
            silent: true,
            message: String::new(),
        }));
    }
    Ok(())
}

/// Load configured resource limits from a JSON file. Unknown keys and
/// non-integer values are rejected so a mistyped limit fails loudly rather than
/// silently disabling a guard. Every failure maps to the read/parse exit code 2.
fn load_limits(path: &Path) -> Result<ResourceLimits, Box<dyn std::error::Error>> {
    let fail = |message: String| ValidateExit {
        code: 2,
        silent: false,
        message,
    };
    let text = std::fs::read_to_string(path)
        .map_err(|error| fail(format!("cannot read limits {}: {error}", path.display())))?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|error| fail(format!("cannot parse limits {}: {error}", path.display())))?;
    let object = value
        .as_object()
        .ok_or_else(|| fail(format!("limits {} must be a JSON object", path.display())))?;

    const KNOWN: [&str; 7] = [
        "max_file_bytes",
        "max_pages",
        "max_components",
        "max_page_pixels",
        "max_total_pixels",
        "max_decoded_bytes",
        "max_render_pixels",
    ];
    for key in object.keys() {
        if !KNOWN.contains(&key.as_str()) {
            return Err(Box::new(fail(format!(
                "limits {}: unknown key '{key}'",
                path.display()
            ))));
        }
    }

    let read = |key: &str| -> Result<Option<u64>, Box<dyn std::error::Error>> {
        match object.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Number(number)) if number.is_u64() => Ok(number.as_u64()),
            Some(_) => Err(Box::new(fail(format!(
                "limits {}: '{key}' must be a non-negative integer",
                path.display()
            ))) as Box<dyn std::error::Error>),
        }
    };

    Ok(ResourceLimits {
        max_file_bytes: read("max_file_bytes")?,
        max_pages: read("max_pages")?,
        max_components: read("max_components")?,
        max_page_pixels: read("max_page_pixels")?,
        max_total_pixels: read("max_total_pixels")?,
        max_decoded_bytes: read("max_decoded_bytes")?,
        max_render_pixels: read("max_render_pixels")?,
    })
}

// ── diff ─────────────────────────────────────────────────────────────────────

fn cmd_diff(
    a: &Path,
    b: &Path,
    json: bool,
    planes: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let read = |path: &Path| {
        std::fs::read(path).map_err(|error| ValidateExit {
            code: 2,
            silent: false,
            message: format!("cannot read {}: {error}", path.display()),
        })
    };
    let bytes_a = read(a)?;
    let bytes_b = read(b)?;
    let filter = (!planes.is_empty()).then_some(planes);
    let diff =
        djvu_rs::semantic_diff::semantic_diff(&bytes_a, &bytes_b, filter).map_err(|error| {
            ValidateExit {
                code: 2,
                silent: false,
                message: format!("cannot parse inputs: {error}"),
            }
        })?;

    if json {
        let planes_json: Vec<serde_json::Value> = diff
            .planes
            .iter()
            .map(|plane| {
                serde_json::json!({
                    "plane": plane.plane,
                    "status": match plane.status {
                        djvu_rs::semantic_diff::PlaneStatus::Match => "match",
                        djvu_rs::semantic_diff::PlaneStatus::Diverge => "diverge",
                    },
                    "details": plane.details,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "a": a.display().to_string(),
                "b": b.display().to_string(),
                "identical": diff.is_identical(),
                "planes": planes_json,
            })
        );
    } else {
        for plane in &diff.planes {
            match plane.status {
                djvu_rs::semantic_diff::PlaneStatus::Match => {
                    println!("{}: match", plane.plane);
                }
                djvu_rs::semantic_diff::PlaneStatus::Diverge => {
                    println!("{}: diverge", plane.plane);
                    for detail in &plane.details {
                        println!("  {detail}");
                    }
                }
            }
        }
    }

    if !diff.is_identical() {
        return Err(Box::new(ValidateExit {
            code: 1,
            silent: true,
            message: String::new(),
        }));
    }
    Ok(())
}

fn print_validate_human(report: &ValidationReport) {
    for layer in [
        ValidationLayer::Structural,
        ValidationLayer::Dependency,
        ValidationLayer::Codec,
        ValidationLayer::Semantic,
        ValidationLayer::Resource,
    ] {
        let findings = report
            .findings
            .iter()
            .filter(|finding| finding.layer == layer)
            .collect::<Vec<_>>();
        if findings.is_empty() {
            continue;
        }
        println!("{}:", layer.as_str());
        for finding in findings {
            let location = match (&finding.component, &finding.chunk, finding.offset) {
                (Some(component), Some(chunk), Some(offset)) => {
                    format!(" [{component} {chunk} @ {offset}]")
                }
                (Some(component), Some(chunk), None) => format!(" [{component} {chunk}]"),
                (Some(component), None, Some(offset)) => format!(" [{component} @ {offset}]"),
                (None, Some(chunk), Some(offset)) => format!(" [{chunk} @ {offset}]"),
                (Some(component), None, None) => format!(" [{component}]"),
                (None, Some(chunk), None) => format!(" [{chunk}]"),
                (None, None, Some(offset)) => format!(" [@ {offset}]"),
                (None, None, None) => String::new(),
            };
            println!(
                "  {} {}{}: {}",
                finding.severity.as_str().to_uppercase(),
                finding.code,
                location,
                finding.message
            );
        }
    }
    let resources = &report.resources;
    println!(
        "resources: {} pages, {} components, {} bytes, {} peak page pixels, {} est. peak decoded bytes",
        resources.pages,
        resources.components,
        resources.file_bytes,
        resources.max_page_pixels,
        resources.peak_decoded_bytes,
    );
    let summary = report.summary();
    println!(
        "{} errors, {} warnings, {} tolerated, {} recovery",
        summary.errors, summary.warnings, summary.tolerated, summary.recovery
    );
}

fn validate_json(path: &Path, report: &ValidationReport) -> Value {
    let summary = report.summary();
    let resources = &report.resources;
    json!({
        "file": path.display().to_string(),
        "valid": report.is_valid(),
        "summary": {
            "errors": summary.errors,
            "warnings": summary.warnings,
            "tolerated": summary.tolerated,
            "recovery": summary.recovery,
        },
        "resources": {
            "file_bytes": resources.file_bytes,
            "pages": resources.pages,
            "components": resources.components,
            "max_page_pixels": resources.max_page_pixels,
            "total_pixels": resources.total_pixels,
            "peak_decoded_bytes": resources.peak_decoded_bytes,
        },
        "findings": report.findings.iter().map(|finding| json!({
            "severity": finding.severity.as_str(),
            "layer": finding.layer.as_str(),
            "code": finding.code,
            "component": &finding.component,
            "chunk": &finding.chunk,
            "offset": finding.offset,
            "message": &finding.message,
        })).collect::<Vec<_>>(),
    })
}

fn inspect_components(data: &[u8], chunks: &[ChunkRecord]) -> Option<InspectComponents> {
    let graph = ComponentGraph::parse(data).ok()?;
    let component_forms: Vec<_> = chunks
        .iter()
        .filter(|chunk| chunk.depth == 1 && chunk.id == *b"FORM")
        .collect();
    let mut by_form_offset = BTreeMap::new();

    for node in graph.nodes() {
        let form = component_forms.get(node.dirm_index)?;
        by_form_offset.insert(
            form.offset,
            (node.id.clone(), component_kind_text(node.kind).to_owned()),
        );
    }

    let json = graph
        .nodes()
        .iter()
        .map(|node| {
            let includes: Vec<_> = node
                .includes
                .iter()
                .filter_map(|&index| graph.nodes().get(index))
                .map(|target| target.id.clone())
                .collect();
            let included_by: Vec<_> = node
                .included_by
                .iter()
                .filter_map(|&index| graph.nodes().get(index))
                .map(|source| source.id.clone())
                .collect();
            json!({
                "id": node.id,
                "kind": component_kind_text(node.kind),
                "dirm_index": node.dirm_index,
                "includes": includes,
                "included_by": included_by,
            })
        })
        .collect();

    Some(InspectComponents {
        by_form_offset,
        json,
    })
}

fn inspect_chunk_json(chunk: &ChunkRecord, components: Option<&InspectComponents>) -> Value {
    let mut json = Map::new();
    json.insert("id".into(), Value::String(chunk_id_text(chunk.id)));
    if let Some(form_type) = chunk.form_type {
        json.insert("form_type".into(), Value::String(chunk_id_text(form_type)));
    }
    json.insert("offset".into(), Value::from(chunk.offset as u64));
    json.insert("length".into(), Value::from(chunk.length as u64));
    json.insert("depth".into(), Value::from(chunk.depth as u64));
    json.insert(
        "path".into(),
        Value::Array(
            chunk
                .path
                .iter()
                .map(|index| Value::from(*index as u64))
                .collect(),
        ),
    );
    if let Some((id, kind)) =
        components.and_then(|components| components.by_form_offset.get(&chunk.offset))
    {
        json.insert("component_id".into(), Value::String(id.clone()));
        json.insert("kind".into(), Value::String(kind.clone()));
    }
    Value::Object(json)
}

fn print_inspect_human(chunks: &[ChunkRecord], components: Option<&InspectComponents>) {
    for chunk in chunks {
        let name = match chunk.form_type {
            Some(form_type) => format!("FORM:{}", chunk_id_text(form_type)),
            None => chunk_id_text(chunk.id),
        };
        let component = components
            .and_then(|components| components.by_form_offset.get(&chunk.offset))
            .map(|(id, _)| format!(" {{{id}}}"))
            .unwrap_or_default();
        println!(
            "{}{} [{}] @ 0x{:08x}{}",
            "  ".repeat(chunk.depth),
            name,
            chunk.length,
            chunk.offset,
            component
        );
    }
}

fn chunk_id_text(id: [u8; 4]) -> String {
    String::from_utf8_lossy(&id).into_owned()
}

fn component_kind_text(kind: ComponentNodeKind) -> &'static str {
    match kind {
        ComponentNodeKind::Page => "page",
        ComponentNodeKind::Dictionary => "dictionary",
        ComponentNodeKind::Annotation => "annotation",
        ComponentNodeKind::SharedOther => "shared_other",
        ComponentNodeKind::Thumbnail => "thumbnail",
    }
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
fn render_epub_structured(path: &Path, output: &Path) -> Result<(), Box<dyn std::error::Error>> {
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
    // instances never cross threads; the onnx backend re-optimizes its tract
    // plans per page here — cross-page plan reuse is a known follow-up); text
    // layers are injected sequentially
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
            #[cfg(feature = "ocr-onnx")]
            {
                // Models come only from the pinned manifest (SHA-256 verified);
                // an ad-hoc --model path would bypass that verification.
                if model_path.is_some() {
                    return Err(
                        "--backend onnx does not take --model: models are pinned by \
                         docs/ocr-model-manifest.toml; fetch them with \
                         scripts/fetch_ocr_models.sh (directory override: \
                         DJVU_OCR_MODELS_DIR)"
                            .into(),
                    );
                }
                Ok(Box::new(
                    djvu_rs::ocr_onnx::pipeline::NeuralOcrBackend::load_default()?,
                ))
            }
            #[cfg(not(feature = "ocr-onnx"))]
            {
                let _ = model_path;
                Err("ONNX OCR backend is not enabled; rebuild with --features ocr-onnx".into())
            }
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

/// Wraps a page-ingestion error's rendered message so it satisfies
/// `std::error::Error + Send + Sync + 'static`, the bound
/// `encode_djvm_layered_shared_streaming`'s page-source closure requires.
///
/// Carries only the original error's `Display` text, not the error value
/// itself: `png_io`'s decode errors (from the `png`/`zune-jpeg`/`tiff`
/// crates, boxed as `Box<dyn std::error::Error>`) aren't guaranteed
/// `Send + Sync`, but that rendered message is all the CLI ever showed the
/// user for a failed page anyway (see `describe_layered_encode_error`,
/// below, for how it's unwrapped back out on the way to the user).
#[derive(Debug)]
struct PageDecodeError(String);

impl std::fmt::Display for PageDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PageDecodeError {}

/// Format an [`djvu_rs::djvu_encode::EncodeError`] from the streaming entry
/// point the way the eager per-path decode loop it replaces reported page
/// failures: [`EncodeError::PageSource`](djvu_rs::djvu_encode::EncodeError::PageSource)
/// (a page that failed to decode) surfaces the original ingestion message
/// verbatim, with no extra prefix — that decode error used to propagate
/// straight out of the eager loop via `?`, before any encode call existed to
/// prefix it. Any other `EncodeError` keeps the "layered encode: " prefix
/// the eager entry points' own error already used.
fn describe_layered_encode_error(e: djvu_rs::djvu_encode::EncodeError) -> String {
    match e {
        djvu_rs::djvu_encode::EncodeError::PageSource(inner) => inner.to_string(),
        other => format!("layered encode: {other}"),
    }
}

fn cmd_encode(
    input: &Path,
    output: &Path,
    dpi: Option<u16>,
    profile_args: EncodeProfileArgs,
    segment_args: EncodeSegmentArgs,
    bg_bpp: Option<f32>,
    bundle_args: EncodeBundleArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    // #694: without an explicit --dpi, a TIFF input's X/YResolution tags set
    // the INFO dpi; everything else keeps the historical 300 default.
    #[cfg(feature = "tiff")]
    let dpi = match dpi {
        Some(d) => d,
        None => {
            let tag_dpi = if input.is_file() && input_is_tiff(input) {
                djvu_rs::png_io::tiff_file_dpi(input)?
            } else {
                None
            };
            match tag_dpi {
                Some(d) => {
                    eprintln!("dpi {d} (from TIFF resolution tags)");
                    d
                }
                None => 300,
            }
        }
    };
    #[cfg(not(feature = "tiff"))]
    let dpi = dpi.unwrap_or(300);
    let EncodeProfileArgs {
        quality,
        bilevel_codec,
        background,
        icc,
    } = profile_args;
    // #694: --background composites transparent PNG/TIFF pixels onto a solid
    // colour at decode time (the default preserves the alpha channel);
    // --icc reject refuses ICC-profiled input instead of dropping the profile.
    let policy = djvu_rs::ingest::IngestPolicy {
        alpha: match background {
            Some((red, green, blue)) => {
                djvu_rs::ingest::AlphaCompositing::CompositeOnBackground { red, green, blue }
            }
            None => djvu_rs::ingest::AlphaCompositing::Preserve,
        },
        icc: match icc {
            IccArg::Ignore => djvu_rs::ingest::IccHandling::Ignore,
            IccArg::Reject => djvu_rs::ingest::IccHandling::Reject,
        },
        ..Default::default()
    };
    let EncodeBundleArgs {
        shared_dict_pages,
        thumbnails,
    } = bundle_args;
    use djvu_rs::djvu_encode::{BilevelCodec, EncodeQuality, PageEncoder};
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

    if input.is_dir() && bilevel_codec != BilevelCodecArg::Jb2 {
        return Err("--bilevel-codec smmr is supported only for single-image input".into());
    }

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
                let pm = djvu_rs::png_io::decode_image_to_pixmap_with_policy(path, policy)?;
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
                let pixmap = djvu_rs::png_io::decode_image_to_pixmap_with_policy(path, policy)?;
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
        //
        // Step 5 (encoder peak-memory plan): pages decode lazily, one window
        // at a time, through the streaming entry point instead of building
        // the whole `Vec<Pixmap>` up front — a directory of scanned pages is
        // exactly the case that plan's RSS-scaling numbers targeted.
        let bytes = djvu_rs::djvu_encode::encode_djvm_layered_shared_streaming(
            entries.len(),
            |idx| {
                djvu_rs::png_io::decode_image_to_pixmap_with_policy(&entries[idx], policy)
                    .map_err(|e| PageDecodeError(e.to_string()))
            },
            q,
            dpi,
            segment_options,
            shared_dict_pages,
            thumbnails,
            None,
            None,
        )
        .map_err(describe_layered_encode_error)?;
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

    // #694: bilevel TIFF fast path — 1-bit pages decode straight to packed
    // Bitmap masks, skipping RGBA expansion and segmentation. A 1-bit TIFF is
    // bilevel by construction, so --quality auto resolves to Lossless without
    // pixel statistics.
    #[cfg(feature = "tiff")]
    if input_is_tiff(input)
        && matches!(quality, EncodeQualityArg::Auto | EncodeQualityArg::Lossless)
        && let Some(bitmaps) = djvu_rs::png_io::decode_tiff_file_to_bitmaps(input, policy)?
    {
        if matches!(quality, EncodeQualityArg::Auto) {
            eprintln!("auto profile: Lossless (1-bit TIFF)");
        }
        if bitmaps.len() > 1 {
            if bilevel_codec != BilevelCodecArg::Jb2 {
                return Err("--bilevel-codec smmr is supported only for single-image input".into());
            }
            if thumbnails {
                eprintln!("--thumbnails is ignored for lossless (JB2-only) bundles");
            }
            let bytes = encode_djvm_bundle_jb2(&bitmaps, shared_dict_pages, dpi);
            std::fs::write(output, &bytes)?;
            eprintln!(
                "{} ({} TIFF pages) → {} ({} bytes, shared-dict threshold = {})",
                input.display(),
                bitmaps.len(),
                output.display(),
                bytes.len(),
                shared_dict_pages,
            );
            return Ok(());
        }
        if thumbnails {
            eprintln!("--thumbnails applies to multi-page bundles only — ignored");
        }
        let codec = match bilevel_codec {
            BilevelCodecArg::Jb2 => BilevelCodec::Jb2,
            BilevelCodecArg::Smmr => BilevelCodec::Smmr,
        };
        let bm = &bitmaps[0];
        let bytes = PageEncoder::from_bitmap(bm)
            .with_dpi(dpi)
            .with_quality(EncodeQuality::Lossless)
            .with_bilevel_codec(codec)
            .encode()
            .map_err(|e| format!("encode: {e}"))?;
        std::fs::write(output, &bytes)?;
        eprintln!(
            "{} → {} ({}×{} px, {} bytes)",
            input.display(),
            output.display(),
            bm.width,
            bm.height,
            bytes.len(),
        );
        return Ok(());
    }

    // #694 slice 2: a multipage TIFF file maps to a multi-page bundle — one
    // DjVu page per TIFF page (IFD), in stored order, same bundle rules as a
    // directory input.
    //
    // Step 5 (encoder peak-memory plan): learn the page count from a cheap
    // IFD-only pass (no pixel decode) instead of eagerly decoding every page
    // just to check `len() > 1` — the actual pixel decoding happens lazily,
    // one page at a time, in `encode_tiff_page_bundle` / below.
    //
    // The file's bytes are read into `tiff_bytes` exactly once here (not
    // once per `LazyTiffPages` pass): `count_pages` and every
    // `LazyTiffPages` reader `encode_tiff_page_bundle` opens borrow the same
    // `Vec<u8>`, which this function keeps alive on the stack for the whole
    // encode and frees on return — no leak, unlike an earlier version of
    // this change.
    #[cfg(feature = "tiff")]
    let tiff_bytes: Option<Vec<u8>> = if input_is_tiff(input) {
        Some(std::fs::read(input).map_err(|e| format!("{}: {e}", input.display()))?)
    } else {
        None
    };
    #[cfg(feature = "tiff")]
    let tiff_page_count: Option<usize> = match &tiff_bytes {
        Some(bytes) => Some(djvu_rs::png_io::tiff_file_page_count(bytes, input)?),
        None => None,
    };
    #[cfg(feature = "tiff")]
    if tiff_page_count.is_some_and(|n| n > 1) {
        if bilevel_codec != BilevelCodecArg::Jb2 {
            return Err("--bilevel-codec smmr is supported only for single-image input".into());
        }
        return encode_tiff_page_bundle(
            tiff_bytes.as_deref().unwrap(),
            input,
            output,
            dpi,
            quality,
            q,
            segment_options,
            shared_dict_pages,
            thumbnails,
            policy,
            tiff_page_count.unwrap(),
        );
    }

    if thumbnails {
        eprintln!("--thumbnails applies to multi-page bundles only — ignored");
    }
    #[cfg(feature = "tiff")]
    let pixmap = match tiff_page_count {
        Some(_) => djvu_rs::png_io::decode_tiff_file_to_pixmap_with_policy(input, policy)?,
        None => djvu_rs::png_io::decode_image_to_pixmap_with_policy(input, policy)?,
    };
    #[cfg(not(feature = "tiff"))]
    let pixmap = djvu_rs::png_io::decode_image_to_pixmap_with_policy(input, policy)?;

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
            let codec = match bilevel_codec {
                BilevelCodecArg::Jb2 => BilevelCodec::Jb2,
                BilevelCodecArg::Smmr => BilevelCodec::Smmr,
            };
            PageEncoder::from_bitmap(&seg.mask)
                .with_dpi(dpi)
                .with_quality(EncodeQuality::Lossless)
                .with_bilevel_codec(codec)
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

/// True when `path` looks like a TIFF input: `.tif`/`.tiff` extension, or a
/// TIFF magic header for extension-less paths (mirrors `decode_image_to_pixmap`).
#[cfg(feature = "tiff")]
fn input_is_tiff(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("tif") | Some("tiff") => return true,
        Some("png") | Some("jpg") | Some("jpeg") => return false,
        _ => {}
    }
    let mut header = [0u8; 4];
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    use std::io::Read;
    if f.read_exact(&mut header).is_err() {
        return false;
    }
    header.starts_with(b"II\x2A\x00") || header.starts_with(b"MM\x00\x2A")
}

/// Encode a multipage TIFF file's pages as a multi-page bundle, mirroring
/// the directory-input rules of `cmd_encode` (auto classification, lossless
/// JB2 bundle, or layered bundle with a shared dictionary).
///
/// Step 5 (encoder peak-memory plan): pages decode lazily via
/// [`djvu_rs::png_io::LazyTiffPages`] instead of the caller holding every
/// page's `Pixmap` in one `Vec` — `page_count` is already known (a cheap
/// IFD-only pass; see the call site) so this never needs to buffer more than
/// the current page.
///
/// `--quality auto` reads every page once to classify, then (for a lossless
/// or layered bundle) a second `LazyTiffPages` pass re-reads the file for the
/// actual encode — `LazyTiffPages` is strictly forward-only and cannot
/// rewind, so a fresh reader is opened per pass rather than trying to buffer
/// pages across two consumers. Unlike the directory-input path (which reused
/// its one already-decoded `Pixmap` per page for both classification and
/// encoding before this step, and still does), **this is a genuine second
/// full pixel decode of every page for TIFF input specifically** — the
/// eager `Vec<Pixmap>` this step replaced decoded each TIFF page once and
/// reused it for both passes. Measured worst case (an all-bilevel multipage
/// TIFF, where the classify loop cannot break early and both passes run to
/// completion): 8 pages 649.8ms → 651.0ms (+0.18%), 24 pages 1.948s → 1.948s
/// (~0%) — within measurement noise both times, because JB2 mask encoding
/// dominates total time far more than TIFF decode does. Accepted as a
/// deliberate trade, not fixed: TIFF `-q auto`'s doubled decode is real but
/// currently unmeasurable in wall clock, against a 94%-plus memory win: see
/// `PERF_EXPERIMENTS.md`'s "Stream pages in the CLI encoder" entry.
///
/// All readers borrow the same `file_bytes` (read once by the caller), so
/// re-opening a reader for a second pass costs nothing beyond re-parsing the
/// TIFF header (the IFD directory), not another disk
/// read.
#[cfg(feature = "tiff")]
#[allow(clippy::too_many_arguments)]
fn encode_tiff_page_bundle(
    file_bytes: &[u8],
    input: &Path,
    output: &Path,
    dpi: u16,
    quality: EncodeQualityArg,
    q: djvu_rs::djvu_encode::EncodeQuality,
    segment_options: Option<djvu_rs::segment::SegmentOptions>,
    shared_dict_pages: usize,
    thumbnails: bool,
    policy: djvu_rs::ingest::IngestPolicy,
    page_count: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    use djvu_rs::jb2_encode::encode_djvm_bundle_jb2;
    use djvu_rs::png_io::LazyTiffPages;
    use djvu_rs::segment::{SegmentOptions, segment_page};

    let quality = if matches!(quality, EncodeQualityArg::Auto) {
        let mut reader = LazyTiffPages::new(file_bytes, input, policy)?;
        let mut all_bilevel = true;
        for _ in 0..page_count {
            let pm = reader.next_page()?;
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

    let bytes = if matches!(quality, EncodeQualityArg::Lossless) {
        if thumbnails {
            eprintln!("--thumbnails is ignored for lossless (JB2-only) bundles");
        }
        let mut reader = LazyTiffPages::new(file_bytes, input, policy)?;
        let mut masks = Vec::with_capacity(page_count);
        for _ in 0..page_count {
            let pm = reader.next_page()?;
            masks.push(segment_page(&pm, &SegmentOptions::default()).mask);
        }
        encode_djvm_bundle_jb2(&masks, shared_dict_pages, dpi)
    } else {
        let mut reader = LazyTiffPages::new(file_bytes, input, policy)?;
        djvu_rs::djvu_encode::encode_djvm_layered_shared_streaming(
            page_count,
            |_idx| {
                reader
                    .next_page()
                    .map_err(|e| PageDecodeError(e.to_string()))
            },
            q,
            dpi,
            segment_options,
            shared_dict_pages,
            thumbnails,
            None,
            None,
        )
        .map_err(describe_layered_encode_error)?
    };
    std::fs::write(output, &bytes)?;
    eprintln!(
        "{} ({page_count} TIFF pages) → {} ({} bytes, shared-dict threshold = {})",
        input.display(),
        output.display(),
        bytes.len(),
        shared_dict_pages,
    );
    Ok(())
}

/// Multi-page-bundle options of `djvu encode` (single-page paths ignore them).
#[derive(Clone, Copy)]
struct EncodeBundleArgs {
    shared_dict_pages: usize,
    thumbnails: bool,
}

#[derive(Clone)]
struct EncodeProfileArgs {
    quality: EncodeQualityArg,
    bilevel_codec: BilevelCodecArg,
    background: Option<(u8, u8, u8)>,
    icc: IccArg,
}

/// Parse `--background`: `RRGGBB` hex with optional `#`, or `white`/`black`.
fn parse_background_color(s: &str) -> Result<(u8, u8, u8), String> {
    match s.to_ascii_lowercase().as_str() {
        "white" => return Ok((255, 255, 255)),
        "black" => return Ok((0, 0, 0)),
        _ => {}
    }
    let hex = s.strip_prefix('#').unwrap_or(s);
    if hex.len() == 6
        && let Ok(v) = u32::from_str_radix(hex, 16)
    {
        return Ok((
            ((v >> 16) & 0xFF) as u8,
            ((v >> 8) & 0xFF) as u8,
            (v & 0xFF) as u8,
        ));
    }
    Err(format!(
        "invalid colour '{s}' (expected RRGGBB hex, 'white', or 'black')"
    ))
}

#[derive(Clone, Copy)]
struct EncodeSegmentArgs {
    binarization: BinarizationArg,
    sauvola_window: u32,
    sauvola_k: f32,
    bg_inpaint: bool,
    block_classify: bool,
    adaptive_bg_subsample: bool,
}

impl EncodeSegmentArgs {
    fn to_options(
        self,
        quality: djvu_rs::djvu_encode::EncodeQuality,
    ) -> Result<Option<djvu_rs::segment::SegmentOptions>, Box<dyn std::error::Error>> {
        use djvu_rs::djvu_encode::EncodeQuality;
        use djvu_rs::segment::Binarization;

        let has_segment_flags = self.binarization != BinarizationArg::Fixed
            || self.bg_inpaint
            || self.block_classify
            || self.adaptive_bg_subsample;
        if !has_segment_flags {
            return Ok(None);
        }
        if matches!(quality, EncodeQuality::Lossless) {
            return Err("--binarization, --bg-inpaint, --block-classify and \
                 --adaptive-bg-subsample require --quality quality or --quality archival"
                .into());
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
        opts.block_classify = self.block_classify;
        opts.adaptive_bg_subsample = self.adaptive_bg_subsample;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_stream_failure_preserves_destination_and_cleans_temp() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("export.pdf");
        std::fs::write(&output, b"original destination").unwrap();
        let temp = dir
            .path()
            .join(format!(".export.pdf.{}.tmp", std::process::id()));

        let result = write_atomic_with(&output, |mut staged| {
            use std::io::Write;
            staged.write_all(b"partial export")?;
            Err(std::io::Error::other("cancelled export").into())
        });

        assert!(result.is_err());
        assert_eq!(std::fs::read(&output).unwrap(), b"original destination");
        assert!(!temp.exists(), "failed export must not leave a temp file");
    }
}
