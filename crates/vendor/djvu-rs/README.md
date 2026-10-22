# djvu-rs

[![Crates.io](https://badgen.net/crates/v/djvu-rs)](https://crates.io/crates/djvu-rs)
[![PyPI](https://img.shields.io/pypi/v/djvu-rs)](https://pypi.org/project/djvu-rs/)
[![npm](https://img.shields.io/npm/v/djvu-rs)](https://www.npmjs.com/package/djvu-rs)
[![docs.rs](https://docs.rs/djvu-rs/badge.svg)](https://docs.rs/djvu-rs)
[![CI](https://github.com/matyushkin/djvu-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/matyushkin/djvu-rs/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Read, render, convert, and create DjVu files. Pure-Rust library with a CLI,
WebAssembly, and Python bindings — all published as `djvu-rs`. MIT licensed,
no GPL dependencies, written from the public DjVu v3 specification. Renders
and encodes [1.4–3.8× faster than DjVuLibre](#performance).

| Your task | How |
|-----------|-----|
| Convert DjVu → PDF, EPUB, TIFF, PNG, CBZ | [`djvu render`](docs/cli.md) or [`djvu_to_pdf`](#quick-start) / [`djvu_to_epub`](docs/guide.md#epub-export) / [`djvu_to_tiff`](docs/guide.md#tiff-export) |
| Extract text (plain, hOCR, ALTO XML) | [`djvu text`](docs/cli.md) or [`page.text()`](#quick-start), [`to_hocr` / `to_alto`](docs/guide.md#hocr-and-alto-xml-export) |
| Render pages to RGBA pixels | [`render_pixmap`](#quick-start) — sync, [async](docs/guide.md#async-render), or [parallel](#feature-flags) |
| Build a zoomable viewer (tiles) | [`djvu_tile`](docs/guide.md#tile-rendering) — cached, prefetchable, cancellable tile rendering |
| Show DjVu in the browser | [WebAssembly bindings](#webassembly), incl. lazy HTTP-Range loading |
| Read and edit DjVu from Python | `pip install djvu-rs` — [PyO3 bindings](#python) |
| Create DjVu from images (PNG/JPEG/TIFF) | [`djvu encode`](docs/cli.md#encoding-profiles) or [`PageEncoder`](docs/guide.md#encoding--low-level-api) |
| Add an OCR text layer to a scan | [`djvu ocr`](docs/guide.md#ocr-recognition-backends) (Tesseract) |
| Merge, split, edit documents | [`djvu merge` / `djvu split`](docs/cli.md), [`DocumentEditor`](docs/guide.md#typed-document-editing), `DjVuDocumentMut` |
| Stream huge books page-by-page | [Lazy async loading](docs/guide.md#lazy-async-loading) — first pixel after ~29 KB of a 100 MB file |

## Install

```sh
cargo add djvu-rs                          # Rust library
cargo install djvu-rs --features cli       # `djvu` command-line tool
pip install djvu-rs                        # Python
npm install djvu-rs                        # JavaScript / WebAssembly
```

## Quick start

Every Rust example in this README and in [`docs/guide.md`](docs/guide.md) is a
complete program, compiled as a doctest on every CI run.

```rust,no_run
use djvu_rs::{DjVuDocument, djvu_render::{render_pixmap, RenderOptions}};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read("file.djvu")?;
    let doc = DjVuDocument::parse(&data)?;
    println!("{} pages", doc.page_count());

    let page = doc.page(0)?;
    println!("{}×{} @ {} dpi", page.width(), page.height(), page.dpi());

    // Render to RGBA at 150 dpi.
    let target_dpi = 150u32;
    let opts = RenderOptions {
        width: ((page.width() as u32 * target_dpi) / page.dpi() as u32).max(1),
        height: ((page.height() as u32 * target_dpi) / page.dpi() as u32).max(1),
        ..Default::default()
    };
    let pixmap = render_pixmap(page, &opts)?;
    // pixmap.data — RGBA bytes (width × height × 4), row-major
    let _ = pixmap;

    // Text layer, if the page has one.
    if let Some(text) = page.text()? {
        println!("{text}");
    }
    Ok(())
}
```

PDF export (`pdf` feature) keeps selectable text, bookmarks, and hyperlinks,
and embeds the IW44/JB2 image data losslessly:

```rust,no_run
use djvu_rs::{DjVuDocument, pdf::djvu_to_pdf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let doc = DjVuDocument::parse(&std::fs::read("book.djvu")?)?;
    std::fs::write("book.pdf", djvu_to_pdf(&doc)?)?;
    Ok(())
}
```

More: EPUB/TIFF/hOCR/ALTO export, async and lazy loading, render requests,
tiles, encoders, editing, OCR — see the [library guide](docs/guide.md).

## CLI

```sh
djvu info file.djvu                                         # page count, sizes (--json)
djvu render file.djvu --dpi 200 --output page1.png          # one page → PNG
djvu render file.djvu --all --format pdf --output out.pdf   # also epub, cbz
djvu text file.djvu --all --format hocr --output out.hocr   # plain, hocr, alto
djvu encode scan.png --quality quality --output scan.djvu   # image → DjVu
djvu merge a.djvu b.djvu --output merged.djvu
djvu validate book.djvu --strict --json
```

Every subcommand and flag — `inspect`, `diff`, `split`, `optimize`, `ocr`,
encoding profiles — is in the [CLI reference](docs/cli.md).

## Python

```python
import djvu_rs as djvu

doc = djvu.Document.open('scan.djvu')
page = doc.page(0)
page.render(dpi=150).to_pil().save('page.png')   # or .to_numpy()
text = page.text()
```

Wheels for CPython 3.9–3.13 (manylinux/musllinux, macOS, Windows) cover
reading, rendering, export (PDF/EPUB/CBZ/TIFF), and editing annotations, text,
metadata and bookmarks. See [`djvu-py/README.md`](djvu-py/README.md).

## WebAssembly

```js
import init, { WasmDocument } from 'djvu-rs';

await init();
const doc = WasmDocument.from_bytes(new Uint8Array(arrayBuffer));
const page = doc.page(0);
const pixels = page.render(150);   // Uint8ClampedArray, RGBA
ctx.putImageData(new ImageData(pixels, page.width_at(150), page.height_at(150)), 0, 0);
```

The npm package ships TypeScript declarations and scalar + `simd128` builds
(picked at runtime). See [`examples/wasm/`](examples/wasm/) for a drag-and-drop
demo and [lazy HTTP `Range` loading](examples/wasm/range_lazy.md)
(`wasm-lazy` feature); packaging details in [`docs/packaging.md`](docs/packaging.md).

## Status & limitations

- **Library + CLI, not a viewer.** No GUI; the WASM demo is the closest thing.
- **Python covers reading, export and metadata-level editing.** Encoding and
  structural edits (merge, split, page insert) stay on the Rust crate / CLI.
- **Indirect DJVM mutation goes through two paths:** `from_indirect_resolved`
  (rebundles) or `IndirectRewritePlan` (rewrites component files; per-file
  atomic, not transactional). `DjVuDocumentMut::from_bytes` + `page_mut` on an
  indirect index errors. See [`docs/indirect-djvm-mutation.md`](docs/indirect-djvm-mutation.md).
- **Legacy `FORM:BM44`/`FORM:PM44` pages are read-only** (`page_mut` returns
  `MutError::LegacyIw44Page`); they still render and pass through save, merge,
  and split.
- **Encoder output size is close to DjVuLibre, not always smaller:**
  1.025–1.040× `c44` for IW44 at matched-or-better fidelity, 0.952–1.011× `cjb2`
  for lossless JB2. See the [encoder parity scorecard](docs/encoder-parity.md).
- **Document optimization is conservative.** `lossless-cleanup` only drops
  `FREE` padding; `archival` re-encodes under an SSIM floor and skips `FG44`,
  thumbnails, legacy pages, and shared-dictionary masks. See
  [`docs/optimizer.md`](docs/optimizer.md).
- **OCR: Tesseract is the supported backend.** `ocr-onnx` is experimental
  (pinned models, options ignored); `ocr-neural` is a placeholder that errors.

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
| `ocr-onnx` | disabled | Experimental neural OCR via `tract-onnx` (#693; needs Rust 1.91): pinned manifest + SHA-256-verified weights, DBNet detection, Cyrillic CTC recognition, CLI `--backend onnx` |
| `ocr-neural` | disabled | Placeholder backend only — `CandleBackend::load` returns a clear unsupported error |
| `ocr-neural-candle` | disabled | Deprecated no-op alias for `ocr-neural` |
| `experimental` | disabled | Experimental JB2 encoder paths used by internal example binaries |
| `iw44-probe` | disabled | IW44 encoder diagnostics probe (dev-only) |
| `alloc-profile` | disabled | dhat allocation-profiling harness for `examples/alloc_profile.rs` (dev-only) |

Without `std`, the crate provides IFF parsing, BZZ decompression, JB2/IW44 decoding,
text/annotation parsing — all codec primitives that work on byte slices.

Supported combinations and targets: [`docs/feature-matrix.md`](docs/feature-matrix.md).

## Performance

Faster than DjVuLibre 3.5.29 on every measured render and encode case (Apple
M1 Max, same machine for both):

| Case | djvu-rs | DjVuLibre | Speedup |
|------|--------:|----------:|--------:|
| Render `colorbook.djvu` @ 150 dpi (page open) | 4.46 ms | 6.37 ms | **1.4×** |
| Render `cable_1973_100133.djvu` @ 300 dpi, B&W | 22.6 ms | 36.8 ms | **1.6×** |
| Cold open + decode + render, `colorbook.djvu` | 13.7 ms | 41.6 ms | **3.0×** |
| Encode colour scan, IW44 vs `c44` | 151 ms | 474 ms | **3.1×** |
| Encode text page, lossless JB2 vs `cjb2` | 7.0 ms | 26.6 ms | **3.8×** |

Full matrix and methodology: [BENCHMARKS_RESULTS.md](BENCHMARKS_RESULTS.md);
live [benchmark](https://matyushkin.github.io/djvu-rs/dev/bench/) and
[conformance](https://matyushkin.github.io/djvu-rs/dev/conformance/) dashboards;
experiment log in [PERF_EXPERIMENTS.md](PERF_EXPERIMENTS.md).

## Documentation

| Topic | Where |
|-------|-------|
| API reference | [docs.rs/djvu-rs](https://docs.rs/djvu-rs) |
| Library guide (export, async, tiles, encoders, editing, OCR) | [`docs/guide.md`](docs/guide.md) |
| CLI reference | [`docs/cli.md`](docs/cli.md) |
| Chunk-level format coverage, `no_std` codec crates | [`docs/format-coverage.md`](docs/format-coverage.md) |
| API stability, SemVer, thread-safety, panic-freedom | [`docs/api-compatibility.md`](docs/api-compatibility.md) |
| Untrusted input and resource limits | [`SECURITY.md`](SECURITY.md) |
| Contributing | [`CONTRIBUTING.md`](CONTRIBUTING.md) |
| Roadmap | [GitHub milestones](https://github.com/matyushkin/djvu-rs/milestones) |

**MSRV:** Rust 1.88 (edition 2024); the experimental `ocr-onnx` feature needs
1.91.

## License & specification

MIT — see [LICENSE](LICENSE). Written from the public DjVu v3 specification
([sndjvu.org](https://www.sndjvu.org/spec.html),
[archived DjVu3Spec](https://web.archive.org/web/20251005122807/http://www.djvu.org/docs/DjVu3Spec.djvu));
no code derived from GPL-licensed DjVuLibre or any other GPL source.
