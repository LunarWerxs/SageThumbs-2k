# djvu-rs

[![Crates.io](https://badgen.net/crates/v/djvu-rs)](https://crates.io/crates/djvu-rs)
[![PyPI](https://img.shields.io/pypi/v/djvu-rs)](https://pypi.org/project/djvu-rs/)
[![npm](https://img.shields.io/npm/v/djvu-rs)](https://www.npmjs.com/package/djvu-rs)
[![docs.rs](https://docs.rs/djvu-rs/badge.svg)](https://docs.rs/djvu-rs)
[![CI](https://github.com/matyushkin/djvu-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/matyushkin/djvu-rs/actions/workflows/ci.yml)
[![Benchmarks](https://img.shields.io/badge/benchmarks-dashboard-blue)](https://matyushkin.github.io/djvu-rs/dev/bench/)
[![Conformance](https://img.shields.io/badge/conformance-dashboard-blue)](https://matyushkin.github.io/djvu-rs/dev/conformance/)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Read, render, convert, and create DjVu files. Pure-Rust library with a CLI,
WebAssembly, and Python bindings — on [crates.io](https://crates.io/crates/djvu-rs),
[PyPI](https://pypi.org/project/djvu-rs/), and [npm](https://www.npmjs.com/package/djvu-rs)
as `djvu-rs`. MIT licensed, no GPL dependencies, written from the public DjVu v3
specification.

| Your task | How |
|-----------|-----|
| Convert DjVu → PDF, EPUB, TIFF, PNG, CBZ | [`djvu render`](#cli) or [`djvu_to_pdf`](#pdf-export) / [`djvu_to_epub`](#epub-export) / [`djvu_to_tiff`](#tiff-export) |
| Extract text (plain, hOCR, ALTO XML) | [`djvu text`](#cli) or [`page.text()`](#text-extraction), [`to_hocr` / `to_alto`](#hocr-and-alto-xml-export) |
| Render pages to RGBA pixels | [`render_pixmap`](#quick-start) — sync, [async](#async-render), or [parallel](#feature-flags) |
| Build a zoomable viewer (tiles) | [`djvu_tile`](#tile-rendering) — cached, prefetchable, cancellable tile rendering |
| Show DjVu in the browser | [WebAssembly bindings](#webassembly), incl. lazy HTTP-Range loading |
| Read DjVu from Python | `pip install djvu-rs` — [PyO3 bindings](#python) |
| Create DjVu from images (PNG/JPEG/TIFF) | [`djvu encode`](#cli) or [`PageEncoder`](#encoding--low-level-api) |
| Add an OCR text layer to a scan | [`djvu ocr`](#ocr-recognition-backends) (Tesseract) |
| Merge, split, edit documents | [`djvu merge` / `djvu split`](#cli), `DocumentEditor`, `DjVuDocumentMut` |
| Stream huge books page-by-page | [Lazy async loading](#lazy-async-loading) — first pixel after ~29 KB of a 100 MB file |

Every Rust example below is a complete program, compiled as a doctest on every
CI run — what you copy-paste is guaranteed to build against the current API.

## Quick start

```rust,no_run
use djvu_rs::{DjVuDocument, djvu_render::{render_pixmap, RenderOptions}};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read("file.djvu")?;
    let doc = DjVuDocument::parse(&data)?;

    println!("{} pages", doc.page_count());

    let page = doc.page(0)?;
    println!("{}×{} @ {} dpi", page.width(), page.height(), page.dpi());

    let target_dpi = 150u32;
    let opts = RenderOptions {
        width: ((page.width() as u32 * target_dpi) / page.dpi() as u32).max(1),
        height: ((page.height() as u32 * target_dpi) / page.dpi() as u32).max(1),
        ..Default::default()
    };
    let pixmap = render_pixmap(page, &opts)?;
    // pixmap.data — RGBA bytes (width × height × 4), row-major
    Ok(())
}
```

## Text extraction

```rust,no_run
use djvu_rs::DjVuDocument;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read("scanned.djvu")?;
    let doc = DjVuDocument::parse(&data)?;
    let page = doc.page(0)?;

    if let Some(text) = page.text()? {
        println!("{text}");
    }
    Ok(())
}
```

## PDF export

Requires the `pdf` feature flag: `djvu-rs = { version = "…", features = ["pdf"] }`.
The PDF keeps selectable text, bookmarks, and hyperlinks, and embeds the
IW44/JB2 image data losslessly.

```rust,no_run
use djvu_rs::{DjVuDocument, pdf::djvu_to_pdf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read("book.djvu")?;
    let doc = DjVuDocument::parse(&data)?;

    let pdf_bytes = djvu_to_pdf(&doc)?;
    std::fs::write("book.pdf", pdf_bytes)?;
    Ok(())
}
```

## CLI

The `djvu` binary is enabled by the `cli` feature.

```sh
# Install
cargo install djvu-rs --features cli

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
djvu optimize book.djvu --output optimized.djvu --preset archival --target-size 26214400
# (--max-ssim-loss is reserved for the planned archival re-encode; the current
#  lossless cleanup is pixel-exact by construction and reports this)
djvu optimize book.djvu --output optimized.djvu --max-ssim-loss 0.001

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
For newly encoded pages, `PageEncoder::with_metadata` emits a `METz` chunk;
for existing documents, `DjVuDocumentMut::page_mut(...).set_metadata(...)`
performs a mutation while preserving untouched chunks. These are deliberately
separate fresh-encode and mutation APIs.

## Python

```sh
pip install djvu-rs
```

```python
import djvu_rs as djvu

doc = djvu.Document.open('scan.djvu')
print(f'{doc.page_count()} pages')

page = doc.page(0)
img = page.render(dpi=150).to_pil()    # or .to_numpy()
img.save('page.png')

text = page.text()
```

PyO3 bindings live in [`djvu-py/`](djvu-py/). Wheels track the crate version
(CPython 3.9–3.13 on manylinux/musllinux, macOS, and Windows). The bindings
cover the reading surface: open documents, render pages (including region and
progressive rendering, with zero-copy numpy/PIL paths), and extract the text
layer. Encode, mutation, and PDF/EPUB/TIFF export stay on the Rust crate / CLI
for now. See [`djvu-py/README.md`](djvu-py/README.md) and
[`docs/packaging.md`](docs/packaging.md).

## WebAssembly

```sh
npm install djvu-rs
```

```js
import init, { WasmDocument, selectedWasmVariant } from 'djvu-rs';

await init();
console.log(`djvu-rs wasm variant: ${selectedWasmVariant()}`);

const doc = WasmDocument.from_bytes(new Uint8Array(arrayBuffer));
console.log(doc.page_count());

const page = doc.page(0);
const pixels = page.render(150);   // Uint8ClampedArray, RGBA
const img = new ImageData(pixels, page.width_at(150), page.height_at(150));
ctx.putImageData(img, 0, 0);
```

The npm package ships TypeScript declarations plus scalar and `simd128` wasm
artifacts; at runtime a tiny `WebAssembly.validate()` probe selects SIMD when
supported. Package versions match the Rust crate — see
[`docs/packaging.md`](docs/packaging.md).

To rebuild the package from this repository:

```sh
make wasm   # → examples/wasm/pkg (dual scalar + simd128 loader)
```

See [`examples/wasm/`](examples/wasm/) for a complete drag-and-drop demo, and
[`examples/wasm/range_lazy.md`](examples/wasm/range_lazy.md) for lazy loading
over HTTP `Range` requests (`wasm-lazy` feature) — the browser fetches only
the index plus the pages actually opened.

## Advanced usage

### TIFF export

Requires the `tiff` feature flag: `djvu-rs = { version = "…", features = ["tiff"] }`.

```rust,no_run
use djvu_rs::{DjVuDocument, tiff_export::{djvu_to_tiff, TiffOptions}};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read("scan.djvu")?;
    let doc = DjVuDocument::parse(&data)?;

    let tiff_bytes = djvu_to_tiff(&doc, &TiffOptions::default())?;
    std::fs::write("scan.tiff", tiff_bytes)?;
    Ok(())
}
```

### EPUB export

Requires the `epub` feature flag: `djvu-rs = { version = "…", features = ["epub"] }`.
Produces EPUB 3 with page images, an invisible text overlay, and bookmarks as
navigation.

```rust,no_run
use djvu_rs::{DjVuDocument, epub::{djvu_to_epub, EpubOptions}};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read("book.djvu")?;
    let doc = DjVuDocument::parse(&data)?;

    let epub_bytes = djvu_to_epub(&doc, &EpubOptions::default())?;
    std::fs::write("book.epub", epub_bytes)?;
    Ok(())
}
```

### hOCR and ALTO XML export

```rust,no_run
use djvu_rs::{DjVuDocument, text_serialize::{to_hocr, to_alto, HocrOptions, AltoOptions}};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read("scanned.djvu")?;
    let doc = DjVuDocument::parse(&data)?;

    // hOCR — compatible with Tesseract, ABBYY, and most OCR toolchains
    let hocr = to_hocr(&doc, &HocrOptions::default())?;
    std::fs::write("output.hocr", hocr)?;

    // ALTO XML — used by libraries and archives (DFG, Europeana, etc.)
    let alto = to_alto(&doc, &AltoOptions::default())?;
    std::fs::write("output.xml", alto)?;
    Ok(())
}
```

### Async render

Requires the `async` feature flag: `djvu-rs = { version = "…", features = ["async"] }`.

The render entry points are synchronous and CPU-bound; run them on the
blocking thread pool with `tokio::task::spawn_blocking` so they stay off the
async runtime. The render error type stays the typed `RenderError` — there is
no wrapper enum.

```rust,no_run
use djvu_rs::{DjVuDocument, djvu_render::{self, RenderOptions}};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read("file.djvu")?;
    let doc = DjVuDocument::parse(&data)?;
    let page = doc.page(0)?.clone();

    let target_dpi = 150u32;
    let opts = RenderOptions {
        width: ((page.width() as u32 * target_dpi) / page.dpi() as u32).max(1),
        height: ((page.height() as u32 * target_dpi) / page.dpi() as u32).max(1),
        ..Default::default()
    };
    let pixmap = tokio::task::spawn_blocking(move || {
        djvu_render::render_pixmap(&page, &opts)
    })
    .await??; // outer `?`: join error (panic); inner `?`: RenderError
    println!("{} bytes", pixmap.data.len());
    Ok(())
}
```

For progressive (per-BG44-chunk) rendering, `djvu_async::render_progressive_stream`
yields a `Stream` of frames, each produced on the blocking pool.

### Lazy async loading

Requires the `async` feature flag. The lazy loader keeps a seekable async
reader and fetches page/component byte ranges only when `page_async(i)` is
called. Parsed pages are cached as `Arc<DjVuPage>`.

```rust,no_run
use djvu_rs::djvu_async::from_async_reader_lazy;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let file = tokio::fs::File::open("book.djvu").await?;
    let doc = from_async_reader_lazy(file).await?;
    println!("{} pages", doc.page_count());

    let page = doc.page_async(0).await?;
    println!("first page: {}×{}", page.width(), page.height());
    Ok(())
}
```

Supported shapes: single-page `FORM:DJVU` and bundled `FORM:DJVM`, including
shared `DJVI` dictionaries referenced via `INCL`. For browser-local `!Send`
readers on `wasm32`, use `from_async_reader_lazy_local`.

See [`examples/async_lazy_first_page.rs`](examples/async_lazy_first_page.rs)
for a native first-page latency probe and
[`examples/wasm/range_lazy.md`](examples/wasm/range_lazy.md) for the HTTP
`Range: bytes=start-end` integration shape.

### Serde support

Requires the `serde` feature flag: `djvu-rs = { version = "…", features = ["serde"] }`.

All public data types (`DjVuBookmark`, `TextZone`, `MapArea`, `PageInfo`, etc.) implement
`Serialize` and `Deserialize`.

```rust,no_run
use djvu_rs::DjVuDocument;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read("book.djvu")?;
    let doc = DjVuDocument::parse(&data)?;

    let json = serde_json::to_string_pretty(doc.bookmarks())?;
    println!("{json}");
    Ok(())
}
```

### image-rs integration

Requires the `image` feature flag: `djvu-rs = { version = "…", features = ["image"] }`.

```rust,no_run
use djvu_rs::{DjVuDocument, image_compat::DjVuDecoder};
use image::DynamicImage;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read("file.djvu")?;
    let doc = DjVuDocument::parse(&data)?;
    let page = doc.page(0)?;

    let decoder = DjVuDecoder::new(page)?.with_size(1200, 1600);
    let img = DynamicImage::from_decoder(decoder)?;
    img.save("page.png")?;
    Ok(())
}
```

### Tile rendering

For viewer engines: [`djvu_tile`](https://docs.rs/djvu-rs/latest/djvu_rs/djvu_tile/)
renders a page as a display-space tile grid over the region renderer. Tile
pixels are byte-identical to the same rectangle of a full-page render, in any
request order.

```rust,no_run
use djvu_rs::{DjVuDocument, djvu_render::RenderOptions};
use djvu_rs::djvu_tile::{TileLayout, render_tile_cached};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read("book.djvu")?;
    let doc = DjVuDocument::parse(&data)?;
    let page = doc.page(0)?;

    let opts = RenderOptions { width: 2400, height: 3200, ..Default::default() };
    let layout = TileLayout::new(page, &opts, 256)?;

    for row in 0..layout.rows() {
        for col in 0..layout.cols() {
            let tile = render_tile_cached(page, &opts, 256, col, row)?;
            // tile.data — RGBA bytes of exactly this tile rectangle
            let _ = tile;
        }
    }
    Ok(())
}
```

`render_tile_cached` memoizes composited tiles per page; the cache is
tile-granular and controllable (`tile_cache_usage`, `set_tile_cache_budget`,
`clear_tile_cache`, `invalidate_tile_region`). `render_tile_with` +
`TileRenderControls` / `TileCancelToken` add progressive quality steps and
cooperative cancellation, and with the `parallel` feature `prefetch_tiles` /
`prefetch_tiles_cancellable` warm the cache in the background with a bounded
worker pool. The full contract lives in
[`docs/tile-rendering.md`](docs/tile-rendering.md).

## Encoding & low-level API

### JB2 bilevel image encoder

```rust
use djvu_rs::{Bitmap, jb2_encode::encode_jb2};

fn main() {
    let mut bm = Bitmap::new(800, 1000);
    // ... fill bitmap pixels ...
    let sjbz_payload = encode_jb2(&bm);
    // Wrap in a Sjbz IFF chunk and embed in a DjVu FORM:DJVU.
    assert!(!sjbz_payload.is_empty());
}
```

### IW44 wavelet encoder

```rust
use djvu_rs::{Pixmap, iw44_encode::{encode_iw44_color, encode_iw44_gray, Iw44EncodeOptions}};

fn main() {
    // Color: encode a Pixmap (RGBA) into BG44 chunk payloads.
    let pixmap = Pixmap::new(640, 480, 255, 255, 255, 255);
    let chunks: Vec<Vec<u8>> = encode_iw44_color(&pixmap, &Iw44EncodeOptions::default());
    // Each Vec<u8> is a BG44 chunk payload; wrap each in a BG44 IFF tag.

    // Grayscale: encode a GrayPixmap the same way.
    let gray = pixmap.to_gray8();
    let gray_chunks: Vec<Vec<u8>> = encode_iw44_gray(&gray, &Iw44EncodeOptions::default());
    assert!(!chunks.is_empty() && !gray_chunks.is_empty());
}
```

`Iw44EncodeOptions` fields (all have sensible defaults):

| Field | Default | Description |
|-------|---------|-------------|
| `slices_per_chunk` | 10 | Slices packed into each BG44/FG44 chunk |
| `total_slices` | 100 | Total refinement slices to encode |
| `chroma_delay` | 0 | Y slices before Cb/Cr encoding begins |
| `chroma_half` | false | Legacy no-op; IW44 v1.2 always emits full-resolution chroma |

### Bookmark encoder

```rust
use djvu_rs::{djvu_document::DjVuBookmark, navm_encode::encode_navm};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bookmarks = vec![
        DjVuBookmark { title: "Chapter 1".into(), url: "#page=1".into(), children: vec![] },
    ];
    let navm_payload = encode_navm(&bookmarks)?;
    assert!(!navm_payload.is_empty());
    Ok(())
}
```

### Annotation encoder

```rust
use djvu_rs::annotation::{Annotation, MapArea, encode_annotations, encode_annotations_bzz};

fn main() {
    let ann = Annotation::default();
    let areas: Vec<MapArea> = vec![];

    let anta_payload = encode_annotations(&ann, &areas);      // uncompressed ANTa
    let antz_payload = encode_annotations_bzz(&ann, &areas);  // BZZ-compressed ANTz
    assert!(anta_payload.len() <= antz_payload.len() || !antz_payload.is_empty());
}
```

### Indirect multi-page documents

Create an indirect DJVM index file that references per-page `.djvu` files:

```rust,no_run
use djvu_rs::djvm::create_indirect;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let index = create_indirect(&["page001.djvu", "page002.djvu", "page003.djvu"])?;
    std::fs::write("book.djvu", index)?;
    // Distribute book.djvu alongside the individual page files.
    Ok(())
}
```

Load an indirect document by resolving component files from a directory:

```rust,no_run
use djvu_rs::DjVuDocument;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let index = std::fs::read("book.djvu")?;
    let doc = DjVuDocument::parse_from_dir(&index, "/path/to/pages")?;
    println!("{} pages", doc.page_count());
    Ok(())
}
```

Applications that need the full DIRM component identity can use
`DjVuDocument::parse_with_component_resolver`. Its
`ComponentResolver` receives a `ComponentId` containing both the external name
and its `ComponentKind` (`Page`, `Shared`, or `Thumbnail`), and is called for
every directory entry. Page/shared/thumbnail FORM mismatches and resolver
failures surface as typed errors; shared `Djbz` dictionaries referenced by
`INCL` are connected to the parsed pages. See
[`docs/indirect-djvm-resolver.md`](docs/indirect-djvm-resolver.md).

Two mutation paths cover indirect documents:
`DjVuDocumentMut::from_indirect_resolved` resolves the component files and
rebundles them into a mutable bundled document, and `IndirectRewritePlan`
rewrites individual component files on disk while keeping the document
indirect (each file is renamed atomically, but the multi-file commit as a
whole is not transactional). Opening an indirect index directly with
`DjVuDocumentMut::from_bytes` and calling `page_mut` remains unsupported; see
[`docs/indirect-djvm-mutation.md`](docs/indirect-djvm-mutation.md).

The reverse direction is covered too: `djvm::to_indirect` splits a bundled
`FORM:DJVM` into an indirect index plus standalone component files, keeping
component ids, names, titles, and the document `NAVM` stable. Related
bundled-document operations in the same module: `djvm::remove_pages` deletes
pages with an explicit `UnreachablePolicy` (preserve or garbage-collect shared
components that lose their last including page),
`djvm::dedup_shared_components` merges byte-identical shared components, and
`djvm::DjvmStreamWriter` writes a bundle to any `io::Write` sink with memory
bounded to the spooled component being appended.

### Typed document editing

`DocumentEditor` provides a versioned, typed operation list with a semantic
dry-run plan and validation of every operation before bytes are emitted. The
current schema covers page text, page annotations, page/document METa/METz
metadata, and bundled-document NAVM bookmarks:

```rust,no_run
use djvu_rs::{DocumentEditor, EditOperation, EditRequest};
use djvu_rs::metadata::DjVuMetadata;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let input = std::fs::read("book.djvu")?;
    let request = EditRequest::new(vec![EditOperation::SetDocumentMetadata {
        metadata: DjVuMetadata {
            title: Some("Updated title".into()),
            ..Default::default()
        },
    }]);

    let plan = DocumentEditor::plan(&input, &request)?;
    println!("{} operation(s), {} page(s)", plan.operations.len(), plan.page_count);
    let edited = DocumentEditor::apply(&input, &request)?;
    std::fs::write("edited.djvu", edited)?;
    Ok(())
}
```

`DocumentEditor::apply_to_path` stages output beside the destination and
renames it only after validation, serialization, and sync succeed. The first
slice intentionally does not yet cover the declarative CLI, XMP, thumbnails,
page insertion/deletion/reordering/extraction, semantic diff, or multi-file
indirect-DJVM commits; those require separate operation and commit contracts.
With the `serde` feature, requests and plans are JSON-serializable using the
versioned schema.

### Low-level IFF access

```rust,no_run
use djvu_rs::iff::parse_form;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read("file.djvu")?;
    let form = parse_form(&data)?;
    println!("FORM type: {:?}", std::str::from_utf8(&form.form_type));
    for chunk in &form.chunks {
        println!("  chunk {:?} ({} bytes)", std::str::from_utf8(&chunk.id), chunk.data.len());
    }
    Ok(())
}
```

### OCR recognition backends

The supported OCR recognition path is the `ocr-tesseract` feature, which uses a
system Tesseract installation and tessdata files. Recognized text is embedded
into the output document as a compressed `TXTz` text layer, page by page:

```sh
cargo build --features cli,ocr-tesseract
# Requires Tesseract + the requested language data, e.g. eng.traineddata.
djvu ocr scanned.djvu --backend tesseract --lang eng --output with-text.djvu
```

Library callers can attach recognized text at encode time instead, via
`PageEncoder::with_ocr_text_layer` (or `with_text_layer` for an existing
`TextLayer`).

`ocr-onnx` is experimental but now CLI-live (#693): `--backend onnx` runs the
full PP-OCR neural pipeline — DBNet text detection plus Cyrillic PP-OCRv5 CTC
line recognition (its pinned dictionary also covers Latin, digits, and
punctuation) assembled into a `page → line → word` text layer with heuristic
word rectangles. Models come only from the pinned manifest with mandatory
SHA-256 verification (`docs/ocr-model-manifest.toml`, fetched explicitly via
`scripts/fetch_ocr_models.sh` — weights are never committed and never
downloaded implicitly; directory override: `DJVU_OCR_MODELS_DIR`). The
`--model` flag is not used by this backend, and `OcrOptions`
(`languages`/`dpi`) are advisory and ignored. Recognition quality of the
pinned models is gated by a deterministic synthetic corpus with a recorded
CER/WER/IoU baseline (`docs/ocr-model-metrics.md`). `ocr-neural` is a placeholder only: `CandleBackend` now
returns a clear unsupported-backend error instead of constructing a backend that
always fails at recognition time. The compatibility feature name
`ocr-neural-candle` is a no-op and no longer pulls Candle/tokenizers into
`--all-features` builds.

## Format coverage

Chunk-level coverage of the DjVu v3 format, for readers who need to know
exactly what decodes and what encodes:

The fresh-encode versus existing-document mutation contract is expanded in
[`docs/writer-coverage.md`](docs/writer-coverage.md).

| Format element | Decode | Encode |
|----------------|--------|--------|
| IFF container (`FORM:DJVU`, `FORM:DJVM`) | ✓ zero-copy parser | ✓ |
| JB2 bilevel images (`Sjbz`), shared dictionaries (`Djbz` via `INCL`) | ✓ ZP arithmetic coding + symbol dictionary | ✓ incl. multi-page shared Djbz |
| IW44 wavelet images (`BG44` / `FG44`) | ✓ planar YCbCr, multiple refinement chunks | ✓ color and grayscale |
| G4/MMR fax images (`Smmr`, ITU-T T.6) | ✓ | ✓ (explicit `BilevelCodec::Smmr` / `--bilevel-codec smmr`) |
| JPEG background/foreground (`BGjp` / `FGjp`) | ✓ | — (encoder emits IW44) |
| Foreground palette (`FGbz`) | ✓ | ✓ (layered encoder) |
| BZZ compression (BWT + MTF + ZP) | ✓ | ✓ |
| Text layer (`TXTa` / `TXTz`), zone hierarchy down to characters | ✓ | ✓ (incl. OCR injection) |
| Annotations (`ANTa` / `ANTz`): hyperlinks, map areas, colors | ✓ | ✓ |
| Bookmarks (`NAVM`) | ✓ | ✓ |
| Multi-page directory (`DIRM`), bundled and indirect | ✓ | ✓ (DjVuLibre-clean directory v1) |
| Thumbnails (`TH44`) | ✓ | ✓ (`--thumbnails`) |
| Metadata (`METa` / `METz`) | ✓ | ✓ (`PageEncoder::with_metadata`; `PageMut::set_metadata`) |
| Legacy standalone `FORM:BM44` / `FORM:PM44` files | ✓ | — |
| Unknown chunk IDs | preserved byte-exact for round-trip | n/a |

The codec internals are also published as standalone workspace crates for
focused consumers: [`djvu-iff`](crates/djvu-iff), [`djvu-bzz`](crates/djvu-bzz),
[`djvu-bitmap`](crates/djvu-bitmap), [`djvu-jb2`](crates/djvu-jb2),
[`djvu-pixmap`](crates/djvu-pixmap), [`djvu-iw44`](crates/djvu-iw44), and
[`djvu-zp`](crates/djvu-zp). All of them (and the codec modules of the main
crate) are `no_std`-compatible with `alloc` only, and are continuously fuzzed
via in-tree libFuzzer targets and OSS-Fuzz project files.

## Status & limitations

Honest boundaries, so you can decide fast:

- **Library + CLI, not a viewer.** There is no GUI; the WASM demo is the
  closest thing to one.
- **Python bindings cover the reading surface only.** Open, render, and text
  extraction ship in the PyPI wheels; encode, mutation, and PDF/EPUB/TIFF
  export stay on the Rust crate / CLI for now.
- **Indirect DJVM mutation is indirect-only via two paths.**
  `DjVuDocumentMut::from_bytes` + `page_mut` on an indirect index errors;
  use `from_indirect_resolved` (rebundles) or `IndirectRewritePlan` (rewrites
  component files; per-file atomic, whole-commit not transactional).
- **Lazy async loading does not cover indirect DJVM** — bundled `FORM:DJVM`
  and single-page `FORM:DJVU` only; indirect returns a clean `Unsupported`
  error.
- **`create_indirect` does not emit shared `DJVI` dictionary components** —
  build a bundled document with `djvu merge` when pages share a dictionary, or
  convert an existing bundled document with `djvm::to_indirect`, which
  preserves shared components.
- **Encoder size parity is corpus- and profile-dependent.** Run the
  reproducible [`encoder parity scorecard`](docs/encoder-parity.md) to compare
  the same raster through DjVuLibre 3.5.29's `c44`/`cjb2` and the archival-safe
  `PageEncoder` profiles. The 2026-07-16 snapshot ranges from 1.025–1.040×
  `c44` for IW44 photo pages — at matched-or-better fidelity (decoded PSNR/SSIM
  meet or exceed `c44` on the measured pages) — and 0.952–2.100× `cjb2` for the
  public direct JB2 lossless profile; every measured output passed its
  interop/fidelity gate. The earlier IW44 gap (up to 1.345×, and lower fidelity)
  came from two encoder bugs since fixed: an activation threshold that stranded
  dense-page coefficients (`IW44_LUMA_PLATEAU`) and a colour transform that did
  not match the decoder's Pigeon `YCbCr` basis (`IW44_PIGEON_COLOR`); see
  `PERF_EXPERIMENTS.md`. Same-size record-6 and lossy rec-7 remain experimental
  and are tracked in [`docs/jb2-size-gap-plan.md`](docs/jb2-size-gap-plan.md).
- **Document optimization is conservative in the first slice.** `djvu optimize`
  currently removes only semantically inert `FREE` padding and reports unmet
  size targets; archival codec search, progress callbacks, and cancellation
  remain planned. See [`docs/optimizer.md`](docs/optimizer.md); it always writes
  a separate output file.
- **OCR: Tesseract is the supported recognition backend.** `OcrOptions`
  (languages, dpi) are honored by Tesseract only; the `ocr-onnx` neural
  pipeline is CLI-live but experimental (fixed pinned models, options
  ignored) and `ocr-neural` is a placeholder that returns an error.

## Feature flags

| Flag | Default | Description |
|------|---------|-------------|
| `std` | enabled | `DjVuDocument`, file I/O, rendering — the decode-only surface |
| `pdf` | disabled | PDF export via `djvu_to_pdf` (owns `miniz_oxide` + `jpeg-encoder`) |
| `cli` | disabled | Build the `djvu` command-line binary (implies `pdf` and `cbz`) |
| `cbz` | disabled | CBZ (comic-book ZIP) export — backs `render --format cbz` (owns `zip`) |
| `tiff` | disabled | TIFF export (`djvu_to_tiff`) **and** TIFF encode input for `djvu encode` / `decode_image_to_pixmap` |
| `async` | disabled | Async render API and lazy `AsyncRead + AsyncSeek` document loading |
| `parallel` | disabled | Parallel multi-page render via `rayon` (`render_pages_parallel`) |
| `jpeg` | disabled | Standalone JPEG decode without full `std` (JPEG is included in `std` by default) |
| `mmap` | disabled | Memory-mapped file I/O via `memmap2` (`MmapDocument::open`) |
| `serde` | disabled | `Serialize` + `Deserialize` for all public data types |
| `image` | disabled | `image::ImageDecoder` impl via `DjVuDecoder` — integrates with the `image` crate |
| `epub` | disabled | EPUB 3 export via `djvu_to_epub` — page images, text overlay, bookmarks as nav (owns `zip`) |
| `wasm` | disabled | WebAssembly bindings via `wasm-bindgen` (`WasmDocument`, `WasmPage`) |
| `wasm-lazy` | disabled | Lazy Range-based document loading in the browser: a JS `(offset, len)` reader fetches only the pages you open |
| `wasm-threads` | disabled | wasm32 thread pool (rayon via Web Workers); requires a nightly toolchain, not part of the stable CI gate |
| `ocr-tesseract` | disabled | OCR recognition via a system Tesseract installation (the supported OCR backend) |
| `ocr-onnx` | disabled | Experimental neural OCR via `tract-onnx` (#693): pinned manifest + SHA-256-verified weights, DBNet detection, Cyrillic CTC recognition, CLI `--backend onnx` |
| `ocr-neural` | disabled | Placeholder backend only — `CandleBackend::load` returns a clear unsupported error |
| `ocr-neural-candle` | disabled | Deprecated no-op alias for `ocr-neural` |
| `experimental` | disabled | Experimental JB2 encoder paths used by internal example binaries |
| `iw44-probe` | disabled | IW44 encoder diagnostics probe (dev-only) |
| `alloc-profile` | disabled | dhat allocation-profiling harness for `examples/alloc_profile.rs` (dev-only) |

Without `std`, the crate provides IFF parsing, BZZ decompression, JB2/IW44 decoding,
text/annotation parsing — all codec primitives that work on byte slices.

## API stability & compatibility

The full contract lives in [`docs/api-compatibility.md`](docs/api-compatibility.md)
(policy) and [`docs/feature-matrix.md`](docs/feature-matrix.md) (supported
combinations and targets), and is enforced in CI. In short:

- **Stable surface** — the document model (`Document`/`Page`, `DjVuDocument`/
  `DjVuPage`), the render entry points, the codec entry points, the parsers, and
  the writer `djvu_to_*` functions. Follows SemVer; breakage is caught by
  `cargo-semver-checks`.
- **Experimental / placeholder** — `experimental`, `iw44-probe`, `alloc-profile`,
  `ocr-onnx`, `wasm-threads`, and the `ocr-neural` placeholder. These may change
  in any release and are the ones marked *Experimental*/*Placeholder* in the
  feature table above.
- **Deprecated (kept for ≥ 2 minor releases / 90 days)** — the `bzz_new` and
  `iw44_new` module aliases and the `ocr-neural-candle` feature alias.
- **MSRV** — Rust 1.88, a required CI gate.
- **Thread-safety** — `Document`, `DjVuDocument`, `DjVuPage`, pixel buffers, and
  the parsed content/error types are `Send + Sync`; the mutable editor is
  `Send`; `LazyDocument<R>` inherits its thread-safety from `R`. Asserted in
  [`tests/send_sync_contract.rs`](tests/send_sync_contract.rs).
- **Untrusted input** — no public parse/decode/render entry point panics on any
  input; malformed bytes surface as typed errors. Covered by
  [`tests/panic_free_corpus.rs`](tests/panic_free_corpus.rs), proptests, and
  libFuzzer/OSS-Fuzz targets.
- **Resource limits** — decode/render inherit documented, bounded memory/work
  ceilings; exceeding one returns a typed error naming the codec and axis. The
  ceilings are caller-configurable: pass `ResourceLimits` via `ParseOptions` to
  `DjVuDocument::parse_with_options` (pages inherit them at render time), or
  use `render_pixmap_with_limits` / `render_into_with_limits` directly. See
  [`SECURITY.md`](SECURITY.md#decode-time-resource-ceilings).

## Performance

See [BENCHMARKS_RESULTS.md](BENCHMARKS_RESULTS.md) for Criterion numbers,
methodology, and a DjVuLibre comparison (run via
[`scripts/bench_djvulibre.sh`](scripts/bench_djvulibre.sh) +
[`scripts/djvulibre_compare.py`](scripts/djvulibre_compare.py)).
Historical multi-platform results are in [BENCHMARKS.md](BENCHMARKS.md),
including the local WASM scalar-vs-simd128 harness.

Recent targeted experiments are recorded in
[PERF_EXPERIMENTS.md](PERF_EXPERIMENTS.md), including:

- **#233 lazy async loading:** a 100 MiB padded 520-page DJVM reached first
  pixel in **491.469 ms** while reading only **28,578 bytes** at simulated
  12.5 MiB/s throughput.
- **#189 x86-64-v3 AVX2 validation:** existing AVX2 decode paths showed
  `iw44_decode_corpus_color` **-18.88%** and `iw44_decode_first_chunk`
  **-4.85%** on GitHub-hosted x86_64, with one sub4 partial-decode regression
  recorded for follow-up.
- **#258 shared-Djbz clustering:** Hamming shared clustering was rejected as
  default; byte-exact shared-Djbz remains the measured safe path.

## Minimum supported Rust version (MSRV)

Rust **1.88** (edition 2024 — let-chains stabilized in 1.88)

## Roadmap

See [GitHub milestones](https://github.com/matyushkin/djvu-rs/milestones) for the full roadmap and progress tracking.

## License

MIT. See [LICENSE](LICENSE).

## Specification

Written from the public DjVu v3 specification:
- https://www.sndjvu.org/spec.html
- https://web.archive.org/web/20251005122807/http://www.djvu.org/docs/DjVu3Spec.djvu
  (the spec is itself a DjVu file; archived copy — djvu.org and
  djvu.sourceforge.net no longer serve the original)

No code derived from GPL-licensed DjVuLibre or any other GPL source.
All algorithms are independent implementations from the spec.
