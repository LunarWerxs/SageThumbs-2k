# CLI reference

The `djvu` binary is enabled by the `cli` feature:

```sh
cargo install djvu-rs --features cli
```

Every subcommand and long flag of `djvu --help` must appear on this page —
`tests/readme_sync.rs` fails otherwise. Run `djvu <subcommand> --help` for the
authoritative option list.

## Commands

```sh
# Document info (--json for machine-readable output, --count for page count only)
djvu info file.djvu

# Inspect IFF chunk identities, offsets, sizes, and bundled component relationships
djvu inspect book.djvu --json

# Layered validation: structural, dependency, codec, and resource findings with
# stable codes (--strict makes warnings fail the exit code; --decode-pages adds
# full codec decodes; --limits gates size/page/pixel/memory budgets before decode)
djvu validate book.djvu --strict --decode-pages --limits server.json --json

# Semantic comparison of two documents: pages, text, annotations, metadata,
# bookmarks, and the component graph (--plane filters the compared planes)
djvu diff a.djvu b.djvu --plane text --json

# Render page 1 to PNG at 200 DPI
djvu render file.djvu --dpi 200 --output page1.png

# Render all pages to a PDF, EPUB, or CBZ
djvu render file.djvu --all --format pdf --output out.pdf
djvu render file.djvu --all --format epub --output out.epub
djvu render file.djvu --all --format cbz --output out.cbz

# Render a single layer (mask, foreground, background), with optional rotation
djvu render file.djvu --layer mask --rotate cw90 --output mask.png

# Extract text from page 2 (plain), or from all pages as hOCR / ALTO XML
djvu text file.djvu --page 2
djvu text file.djvu --all --format hocr --output out.hocr

# Merge documents into a bundled DJVM / extract a page range
djvu merge a.djvu b.djvu --output merged.djvu
djvu split book.djvu --pages 10-25 --output chapter.djvu

# Preview safe cleanup as machine-readable JSON, or write an optimized copy
djvu optimize book.djvu --output optimized.djvu --preset lossless-cleanup --dry-run
djvu optimize book.djvu --output optimized.djvu --preset lossless-cleanup
# Archival re-encode: shrink each page background as far as an SSIM loss of
# 0.02 against the input allows; --lossy-text lets the JB2 text mask join in.
djvu optimize book.djvu --output optimized.djvu --preset archival --max-ssim-loss 0.02
# With a byte budget, search for the least loss (still within 0.02) that fits.
djvu optimize book.djvu --output optimized.djvu --preset archival --max-ssim-loss 0.02 --target-size 26214400
djvu optimize book.djvu --output optimized.djvu --preset archival --max-ssim-loss 0.02 --lossy-text

# Encode an image (PNG, JPEG, or TIFF) into a single-page DjVu (bilevel JB2, lossless)
# TIFF input requires building/installing with --features tiff (cli alone does not enable it).
djvu encode scan.png --output scan.djvu --dpi 300

# Opt into a DjVuLibre-compatible G4/MMR mask for fax/scanner workflows
djvu encode scan.png --quality lossless --bilevel-codec smmr --output scan.djvu

# Encode into a layered lossy DjVu (JB2 mask + IW44 background + FGbz foreground color)
djvu encode scan.jpg --quality quality --output scan.djvu --dpi 300

# Use the conservative archival color profile
djvu encode scan.png --quality archival --output scan.djvu --dpi 300

# Opt into adaptive mask segmentation for uneven scans
djvu encode scan.png --quality quality --binarization sauvola --bg-inpaint --output scan.djvu

# Cap the IW44 background at a bits-per-pixel budget (smaller file, lower quality)
djvu encode scan.jpg --quality quality --bg-bpp 0.8 --output scan.djvu

# Composite transparent PNG/TIFF pixels onto a solid colour (hex or white/black)
djvu encode logo.png --background white --output logo.djvu

# Refuse ICC-profiled input instead of silently dropping the profile
djvu encode scan.png --icc reject --output scan.djvu

# Encode a directory of images into a bundled DJVM with shared Djbz
djvu encode pages/ --output book.djvu --shared-dict-pages 2

# Embed TH44 color thumbnails while bundling (multi-page layered)
djvu encode pages/ --quality quality --thumbnails --output book.djvu

# Raw BZZ compression utilities
djvu bzz-encode notes.txt --output notes.bzz
djvu bzz-decode notes.bzz --output notes.txt
```

## Encoding profiles

For single image input (PNG, JPEG, or TIFF), `--quality lossless`
luminance-thresholds the image into a JB2 mask and writes `INFO + Sjbz`.
`--bilevel-codec smmr` is an explicit single-image opt-in that writes a
DjVuLibre-compatible `Smmr` G4/MMR mask instead; it preserves the default JB2
path and is not available for directory bundles. The Smmr path is intended for
fax/scanner interoperability and is usually larger than JB2.
`--quality quality` uses the layered encoder (`INFO + Sjbz + BG44...` plus
`FGbz` when colored foreground is detected) for color input. `--quality
archival` uses the same layered shape with a denser background sample grid.
Directory input supports all three profiles, and both directory paths share a
Djbz symbol dictionary across pages: `lossless` uses the shared-Djbz
multi-page JB2 path, while `quality` / `archival` bundle layered pages that
keep their own `Sjbz`, `BG44`, and optional `FGbz` chunks on top of the shared
dictionary. `--shared-dict-pages` sets the page-count threshold for promoting
a symbol into the shared dictionary on either path.

Layered `quality` / `archival` encodes default to fixed BT.601 thresholding.
`--binarization sauvola` opts into adaptive local thresholding for mixed or
uneven lighting; tune it with `--sauvola-window` and `--sauvola-k`.
`--bg-inpaint` fills fully masked background blocks from neighbouring unmasked
pixels, which can reduce dark boxes under heavy text strokes.
`--block-classify` routes photo and halftone blocks wholly to the background
layer instead of shredding them into mask speckle (mixed text+photo layouts);
pair it with `--adaptive-bg-subsample`, which densifies the background grid
where unmasked detail warrants it, so routed photos keep their detail. These
knobs are opt-in, only affect layered profiles, and do not change lossless
JB2 defaults.
Library callers can use the same controls with `PageEncoder::with_segment_options`.
For newly encoded pages, `PageEncoder::with_metadata` emits the metadata in an
`ANTz` annotation chunk, where DjVuLibre reads it; for existing documents,
`DjVuDocumentMut::set_metadata(...)` and `page_mut(...).set_metadata(...)`
perform a mutation while preserving untouched chunks. These are deliberately
separate fresh-encode and mutation APIs.
