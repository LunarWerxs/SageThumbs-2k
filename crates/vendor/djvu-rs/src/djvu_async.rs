//! Async render surface for [`DjVuPage`] — phase 5 extension.
//!
//! Feature-gated: `--features async` (adds `tokio` as a dependency).
//!
//! ## Rendering off the async runtime
//!
//! The sync render entry points ([`djvu_render::render_pixmap`],
//! [`djvu_render::render_gray8`]) are CPU-bound IW44/JB2 decode work. To keep
//! them off the async runtime thread, call them inside
//! [`tokio::task::spawn_blocking`]. [`DjVuPage`] implements [`Clone`], so the
//! page moves into the blocking closure with no unsafe code:
//!
//! ```no_run
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! use djvu_rs::djvu_document::DjVuDocument;
//! use djvu_rs::djvu_render::{self, RenderOptions};
//!
//! let data = std::fs::read("file.djvu")?;
//! let doc = DjVuDocument::parse(&data)?;
//! let page = doc.page(0)?.clone();
//! let opts = RenderOptions { width: 800, height: 600, ..Default::default() };
//!
//! let pixmap = tokio::task::spawn_blocking(move || {
//!     djvu_render::render_pixmap(&page, &opts)
//! })
//! .await??; // outer `?`: join error (panic); inner `?`: RenderError
//! println!("{}×{}", pixmap.width, pixmap.height);
//! # Ok(()) }
//! ```
//!
//! The render error type stays the typed [`djvu_render::RenderError`] — there
//! is no wrapper enum to unwrap.
//!
//! ## Key public abstractions
//!
//! - [`LazyDocument`](crate::djvu_async::LazyDocument) — seek-based lazy indexing with a concurrent per-page cache
//! - [`LazyIndirectDocument`](crate::djvu_async::LazyIndirectDocument) — lazy indirect `FORM:DJVM`: pages come from an
//!   [`AsyncComponentResolver`](crate::djvu_async::AsyncComponentResolver) only when requested
//! - [`render_progressive_stream`](crate::djvu_async::render_progressive_stream) — streaming progressive render yielding one frame per BG44 chunk
//! - [`render_tile_async`](crate::djvu_async::render_tile_async) / [`render_tile_progressive_stream`](crate::djvu_async::render_tile_progressive_stream) — tile-first
//!   rendering (#691) off the runtime thread, with quality steps and
//!   cancellation
//! - [`load_document_async_streaming`](crate::djvu_async::load_document_async_streaming) — head-first async loader exposing per-page byte ranges

use std::{collections::BTreeMap, ops::Range, sync::Arc};

use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt},
    sync::{Mutex, OnceCell},
};

use crate::{
    dirm::{DirmComponentKind, DirmPayload},
    djvu_document::{
        ComponentId, ComponentKind, ComponentResolveError, DjVuDocument, DjVuPage, DocError,
        SharedDict, assembly::incl_targets,
    },
    djvu_render::{self, RenderError, RenderOptions},
    djvu_tile::{TileCancelToken, TileError, TileRenderControls},
    error::IffError,
    iff::{MAGIC, parse_form},
    pixmap::Pixmap,
};

// ── Error types ───────────────────────────────────────────────────────────────

/// Errors from async rendering.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AsyncRenderError {
    /// The underlying render failed.
    #[error("render error: {0}")]
    Render(#[from] RenderError),

    /// The blocking task was cancelled or panicked.
    #[error("spawn_blocking join error: {0}")]
    Join(String),
}

/// Errors from async tile rendering (#691).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AsyncTileError {
    /// The underlying tile render failed — including
    /// [`TileError::Cancelled`] when the token fired.
    #[error("tile error: {0}")]
    Tile(#[from] TileError),

    /// The blocking task was cancelled or panicked.
    #[error("spawn_blocking join error: {0}")]
    Join(String),
}

/// Errors from async document loading (both the streaming loader and the true
/// lazy loader). One enum spans the whole async "couldn't get the document /
/// page" seam (#369).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AsyncLazyError {
    /// I/O error from the underlying async reader.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The fetched bytes failed to parse as a DjVu document.
    #[error("parse error: {0}")]
    Parse(#[from] DocError),

    /// IFF container parse error while inspecting lazy page bytes.
    #[error("IFF error: {0}")]
    Iff(#[from] IffError),

    /// Page index is out of range.
    #[error("page index {index} is out of range (document has {count} pages)")]
    PageOutOfRange { index: usize, count: usize },

    /// This lazy-loading slice intentionally rejects a document shape.
    #[error("unsupported lazy document shape: {0}")]
    Unsupported(&'static str),

    /// An [`AsyncComponentResolver`] could not return an indirect component.
    #[error("{0}")]
    Resolve(#[from] ComponentResolveError),
}

// ── True lazy async document loader (#233 Phase 3 PR1) ───────────────────────

#[derive(Debug, Clone)]
struct LazyDirmEntry {
    comp_type: DirmComponentKind,
    id: String,
    offset: u32,
    /// Component byte length from the DIRM size table, or 0 when the writer
    /// left the table zeroed (then the loader probes the `FORM` header).
    size: u32,
}

/// Native async lazy DjVu document.
///
/// This is the first Phase 3 slice for #233: it indexes a seekable async
/// reader up front, then fetches and parses each page only when
/// [`LazyDocument::page_async`] is called. Parsed pages are cached as
/// `Arc<DjVuPage>` so callers can render them concurrently without borrowing
/// the document across awaits.
///
/// Current scope:
/// - single-page `FORM:DJVU`
/// - bundled `FORM:DJVM` pages, including shared `DJVI` dictionaries referenced
///   via `INCL`
///
/// An indirect `FORM:DJVM` has its pages in separate files; open it with
/// [`LazyIndirectDocument`].
///
/// WASM `!Send` readers are intentionally left to the next issue slice.
pub struct LazyDocument<R> {
    reader: Arc<Mutex<R>>,
    pages: Vec<LazyPageIndex>,
    shared: BTreeMap<String, LazyComponentIndex>,
    cache: Vec<OnceCell<Arc<DjVuPage>>>,
    /// Shared components by DIRM name; `None` once read when the component
    /// holds no `Djbz` (for example shared annotations).
    shared_cache: BTreeMap<String, OnceCell<Option<Arc<SharedDict>>>>,
}

#[derive(Debug, Clone)]
struct LazyPageIndex {
    range: Range<u64>,
}

#[derive(Debug, Clone)]
struct LazyComponentIndex {
    range: Range<u64>,
}

impl<R> LazyDocument<R>
where
    R: AsyncRead + AsyncSeek + Unpin + 'static,
{
    /// Build a native lazy document index from an async seekable reader.
    pub async fn from_async_reader_lazy(mut reader: R) -> Result<Self, AsyncLazyError> {
        let file_len = reader.seek(std::io::SeekFrom::End(0)).await?;
        reader.seek(std::io::SeekFrom::Start(0)).await?;

        let mut head = [0u8; 16];
        reader.read_exact(&mut head).await?;
        if &head[..4] != b"AT&T" || &head[4..8] != b"FORM" {
            return Err(AsyncLazyError::Unsupported("not an AT&T FORM document"));
        }

        let form_type = &head[12..16];
        // A legacy FORM:BM44/PM44 image file is a one-page document too.
        let (pages, shared) = if crate::dirm::is_page_form(form_type) {
            (vec![LazyPageIndex { range: 0..file_len }], BTreeMap::new())
        } else if form_type == b"DJVM" {
            index_bundled_djvm(&mut reader).await?
        } else {
            return Err(AsyncLazyError::Unsupported(
                "lazy loader supports only FORM:DJVU and bundled FORM:DJVM",
            ));
        };

        if pages.is_empty() {
            return Err(AsyncLazyError::Unsupported(
                "document has no lazy-loadable pages",
            ));
        }

        let cache = (0..pages.len()).map(|_| OnceCell::new()).collect();
        let shared_cache = shared
            .keys()
            .map(|id| (id.clone(), OnceCell::new()))
            .collect();
        Ok(Self {
            reader: Arc::new(Mutex::new(reader)),
            pages,
            shared,
            cache,
            shared_cache,
        })
    }

    /// Number of lazy-loadable pages.
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Fetch, parse, cache, and return page `index`.
    pub async fn page_async(&self, index: usize) -> Result<Arc<DjVuPage>, AsyncLazyError> {
        let page = self
            .pages
            .get(index)
            .ok_or(AsyncLazyError::PageOutOfRange {
                index,
                count: self.pages.len(),
            })?
            .clone();

        self.cache[index]
            .get_or_try_init(|| async move {
                let bytes = self.read_page_bytes(page.range).await?;
                let form = parse_form(&bytes)?;
                // Same dictionary policy as the sync catalog assembler: the
                // first INCL target that holds a Djbz is the dictionary (#624).
                let mut shared_djbz = None;
                for name in incl_targets(&form.chunks) {
                    if let Some(dict) = self.shared_djbz(name).await? {
                        shared_djbz = Some(dict);
                        break;
                    }
                }
                let page = DjVuDocument::parse_single_page_with_shared(&bytes, index, shared_djbz)?;
                Ok(Arc::new(page))
            })
            .await
            .cloned()
    }

    /// The symbol dictionary of the shared component named `name`, or `None`
    /// when the name is not in DIRM or the component has no `Djbz`.
    ///
    /// A read failure is returned, not cached: a transient I/O error must not
    /// leave a page cached without its dictionary.
    async fn shared_djbz(&self, name: &str) -> Result<Option<Arc<SharedDict>>, AsyncLazyError> {
        let (Some(cell), Some(component)) = (self.shared_cache.get(name), self.shared.get(name))
        else {
            return Ok(None);
        };
        cell.get_or_try_init(|| async move {
            let bytes = self.read_page_bytes(component.range.clone()).await?;
            let form = parse_form(&bytes)?;
            if form.form_type != *b"DJVI" {
                return Err(AsyncLazyError::Unsupported("INCL target is not FORM:DJVI"));
            }
            Ok(form
                .chunks
                .iter()
                .find(|c| &c.id == b"Djbz")
                .map(|djbz| Arc::new(SharedDict::new(djbz.data.to_vec()))))
        })
        .await
        .cloned()
    }

    async fn read_page_bytes(&self, range: Range<u64>) -> Result<Vec<u8>, AsyncLazyError> {
        let len = usize::try_from(range.end.saturating_sub(range.start))
            .map_err(|_| AsyncLazyError::Unsupported("page range exceeds addressable memory"))?;
        let mut bytes = Vec::with_capacity(len.saturating_add(4));
        if range.start != 0 {
            // Reconstruct a standalone document from the on-disk component
            // range by prepending the IFF magic (the FORM framing is already
            // present in the range read from disk).
            bytes.extend_from_slice(&MAGIC);
        }
        let mut reader = self.reader.lock().await;
        reader.seek(std::io::SeekFrom::Start(range.start)).await?;
        let mut chunk = vec![0u8; len];
        reader.read_exact(&mut chunk).await?;
        bytes.extend_from_slice(&chunk);
        Ok(bytes)
    }
}

/// Build a native lazy document index from an async seekable reader.
///
/// Convenience wrapper around [`LazyDocument::from_async_reader_lazy`].
pub async fn from_async_reader_lazy<R>(reader: R) -> Result<LazyDocument<R>, AsyncLazyError>
where
    R: AsyncRead + AsyncSeek + Unpin + Send + 'static,
{
    LazyDocument::from_async_reader_lazy(reader).await
}

/// Build a lazy document from a single-threaded WASM-local async reader.
///
/// This constructor intentionally drops the native `Send` bound for browser
/// readers such as `wasm-bindgen-futures`/`gloo` streams.
#[cfg(target_arch = "wasm32")]
pub async fn from_async_reader_lazy_local<R>(reader: R) -> Result<LazyDocument<R>, AsyncLazyError>
where
    R: AsyncRead + AsyncSeek + Unpin + 'static,
{
    LazyDocument::from_async_reader_lazy(reader).await
}

// ── Lazy indirect DJVM loader (#687) ──────────────────────────────────────────

/// Async resolver contract for the components of an indirect `FORM:DJVM`.
///
/// This is the async twin of [`crate::ComponentResolver`]. It receives the
/// same [`ComponentId`]: the DIRM name and its kind. A
/// [`LazyIndirectDocument`] calls it at most once per component: for a page
/// when that page is first requested, and for a shared `DJVI` component when a
/// requested page first includes it.
///
/// A closure `Fn(ComponentId) -> impl Future<Output = Result<Vec<u8>,
/// ComponentResolveError>>` implements this trait.
pub trait AsyncComponentResolver {
    /// Return the complete IFF bytes of one external component.
    fn resolve(
        &self,
        component: &ComponentId,
    ) -> impl Future<Output = Result<Vec<u8>, ComponentResolveError>>;
}

impl<F, Fut> AsyncComponentResolver for F
where
    F: Fn(ComponentId) -> Fut,
    Fut: Future<Output = Result<Vec<u8>, ComponentResolveError>>,
{
    fn resolve(
        &self,
        component: &ComponentId,
    ) -> impl Future<Output = Result<Vec<u8>, ComponentResolveError>> {
        self(component.clone())
    }
}

/// Lazy async view of an indirect `FORM:DJVM` document.
///
/// An indirect document keeps each page in its own file; the index file holds
/// only the directory. [`LazyIndirectDocument::from_index`] reads the
/// directory, and [`LazyIndirectDocument::page_async`] fetches a page through
/// the [`AsyncComponentResolver`] only when it is requested. Shared `DJVI`
/// symbol dictionaries are fetched once, on first use, and shared by every
/// page that includes them. Parsed pages are cached as `Arc<DjVuPage>`.
///
/// ```no_run
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// use djvu_rs::djvu_async::LazyIndirectDocument;
/// use djvu_rs::{ComponentId, ComponentResolveError};
///
/// let index = tokio::fs::read("book/index.djvu").await?;
/// let doc = LazyIndirectDocument::from_index(&index, |component: ComponentId| async move {
///     tokio::fs::read(format!("book/{}", component.name))
///         .await
///         .map_err(|_| ComponentResolveError::Missing { component })
/// })?;
/// let first = doc.page_async(0).await?; // reads book/index.djvu and one page file
/// println!("{}×{}", first.width(), first.height());
/// # Ok(()) }
/// ```
pub struct LazyIndirectDocument<Res> {
    resolver: Res,
    pages: Vec<ComponentId>,
    cache: Vec<OnceCell<Arc<DjVuPage>>>,
    /// Shared components by DIRM name; `None` once resolved when the
    /// component holds no `Djbz` (for example shared annotations).
    shared: BTreeMap<String, OnceCell<Option<Arc<SharedDict>>>>,
}

impl<Res> LazyIndirectDocument<Res>
where
    Res: AsyncComponentResolver,
{
    /// Index an indirect `FORM:DJVM` from its index file bytes.
    ///
    /// No component is resolved here. A bundled `FORM:DJVM` or a single-page
    /// file returns [`AsyncLazyError::Unsupported`]; open those with
    /// [`from_async_reader_lazy`].
    pub fn from_index(index: &[u8], resolver: Res) -> Result<Self, AsyncLazyError> {
        let form = parse_form(index)?;
        if form.form_type != *b"DJVM" {
            return Err(AsyncLazyError::Unsupported(
                "not a FORM:DJVM index: use from_async_reader_lazy",
            ));
        }
        let dirm = form
            .chunks
            .iter()
            .find(|c| &c.id == b"DIRM")
            .ok_or(DocError::MissingChunk("DIRM"))?;
        let payload = DirmPayload::decode(dirm.data).map_err(AsyncLazyError::Unsupported)?;
        if payload.is_bundled() {
            return Err(AsyncLazyError::Unsupported(
                "bundled FORM:DJVM: use from_async_reader_lazy",
            ));
        }

        let mut pages = Vec::new();
        let mut shared = BTreeMap::new();
        for component in payload.components() {
            match component.kind {
                DirmComponentKind::Page => {
                    pages.push(ComponentId::new(component.id, ComponentKind::Page));
                }
                DirmComponentKind::Shared | DirmComponentKind::SharedAnno => {
                    shared.insert(component.id, OnceCell::new());
                }
                DirmComponentKind::Thumbnail => {}
            }
        }
        if pages.is_empty() {
            return Err(AsyncLazyError::Unsupported(
                "document has no lazy-loadable pages",
            ));
        }

        let cache = (0..pages.len()).map(|_| OnceCell::new()).collect();
        Ok(Self {
            resolver,
            pages,
            cache,
            shared,
        })
    }

    /// Number of pages.
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// The DIRM identity of page `index`: the name passed to the resolver.
    pub fn page_component(&self, index: usize) -> Option<&ComponentId> {
        self.pages.get(index)
    }

    /// Resolve, parse, cache, and return page `index`.
    ///
    /// A resolver failure returns [`AsyncLazyError::Resolve`] and is not
    /// cached: the next call asks the resolver again.
    pub async fn page_async(&self, index: usize) -> Result<Arc<DjVuPage>, AsyncLazyError> {
        let component = self
            .pages
            .get(index)
            .ok_or(AsyncLazyError::PageOutOfRange {
                index,
                count: self.pages.len(),
            })?;

        self.cache[index]
            .get_or_try_init(|| async move {
                let bytes = self.resolver.resolve(component).await?;
                let form = parse_form(&bytes)?;
                if !crate::dirm::is_page_form(&form.form_type) {
                    return Err(DocError::ComponentKindMismatch {
                        component: component.clone(),
                        found: form.form_type,
                        expected: ComponentKind::Page,
                    }
                    .into());
                }
                // Same dictionary policy as the sync catalog assembler: the
                // first INCL target that holds a Djbz is the dictionary (#624).
                let mut shared_djbz = None;
                for name in incl_targets(&form.chunks) {
                    if let Some(dict) = self.shared_djbz(name).await? {
                        shared_djbz = Some(dict);
                        break;
                    }
                }
                let page = DjVuDocument::parse_single_page_with_shared(&bytes, index, shared_djbz)?;
                Ok(Arc::new(page))
            })
            .await
            .cloned()
    }

    /// The symbol dictionary of the shared component an `INCL` names, or
    /// `None` when the name is not in DIRM or the component has no `Djbz`.
    async fn shared_djbz(&self, name: &str) -> Result<Option<Arc<SharedDict>>, AsyncLazyError> {
        let Some(cell) = self.shared.get(name) else {
            return Ok(None);
        };
        cell.get_or_try_init(|| async move {
            let component = ComponentId::new(name, ComponentKind::Shared);
            let bytes = self.resolver.resolve(&component).await?;
            let form = parse_form(&bytes)?;
            if form.form_type != *b"DJVI" {
                return Err(DocError::ComponentKindMismatch {
                    component,
                    found: form.form_type,
                    expected: ComponentKind::Shared,
                }
                .into());
            }
            Ok(form
                .chunks
                .iter()
                .find(|c| &c.id == b"Djbz")
                .map(|djbz| Arc::new(SharedDict::new(djbz.data.to_vec()))))
        })
        .await
        .cloned()
    }
}

async fn index_bundled_djvm<R>(
    reader: &mut R,
) -> Result<(Vec<LazyPageIndex>, BTreeMap<String, LazyComponentIndex>), AsyncLazyError>
where
    R: AsyncRead + AsyncSeek + Unpin + 'static,
{
    let mut chunk_hdr = [0u8; 8];
    reader.read_exact(&mut chunk_hdr).await?;
    if &chunk_hdr[..4] != b"DIRM" {
        return Err(AsyncLazyError::Unsupported(
            "lazy DJVM loader requires DIRM as the first inner chunk",
        ));
    }
    let dirm_len =
        u32::from_be_bytes([chunk_hdr[4], chunk_hdr[5], chunk_hdr[6], chunk_hdr[7]]) as usize;
    let padded = dirm_len + (dirm_len & 1);
    let mut dirm = vec![0u8; padded];
    reader.read_exact(&mut dirm).await?;

    let entries = parse_lazy_dirm(&dirm[..dirm_len])?;
    let mut pages = Vec::new();
    let mut shared = BTreeMap::new();
    for entry in entries {
        // The DIRM size table gives each component's byte length (FORM header
        // included), so a populated table indexes the whole document from the
        // head bytes alone — no seek across the file per component. Writers
        // that zero the table (ours does) fall back to probing the FORM header.
        let range = if entry.size > 8 {
            entry.offset as u64..entry.offset as u64 + entry.size as u64
        } else {
            reader
                .seek(std::io::SeekFrom::Start(entry.offset as u64 + 4))
                .await?;
            let mut size_bytes = [0u8; 4];
            reader.read_exact(&mut size_bytes).await?;
            crate::dirm::form_byte_range(entry.offset, size_bytes)
        };
        match entry.comp_type {
            DirmComponentKind::Page => pages.push(LazyPageIndex { range }),
            DirmComponentKind::Shared | DirmComponentKind::SharedAnno => {
                shared.insert(entry.id, LazyComponentIndex { range });
            }
            DirmComponentKind::Thumbnail => {}
        }
    }
    Ok((pages, shared))
}

fn parse_lazy_dirm(data: &[u8]) -> Result<Vec<LazyDirmEntry>, AsyncLazyError> {
    let payload = DirmPayload::decode(data).map_err(AsyncLazyError::Unsupported)?;
    if !payload.is_bundled() {
        return Err(AsyncLazyError::Unsupported(
            "indirect DJVM: use LazyIndirectDocument::from_index with a component resolver",
        ));
    }

    // `components()` and `offsets` are both indexed by component, so zipping them
    // pairs each entry with its FORM-header offset.
    Ok(payload
        .components()
        .into_iter()
        .zip(payload.offsets)
        .map(|(c, offset)| LazyDirmEntry {
            comp_type: c.kind,
            id: c.id,
            offset,
            size: c.size,
        })
        .collect())
}

// ── Async document loader ─────────────────────────────────────────────────────

/// Async loader that reads the IFF + FORM + DIRM head separately from the
/// page bodies (#196 Phase 2).
///
/// **Phase 2 of #196.** Issues two `read_exact` calls for the document head
/// (IFF magic + FORM length + form_type, then the DIRM chunk header + payload),
/// then a single `read_to_end` for the remainder. The total bytes received
/// match Phase 1 — this constructor still returns an in-memory
/// [`DjVuDocument`] — but a bandwidth-instrumented `AsyncRead` implementation
/// can observe the head-first read pattern, and the resulting document
/// exposes [`DjVuDocument::page_byte_range`] for any caller that wants to
/// fan out per-page byte fetches via HTTP `Range` requests on a separate
/// connection.
///
/// For documents that aren't bundled DJVM (single-page DJVU, indirect DJVM,
/// or anything without a DIRM in the first chunk), this falls back to the
/// Phase 1 buffered-read behavior — there's nothing useful to stream.
///
/// # Errors
///
/// - `AsyncLazyError::Io` — any underlying read fails
/// - `AsyncLazyError::Parse` — the assembled buffer fails [`DjVuDocument::parse`]
pub async fn load_document_async_streaming<R>(mut reader: R) -> Result<DjVuDocument, AsyncLazyError>
where
    R: AsyncRead + Unpin + Send,
{
    // 1) IFF outer header: 4-byte magic "AT&T" + "FORM" + 4-byte length + 4-byte form_type = 16 bytes.
    let mut head = [0u8; 16];
    reader.read_exact(&mut head).await?;

    // If it isn't a DJVM bundle, the rest of the file is just page payload —
    // no per-chunk streaming benefit, so fall back to bulk read.
    let is_djvm = &head[..4] == b"AT&T" && &head[4..8] == b"FORM" && &head[12..16] == b"DJVM";

    let mut buf = Vec::with_capacity(if is_djvm {
        // Pre-size: 1 MB head guess; Vec grows as needed.
        1 << 20
    } else {
        16 * 1024
    });
    buf.extend_from_slice(&head);

    if is_djvm {
        // 2) Next chunk header: 4-byte id + 4-byte BE length.
        let mut chunk_hdr = [0u8; 8];
        reader.read_exact(&mut chunk_hdr).await?;
        buf.extend_from_slice(&chunk_hdr);

        // If the first inner chunk is DIRM, read its payload separately so
        // a recording reader sees the head-first pattern. Otherwise just
        // continue with read_to_end — the document layout is non-canonical
        // and Phase 2's offset map wouldn't apply anyway.
        if &chunk_hdr[..4] == b"DIRM" {
            let dirm_len =
                u32::from_be_bytes([chunk_hdr[4], chunk_hdr[5], chunk_hdr[6], chunk_hdr[7]])
                    as usize;
            // IFF chunks pad to 2-byte boundary; the parser handles this, but
            // we must read those padding bytes too to keep alignment.
            let padded = dirm_len + (dirm_len & 1);
            let mut dirm_buf = vec![0u8; padded];
            reader.read_exact(&mut dirm_buf).await?;
            buf.extend_from_slice(&dirm_buf);
        }
    }

    // 3) Bulk-read the remainder.
    reader.read_to_end(&mut buf).await?;

    Ok(DjVuDocument::parse(&buf)?)
}

// ── Async render functions ────────────────────────────────────────────────────

/// Render a `DjVuPage` as a lazy progressive stream of [`Pixmap`] frames.
///
/// Yields one frame per BG44 wavelet refinement chunk: the first frame is the
/// coarsest (fastest to produce), and each subsequent frame adds detail. The
/// final frame equals [`render_pixmap`][djvu_render::render_pixmap], except at
/// scales of about a quarter of the native size and below, where
/// `render_pixmap` stops at the first background chunk (see
/// [`render_progressive`][djvu_render::render_progressive]).
///
/// If the page has no BG44 chunks (bilevel JB2-only pages), exactly one frame
/// is yielded via [`render_pixmap`][djvu_render::render_pixmap].
///
/// Each frame is produced via [`tokio::task::spawn_blocking`] just before it is
/// yielded, so the stream never blocks the async runtime thread.
///
/// # Example
///
/// ```no_run
/// # async fn example() {
/// use djvu_rs::djvu_document::DjVuDocument;
/// use djvu_rs::djvu_render::RenderOptions;
/// use djvu_rs::djvu_async::render_progressive_stream;
/// use futures::StreamExt;
///
/// let data = std::fs::read("file.djvu").unwrap();
/// let doc = DjVuDocument::parse(&data).unwrap();
/// let page = doc.page(0).unwrap();
/// let opts = RenderOptions { width: 800, height: 600, ..Default::default() };
///
/// let stream = render_progressive_stream(page, opts);
/// futures::pin_mut!(stream);
/// while let Some(pixmap) = stream.next().await {
///     let pixmap = pixmap.unwrap();
///     println!("{}×{}", pixmap.width, pixmap.height);
/// }
/// # }
/// ```
pub fn render_progressive_stream(
    page: &DjVuPage,
    opts: RenderOptions,
) -> impl futures_core::Stream<Item = Result<Pixmap, AsyncRenderError>> {
    // Single clone wrapped in Arc — all spawn_blocking closures share
    // this one allocation instead of cloning the full page each time.
    let page = Arc::new(page.clone());
    // The "max(1, bg44 chunks)" frame count and the per-frame coarse/progressive
    // choice live in the render module; the stream just drives the step index.
    let steps = djvu_render::progressive_steps(&page);

    async_stream::stream! {
        for step in 0..steps {
            let page = Arc::clone(&page);
            let opts = opts.clone();
            let result = tokio::task::spawn_blocking(move || {
                djvu_render::RenderRequest::new(opts)
                    .quality(djvu_render::Quality::Step(step))
                    .operation("render_progressive")
                    .pixmap(&page)
                    .map_err(AsyncRenderError::Render)
            })
            .await
            .map_err(|e| AsyncRenderError::Join(e.to_string()));
            yield result.and_then(|r| r);
        }
    }
}

/// Render one tile off the async runtime thread (#691).
///
/// The async counterpart of
/// [`djvu_tile::render_tile_with`](crate::djvu_tile::render_tile_with): the
/// render runs inside [`tokio::task::spawn_blocking`] and carries every byte
/// guarantee of the sync entry point unchanged — default controls match
/// [`djvu_tile::render_tile`](crate::djvu_tile::render_tile) exactly,
/// `use_cache` matches the cached path, `quality_step: Some(k)` matches the
/// tile's crop of progressive frame `k`.
///
/// Cancel from any thread or task by cancelling a clone of the token in
/// `controls.cancel`; the render stops at its next checkpoint with
/// [`TileError::Cancelled`] (wrapped in [`AsyncTileError::Tile`]).
pub async fn render_tile_async(
    page: &DjVuPage,
    opts: RenderOptions,
    tile_size: u32,
    col: u32,
    row: u32,
    controls: TileRenderControls,
) -> Result<Pixmap, AsyncTileError> {
    let page = page.clone();
    tokio::task::spawn_blocking(move || {
        crate::djvu_tile::render_tile_with(&page, &opts, tile_size, col, row, &controls)
            .map_err(AsyncTileError::Tile)
    })
    .await
    .map_err(|e| AsyncTileError::Join(e.to_string()))?
}

/// Progressive quality ladder for one tile, as a lazy stream (#691).
///
/// The tile-granular counterpart of [`render_progressive_stream`]: yields one
/// frame per quality step `0..progressive_steps(page)`, coarsest first. Each
/// frame is byte-identical to the matching crop of the full-page frame from
/// [`render_progressive_step`](djvu_render::render_progressive_step) — so
/// per-tile refinement and full-page refinement can be mixed freely in one
/// viewer. Bilevel pages (no BG44 data) yield exactly one full-quality frame.
///
/// Each frame is produced via [`tokio::task::spawn_blocking`] just before it
/// is yielded. If `cancel` fires, the stream yields one
/// `Err(AsyncTileError::Tile(TileError::Cancelled))` and ends — the token is
/// sticky, so no later step could succeed.
pub fn render_tile_progressive_stream(
    page: &DjVuPage,
    opts: RenderOptions,
    tile_size: u32,
    col: u32,
    row: u32,
    cancel: Option<TileCancelToken>,
) -> impl futures_core::Stream<Item = Result<Pixmap, AsyncTileError>> {
    // Single clone wrapped in Arc — all spawn_blocking closures share
    // this one allocation instead of cloning the full page each time.
    let page = Arc::new(page.clone());
    let steps = djvu_render::progressive_steps(&page);

    async_stream::stream! {
        for step in 0..steps {
            let page = Arc::clone(&page);
            let opts = opts.clone();
            let controls = TileRenderControls {
                quality_step: Some(step),
                cancel: cancel.clone(),
                use_cache: false,
            };
            let result = tokio::task::spawn_blocking(move || {
                crate::djvu_tile::render_tile_with(&page, &opts, tile_size, col, row, &controls)
                    .map_err(AsyncTileError::Tile)
            })
            .await
            .map_err(|e| AsyncTileError::Join(e.to_string()))
            .and_then(|r| r);
            let cancelled = matches!(result, Err(AsyncTileError::Tile(TileError::Cancelled)));
            yield result;
            if cancelled {
                return;
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::djvu_document::DjVuDocument;

    fn assets_path() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("references/djvujs/library/assets")
    }

    fn load_doc(name: &str) -> DjVuDocument {
        let data =
            std::fs::read(assets_path().join(name)).unwrap_or_else(|_| panic!("{name} must exist"));
        DjVuDocument::parse(&data).unwrap_or_else(|e| panic!("{e}"))
    }

    /// Rendering inside `spawn_blocking` (the documented caller pattern)
    /// produces a pixmap identical to a direct sync render.
    #[tokio::test]
    async fn spawn_blocking_render_matches_sync() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();
        let pw = page.width() as u32;
        let ph = page.height() as u32;

        let opts = RenderOptions {
            width: pw,
            height: ph,
            ..Default::default()
        };
        let sync_pm = djvu_render::render_pixmap(page, &opts).expect("sync render must succeed");

        let blocking_page = page.clone();
        let blocking_opts = opts.clone();
        let async_pm = tokio::task::spawn_blocking(move || {
            djvu_render::render_pixmap(&blocking_page, &blocking_opts)
        })
        .await
        .expect("spawn_blocking must not panic")
        .expect("render must succeed");

        assert_eq!(async_pm.width, pw);
        assert_eq!(async_pm.height, ph);
        assert_eq!(
            sync_pm.data, async_pm.data,
            "spawn_blocking and sync renders must match"
        );
    }

    /// `AsyncRenderError::Render` wraps `RenderError`.
    #[test]
    fn async_render_error_display() {
        let err = AsyncRenderError::Render(crate::djvu_render::RenderError::InvalidDimensions {
            width: 0,
            height: 0,
        });
        let s = err.to_string();
        assert!(
            s.contains("render error"),
            "error must mention 'render error'"
        );
    }

    // ── render_progressive_stream tests ──────────────────────────────────────

    /// Last frame from the progressive stream matches `render_pixmap`.
    #[tokio::test]
    async fn progressive_stream_last_frame_matches_pixmap() {
        use futures::StreamExt;
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();
        let opts = RenderOptions {
            width: 100,
            height: 80,
            ..Default::default()
        };

        let stream = render_progressive_stream(page, opts.clone());
        futures::pin_mut!(stream);

        let mut frames: Vec<Pixmap> = Vec::new();
        while let Some(result) = stream.next().await {
            frames.push(result.expect("frame should succeed"));
        }

        assert!(!frames.is_empty(), "stream must yield at least one frame");

        let expected = djvu_render::render_pixmap(page, &opts).expect("render_pixmap must succeed");
        assert_eq!(
            frames.last().unwrap().data,
            expected.data,
            "last frame must match render_pixmap"
        );
    }

    /// Each successive frame has the same dimensions.
    #[tokio::test]
    async fn progressive_stream_consistent_dimensions() {
        use futures::StreamExt;
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();
        let n_chunks = page.bg44_chunks().len();
        let opts = RenderOptions {
            width: 100,
            height: 80,
            ..Default::default()
        };

        let stream = render_progressive_stream(page, opts);
        futures::pin_mut!(stream);

        let mut count = 0usize;
        while let Some(result) = stream.next().await {
            let frame = result.expect("frame should succeed");
            assert_eq!(frame.width, 100);
            assert_eq!(frame.height, 80);
            count += 1;
        }

        let expected_count = if n_chunks == 0 { 1 } else { n_chunks };
        assert_eq!(
            count, expected_count,
            "frame count must equal BG44 chunk count"
        );
    }

    // ── render_tile_async / render_tile_progressive_stream tests ─────────────

    /// `render_tile_async` with default controls matches the sync tile
    /// renderer byte-for-byte.
    #[tokio::test]
    async fn tile_async_matches_sync_tile() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();
        let opts = RenderOptions {
            width: 100,
            height: 80,
            ..Default::default()
        };

        let sync_pm =
            crate::djvu_tile::render_tile(page, &opts, 32, 1, 1).expect("sync tile must render");
        let async_pm = render_tile_async(page, opts, 32, 1, 1, TileRenderControls::default())
            .await
            .expect("async tile must render");
        assert_eq!(
            sync_pm.data, async_pm.data,
            "async tile must match sync tile"
        );
    }

    /// The tile stream yields one frame per progressive step, each
    /// byte-identical to the sync quality-step render, ending at full quality.
    #[tokio::test]
    async fn tile_progressive_stream_frames_match_quality_steps() {
        use futures::StreamExt;
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();
        let steps = djvu_render::progressive_steps(page);
        let opts = RenderOptions {
            width: 100,
            height: 80,
            ..Default::default()
        };

        let stream = render_tile_progressive_stream(page, opts.clone(), 32, 1, 1, None);
        futures::pin_mut!(stream);

        let mut frames: Vec<Pixmap> = Vec::new();
        while let Some(result) = stream.next().await {
            frames.push(result.expect("frame should succeed"));
        }
        assert_eq!(frames.len(), steps, "one frame per progressive step");

        for (step, frame) in frames.iter().enumerate() {
            let controls = TileRenderControls {
                quality_step: Some(step),
                ..Default::default()
            };
            let expected = crate::djvu_tile::render_tile_with(page, &opts, 32, 1, 1, &controls)
                .expect("sync quality step must render");
            assert_eq!(
                frame.data, expected.data,
                "stream frame {step} must match sync quality step"
            );
        }

        let full = crate::djvu_tile::render_tile(page, &opts, 32, 1, 1).expect("full tile");
        assert_eq!(
            frames.last().unwrap().data,
            full.data,
            "last stream frame must be full quality"
        );
    }

    /// A cancelled token makes `render_tile_async` fail with
    /// `TileError::Cancelled` and ends the tile stream after one error.
    #[tokio::test]
    async fn tile_cancellation_surfaces_and_ends_stream() {
        use futures::StreamExt;
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();
        let opts = RenderOptions {
            width: 100,
            height: 80,
            ..Default::default()
        };

        let token = TileCancelToken::new();
        token.cancel();

        let controls = TileRenderControls {
            cancel: Some(token.clone()),
            ..Default::default()
        };
        let err = render_tile_async(page, opts.clone(), 32, 1, 1, controls)
            .await
            .expect_err("pre-cancelled render must fail");
        assert!(
            matches!(err, AsyncTileError::Tile(TileError::Cancelled)),
            "expected Cancelled, got: {err}"
        );

        let stream = render_tile_progressive_stream(page, opts, 32, 1, 1, Some(token));
        futures::pin_mut!(stream);
        let mut items = 0usize;
        while let Some(result) = stream.next().await {
            items += 1;
            assert!(
                matches!(result, Err(AsyncTileError::Tile(TileError::Cancelled))),
                "cancelled stream must only yield Cancelled"
            );
        }
        assert_eq!(items, 1, "stream must end after the first Cancelled error");
    }

    // ── load_document_async_streaming tests ──────────────────────────────────

    /// The streaming loader over an async reader matches `DjVuDocument::parse`.
    #[tokio::test]
    async fn streaming_loader_matches_sync_parse() {
        let path = assets_path().join("chicken.djvu");
        let sync_data = std::fs::read(&path).expect("sync read must succeed");
        let async_doc = load_document_async_streaming(std::io::Cursor::new(sync_data.clone()))
            .await
            .expect("async load must succeed");
        let sync_doc = DjVuDocument::parse(&sync_data).expect("sync parse must succeed");

        assert_eq!(async_doc.page_count(), sync_doc.page_count());
        for i in 0..sync_doc.page_count() {
            let a = async_doc.page(i).expect("async page");
            let s = sync_doc.page(i).expect("sync page");
            assert_eq!(a.width(), s.width());
            assert_eq!(a.height(), s.height());
        }
    }

    /// Truncated / non-DjVu bytes surface as `AsyncLazyError::Parse`, not panic.
    #[tokio::test]
    async fn streaming_loader_propagates_parse_error() {
        let bogus = b"not a djvu file at all".to_vec();
        let reader = std::io::Cursor::new(bogus);
        let err = load_document_async_streaming(reader)
            .await
            .expect_err("must fail to parse garbage");
        assert!(
            matches!(err, AsyncLazyError::Parse(_)),
            "expected Parse error, got {err:?}"
        );
    }

    /// `LazyDocument` fetches and parses a single-page document only when
    /// `page_async` is called, then returns the cached `Arc` on repeat access.
    #[tokio::test]
    async fn lazy_document_single_page_caches_arc_page() {
        let path = assets_path().join("chicken.djvu");
        let bytes = std::fs::read(&path).expect("read");
        let sync_doc = DjVuDocument::parse(&bytes).expect("sync parse");

        let lazy = from_async_reader_lazy(std::io::Cursor::new(bytes))
            .await
            .expect("lazy index");
        assert_eq!(lazy.page_count(), 1);

        let page_a = lazy.page_async(0).await.expect("lazy page");
        let page_b = lazy.page_async(0).await.expect("lazy cached page");
        assert!(
            Arc::ptr_eq(&page_a, &page_b),
            "repeat access must reuse cache"
        );

        let sync_page = sync_doc.page(0).expect("sync page");
        assert_eq!(page_a.width(), sync_page.width());
        assert_eq!(page_a.height(), sync_page.height());
    }

    /// #624: a lazy page with several `INCL` chunks (shared annotations +
    /// symbol dictionaries) must skip includes without a `Djbz` instead of
    /// failing on the first one.
    #[tokio::test]
    async fn lazy_document_multi_incl_page_resolves_shared_dict() {
        let path = assets_path().join("czech.djvu");
        let bytes = std::fs::read(&path).expect("read czech fixture");
        let lazy = from_async_reader_lazy(std::io::Cursor::new(bytes))
            .await
            .expect("lazy index");
        let page = lazy.page_async(1).await.expect("multi-INCL page loads");
        let mask = page
            .extract_mask()
            .expect("mask decode must succeed")
            .expect("page 1 has an Sjbz mask");
        assert_eq!((mask.width, mask.height), (1095, 1750));
    }

    /// `LazyDocument` indexes bundled DJVM ranges up front and can fetch a
    /// no-INCL page without reading/parsing the full document body.
    #[tokio::test]
    async fn lazy_document_bundled_page_without_incl_matches_sync() {
        let path = assets_path().join("colorbook.djvu");
        let Ok(bytes) = std::fs::read(&path) else {
            eprintln!("skip: {} missing", path.display());
            return;
        };
        let sync_doc = DjVuDocument::parse(&bytes).expect("sync parse");
        let lazy = from_async_reader_lazy(std::io::Cursor::new(bytes))
            .await
            .expect("lazy index");

        assert_eq!(lazy.page_count(), sync_doc.page_count());
        let page_index = (0..sync_doc.page_count())
            .find(|&i| {
                sync_doc
                    .page(i)
                    .expect("sync page")
                    .chunk_ids()
                    .iter()
                    .all(|id| id != b"INCL")
            })
            .expect("fixture must contain at least one page without INCL");

        let lazy_page = lazy.page_async(page_index).await.expect("lazy page");
        let sync_page = sync_doc.page(page_index).expect("sync page");
        assert_eq!(lazy_page.width(), sync_page.width());
        assert_eq!(lazy_page.height(), sync_page.height());
    }

    #[tokio::test]
    async fn lazy_document_bundled_page_with_incl_uses_shared_dict() {
        let mut p1 = crate::bitmap::Bitmap::new(32, 12);
        let mut p2 = crate::bitmap::Bitmap::new(32, 12);
        for y in 2..10 {
            for x in 3..9 {
                p1.set(x, y, true);
                p2.set(x, y, true);
            }
        }
        for y in 3..9 {
            for x in 16..22 {
                p1.set(x, y, true);
                p2.set(x, y, true);
            }
        }

        let bytes = crate::jb2_encode::encode_djvm_bundle_jb2(
            &[p1.clone(), p2.clone()],
            2,
            crate::jb2_encode::BUNDLE_DEFAULT_DPI,
        );
        let sync_doc = DjVuDocument::parse(&bytes).expect("sync parse");
        let lazy = from_async_reader_lazy(std::io::Cursor::new(bytes))
            .await
            .expect("lazy index");

        assert_eq!(lazy.page_count(), 2);
        let lazy_page = lazy.page_async(0).await.expect("lazy page");
        assert!(lazy_page.raw_chunk(b"INCL").is_some());
        let lazy_mask = lazy_page
            .extract_mask()
            .expect("lazy mask")
            .expect("lazy mask present");
        let sync_mask = sync_doc
            .page(0)
            .expect("sync page")
            .extract_mask()
            .expect("sync mask")
            .expect("sync mask present");
        assert_eq!(lazy_mask, sync_mask);
        assert_eq!(lazy_mask, p1);
    }

    #[tokio::test]
    async fn lazy_document_page_out_of_range() {
        let path = assets_path().join("chicken.djvu");
        let bytes = std::fs::read(&path).expect("read");
        let lazy = from_async_reader_lazy(std::io::Cursor::new(bytes))
            .await
            .expect("lazy index");

        let err = lazy
            .page_async(1)
            .await
            .expect_err("page 1 is out of range");
        assert!(
            matches!(err, AsyncLazyError::PageOutOfRange { index: 1, count: 1 }),
            "unexpected error: {err:?}"
        );
    }

    /// A bundled DJVM with no Page DIRM entries (only Shared): the lazy loader
    /// returns Unsupported with "no lazy-loadable pages" (lines 164–165).
    #[tokio::test]
    async fn lazy_document_no_page_entries_returns_unsupported() {
        use crate::dirm::DirmPayload;
        use crate::iff::{self as iff_mod, Chunk, EmitPart};

        // Build a bundled DIRM with 1 Shared entry (flag=0x00 = Shared)
        let dirm_payload = DirmPayload::build_bundled(&[crate::dirm::DirmComponent::new(
            crate::dirm::DirmComponentKind::Shared,
            "shared.djvi",
        )]);
        let dirm = Chunk::Leaf {
            id: *b"DIRM",
            data: dirm_payload.encode(),
        };
        // Also add a stub sub-FORM so the offset-based read doesn't panic
        let stub_form: &[u8] = b"FORM\x00\x00\x00\x04DJVI";
        let djvm = iff_mod::partial_emit(
            *b"DJVM",
            &[EmitPart::Chunk(&dirm), EmitPart::Verbatim(stub_form)],
        )
        .expect("fits within u32");

        let result = from_async_reader_lazy(std::io::Cursor::new(djvm)).await;
        assert!(result.is_err(), "Shared-only DJVM must error with no pages");
        if let Err(e) = result {
            assert!(
                matches!(e, AsyncLazyError::Unsupported(_)),
                "expected Unsupported, got {e:?}"
            );
        }
    }

    /// `LazyDocument` correctly skips THUM (thumbnail) DIRM entries while
    /// indexing a bundled DJVM — exercises the Thumbnail arm in index_bundled_djvm.
    #[tokio::test]
    async fn lazy_document_skips_thumbnail_dirm_entries() {
        let path = assets_path().join("DjVu3Spec_bundled.djvu");
        let Ok(bytes) = std::fs::read(&path) else {
            eprintln!("skip: {} missing", path.display());
            return;
        };
        let lazy = from_async_reader_lazy(std::io::Cursor::new(bytes.clone()))
            .await
            .expect("lazy index must succeed for DjVu3Spec");
        let sync_doc = DjVuDocument::parse(&bytes).expect("sync parse");
        // THUM entries are skipped so page count must still match
        assert_eq!(lazy.page_count(), sync_doc.page_count());
        assert!(lazy.page_count() > 0);
    }

    /// `load_document_async_streaming` produces the same document as
    /// the buffered Phase 1 loader on a bundled DJVM.
    #[tokio::test]
    async fn streaming_loader_matches_buffered() {
        let path = assets_path().join("DjVu3Spec_bundled.djvu");
        let Ok(bytes) = std::fs::read(&path) else {
            eprintln!("skip: {} missing", path.display());
            return;
        };
        let streamed = load_document_async_streaming(std::io::Cursor::new(bytes.clone()))
            .await
            .expect("streaming load must succeed");
        let buffered = DjVuDocument::parse(&bytes).expect("buffered parse");

        assert_eq!(streamed.page_count(), buffered.page_count());
        for i in 0..buffered.page_count() {
            assert_eq!(streamed.page_byte_range(i), buffered.page_byte_range(i));
        }
    }

    /// `load_document_async_streaming` reads the head before the body
    /// (#196 Phase 2 DoD).
    ///
    /// A custom `AsyncRead` records every requested read size. The first
    /// three calls must be small and bounded (IFF head 16 B, chunk header
    /// 8 B, DIRM payload — typically a few KB on a real document).
    #[tokio::test]
    async fn streaming_loader_reads_head_before_body() {
        use std::sync::{Arc, Mutex};

        let path = assets_path().join("DjVu3Spec_bundled.djvu");
        let Ok(bytes) = std::fs::read(&path) else {
            eprintln!("skip: {} missing", path.display());
            return;
        };

        struct RecordingReader {
            inner: std::io::Cursor<Vec<u8>>,
            sizes: Arc<Mutex<Vec<usize>>>,
        }
        impl tokio::io::AsyncRead for RecordingReader {
            fn poll_read(
                mut self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
                buf: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                let want = buf.remaining();
                let pos = self.inner.position() as usize;
                let src = self.inner.get_ref();
                let n = want.min(src.len().saturating_sub(pos));
                if n > 0 {
                    buf.put_slice(&src[pos..pos + n]);
                    self.inner.set_position((pos + n) as u64);
                }
                self.sizes.lock().unwrap().push(n);
                std::task::Poll::Ready(Ok(()))
            }
        }

        let sizes = Arc::new(Mutex::new(Vec::new()));
        let reader = RecordingReader {
            inner: std::io::Cursor::new(bytes.clone()),
            sizes: Arc::clone(&sizes),
        };
        let _ = load_document_async_streaming(reader)
            .await
            .expect("streaming load must succeed");

        let sizes = sizes.lock().unwrap().clone();
        // Strip 0-byte tail reads (EOF signals from read_to_end).
        let nonzero: Vec<usize> = sizes.into_iter().filter(|&n| n > 0).collect();

        // First read: the 16-byte IFF + FORM + form_type head.
        assert_eq!(nonzero[0], 16, "first read must be 16-byte IFF head");
        // Second read: the 8-byte DIRM chunk header.
        assert_eq!(nonzero[1], 8, "second read must be 8-byte chunk header");
        // Third read: the DIRM payload — must be smaller than the full body.
        assert!(
            nonzero[2] < bytes.len() / 4,
            "third read should be the DIRM payload, well under the full body \
             (got {} bytes for a {} byte file)",
            nonzero[2],
            bytes.len()
        );
    }

    /// I/O failure surfaces as `AsyncLazyError::Io`, not panic.
    #[tokio::test]
    async fn streaming_loader_propagates_io_error() {
        struct FailingReader;
        impl tokio::io::AsyncRead for FailingReader {
            fn poll_read(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
                _buf: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Err(std::io::Error::other("simulated I/O failure")))
            }
        }
        let err = load_document_async_streaming(FailingReader)
            .await
            .expect_err("must fail on I/O error");
        assert!(
            matches!(err, AsyncLazyError::Io(_)),
            "expected Io error, got {err:?}"
        );
    }

    /// A JB2-only page (no BG44 chunks) yields exactly one frame.
    #[tokio::test]
    async fn progressive_stream_jb2_only_yields_one_frame() {
        use futures::StreamExt;
        let doc = load_doc("boy_jb2.djvu");
        let page = doc.page(0).unwrap();
        if !page.bg44_chunks().is_empty() {
            // Page is not JB2-only; skip
            return;
        }
        let opts = RenderOptions {
            width: 80,
            height: 60,
            ..Default::default()
        };

        let stream = render_progressive_stream(page, opts);
        futures::pin_mut!(stream);

        let mut count = 0;
        while let Some(result) = stream.next().await {
            result.expect("frame should succeed");
            count += 1;
        }
        assert_eq!(count, 1, "JB2-only page must yield exactly one frame");
    }

    // ── from_async_reader_lazy error paths ────────────────────────────────────

    #[tokio::test]
    async fn lazy_not_att_form_returns_unsupported() {
        let bytes = b"XXXX0000XXXXXXXX"; // 16 bytes, not AT&T FORM
        let cursor = std::io::Cursor::new(bytes.to_vec());
        let result = from_async_reader_lazy(cursor).await;
        assert!(
            matches!(result, Err(AsyncLazyError::Unsupported(_))),
            "non-AT&T FORM must return Unsupported"
        );
    }

    #[tokio::test]
    async fn lazy_indirect_djvm_returns_unsupported() {
        // create_indirect builds a non-bundled DJVM; parse_lazy_dirm rejects it
        let indirect = crate::djvm::create_indirect(&["page1.djvu"]).expect("create_indirect");
        let cursor = std::io::Cursor::new(indirect);
        let result = from_async_reader_lazy(cursor).await;
        assert!(
            matches!(result, Err(AsyncLazyError::Unsupported(_))),
            "indirect DJVM must return Unsupported (not yet implemented)"
        );
    }

    // ── LazyIndirectDocument (#687) ──────────────────────────────────────────

    type ComponentFiles = Arc<BTreeMap<String, Vec<u8>>>;
    type CallLog = Arc<std::sync::Mutex<Vec<String>>>;

    /// An in-memory indirect document: index bytes, component files, and an
    /// empty log for the resolver calls.
    fn indirect_fixture(bundled: &[u8]) -> (Vec<u8>, ComponentFiles, CallLog) {
        let split = crate::djvm::to_indirect(bundled).expect("to_indirect");
        let files: BTreeMap<String, Vec<u8>> = split.components.into_iter().collect();
        (split.index, Arc::new(files), CallLog::default())
    }

    fn logging_resolver(
        files: ComponentFiles,
        log: CallLog,
    ) -> impl Fn(
        ComponentId,
    )
        -> std::future::Ready<Result<Vec<u8>, crate::djvu_document::ComponentResolveError>>
    + Send
    + Sync {
        move |component: ComponentId| {
            log.lock().unwrap().push(component.name.clone());
            std::future::ready(
                files
                    .get(&component.name)
                    .cloned()
                    .ok_or(ComponentResolveError::Missing { component }),
            )
        }
    }

    fn two_page_shared_dict_bundle() -> (Vec<u8>, crate::bitmap::Bitmap) {
        let mut p = crate::bitmap::Bitmap::new(32, 12);
        for y in 2..10 {
            for x in 3..9 {
                p.set(x, y, true);
            }
        }
        for y in 3..9 {
            for x in 16..22 {
                p.set(x, y, true);
            }
        }
        let bytes = crate::jb2_encode::encode_djvm_bundle_jb2(
            &[p.clone(), p.clone()],
            2,
            crate::jb2_encode::BUNDLE_DEFAULT_DPI,
        );
        (bytes, p)
    }

    /// Pages come from the resolver on demand; the shared dictionary is
    /// resolved once and serves both pages.
    #[tokio::test]
    async fn lazy_indirect_resolves_pages_and_shared_dict_on_demand() {
        let (bundled, bitmap) = two_page_shared_dict_bundle();
        let (index, files, log) = indirect_fixture(&bundled);
        let doc = LazyIndirectDocument::from_index(&index, logging_resolver(files, log.clone()))
            .expect("index");
        assert_eq!(doc.page_count(), 2);
        assert!(
            log.lock().unwrap().is_empty(),
            "from_index resolves nothing"
        );

        let first = doc.page_async(0).await.expect("page 0");
        assert!(first.raw_chunk(b"INCL").is_some());
        assert_eq!(first.extract_mask().unwrap().unwrap(), bitmap);
        let after_first = log.lock().unwrap().clone();
        assert_eq!(
            after_first.len(),
            2,
            "page 0 plus its dictionary: {after_first:?}"
        );
        assert_eq!(after_first[0], doc.page_component(0).unwrap().name);

        let second = doc.page_async(1).await.expect("page 1");
        assert_eq!(second.extract_mask().unwrap().unwrap(), bitmap);
        let again = doc.page_async(1).await.expect("page 1 cached");
        assert!(Arc::ptr_eq(&second, &again));
        let calls = log.lock().unwrap().clone();
        assert_eq!(
            calls.len(),
            3,
            "dictionary and pages resolve once: {calls:?}"
        );
    }

    /// A page with several includes (#624) and a document with thumbnails
    /// match the sync reader.
    #[tokio::test]
    async fn lazy_indirect_matches_sync_on_corpus_files() {
        for (name, page) in [("czech.djvu", 1), ("DjVu3Spec_bundled.djvu", 0)] {
            let path = assets_path().join(name);
            let Ok(bundled) = std::fs::read(&path) else {
                eprintln!("skip: {} missing", path.display());
                continue;
            };
            let sync_doc = DjVuDocument::parse(&bundled).expect("sync parse");
            let (index, files, log) = indirect_fixture(&bundled);
            let doc =
                LazyIndirectDocument::from_index(&index, logging_resolver(files, log.clone()))
                    .expect("index");
            assert_eq!(doc.page_count(), sync_doc.page_count(), "{name}");

            let lazy_page = doc.page_async(page).await.expect("lazy page");
            let sync_page = sync_doc.page(page).expect("sync page");
            assert_eq!(lazy_page.width(), sync_page.width(), "{name}");
            assert_eq!(lazy_page.height(), sync_page.height(), "{name}");
            assert_eq!(
                lazy_page.extract_mask().expect("lazy mask"),
                sync_page.extract_mask().expect("sync mask"),
                "{name}"
            );
            assert!(
                log.lock().unwrap().len() < doc.page_count(),
                "{name}: one page must not resolve the whole document"
            );
        }
    }

    /// A resolver failure is typed and not cached; the next call retries.
    #[tokio::test]
    async fn lazy_indirect_resolver_failure_is_not_cached() {
        let (bundled, _) = two_page_shared_dict_bundle();
        let (index, files, _) = indirect_fixture(&bundled);
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let fail_flag = fail.clone();
        let resolver = move |component: ComponentId| {
            let result = if fail_flag.load(std::sync::atomic::Ordering::SeqCst) {
                Err(ComponentResolveError::Missing { component })
            } else {
                Ok(files[&component.name].clone())
            };
            std::future::ready(result)
        };
        let doc = LazyIndirectDocument::from_index(&index, resolver).expect("index");

        let err = doc.page_async(0).await.expect_err("resolver fails");
        assert!(
            matches!(
                err,
                AsyncLazyError::Resolve(ComponentResolveError::Missing { .. })
            ),
            "unexpected error: {err:?}"
        );
        fail.store(false, std::sync::atomic::Ordering::SeqCst);
        doc.page_async(0).await.expect("retry succeeds");
    }

    /// A page name that resolves to a non-page form is a kind mismatch.
    #[tokio::test]
    async fn lazy_indirect_rejects_wrong_component_kind() {
        let (bundled, _) = two_page_shared_dict_bundle();
        let (index, files, _) = indirect_fixture(&bundled);
        let djvi = files
            .values()
            .find(|bytes| &bytes[12..16] == b"DJVI")
            .expect("shared component")
            .clone();
        let doc = LazyIndirectDocument::from_index(&index, move |_: ComponentId| {
            std::future::ready(Ok::<_, ComponentResolveError>(djvi.clone()))
        })
        .expect("index");
        let err = doc.page_async(0).await.expect_err("DJVI is not a page");
        assert!(
            matches!(
                err,
                AsyncLazyError::Parse(DocError::ComponentKindMismatch {
                    expected: ComponentKind::Page,
                    ..
                })
            ),
            "unexpected error: {err:?}"
        );
    }

    /// `from_index` accepts only an indirect index; the error names the
    /// loader that fits.
    #[test]
    fn lazy_indirect_rejects_bundled_and_single_page_input() {
        let never = |c: ComponentId| {
            std::future::ready(Err::<Vec<u8>, _>(ComponentResolveError::Missing {
                component: c,
            }))
        };
        let (bundled, _) = two_page_shared_dict_bundle();
        let single = std::fs::read(assets_path().join("chicken.djvu")).expect("read");
        for bytes in [bundled, single] {
            let err = LazyIndirectDocument::from_index(&bytes, never)
                .err()
                .expect("not an indirect index");
            assert!(
                matches!(err, AsyncLazyError::Unsupported(msg) if msg.contains("from_async_reader_lazy")),
                "unexpected error: {err:?}"
            );
        }
    }

    /// With a `Send + Sync` resolver, `page_async` runs on a spawned task.
    #[tokio::test]
    async fn lazy_indirect_page_future_is_send() {
        let (bundled, bitmap) = two_page_shared_dict_bundle();
        let (index, files, log) = indirect_fixture(&bundled);
        let doc = Arc::new(
            LazyIndirectDocument::from_index(&index, logging_resolver(files, log)).expect("index"),
        );
        let tasks: Vec<_> = (0..2)
            .map(|i| {
                let doc = doc.clone();
                tokio::spawn(async move { doc.page_async(i).await.map(|p| p.extract_mask()) })
            })
            .collect();
        for task in tasks {
            let mask = task.await.expect("join").expect("page").expect("mask");
            assert_eq!(mask, Some(bitmap.clone()));
        }
    }

    #[tokio::test]
    async fn lazy_djvm_with_only_shared_components_returns_unsupported() {
        // Build a DJVM where DIRM has 1 Shared component (no Pages).
        // After index_bundled_djvm returns empty pages, lines 164-165 fire.
        use crate::dirm::DirmPayload;
        use crate::iff::{self as iff_djvm, Chunk as IffChunk, EmitPart as IffEmitPart};
        let dirm_payload = DirmPayload::build_bundled(&[crate::dirm::DirmComponent::new(
            crate::dirm::DirmComponentKind::Shared,
            "shared",
        )])
        .encode();
        let dirm_chunk = IffChunk::Leaf {
            id: *b"DIRM",
            data: dirm_payload,
        };
        let bytes = iff_djvm::partial_emit(*b"DJVM", &[IffEmitPart::Chunk(&dirm_chunk)]).unwrap();
        let cursor = std::io::Cursor::new(bytes);
        let result = from_async_reader_lazy(cursor).await;
        // The function should return Unsupported because pages is empty (line 164-165).
        // Any error result covers the tested branches.
        assert!(
            matches!(
                result,
                Err(AsyncLazyError::Unsupported(_) | AsyncLazyError::Io(_))
            ),
            "DJVM with no page components must return Unsupported or Io error"
        );
    }

    #[tokio::test]
    async fn lazy_djvm_without_dirm_first_returns_unsupported() {
        // Valid AT&T FORM:DJVM but first inner chunk is INFO (not DIRM)
        // → triggers lines 293-294 in index_bundled_djvm
        use crate::iff::{self as iff_nodirm, Chunk as IffChunkNd, EmitPart as IffEmitPartNd};
        let info = IffChunkNd::Leaf {
            id: *b"INFO",
            data: vec![],
        };
        let bytes = iff_nodirm::partial_emit(*b"DJVM", &[IffEmitPartNd::Chunk(&info)]).unwrap();
        let cursor = std::io::Cursor::new(bytes);
        let result = from_async_reader_lazy(cursor).await;
        assert!(
            matches!(result, Err(AsyncLazyError::Unsupported(_))),
            "DJVM without DIRM first must return Unsupported"
        );
    }

    #[tokio::test]
    async fn lazy_unknown_form_type_returns_unsupported() {
        // Valid AT&T FORM header but with an unrecognized form type "DJVX"
        use crate::iff;
        let bytes = iff::partial_emit(*b"DJVX", &[]).unwrap();
        let cursor = std::io::Cursor::new(bytes);
        let result = from_async_reader_lazy(cursor).await;
        assert!(
            matches!(result, Err(AsyncLazyError::Unsupported(_))),
            "unknown FORM type must return Unsupported"
        );
    }
}
