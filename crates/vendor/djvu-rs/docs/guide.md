# Library guide

Task-oriented examples beyond the [README quick start](../README.md#quick-start).
Every ```` ```rust ```` block on this page is a complete program compiled as a
doctest on every CI run (the `GuideDoctests` include in `src/lib.rs`), so it
cannot drift from the public API.

Feature flags are listed in the [README](../README.md#feature-flags); enable
one with `djvu-rs = { version = "…", features = ["…"] }`.

## Export

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

## Rendering

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

An indirect `FORM:DJVM` keeps each page in its own file. Open its index with
`LazyIndirectDocument::from_index` and an async resolver: a closure that
returns the bytes of one component by name. The resolver runs only for the
pages you open and for the shared dictionaries they include, each at most once.

```rust,no_run
use djvu_rs::djvu_async::LazyIndirectDocument;
use djvu_rs::{ComponentId, ComponentResolveError};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let index = tokio::fs::read("book/index.djvu").await?;
    let doc = LazyIndirectDocument::from_index(&index, |component: ComponentId| async move {
        tokio::fs::read(format!("book/{}", component.name))
            .await
            .map_err(|_| ComponentResolveError::Missing { component })
    })?;
    let page = doc.page_async(0).await?;
    println!("first page: {}×{}", page.width(), page.height());
    Ok(())
}
```

In the browser, `WasmLazyIndirectDocument` (`wasm-lazy` feature) wraps this
loader; its resolver is a JS callback — see
[`examples/wasm/README.md`](../examples/wasm/README.md).

See [`examples/async_lazy_first_page.rs`](../examples/async_lazy_first_page.rs)
for a native first-page latency probe and
[`examples/wasm/range_lazy.md`](../examples/wasm/range_lazy.md) for the HTTP
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

### Render requests

[`RenderRequest`](https://docs.rs/djvu-rs/latest/djvu_rs/djvu_render/struct.RenderRequest.html)
gathers every render choice in one value: the page size and look
(`RenderOptions`), an optional region of the output page, the quality (full,
a progressive step, or a coarse preview), per-render resource limits, a
`CancelToken`, and the composited-tile cache. One method then picks the
output: a new pixmap, a caller's RGBA buffer, or a row sink. A region is
always the exact crop of the whole-page render.

```rust,no_run
use djvu_rs::DjVuDocument;
use djvu_rs::djvu_render::{CancelToken, Quality, RenderOptions, RenderRect, RenderRequest};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read("book.djvu")?;
    let doc = DjVuDocument::parse(&data)?;
    let page = doc.page(0)?;
    let opts = RenderOptions::fit_to_width(page, 1600);

    // A fast preview first, then the viewport at full quality from the tile cache.
    let preview = RenderRequest::new(opts.clone()).quality(Quality::Step(0)).pixmap(page)?;
    let cancel = CancelToken::new(); // call cancel.cancel() to stop a stale render
    let viewport = RenderRect { x: 0, y: 400, width: 1600, height: 900 };
    let part = RenderRequest::new(opts)
        .region(viewport)
        .cached(true)
        .cancel(cancel)
        .pixmap(page)?;
    let _ = (preview, part);
    Ok(())
}
```

`render_pixmap` stays as the shorthand for the plain whole-page render. The
other `render_*` functions (`render_into`, `render_streaming`,
`render_coarse`, `render_progressive`, `render_region`, …) are deprecated:
each deprecation note names the `RenderRequest` call that replaces it. They
still work and will be removed only in a future breaking release. Note that a
`RenderRequest` region is in display (rotated) coordinates, while the old
`render_region` took the rectangle before rotation.

The bindings follow the same shape. In Python, `Page.render(dpi, size=...,
region=(x, y, w, h), quality="coarse" | n)` replaces `render_region`,
`render_coarse` and `render_progressive`, which now raise a
`DeprecationWarning`. In the browser, a `WasmRenderRequest` (a DPI, plus
`set_region`, `set_step`, `set_coarse`) goes to `WasmPage.render_request`,
which replaces `render_coarse`, `render_progressive`,
`render_into_pixmap` and `render_progressive_into_pixmap`. The high-level
`Page` runs any `RenderRequest` with `Page::render_request`.

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
`TileRenderControls` / `TileCancelToken` (the shared `CancelToken`) add progressive quality steps and
cooperative cancellation, and with the `parallel` feature `prefetch_tiles` /
`prefetch_tiles_cancellable` warm the cache in the background with a bounded
worker pool. The full contract lives in
[`docs/tile-rendering.md`](tile-rendering.md).

## Encoding & low-level API

### JB2 bilevel image encoder

```rust
use djvu_rs::{Bitmap, jb2_encode::encode_jb2_lossless};

fn main() {
    let mut bm = Bitmap::new(800, 1000);
    // ... fill bitmap pixels ...
    // Symbol dictionary + refinement of similar glyphs; pixel-exact.
    let sjbz_payload = encode_jb2_lossless(&bm);
    // Wrap in a Sjbz IFF chunk and embed in a DjVu FORM:DJVU.
    assert!(!sjbz_payload.is_empty());
}
```

### IW44 wavelet encoder

```rust
use djvu_rs::{Pixmap, iw44_encode::{encode_iw44_color, encode_iw44_gray, Iw44EncodeOptions}};

fn main() {
    // Color: encode a Pixmap (RGBA) into BG44 chunk payloads.
    let pixmap = Pixmap::try_new(640, 480, 255, 255, 255, 255).expect("640x480 fits");
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

When pages share components — a JB2 symbol dictionary or shared annotations
in a `FORM:DJVI` file — list every component file with
`create_indirect_with_components`. The FORM type of each file sets its kind,
and each page's `INCL` must name a listed shared component:

```rust,no_run
use djvu_rs::djvm::create_indirect_with_components;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dict = std::fs::read("dict0001.iff")?;  // FORM:DJVI with a Djbz
    let p1 = std::fs::read("p0001.djvu")?;      // FORM:DJVU, INCL dict0001.iff
    let p2 = std::fs::read("p0002.djvu")?;
    let index = create_indirect_with_components(&[
        ("dict0001.iff", &dict),
        ("p0001.djvu", &p1),
        ("p0002.djvu", &p2),
    ])?;
    std::fs::write("book.djvu", index)?;
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
[`docs/indirect-djvm-resolver.md`](indirect-djvm-resolver.md).

Two mutation paths cover indirect documents:
`DjVuDocumentMut::from_indirect_resolved` resolves the component files and
rebundles them into a mutable bundled document, and `IndirectRewritePlan`
rewrites individual component files on disk while keeping the document
indirect (each file is renamed atomically, but the multi-file commit as a
whole is not transactional). Opening an indirect index directly with
`DjVuDocumentMut::from_bytes` and calling `page_mut` remains unsupported; see
[`docs/indirect-djvm-mutation.md`](indirect-djvm-mutation.md).

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
current schema covers page text, page annotations, page/document metadata
(written where DjVuLibre reads it), and bundled-document NAVM bookmarks:

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

