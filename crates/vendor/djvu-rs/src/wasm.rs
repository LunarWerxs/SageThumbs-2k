//! WebAssembly bindings for djvu-rs.
//!
//! Exposes a minimal browser-friendly API via `wasm-bindgen`:
//!
//! - [`WasmDocument`] — parsed DjVu document, created from raw bytes.
//! - [`WasmPage`]     — a single page, capable of rendering to RGBA pixels.
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
//! // Progressive render: instant coarse preview, then refine
//! const coarse = page.render_coarse(150); // undefined for bilevel pages
//! if (coarse) ctx.putImageData(new ImageData(coarse, page.width_at(150), page.height_at(150)), 0, 0);
//! for (let n = 0; n < page.bg44_chunk_count(); n++) {
//!   const refined = page.render_progressive(150, n);
//!   ctx.putImageData(new ImageData(refined, page.width_at(150), page.height_at(150)), 0, 0);
//! }
//! ```

use std::sync::Arc;

use wasm_bindgen::prelude::*;

use crate::{
    djvu_document::DjVuDocument,
    djvu_render::{render_coarse, render_progressive},
};

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
///   [`render_into_pixmap`](WasmPage::render_into_pixmap) /
///   [`render_progressive_into_pixmap`](WasmPage::render_progressive_into_pixmap)
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
        let doc = DjVuDocument::parse_backed(backing).map_err(|e| JsError::new(&e.to_string()))?;
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
            idxs.par_iter().map(render_one).collect()
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

    /// Output width in pixels when rendered at `target_dpi`.
    pub fn width_at(&self, target_dpi: u32) -> u32 {
        self.doc
            .page(self.index)
            .map(|p| crate::export_common::size_at_dpi(p, target_dpi as f32).0)
            .unwrap_or(1)
    }

    /// Output height in pixels when rendered at `target_dpi`.
    pub fn height_at(&self, target_dpi: u32) -> u32 {
        self.doc
            .page(self.index)
            .map(|p| crate::export_common::size_at_dpi(p, target_dpi as f32).1)
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
    /// Returns `null` if the page has no text layer.
    /// Throws a JavaScript `Error` on decode failure.
    pub fn text_zones_json(&self, target_dpi: u32) -> Result<Option<String>, JsError> {
        let page = crate::foreign::page(&self.doc, self.index)
            .map_err(|e| JsError::new(&e.to_string()))?;

        let (render_w, render_h) = crate::export_common::size_at_dpi(page, target_dpi as f32);

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
    /// [`render_progressive`]. Returns `0` for bilevel-only pages.
    pub fn bg44_chunk_count(&self) -> u32 {
        self.doc
            .page(self.index)
            .map(|p| p.bg44_chunks().len() as u32)
            .unwrap_or(0)
    }

    /// Fast coarse render — decodes only the first BG44 chunk (~5 ms for a
    /// typical color page).
    ///
    /// Returns `undefined` for bilevel-only pages (no BG44 data); use
    /// [`render`] for those.  For color pages the result is a blurry but
    /// instantly visible preview; call [`render_progressive`] or [`render`]
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
    /// `chunk_n = 0` is equivalent to [`render_coarse`] but also composites
    /// the mask. Each subsequent call with `chunk_n += 1` adds one more
    /// wavelet refinement pass. After the last chunk the result is identical
    /// to [`render`].
    ///
    /// Use [`bg44_chunk_count`] to find the maximum valid `chunk_n`
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

    /// Render into a caller-owned [`WasmPixmap`], reusing its Rust-side
    /// allocation (#611). No JS-side allocation, no wasm→JS copy — consume
    /// the pixels via [`WasmPixmap::view`].
    pub fn render_into_pixmap(&self, target_dpi: u32, out: &mut WasmPixmap) -> Result<(), JsError> {
        let page = crate::foreign::page(&self.doc, self.index)
            .map_err(|e| JsError::new(&e.to_string()))?;
        let opts = crate::foreign::render_opts_for_dpi(page, target_dpi as f32);
        let need = opts.width as usize * opts.height as usize * 4;
        out.data.resize(need, 0);
        crate::djvu_render::render_into(page, &opts, &mut out.data)
            .map_err(|e| JsError::new(&e.to_string()))?;
        out.width = opts.width;
        out.height = opts.height;
        Ok(())
    }

    /// Progressive render into a caller-owned [`WasmPixmap`] (#611): the same
    /// refinement semantics as [`render_progressive`](Self::render_progressive),
    /// but an N-pass progressive session reuses one buffer instead of
    /// allocating and copying N full frames.
    pub fn render_progressive_into_pixmap(
        &self,
        target_dpi: u32,
        chunk_n: u32,
        out: &mut WasmPixmap,
    ) -> Result<(), JsError> {
        let page = crate::foreign::page(&self.doc, self.index)
            .map_err(|e| JsError::new(&e.to_string()))?;
        let opts = crate::foreign::render_opts_for_dpi(page, target_dpi as f32);
        let pm = render_progressive(page, &opts, chunk_n as usize)
            .map_err(|e| JsError::new(&e.to_string()))?;
        out.width = pm.width;
        out.height = pm.height;
        out.data.clear();
        out.data.extend_from_slice(&pm.data);
        Ok(())
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
    use crate::djvu_async::{LazyDocument, from_async_reader_lazy_local};

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
            Self::render(&page, target_dpi, u32::MAX)
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
            Self::render(&page, target_dpi, chunk_n)
        }

        fn render(
            page: &Arc<crate::djvu_document::DjVuPage>,
            target_dpi: u32,
            chunk_n: u32,
        ) -> Result<WasmPixmap, JsError> {
            let opts = crate::foreign::render_opts_for_dpi(page, target_dpi as f32);
            let pm = if chunk_n == u32::MAX {
                crate::djvu_render::render_pixmap(page, &opts)
            } else {
                crate::djvu_render::render_progressive(page, &opts, chunk_n as usize)
            }
            .map_err(|e| JsError::new(&e.to_string()))?;
            Ok(WasmPixmap {
                width: pm.width,
                height: pm.height,
                data: pm.data,
            })
        }
    }
}

#[cfg(all(feature = "async", target_arch = "wasm32"))]
pub use lazy::WasmLazyDocument;

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
}

// WASM browser tests (wasm-pack test --headless --firefox) are defined in
// tests/wasm_browser.rs to keep them separate from the build path.
// See examples/wasm/README.md for how to run them.
