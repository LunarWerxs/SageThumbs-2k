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

// One file per subcommand group (#912). Every item a sibling needs is
// `pub(super)`.

mod bzz;
mod encode;
mod inspect;
mod merge;
#[cfg(any(
    feature = "ocr-tesseract",
    feature = "ocr-onnx",
    feature = "ocr-neural"
))]
mod ocr;
mod optimize;
mod render;
mod text;
mod validate;

use bzz::*;
use encode::*;
use inspect::*;
use merge::*;
#[cfg(any(
    feature = "ocr-tesseract",
    feature = "ocr-onnx",
    feature = "ocr-neural"
))]
use ocr::*;
use optimize::*;
use render::*;
use text::*;
use validate::*;

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
        /// Maximum output size in bytes. With --preset archival and
        /// --max-ssim-loss, the optimizer searches for the least SSIM loss
        /// whose output fits; otherwise it only reports whether the selected
        /// rewrites meet the target.
        #[arg(long)]
        target_size: Option<u64>,
        /// Maximum permitted SSIM loss of a lossy re-encode against the
        /// input's own decode (archival preset). Without it the archival
        /// preset re-encodes nothing.
        #[arg(long)]
        max_ssim_loss: Option<f32>,
        /// Let the archival preset re-encode JB2 text masks with lossy
        /// symbol matching, under the same --max-ssim-loss floor.
        #[arg(long)]
        lossy_text: bool,
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
    /// Prefer archival fidelity: re-encode page backgrounds (and, with
    /// --lossy-text, masks) only within --max-ssim-loss and only when
    /// smaller. Pixel-exact without a floor.
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
            lossy_text,
            dry_run,
        } => cmd_optimize(
            &file,
            &output,
            preset,
            target_size,
            max_ssim_loss,
            lossy_text,
            dry_run,
        ),
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
