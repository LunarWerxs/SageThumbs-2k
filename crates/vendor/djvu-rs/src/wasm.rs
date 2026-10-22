//! WebAssembly bindings for djvu-rs.
//!
//! Exposes a minimal browser-friendly API via `wasm-bindgen`:
//!
//! - [`WasmDocument`] — parsed DjVu document, created from raw bytes.
//! - [`WasmPage`]     — a single page, capable of rendering to RGBA pixels.
//! - [`WasmRenderRequest`] — what to render: a DPI, a region, a quality.
//!
//! ## Usage (JavaScript)
//!
//! ```js
//! import init, { WasmDocument } from 'djvu-rs';
//! await init();
//! const doc = WasmDocument.from_bytes(new Uint8Array(buffer));
//! const page = doc.page(0);
//!
//! // Full render (all IW44 chunks)
//! const pixels = page.render(150);   // Uint8ClampedArray, RGBA
//! const img = new ImageData(pixels, page.width_at(150), page.height_at(150));
//! ctx.putImageData(img, 0, 0);
//!
//! // One request for everything else: a region, a progressive step, a
//! // coarse preview. The pixmap buffer is reused across renders.
//! const pm = new WasmPixmap();
//! const req = new WasmRenderRequest(150);
//! for (let n = 0; n < page.bg44_chunk_count(); n++) {
//!   req.set_step(n); // background chunks 0..=n: refine step by step
//!   page.render_request(req, pm);
//!   ctx.putImageData(new ImageData(pm.view(), pm.width(), pm.height()), 0, 0);
//! }
//! req.set_full();
//! req.set_region(0, 0, 400, 300); // only the visible part, from the tile cache
//! page.render_request(req, pm);
//!
//! // Tile-first render (#691): only composite what is on screen
//! const ts = 256;
//! for (let row = 0; row < page.tile_rows(150, ts); row++) {
//!   for (let col = 0; col < page.tile_cols(150, ts); col++) {
//!     const tile = page.render_tile(150, ts, col, row); // WasmPixmap
//!     ctx.putImageData(new ImageData(tile.view(), tile.width(), tile.height()),
//!                      col * ts, row * ts);
//!   }
//! }
//! ```

use std::sync::Arc;

use wasm_bindgen::prelude::*;

use crate::{
    djvu_document::DjVuDocument,
    djvu_render::{Quality, RenderError, RenderOptions, RenderRect, RenderRequest},
    djvu_tile,
    pixmap::Pixmap,
    render_size::RenderSize,
};

/// The coarse preview, or `None` when the page has no background.
fn render_coarse(
    page: &crate::djvu_document::DjVuPage,
    opts: &RenderOptions,
) -> Result<Option<Pixmap>, RenderError> {
    match RenderRequest::new(opts.clone())
        .quality(Quality::Coarse)
        .operation("render_coarse")
        .pixmap(page)
    {
        Err(RenderError::NoBackground) => Ok(None),
        result => result.map(Some),
    }
}

/// Progressive frame `chunk_n`: background chunks `0..=chunk_n` and every
/// other layer.
fn render_progressive(
    page: &crate::djvu_document::DjVuPage,
    opts: &RenderOptions,
    chunk_n: usize,
) -> Result<Pixmap, RenderError> {
    RenderRequest::new(opts.clone())
        .quality(Quality::Step(chunk_n))
        .operation("render_progressive")
        .pixmap(page)
}

// ── WasmPixmap — Rust-owned pixel buffer (#611) ──────────────────────────────

/// A Rust-owned RGBA pixel buffer that stays alive as long as JS holds the
/// handle, so pixels can be consumed **without** the per-frame
/// `Uint8ClampedArray` allocation + full-buffer copy the plain `render*`
/// methods pay.
///
/// Two usage modes:
/// - **Zero-copy view**: [`view`](WasmPixmap::view) returns a typed-array view
///   directly into wasm linear memory. Consume it immediately (e.g.
///   `ctx.putImageData(new ImageData(pm.view(), pm.width(), pm.height()), 0, 0)`
///   — `ImageData` copies). The view is invalidated by wasm memory growth and
///   by dropping/re-rendering the pixmap; never store it.
/// - **Buffer reuse**: pass the same `WasmPixmap` back to
///   [`render_request`](WasmPage::render_request)
///   — the Rust-side allocation is reused across frames (a progressive
///   session allocates once instead of once per refinement pass).
///
/// The existing copying `render*` methods are unchanged for callers that need
/// independently owned JS bytes.
#[wasm_bindgen]
pub struct WasmPixmap {
    data: Vec<u8>,
    width: u32,
    height: u32,
}

#[wasm_bindgen]
impl WasmPixmap {
    /// An empty pixmap for use with the `*_into_pixmap` methods.
    #[wasm_bindgen(constructor)]
    pub fn new() -> WasmPixmap {
        WasmPixmap {
            data: Vec::new(),
            width: 0,
            height: 0,
        }
    }

    /// Pixel width of the last render written into this pixmap.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Pixel height of the last render written into this pixmap.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// RGBA byte length (`width * height * 4`).
    pub fn byte_length(&self) -> u32 {
        self.data.len() as u32
    }

    /// Zero-copy `Uint8ClampedArray` view into wasm memory.
    ///
    /// Valid only until the next wasm memory growth, the next render into
    /// this pixmap, or the pixmap being freed — consume it immediately and
    /// never store it. `new ImageData(view, w, h)` copies, so canvas
    /// consumption is safe.
    // The crate denies unsafe; this is one of the few justified exceptions
    // (like the SIMD intrinsics in djvu-iw44): `js_sys::…::view` is the only
    // zero-copy wasm→JS surface, and the safety contract is documented above.
    #[allow(unsafe_code)]
    pub fn view(&self) -> js_sys::Uint8ClampedArray {
        // SAFETY (lifetime contract): the view aliases `self.data`'s wasm
        // linear memory. `self` is heap-boxed and owned by the JS handle, so
        // the allocation outlives this call; the documented contract forbids
        // holding the view across anything that can grow memory or mutate
        // the buffer.
        unsafe { js_sys::Uint8ClampedArray::view(&self.data) }
    }

    /// Copy the pixels into a fresh, independently owned
    /// `Uint8ClampedArray` (same guarantee as the plain `render` API).
    pub fn to_bytes(&self) -> js_sys::Uint8ClampedArray {
        let arr = js_sys::Uint8ClampedArray::new_with_length(self.data.len() as u32);
        arr.copy_from(&self.data);
        arr
    }
}

impl Default for WasmPixmap {
    fn default() -> Self {
        Self::new()
    }
}

// ── WasmRenderRequest — what to render ───────────────────────────────────────

/// What to render: the page at a DPI, optionally one rectangle of it, at a
/// chosen quality. Pass it to [`WasmPage::render_request`], the one render
/// call that covers a viewport, a progressive step, and a coarse preview.
///
/// ```js
/// const req = new WasmRenderRequest(150);
/// req.set_region(0, 200, 800, 600); // a viewport, from the tile cache
/// req.set_step(0);                  // background chunk 0 only
/// page.render_request(req, pixmap);
/// ```
#[wasm_bindgen]
#[derive(Debug, Clone)]
pub struct WasmRenderRequest {
    target_dpi: u32,
    region: Option<RenderRect>,
    quality: Quality,
}

#[wasm_bindgen]
impl WasmRenderRequest {
    /// A full-quality render of the whole page at `target_dpi`.
    #[wasm_bindgen(constructor)]
    pub fn new(target_dpi: u32) -> WasmRenderRequest {
        WasmRenderRequest {
            target_dpi,
            region: None,
            quality: Quality::Full,
        }
    }

    /// Render only the rectangle `(x, y, width, height)` of the page at
    /// `target_dpi`, in canvas pixels after the page's INFO rotation. The
    /// output is `width × height`; pixels outside the page are white. A
    /// full-quality region comes from the page's composited-tile cache, so
    /// a viewer's pans cost only their new tiles.
    pub fn set_region(&mut self, x: u32, y: u32, width: u32, height: u32) {
        self.region = Some(RenderRect {
            x,
            y,
            width,
            height,
        });
    }

    /// Render the whole page again (undo [`set_region`](Self::set_region)).
    pub fn clear_region(&mut self) {
        self.region = None;
    }

    /// Full quality: every background chunk (the default).
    pub fn set_full(&mut self) {
        self.quality = Quality::Full;
    }

    /// A progressive step: background chunks `0..=chunk_n` plus every other
    /// layer. `chunk_n = bg44_chunk_count() - 1` is the full render; a step
    /// past it throws.
    pub fn set_step(&mut self, chunk_n: u32) {
        self.quality = Quality::Step(chunk_n as usize);
    }

    /// A fast, blurry preview from the first background chunk only. Throws
    /// on a page without a background (`bg44_chunk_count() == 0`).
    pub fn set_coarse(&mut self) {
        self.quality = Quality::Coarse;
    }
}

/// Run `request` on `page` into `out`, reusing its buffer. A whole-page
/// render that can stream writes straight into the buffer; the rest go
/// through a pixmap.
fn render_request_into(
    page: &crate::djvu_document::DjVuPage,
    request: &WasmRenderRequest,
    out: &mut WasmPixmap,
) -> Result<(), RenderError> {
    let opts = crate::foreign::render_opts_for_dpi(page, request.target_dpi as f32);
    let streams = request.region.is_none() && opts.can_stream(page);
    let (width, height) = (opts.width, opts.height);
    let mut render = RenderRequest::new(opts).quality(request.quality);
    if let Some(region) = request.region {
        render = render.region(region).cached(true);
    }
    if streams {
        out.data.resize(width as usize * height as usize * 4, 0);
        render.write_rgba(page, &mut out.data)?;
        out.width = width;
        out.height = height;
    } else {
        let pm = render.pixmap(page)?;
        out.data.clear();
        out.data.extend_from_slice(&pm.data);
        out.width = pm.width;
        out.height = pm.height;
    }
    Ok(())
}

// ── Thread pool (opt-in, `wasm-threads` feature) ─────────────────────────────
//
// Re-exports `wasm-bindgen-rayon`'s pool initializer so JS callers can spin up
// the Web Worker pool that backs rayon's parallel compositor (PARALLEL) and
// IDWT (IW44_PAR) paths inside the browser. Building this feature requires a
// nightly toolchain with `-Z build-std` and wasm atomics (see
// `make wasm-threads-check`); it is never part of the default `wasm` build.
//
// JS usage (after `await init()` from the generated glue):
// ```js
// import init, { initThreadPool } from 'djvu-rs';
// await init();
// await initThreadPool(navigator.hardwareConcurrency);
// ```
//
// The page/document also needs `Cross-Origin-Opener-Policy: same-origin` and
// `Cross-Origin-Embedder-Policy: require-corp` response headers so the
// browser exposes `SharedArrayBuffer` — without them `initThreadPool` throws.
#[cfg(feature = "wasm-threads")]
pub use wasm_bindgen_rayon::init_thread_pool;

// ── WasmDocument ─────────────────────────────────────────────────────────────

/// A parsed DjVu document.
///
/// Created from raw bytes via [`WasmDocument::from_bytes`].
#[wasm_bindgen]
pub struct WasmDocument {
    inner: Arc<DjVuDocument>,
}

#[wasm_bindgen]
impl WasmDocument {
    /// Parse a DjVu document from a byte buffer.
    ///
    /// The buffer is moved into a shared backing store and bundled pages
    /// materialize lazily on first access (#609) — the same owned-bytes path
    /// as the native `Document::from_bytes` (LAZY_PAGE_CONSTRUCT), instead of
    /// the eager parser that copied every page at open time. The JS-visible
    /// signature is unchanged (pass a `Uint8Array`); the JS→wasm transfer is
    /// the single unavoidable copy.
    ///
    /// Throws a JavaScript `Error` if the bytes are not a valid DjVu file.
    pub fn from_bytes(data: Vec<u8>) -> Result<WasmDocument, JsError> {
        let backing: crate::djvu_document::Backing = Arc::new(data);
        let doc = DjVuDocument::parse_backed_with_options(
            backing,
            &crate::resource_limits::ParseOptions::default(),
        )
        .map_err(|e| JsError::new(&e.to_string()))?;
        Ok(WasmDocument {
            inner: Arc::new(doc),
        })
    }

    /// Total number of pages in the document.
    pub fn page_count(&self) -> u32 {
        self.inner.page_count() as u32
    }

    /// Render a contiguous batch of pages at `target_dpi`, returning one
    /// [`WasmPixmap`] per page in input order (#610).
    ///
    /// With the opt-in `wasm-threads` build (rayon Web-Worker pool via
    /// `initThreadPool`), pages render concurrently as coarse one-page tasks —
    /// the threading shape WASM_THREADS measured as viable (fine-grained
    /// compositor parallelism regressed ~9× and stays disabled). Without the
    /// pool the batch renders sequentially with identical results.
    ///
    /// Memory is bounded by the caller-chosen batch size: `count` full-size
    /// pixmaps are alive at once. Failed pages yield an error for the whole
    /// batch (all-or-nothing keeps the ordering contract simple).
    pub fn render_pages_batch(
        &self,
        target_dpi: u32,
        start: u32,
        count: u32,
    ) -> Result<Vec<WasmPixmap>, JsError> {
        let total = self.inner.page_count();
        let end = (start as usize).saturating_add(count as usize).min(total);
        let idxs: Vec<usize> = ((start as usize).min(total)..end).collect();

        let render_one = |&i: &usize| -> Result<WasmPixmap, String> {
            let pm = crate::foreign::render_at_dpi(&self.inner, i, target_dpi as f32)
                .map_err(|e| e.to_string())?;
            Ok(WasmPixmap {
                width: pm.width,
                height: pm.height,
                data: pm.data,
            })
        };

        #[cfg(feature = "parallel")]
        let out: Result<Vec<WasmPixmap>, String> = {
            use rayon::prelude::*;
            // First error in page order, not the first one in time.
            let built: Vec<Result<WasmPixmap, String>> = idxs.par_iter().map(render_one).collect();
            built.into_iter().collect()
        };
        #[cfg(not(feature = "parallel"))]
        let out: Result<Vec<WasmPixmap>, String> = idxs.iter().map(render_one).collect();

        out.map_err(|e| JsError::new(&e))
    }

    /// Return a handle to page `index` (0-based).
    ///
    /// Throws if `index >= page_count()`.
    pub fn page(&self, index: u32) -> Result<WasmPage, JsError> {
        let count = self.inner.page_count();
        if index as usize >= count {
            return Err(JsError::new(&format!(
                "page index {index} out of range (document has {count} pages)"
            )));
        }
        Ok(WasmPage {
            doc: Arc::clone(&self.inner),
            index: index as usize,
        })
    }
}

// ── WasmPage ─────────────────────────────────────────────────────────────────

/// A single page within a [`WasmDocument`].
#[wasm_bindgen]
pub struct WasmPage {
    doc: Arc<DjVuDocument>,
    index: usize,
}

#[wasm_bindgen]
impl WasmPage {
    /// Native DPI stored in the INFO chunk.
    pub fn dpi(&self) -> u32 {
        crate::foreign::page_dpi(&self.doc, self.index).unwrap_or(300)
    }

    /// Output width in pixels when rendered at `target_dpi` (after the
    /// page's INFO rotation).
    pub fn width_at(&self, target_dpi: u32) -> u32 {
        self.doc
            .page(self.index)
            .map(|p| RenderSize::at_dpi(p, target_dpi as f32).display.0)
            .unwrap_or(1)
    }

    /// Output height in pixels when rendered at `target_dpi` (after the
    /// page's INFO rotation).
    pub fn height_at(&self, target_dpi: u32) -> u32 {
        self.doc
            .page(self.index)
            .map(|p| RenderSize::at_dpi(p, target_dpi as f32).display.1)
            .unwrap_or(1)
    }

    /// Extract the plain text content of this page from the TXTz/TXTa layer.
    ///
    /// Returns `undefined` (JS `None`) if the page has no text layer.
    /// Throws a JavaScript `Error` on decode failure.
    pub fn text(&self) -> Result<Option<String>, JsError> {
        crate::foreign::text(&self.doc, self.index).map_err(|e| JsError::new(&e.to_string()))
    }

    /// Return text zone data for this page, scaled to match a render at `target_dpi`.
    ///
    /// Returns a JSON string — array of `{"t":"…","x":N,"y":N,"w":N,"h":N}` objects,
    /// one per leaf text zone, with pixel coordinates identical to the canvas produced
    /// by `render(target_dpi)`.  Leaf zones are the finest granularity stored in the
    /// text layer (word-level for richly OCR'd files, line-level otherwise).
    ///
    /// Returns `undefined` (JS `None`) if the page has no text layer.
    /// Throws a JavaScript `Error` on decode failure.
    pub fn text_zones_json(&self, target_dpi: u32) -> Result<Option<String>, JsError> {
        let page = crate::foreign::page(&self.doc, self.index)
            .map_err(|e| JsError::new(&e.to_string()))?;

        let (render_w, render_h) = RenderSize::at_dpi(page, target_dpi as f32).display;

        let Some(layer) = page
            .text_layer_at_size(render_w, render_h)
            .map_err(|e| JsError::new(&e.to_string()))?
        else {
            return Ok(None);
        };

        let mut buf = String::from("[");
        let mut first = true;
        for zone in &layer.zones {
            collect_leaf_zones(zone, &mut buf, &mut first);
        }
        buf.push(']');
        Ok(Some(buf))
    }

    /// Number of BG44 background chunks on this page.
    ///
    /// Determines how many refinement steps are available via
    /// [`render_progressive`](Self::render_progressive). Returns `0` for bilevel-only pages.
    pub fn bg44_chunk_count(&self) -> u32 {
        self.doc
            .page(self.index)
            .map(|p| p.bg44_chunks().len() as u32)
            .unwrap_or(0)
    }

    /// Fast coarse render — decodes only the first BG44 chunk (~5 ms for a
    /// typical color page).
    ///
    /// @deprecated Use `render_request` with `set_coarse()`.
    ///
    /// Returns `undefined` for bilevel-only pages (no BG44 data); use
    /// [`render`](Self::render) for those.  For color pages the result is a blurry but
    /// instantly visible preview; call [`render_progressive`](Self::render_progressive) or [`render`](Self::render)
    /// on a Web Worker to produce the final image.
    ///
    /// Throws on decode error.
    pub fn render_coarse(
        &self,
        target_dpi: u32,
    ) -> Result<Option<js_sys::Uint8ClampedArray>, JsError> {
        let page = crate::foreign::page(&self.doc, self.index)
            .map_err(|e| JsError::new(&e.to_string()))?;

        let opts = crate::foreign::render_opts_for_dpi(page, target_dpi as f32);
        let pm = render_coarse(page, &opts).map_err(|e| JsError::new(&e.to_string()))?;
        Ok(pm.map(|p| {
            let arr = js_sys::Uint8ClampedArray::new_with_length(p.data.len() as u32);
            arr.copy_from(&p.data);
            arr
        }))
    }

    /// Progressive render — decodes BG44 chunks 0..=`chunk_n` plus all
    /// foreground layers (JB2 mask, text).
    ///
    /// @deprecated Use `render_request` with `set_step(chunk_n)`.
    ///
    /// `chunk_n = 0` is equivalent to [`render_coarse`](Self::render_coarse) but also composites
    /// the mask. Each subsequent call with `chunk_n += 1` adds one more
    /// wavelet refinement pass. After the last chunk the result is identical
    /// to [`render`](Self::render).
    ///
    /// Use [`bg44_chunk_count`](Self::bg44_chunk_count) to find the maximum valid `chunk_n`
    /// (`bg44_chunk_count() - 1`).
    ///
    /// Throws on decode error or if `chunk_n` is out of range.
    pub fn render_progressive(
        &self,
        target_dpi: u32,
        chunk_n: u32,
    ) -> Result<js_sys::Uint8ClampedArray, JsError> {
        let page = crate::foreign::page(&self.doc, self.index)
            .map_err(|e| JsError::new(&e.to_string()))?;

        let opts = crate::foreign::render_opts_for_dpi(page, target_dpi as f32);
        let pm = render_progressive(page, &opts, chunk_n as usize)
            .map_err(|e| JsError::new(&e.to_string()))?;
        let arr = js_sys::Uint8ClampedArray::new_with_length(pm.data.len() as u32);
        arr.copy_from(&pm.data);
        Ok(arr)
    }

    /// Render the page at `target_dpi` and return raw RGBA pixels
    /// (`Uint8ClampedArray`, suitable for `new ImageData(pixels, w, h)`).
    ///
    /// Throws on decode error.
    pub fn render(&self, target_dpi: u32) -> Result<js_sys::Uint8ClampedArray, JsError> {
        let pm = crate::foreign::render_at_dpi(&self.doc, self.index, target_dpi as f32)
            .map_err(|e| JsError::new(&e.to_string()))?;
        // Allocate a new JS-side Uint8ClampedArray and copy the RGBA bytes
        // into it.  Using `Uint8ClampedArray::from(&[u8])` (which creates a
        // view into WASM linear memory) causes incorrect `length` values in
        // Node.js and with the externref ABI because the backing memory may
        // be freed before the caller reads the length.
        let arr = js_sys::Uint8ClampedArray::new_with_length(pm.data.len() as u32);
        arr.copy_from(&pm.data);
        Ok(arr)
    }

    /// Render `request` into a caller-owned [`WasmPixmap`], reusing its
    /// Rust-side allocation (#611): the page or a region of it, at full,
    /// progressive, or coarse quality (see [`WasmRenderRequest`]). No
    /// JS-side allocation, no wasm→JS copy — consume the pixels via
    /// [`WasmPixmap::view`], or copy them with [`WasmPixmap::to_bytes`].
    ///
    /// Throws on decode error, a step past the last chunk, or a coarse
    /// render of a page without a background.
    pub fn render_request(
        &self,
        request: &WasmRenderRequest,
        out: &mut WasmPixmap,
    ) -> Result<(), JsError> {
        let page = crate::foreign::page(&self.doc, self.index)
            .map_err(|e| JsError::new(&e.to_string()))?;
        render_request_into(page, request, out).map_err(|e| JsError::new(&e.to_string()))
    }

    /// Render into a caller-owned [`WasmPixmap`], reusing its Rust-side
    /// allocation (#611).
    ///
    /// @deprecated Use `render_request(new WasmRenderRequest(target_dpi), out)`.
    pub fn render_into_pixmap(&self, target_dpi: u32, out: &mut WasmPixmap) -> Result<(), JsError> {
        self.render_request(&WasmRenderRequest::new(target_dpi), out)
    }

    /// Progressive render into a caller-owned [`WasmPixmap`] (#611): the same
    /// refinement semantics as [`render_progressive`](Self::render_progressive).
    ///
    /// @deprecated Use `render_request` with `set_step(chunk_n)`.
    pub fn render_progressive_into_pixmap(
        &self,
        target_dpi: u32,
        chunk_n: u32,
        out: &mut WasmPixmap,
    ) -> Result<(), JsError> {
        let mut request = WasmRenderRequest::new(target_dpi);
        request.set_step(chunk_n);
        self.render_request(&request, out)
    }

    // ── Tile-first rendering (#691, contract in docs/tile-rendering.md) ──────

    /// Number of tile columns at `target_dpi` for `tile_size`-pixel tiles.
    ///
    /// Tiles live in display space: tile `(col, row)` starts at canvas pixel
    /// `(col * tile_size, row * tile_size)`; edge tiles are clipped, never
    /// padded, so blitting every tile covers the canvas exactly once.
    pub fn tile_cols(&self, target_dpi: u32, tile_size: u32) -> Result<u32, JsError> {
        self.tile_layout(target_dpi, tile_size)
            .map(|layout| layout.cols())
    }

    /// Number of tile rows at `target_dpi` for `tile_size`-pixel tiles.
    pub fn tile_rows(&self, target_dpi: u32, tile_size: u32) -> Result<u32, JsError> {
        self.tile_layout(target_dpi, tile_size)
            .map(|layout| layout.rows())
    }

    /// Render one full-quality tile, returning a [`WasmPixmap`] whose
    /// `width()`/`height()` give the (possibly clipped) tile dimensions.
    ///
    /// Byte-identical to the matching rectangle of [`render`](Self::render);
    /// assembled from the page's composited-tile cache (cache state never
    /// changes bytes, only latency).
    ///
    /// Throws on decode error or a grid violation.
    pub fn render_tile(
        &self,
        target_dpi: u32,
        tile_size: u32,
        col: u32,
        row: u32,
    ) -> Result<WasmPixmap, JsError> {
        let page = crate::foreign::page(&self.doc, self.index)
            .map_err(|e| JsError::new(&e.to_string()))?;
        let opts = crate::foreign::render_opts_for_dpi(page, target_dpi as f32);
        let pm = djvu_tile::render_tile_cached(page, &opts, tile_size, col, row)
            .map_err(|e| JsError::new(&e.to_string()))?;
        Ok(WasmPixmap {
            data: pm.data,
            width: pm.width,
            height: pm.height,
        })
    }

    /// Render one tile at progressive quality step `chunk_n` (BG44 chunks
    /// `0..=chunk_n` only), byte-identical to the tile's rectangle of
    /// [`render_progressive`](Self::render_progressive) with the same
    /// `chunk_n`. Partial-quality tiles are never cached.
    ///
    /// On bilevel pages (no BG44 data) `chunk_n = 0` is the full render.
    /// Throws on decode error, a grid violation, or `chunk_n` out of range.
    pub fn render_tile_progressive(
        &self,
        target_dpi: u32,
        tile_size: u32,
        col: u32,
        row: u32,
        chunk_n: u32,
    ) -> Result<WasmPixmap, JsError> {
        let page = crate::foreign::page(&self.doc, self.index)
            .map_err(|e| JsError::new(&e.to_string()))?;
        let opts = crate::foreign::render_opts_for_dpi(page, target_dpi as f32);
        let controls = djvu_tile::TileRenderControls {
            quality_step: Some(chunk_n as usize),
            ..Default::default()
        };
        let pm = djvu_tile::render_tile_with(page, &opts, tile_size, col, row, &controls)
            .map_err(|e| JsError::new(&e.to_string()))?;
        Ok(WasmPixmap {
            data: pm.data,
            width: pm.width,
            height: pm.height,
        })
    }

    /// Render one full-quality tile into a caller-owned [`WasmPixmap`]
    /// (#611 pattern): a pan/zoom session reuses one Rust-side allocation
    /// per on-screen tile slot instead of allocating per frame.
    pub fn render_tile_into_pixmap(
        &self,
        target_dpi: u32,
        tile_size: u32,
        col: u32,
        row: u32,
        out: &mut WasmPixmap,
    ) -> Result<(), JsError> {
        let page = crate::foreign::page(&self.doc, self.index)
            .map_err(|e| JsError::new(&e.to_string()))?;
        let opts = crate::foreign::render_opts_for_dpi(page, target_dpi as f32);
        let pm = djvu_tile::render_tile_cached(page, &opts, tile_size, col, row)
            .map_err(|e| JsError::new(&e.to_string()))?;
        out.width = pm.width;
        out.height = pm.height;
        out.data.clear();
        out.data.extend_from_slice(&pm.data);
        Ok(())
    }
}

impl WasmPage {
    fn tile_layout(
        &self,
        target_dpi: u32,
        tile_size: u32,
    ) -> Result<djvu_tile::TileLayout, JsError> {
        let page = crate::foreign::page(&self.doc, self.index)
            .map_err(|e| JsError::new(&e.to_string()))?;
        let opts = crate::foreign::render_opts_for_dpi(page, target_dpi as f32);
        djvu_tile::TileLayout::new(page, &opts, tile_size).map_err(|e| JsError::new(&e.to_string()))
    }
}

// ── Text zone helpers ─────────────────────────────────────────────────────────

/// Recursively collect leaf zones (zones without children) into a JSON array.
fn collect_leaf_zones(zone: &crate::text::TextZone, buf: &mut String, first: &mut bool) {
    if zone.children.is_empty() {
        let t = zone.text.trim();
        if t.is_empty() {
            return;
        }
        if !*first {
            buf.push(',');
        }
        *first = false;
        buf.push_str("{\"t\":\"");
        json_escape_into(t, buf);
        buf.push_str("\",\"x\":");
        buf.push_str(&zone.rect.x.to_string());
        buf.push_str(",\"y\":");
        buf.push_str(&zone.rect.y.to_string());
        buf.push_str(",\"w\":");
        buf.push_str(&zone.rect.width.to_string());
        buf.push_str(",\"h\":");
        buf.push_str(&zone.rect.height.to_string());
        buf.push('}');
    } else {
        for child in &zone.children {
            collect_leaf_zones(child, buf, first);
        }
    }
}

/// Append `s` to `buf` with JSON string escaping (no surrounding quotes).
fn json_escape_into(s: &str, buf: &mut String) {
    for ch in s.chars() {
        match ch {
            '"' => buf.push_str("\\\""),
            '\\' => buf.push_str("\\\\"),
            '\n' => buf.push_str("\\n"),
            '\r' => buf.push_str("\\r"),
            '\t' => buf.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                buf.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => buf.push(c),
        }
    }
}

// ── Lazy Range-based open (#588, `wasm-lazy` feature) ────────────────────────

#[cfg(all(feature = "async", target_arch = "wasm32"))]
mod lazy {
    use std::{
        collections::{BTreeMap, VecDeque},
        future::Future,
        io::SeekFrom,
        pin::Pin,
        sync::Arc,
        task::{Context, Poll},
    };

    use tokio::io::{AsyncRead, AsyncSeek, ReadBuf};
    use wasm_bindgen::prelude::*;
    use wasm_bindgen_futures::JsFuture;

    use super::WasmPixmap;
    use crate::djvu_async::{
        AsyncComponentResolver, LazyDocument, LazyIndirectDocument, from_async_reader_lazy_local,
    };
    use crate::djvu_document::{ComponentId, ComponentKind, ComponentResolveError, DjVuPage};

    /// Fetch granularity. 64 KiB matches the native HTTP-Range probe (#584):
    /// the whole head + DIRM index of a 500-component book fits in one block,
    /// and a typical page spans 1–3 blocks.
    const LAZY_BLOCK: u64 = 64 * 1024;
    /// Block cache budget (64 × 64 KiB = 4 MiB).
    const LAZY_CACHE_BLOCKS: usize = 64;

    fn js_io_err(e: JsValue) -> std::io::Error {
        std::io::Error::other(
            e.as_string()
                .unwrap_or_else(|| "JS range fetch failed".to_string()),
        )
    }

    /// `AsyncRead + AsyncSeek` over a JS-supplied
    /// `(offset: number, len: number) -> Promise<Uint8Array>` callback, with a
    /// 64 KiB-block LRU cache so the DIRM index walk and page fetches issue a
    /// handful of coarse range requests instead of one per small read.
    struct JsRangeReader {
        fetch: js_sys::Function,
        len: u64,
        pos: u64,
        cache: BTreeMap<u64, Vec<u8>>,
        order: VecDeque<u64>,
        pending: Option<(u64, JsFuture)>,
    }

    impl JsRangeReader {
        fn new(fetch: js_sys::Function, len: u64) -> Self {
            Self {
                fetch,
                len,
                pos: 0,
                cache: BTreeMap::new(),
                order: VecDeque::new(),
                pending: None,
            }
        }

        /// Poll the (possibly newly started) fetch of `block` to completion.
        fn poll_block(&mut self, block: u64, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            if self.cache.contains_key(&block) {
                return Poll::Ready(Ok(()));
            }
            if self.pending.as_ref().map(|(b, _)| *b) != Some(block) {
                let start = block * LAZY_BLOCK;
                let len = LAZY_BLOCK.min(self.len.saturating_sub(start));
                let promise = self
                    .fetch
                    .call2(
                        &JsValue::NULL,
                        &JsValue::from_f64(start as f64),
                        &JsValue::from_f64(len as f64),
                    )
                    .map_err(js_io_err)?;
                self.pending = Some((block, JsFuture::from(js_sys::Promise::from(promise))));
            }
            let (_, fut) = self.pending.as_mut().expect("pending fetch just set");
            match Pin::new(fut).poll(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Err(e)) => {
                    self.pending = None;
                    Poll::Ready(Err(js_io_err(e)))
                }
                Poll::Ready(Ok(value)) => {
                    self.pending = None;
                    let bytes = js_sys::Uint8Array::new(&value).to_vec();
                    self.cache.insert(block, bytes);
                    self.order.push_back(block);
                    if self.order.len() > LAZY_CACHE_BLOCKS
                        && let Some(old) = self.order.pop_front()
                    {
                        self.cache.remove(&old);
                    }
                    Poll::Ready(Ok(()))
                }
            }
        }
    }

    impl AsyncRead for JsRangeReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            if self.pos >= self.len {
                return Poll::Ready(Ok(())); // EOF: leave `buf` unfilled
            }
            let block = self.pos / LAZY_BLOCK;
            match self.poll_block(block, cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Ready(Ok(())) => {}
            }
            let data = &self.cache[&block];
            let off = (self.pos - block * LAZY_BLOCK) as usize;
            let n = buf.remaining().min(data.len().saturating_sub(off));
            if n == 0 {
                return Poll::Ready(Err(std::io::Error::other(
                    "range fetch returned fewer bytes than requested",
                )));
            }
            buf.put_slice(&data[off..off + n]);
            self.pos += n as u64;
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncSeek for JsRangeReader {
        fn start_seek(mut self: Pin<&mut Self>, position: SeekFrom) -> std::io::Result<()> {
            self.pos = match position {
                SeekFrom::Start(o) => o,
                SeekFrom::End(o) => (self.len as i64).saturating_add(o).max(0) as u64,
                SeekFrom::Current(o) => (self.pos as i64).saturating_add(o).max(0) as u64,
            };
            Ok(())
        }

        fn poll_complete(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<u64>> {
            Poll::Ready(Ok(self.pos))
        }
    }

    /// Lazily opened DjVu document driven by a JS range-fetch callback (#588).
    ///
    /// `open(totalLen, fetch)` indexes the document from ~one block of head
    /// bytes; each `page(i)` / `render_page(i, dpi)` then fetches only that
    /// page's byte range (plus any shared dictionary it references, cached).
    /// The `fetch` callback receives `(offset, len)` and must resolve to a
    /// `Uint8Array` of exactly `len` bytes — e.g. an HTTP `Range` request:
    ///
    /// ```js
    /// const doc = await WasmLazyDocument.open(totalLen, async (offset, len) => {
    ///   const r = await fetch(url, { headers: { Range: `bytes=${offset}-${offset + len - 1}` } });
    ///   return new Uint8Array(await r.arrayBuffer());
    /// });
    /// ```
    #[wasm_bindgen]
    pub struct WasmLazyDocument {
        inner: LazyDocument<JsRangeReader>,
    }

    #[wasm_bindgen]
    impl WasmLazyDocument {
        /// Index a document of `total_len` bytes through the `fetch` callback.
        pub async fn open(
            total_len: f64,
            fetch: js_sys::Function,
        ) -> Result<WasmLazyDocument, JsError> {
            let reader = JsRangeReader::new(fetch, total_len as u64);
            let inner = from_async_reader_lazy_local(reader)
                .await
                .map_err(|e| JsError::new(&e.to_string()))?;
            Ok(WasmLazyDocument { inner })
        }

        /// Number of pages in the index.
        pub fn page_count(&self) -> u32 {
            self.inner.page_count() as u32
        }

        /// Fetch (or reuse) page `index` and return `[width_px, height_px, dpi]`
        /// at the page's native resolution.
        pub async fn page_info(&self, index: u32) -> Result<js_sys::Uint32Array, JsError> {
            let page = self
                .inner
                .page_async(index as usize)
                .await
                .map_err(|e| JsError::new(&e.to_string()))?;
            let out = [page.width() as u32, page.height() as u32, page.dpi() as u32];
            Ok(js_sys::Uint32Array::from(&out[..]))
        }

        /// Fetch (or reuse) page `index` and render it at `target_dpi` into a
        /// [`WasmPixmap`] (zero-copy `view()` / owned `to_bytes()`).
        pub async fn render_page(
            &self,
            index: u32,
            target_dpi: u32,
        ) -> Result<WasmPixmap, JsError> {
            let page = self
                .inner
                .page_async(index as usize)
                .await
                .map_err(|e| JsError::new(&e.to_string()))?;
            render(&page, target_dpi, u32::MAX)
        }

        /// Progressive variant of [`render_page`](Self::render_page): decode at
        /// most `chunk_n` BG44 refinement chunks (0 ⇒ mask/foreground only) for
        /// a fast blurry-to-sharp first paint. Fetching is unchanged (the page
        /// range is one transfer); only decode work is bounded.
        pub async fn render_page_progressive(
            &self,
            index: u32,
            target_dpi: u32,
            chunk_n: u32,
        ) -> Result<WasmPixmap, JsError> {
            let page = self
                .inner
                .page_async(index as usize)
                .await
                .map_err(|e| JsError::new(&e.to_string()))?;
            render(&page, target_dpi, chunk_n)
        }
    }

    /// Render `page` at `target_dpi`; `chunk_n == u32::MAX` decodes every
    /// BG44 chunk, any other value renders progressively.
    fn render(page: &Arc<DjVuPage>, target_dpi: u32, chunk_n: u32) -> Result<WasmPixmap, JsError> {
        let opts = crate::foreign::render_opts_for_dpi(page, target_dpi as f32);
        let pm = if chunk_n == u32::MAX {
            crate::djvu_render::render_pixmap(page, &opts)
        } else {
            crate::wasm::render_progressive(page, &opts, chunk_n as usize)
        }
        .map_err(|e| JsError::new(&e.to_string()))?;
        Ok(WasmPixmap {
            width: pm.width,
            height: pm.height,
            data: pm.data,
        })
    }

    /// [`AsyncComponentResolver`] over a JS
    /// `(name: string, kind: string) -> Promise<Uint8Array | null>` callback.
    ///
    /// `kind` is `"page"`, `"shared"`, or `"thumbnail"`. A `null` or
    /// `undefined` result means the component is missing; a thrown error or a
    /// rejected promise means it could not be read.
    struct JsComponentResolver {
        resolve: js_sys::Function,
    }

    impl AsyncComponentResolver for JsComponentResolver {
        fn resolve(
            &self,
            component: &ComponentId,
        ) -> impl Future<Output = Result<Vec<u8>, ComponentResolveError>> {
            let kind = match component.kind {
                ComponentKind::Page => "page",
                ComponentKind::Shared => "shared",
                ComponentKind::Thumbnail => "thumbnail",
            };
            let call = self.resolve.call2(
                &JsValue::NULL,
                &JsValue::from_str(&component.name),
                &JsValue::from_str(kind),
            );
            let component = component.clone();
            async move {
                let failed = |e: JsValue| ComponentResolveError::Failed {
                    component: component.clone(),
                    reason: e
                        .as_string()
                        .or_else(|| {
                            e.dyn_ref::<js_sys::Error>()
                                .map(|err| String::from(err.message()))
                        })
                        .unwrap_or_else(|| "JS component resolver failed".to_string()),
                };
                let value = call.map_err(failed)?;
                // `Promise.resolve` also accepts a plain (non-promise) value.
                let value = JsFuture::from(js_sys::Promise::resolve(&value))
                    .await
                    .map_err(failed)?;
                if value.is_null() || value.is_undefined() {
                    return Err(ComponentResolveError::Missing { component });
                }
                Ok(js_sys::Uint8Array::new(&value).to_vec())
            }
        }
    }

    /// Lazily opened indirect DjVu document: the index file lists the pages,
    /// and each page lives in its own file.
    ///
    /// `open(indexBytes, resolve)` reads only the directory. Each
    /// `render_page(i, dpi)` then asks `resolve` for that page file, and for a
    /// shared symbol dictionary the page includes (fetched once, cached).
    /// `resolve` receives `(name, kind)` — the file name from the index and
    /// `"page"` or `"shared"` — and resolves to the file's bytes as a
    /// `Uint8Array`, or to `null` when the file does not exist:
    ///
    /// ```js
    /// const base = new URL("book/", location.href);
    /// const index = new Uint8Array(await (await fetch(new URL("index.djvu", base))).arrayBuffer());
    /// const doc = WasmLazyIndirectDocument.open(index, async (name, kind) => {
    ///   const r = await fetch(new URL(name, base));
    ///   return r.ok ? new Uint8Array(await r.arrayBuffer()) : null;
    /// });
    /// ```
    ///
    /// A bundled or single-page file is not an index: open it with
    /// [`WasmLazyDocument`].
    #[wasm_bindgen]
    pub struct WasmLazyIndirectDocument {
        inner: LazyIndirectDocument<JsComponentResolver>,
    }

    #[wasm_bindgen]
    impl WasmLazyIndirectDocument {
        /// Read the directory of an indirect index file. No component is
        /// fetched here.
        pub fn open(
            index: &[u8],
            resolve: js_sys::Function,
        ) -> Result<WasmLazyIndirectDocument, JsError> {
            let inner = LazyIndirectDocument::from_index(index, JsComponentResolver { resolve })
                .map_err(|e| JsError::new(&e.to_string()))?;
            Ok(WasmLazyIndirectDocument { inner })
        }

        /// Number of pages in the index.
        pub fn page_count(&self) -> u32 {
            self.inner.page_count() as u32
        }

        /// The file name of page `index`, as passed to `resolve`, or
        /// `undefined` when `index` is out of range.
        pub fn page_name(&self, index: u32) -> Option<String> {
            self.inner
                .page_component(index as usize)
                .map(|c| c.name.clone())
        }

        /// Fetch (or reuse) page `index` and return `[width_px, height_px, dpi]`
        /// at the page's native resolution.
        pub async fn page_info(&self, index: u32) -> Result<js_sys::Uint32Array, JsError> {
            let page = self.page(index).await?;
            let out = [page.width() as u32, page.height() as u32, page.dpi() as u32];
            Ok(js_sys::Uint32Array::from(&out[..]))
        }

        /// Fetch (or reuse) page `index` and render it at `target_dpi` into a
        /// [`WasmPixmap`].
        pub async fn render_page(
            &self,
            index: u32,
            target_dpi: u32,
        ) -> Result<WasmPixmap, JsError> {
            let page = self.page(index).await?;
            render(&page, target_dpi, u32::MAX)
        }

        /// Progressive variant of [`render_page`](Self::render_page): decode at
        /// most `chunk_n` BG44 refinement chunks (0 ⇒ mask/foreground only).
        pub async fn render_page_progressive(
            &self,
            index: u32,
            target_dpi: u32,
            chunk_n: u32,
        ) -> Result<WasmPixmap, JsError> {
            let page = self.page(index).await?;
            render(&page, target_dpi, chunk_n)
        }

        async fn page(&self, index: u32) -> Result<Arc<DjVuPage>, JsError> {
            self.inner
                .page_async(index as usize)
                .await
                .map_err(|e| JsError::new(&e.to_string()))
        }
    }
}

#[cfg(all(feature = "async", target_arch = "wasm32"))]
pub use lazy::{WasmLazyDocument, WasmLazyIndirectDocument};

// ── Tests ─────────────────────────────────────────────────────────────────────
//
// Native tests (`#[cfg(not(target_arch = "wasm32"))]`) exercise the underlying
// DjVuDocument/render_pixmap logic directly, bypassing JsError (which panics
// outside a WASM runtime).
//
// WASM tests (`#[cfg(target_arch = "wasm32")]`) use `#[wasm_bindgen_test]` and
// run with `wasm-pack test --node` or `--headless --firefox/chrome`.

#[cfg(not(target_arch = "wasm32"))]
#[cfg(test)]
mod native_tests {
    use super::*;
    use crate::djvu_render::{RenderOptions, render_pixmap};

    fn boy_bytes() -> Vec<u8> {
        // boy.djvu: 192×256 px, 300 dpi, color IW44.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/boy.djvu");
        std::fs::read(&path).expect("boy.djvu not found in tests/fixtures/")
    }

    /// Valid DjVu bytes must parse to a 1-page document.
    #[test]
    fn wasm_document_from_bytes_page_count() {
        let bytes = boy_bytes();
        let doc = DjVuDocument::parse(&bytes).expect("parse failed");
        assert_eq!(doc.page_count(), 1);
    }

    /// Garbage bytes must produce a parse error.
    #[test]
    fn wasm_document_from_bytes_invalid_returns_error() {
        assert!(DjVuDocument::parse(b"not a djvu file").is_err());
    }

    /// Page 0 of boy.djvu must report 100 dpi (value in INFO chunk).
    #[test]
    fn wasm_page_dpi() {
        let bytes = boy_bytes();
        let doc = DjVuDocument::parse(&bytes).unwrap();
        assert_eq!(doc.page(0).unwrap().dpi(), 100);
    }

    /// boy.djvu (192×256 px @ 100 dpi) rendered at 50 dpi → 96×128 px.
    #[test]
    fn wasm_page_dimensions_at_dpi() {
        let bytes = boy_bytes();
        let doc = DjVuDocument::parse(&bytes).unwrap();
        let page = doc.page(0).unwrap();
        let scale = 50_f32 / page.dpi() as f32; // 50/100 = 0.5
        let w = ((page.width() as f32 * scale).round() as u32).max(1);
        let h = ((page.height() as f32 * scale).round() as u32).max(1);
        assert_eq!(w, 96);
        assert_eq!(h, 128);
    }

    /// Rendering must produce exactly w×h×4 RGBA bytes.
    #[test]
    fn wasm_page_render_pixel_count() {
        let bytes = boy_bytes();
        let doc = DjVuDocument::parse(&bytes).unwrap();
        let page = doc.page(0).unwrap();
        let scale = 150_f32 / page.dpi() as f32;
        let w = ((page.width() as f32 * scale).round() as u32).max(1);
        let h = ((page.height() as f32 * scale).round() as u32).max(1);
        let opts = RenderOptions {
            width: w,
            height: h,
            ..Default::default()
        };
        let pm = render_pixmap(page, &opts).expect("render failed");
        assert_eq!(pm.data.len(), (w * h * 4) as usize);
    }

    /// render_coarse on a color page returns Some with correct pixel count.
    #[test]
    fn wasm_render_coarse_color_page() {
        let bytes = boy_bytes();
        let doc = DjVuDocument::parse(&bytes).unwrap();
        let page = doc.page(0).unwrap();
        let scale = 150_f32 / page.dpi() as f32;
        let w = ((page.width() as f32 * scale).round() as u32).max(1);
        let h = ((page.height() as f32 * scale).round() as u32).max(1);
        let opts = crate::foreign::render_opts_for_dpi(page, 150.0);
        let result = render_coarse(page, &opts).expect("render_coarse failed");
        // boy.djvu has BG44 chunks — coarse result must be Some
        let pm = result.expect("expected Some for color page");
        assert_eq!(pm.data.len(), (w * h * 4) as usize);
    }

    /// The viewer render options are permissive; `render_tile` must still
    /// fill the page's tile cache (before, permissive requests bypassed it).
    #[test]
    fn wasm_render_tile_fills_tile_cache() {
        let bytes = boy_bytes();
        let doc = DjVuDocument::parse(&bytes).unwrap();
        let page = doc.page(0).unwrap();
        let opts = crate::foreign::render_opts_for_dpi(page, 150.0);
        assert!(opts.permissive);
        assert_eq!(page.render_layers().tile_cache_len(), 0);
        let tile = djvu_tile::render_tile_cached(page, &opts, 64, 1, 1).unwrap();
        assert!(page.render_layers().tile_cache_len() > 0);
        let again = djvu_tile::render_tile_cached(page, &opts, 64, 1, 1).unwrap();
        assert_eq!(tile.data, again.data);
    }

    /// bg44_chunk_count is > 0 for a color page.
    #[test]
    fn wasm_bg44_chunk_count_color_page() {
        let bytes = boy_bytes();
        let doc = DjVuDocument::parse(&bytes).unwrap();
        assert!(!doc.page(0).unwrap().bg44_chunks().is_empty());
    }

    /// render_progressive with chunk_n = 0 succeeds and returns correct pixel count.
    #[test]
    fn wasm_render_progressive_chunk0() {
        let bytes = boy_bytes();
        let doc = DjVuDocument::parse(&bytes).unwrap();
        let page = doc.page(0).unwrap();
        let scale = 150_f32 / page.dpi() as f32;
        let w = ((page.width() as f32 * scale).round() as u32).max(1);
        let h = ((page.height() as f32 * scale).round() as u32).max(1);
        let opts = crate::foreign::render_opts_for_dpi(page, 150.0);
        let pm = render_progressive(page, &opts, 0).expect("render_progressive failed");
        assert_eq!(pm.data.len(), (w * h * 4) as usize);
    }

    fn run(page: &crate::djvu_document::DjVuPage, request: &WasmRenderRequest) -> WasmPixmap {
        let mut out = WasmPixmap::new();
        render_request_into(page, request, &mut out).expect("render_request failed");
        assert_eq!(out.data.len(), out.width as usize * out.height as usize * 4);
        out
    }

    /// Every request gives the bytes of the matching render; one pixmap
    /// serves them all.
    #[test]
    fn wasm_render_request_matches_renders() {
        let bytes = boy_bytes();
        let doc = DjVuDocument::parse(&bytes).unwrap();
        let page = doc.page(0).unwrap();
        let opts = crate::foreign::render_opts_for_dpi(page, 150.0);
        let full = render_pixmap(page, &opts).unwrap();

        let mut request = WasmRenderRequest::new(150);
        let out = run(page, &request);
        assert_eq!((out.width, out.height), (full.width, full.height));
        assert_eq!(out.data, full.data);

        request.set_region(20, 30, 64, 48);
        let out = run(page, &request);
        assert_eq!((out.width, out.height), (64, 48));
        let stride = full.width as usize * 4;
        for row in 0..48 {
            let start = (30 + row) * stride + 20 * 4;
            assert_eq!(
                &out.data[row * 64 * 4..(row + 1) * 64 * 4],
                &full.data[start..start + 64 * 4],
                "row {row}"
            );
        }

        request.clear_region();
        request.set_step(0);
        assert_eq!(
            run(page, &request).data,
            render_progressive(page, &opts, 0).unwrap().data
        );

        request.set_coarse();
        assert_eq!(
            run(page, &request).data,
            render_coarse(page, &opts).unwrap().unwrap().data
        );

        request.set_full();
        assert_eq!(run(page, &request).data, full.data);
    }

    /// A coarse render of a page without a background, and a step past the
    /// last chunk, are errors.
    #[test]
    fn wasm_render_request_errors() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/boy_jb2.djvu");
        let bytes = std::fs::read(path).unwrap();
        let doc = DjVuDocument::parse(&bytes).unwrap();
        let page = doc.page(0).unwrap();
        let mut request = WasmRenderRequest::new(100);
        request.set_coarse();
        let mut out = WasmPixmap::new();
        assert!(matches!(
            render_request_into(page, &request, &mut out),
            Err(RenderError::NoBackground)
        ));

        let bytes = boy_bytes();
        let doc = DjVuDocument::parse(&bytes).unwrap();
        let page = doc.page(0).unwrap();
        request.set_step(page.bg44_chunks().len() as u32);
        assert!(render_request_into(page, &request, &mut out).is_err());
    }
}

// Browser-side WASM checks live under examples/wasm; see examples/wasm/README.md.
