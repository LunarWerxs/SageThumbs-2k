# djvu-rs

[![Crates.io](https://badgen.net/crates/v/djvu-rs)](https://crates.io/crates/djvu-rs)
[![docs.rs](https://docs.rs/djvu-rs/badge.svg)](https://docs.rs/djvu-rs)
[![CI](https://github.com/matyushkin/djvu-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/matyushkin/djvu-rs/actions/workflows/ci.yml)
[![Benchmarks](https://img.shields.io/badge/benchmarks-dashboard-blue)](https://matyushkin.github.io/djvu-rs/dev/bench/)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Read, render, convert, and create DjVu files. Pure-Rust library with a CLI,
WebAssembly, and Python bindings — MIT licensed, no GPL dependencies, written
from the public DjVu v3 specification.

| Your task | How |
|-----------|-----|
| Convert DjVu → PDF, EPUB, TIFF, PNG, CBZ | [`djvu render`](#cli) or [`djvu_to_pdf`](#pdf-export) / [`djvu_to_epub`](#epub-export) / [`djvu_to_tiff`](#tiff-export) |
| Extract text (plain, hOCR, ALTO XML) | [`djvu text`](#cli) or [`page.text()`](#text-extraction), [`to_hocr` / `to_alto`](#hocr-and-alto-xml-export) |
| Render pages to RGBA pixels | [`render_pixmap`](#quick-start) — sync, [async](#async-render), or [parallel](#feature-flags) |
| Show DjVu in the browser | [WebAssembly bindings](#webassembly), incl. lazy HTTP-Range loading |
| Read DjVu from Python | [PyO3 bindings, built from source](#python) |
| Create DjVu from images (PNG/JPEG/TIFF) | [`djvu encode`](#cli) or [`PageEncoder`](#encoding--low-level-api) |
| Add an OCR text layer to a scan | [`djvu ocr`](#ocr-recognition-backends) (Tesseract) |
| Merge, split, edit documents | [`djvu merge` / `djvu split`](#cli), `DjVuDocumentMut` |
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

# Encode an image (PNG, JPEG, or TIFF) into a single-page DjVu (bilevel JB2, lossless)
djvu encode scan.png --output scan.djvu --dpi 300

# Encode into a layered lossy DjVu (JB2 mask + IW44 background + FGbz foreground color)
djvu encode scan.jpg --quality quality --output scan.djvu --dpi 300

# Use the conservative archival color profile
djvu encode scan.png --quality archival --output scan.djvu --dpi 300

# Opt into adaptive mask segmentation for uneven scans
djvu encode scan.png --quality quality --binarization sauvola --bg-inpaint --output scan.djvu

# Cap the IW44 background at a bits-per-pixel budget (smaller file, lower quality)
djvu encode scan.jpg --quality quality --bg-bpp 0.8 --output scan.djvu

# Encode a directory of images into a bundled DJVM with shared Djbz
djvu encode pages/ --output book.djvu --shared-dict-pages 2

# Embed TH44 color thumbnails while bundling (multi-page layered)
djvu encode pages/ --quality quality --thumbnails --output book.djvu

# Raw BZZ compression utilities
djvu bzz-encode notes.txt --output notes.bzz
djvu bzz-decode notes.bzz --output notes.txt
```

For single image input (PNG, JPEG, or TIFF), `--quality lossless`
luminance-thresholds the image into a JB2 mask and writes `INFO + Sjbz`;
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
pixels, which can reduce dark boxes under heavy text strokes. These knobs are
opt-in, only affect layered profiles, and do not change lossless JB2 defaults.
Library callers can use the same controls with `PageEncoder::with_segment_options`.

## Python

PyO3 bindings live in [`djvu-py/`](djvu-py/). They are **not published to PyPI
yet** — build them from the repository (requires a Rust toolchain):

```sh
pip install ./djvu-py
# or, for development: pip install maturin && cd djvu-py && maturin develop --release
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

The bindings cover the reading surface: open documents, render pages
(including region and progressive rendering, with zero-copy numpy/PIL paths),
and extract the text layer. See [`djvu-py/README.md`](djvu-py/README.md).

## WebAssembly

Build the browser package with [wasm-pack](https://rustwasm.github.io/wasm-pack/)
through the checked-in wrapper:

```sh
make wasm
```

This produces `examples/wasm/pkg/` with one JavaScript entry point, a scalar
fallback `.wasm`, and a `simd128` `.wasm`. At runtime the loader validates a
tiny WebAssembly SIMD probe and selects the faster `simd128` artifact when the
browser supports it, otherwise it loads the scalar artifact.

Then use in JavaScript/TypeScript:

```js
import init, { WasmDocument, selectedWasmVariant } from './pkg/djvu_rs.js';

await init();
console.log(`djvu-rs wasm variant: ${selectedWasmVariant()}`);

const doc = WasmDocument.from_bytes(new Uint8Array(arrayBuffer));
console.log(doc.page_count());

const page = doc.page(0);
const pixels = page.render(150);   // Uint8ClampedArray, RGBA
const img = new ImageData(pixels, page.width_at(150), page.height_at(150));
ctx.putImageData(img, 0, 0);
```

See [`examples/wasm/`](examples/wasm/) for a complete drag-and-drop demo, and
[`examples/wasm/range_lazy.md`](examples/wasm/range_lazy.md) for lazy loading
over HTTP `Range` requests (`wasm-lazy` feature) — the browser fetches only
the index plus the pages actually opened.

The generated npm package follows the Rust crate version; there is no separate
WASM release train. The local `pkg/` directory is ignored wasm-pack output, so
regenerate it with `make wasm` from the checked-in `Cargo.toml` before
publishing instead of editing generated `pkg/package.json` by hand.

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

Two mutation paths cover indirect documents:
`DjVuDocumentMut::from_indirect_resolved` resolves the component files and
rebundles them into a mutable bundled document, and `IndirectRewritePlan`
rewrites individual component files on disk while keeping the document
indirect (each file is renamed atomically, but the multi-file commit as a
whole is not transactional). Opening an indirect index directly with
`DjVuDocumentMut::from_bytes` and calling `page_mut` remains unsupported; see
[`docs/indirect-djvm-mutation.md`](docs/indirect-djvm-mutation.md).

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

`ocr-onnx` is an experimental library-level CTC helper; the CLI accepts
`--backend onnx --model <path>` but does not treat it as a stable backend
because no specific model family, preprocessing contract, or
fixture is guaranteed yet. `ocr-neural` is a placeholder only: `CandleBackend` now
returns a clear unsupported-backend error instead of constructing a backend that
always fails at recognition time. The compatibility feature name
`ocr-neural-candle` is a no-op and no longer pulls Candle/tokenizers into
`--all-features` builds.

## Format coverage

Chunk-level coverage of the DjVu v3 format, for readers who need to know
exactly what decodes and what encodes:

| Format element | Decode | Encode |
|----------------|--------|--------|
| IFF container (`FORM:DJVU`, `FORM:DJVM`) | ✓ zero-copy parser | ✓ |
| JB2 bilevel images (`Sjbz`), shared dictionaries (`Djbz` via `INCL`) | ✓ ZP arithmetic coding + symbol dictionary | ✓ incl. multi-page shared Djbz |
| IW44 wavelet images (`BG44` / `FG44`) | ✓ planar YCbCr, multiple refinement chunks | ✓ color and grayscale |
| G4/MMR fax images (`Smmr`, ITU-T T.6) | ✓ | — |
| JPEG background/foreground (`BGjp` / `FGjp`) | ✓ | — (encoder emits IW44) |
| Foreground palette (`FGbz`) | ✓ | ✓ (layered encoder) |
| BZZ compression (BWT + MTF + ZP) | ✓ | ✓ |
| Text layer (`TXTa` / `TXTz`), zone hierarchy down to characters | ✓ | ✓ (incl. OCR injection) |
| Annotations (`ANTa` / `ANTz`): hyperlinks, map areas, colors | ✓ | ✓ |
| Bookmarks (`NAVM`) | ✓ | ✓ |
| Multi-page directory (`DIRM`), bundled and indirect | ✓ | ✓ (DjVuLibre-clean directory v1) |
| Thumbnails (`TH44`) | ✓ | ✓ (`--thumbnails`) |
| Metadata (`METa` / `METz`) | ✓ | — |
| Legacy standalone `FORM:BM44` / `FORM:PM44` files | — (clean `NotDjVu` error) | — |
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
- **Python bindings are source-only for now.** The `djvu-py` package is not
  published to PyPI yet — install it from the repository checkout.
- **Indirect DJVM mutation is indirect-only via two paths.**
  `DjVuDocumentMut::from_bytes` + `page_mut` on an indirect index errors;
  use `from_indirect_resolved` (rebundles) or `IndirectRewritePlan` (rewrites
  component files; per-file atomic, whole-commit not transactional).
- **Lazy async loading does not cover indirect DJVM** — bundled `FORM:DJVM`
  and single-page `FORM:DJVU` only; indirect returns a clean `Unsupported`
  error.
- **`create_indirect` does not emit shared `DJVI` dictionary components** —
  build a bundled document with `djvu merge` when pages share a dictionary.
- **Legacy standalone `FORM:BM44` / `FORM:PM44` files (pre-v3 DjVu) do not
  parse** — they fail with a clean `NotDjVu` error rather than decoding.
- **Encoded files run larger than DjVuLibre's encoders** — measured ~14% on
  IW44 color output vs `c44`, and the JB2 encoder lacks same-size record-6
  refinement vs `cjb2`. A size gap, not a fidelity gap; tracked in
  [`docs/jb2-size-gap-plan.md`](docs/jb2-size-gap-plan.md).
- **OCR: Tesseract is the only supported recognition backend.** `OcrOptions`
  (languages, dpi) are honored by Tesseract only; `ocr-onnx` is experimental
  and `ocr-neural` is a placeholder that returns an error.

## Feature flags

| Flag | Default | Description |
|------|---------|-------------|
| `std` | enabled | `DjVuDocument`, file I/O, rendering — the decode-only surface |
| `pdf` | disabled | PDF export via `djvu_to_pdf` (owns `miniz_oxide` + `jpeg-encoder`) |
| `cli` | disabled | Build the `djvu` command-line binary (implies `pdf` and `cbz`) |
| `cbz` | disabled | CBZ (comic-book ZIP) export — backs `render --format cbz` (owns `zip`) |
| `tiff` | disabled | TIFF export via the `tiff` crate |
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
| `ocr-onnx` | disabled | Experimental ONNX CTC recognition helper via `tract-onnx`; no stable model contract |
| `ocr-neural` | disabled | Placeholder backend only — `CandleBackend::load` returns a clear unsupported error |
| `ocr-neural-candle` | disabled | Deprecated no-op alias for `ocr-neural` |
| `experimental` | disabled | Experimental JB2 encoder paths used by internal example binaries |
| `iw44-probe` | disabled | IW44 encoder diagnostics probe (dev-only) |
| `alloc-profile` | disabled | dhat allocation-profiling harness for `examples/alloc_profile.rs` (dev-only) |

Without `std`, the crate provides IFF parsing, BZZ decompression, JB2/IW44 decoding,
text/annotation parsing — all codec primitives that work on byte slices.

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
- https://djvu.sourceforge.net/spec/DjVu3Spec.djvu (the spec is itself a DjVu file)

No code derived from GPL-licensed DjVuLibre or any other GPL source.
All algorithms are independent implementations from the spec.
