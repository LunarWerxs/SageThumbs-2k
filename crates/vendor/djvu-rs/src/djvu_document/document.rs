//! The parsed document: loaders, pages and document-level chunks.

use super::*;

// ---- Document ---------------------------------------------------------------

/// Options for [`DjVuDocument::enforce_cache_budget_with`].
///
/// Default (`downgrade_before_drop: false`) is byte-identical to
/// [`DjVuDocument::enforce_cache_budget`]'s all-or-nothing eviction.
#[cfg(feature = "std")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheBudgetOptions {
    /// C5_COMPRESS's cheaper middle tier: when true, a least-recently-used
    /// page over budget is first [`DjVuPage::downgrade_render_cache`]d —
    /// keeping its already-cached downscaled RGB pixmap
    /// (`bg_rgb_s2`/`bg_rgb_s4`) alive while dropping the expensive
    /// full-resolution derivations — rather than fully dropped. If the
    /// document is still over budget after downgrading every eligible page
    /// (or a page has nothing left to downgrade), the sweep falls back to a
    /// full drop, LRU-first, exactly like `enforce_cache_budget`. So the byte
    /// ceiling is honoured identically either way; only the *shape* of what
    /// stays cached changes.
    pub downgrade_before_drop: bool,
}

/// An opened DjVu document.
///
/// Supports single-page FORM:DJVU, bundled multi-page FORM:DJVM, and indirect
/// multi-page FORM:DJVM (via resolver callback).
#[derive(Debug)]
pub struct DjVuDocument {
    /// All pages, indexed by 0-based page number.
    pub(super) pages: Vec<DjVuPage>,
    /// Parsed NAVM bookmarks, or empty if none.
    pub(super) bookmarks: Vec<DjVuBookmark>,
    /// Raw document-level chunks (NAVM, DIRM, etc.) from the DJVM container,
    /// or from the top-level DJVU form for single-page documents.
    pub(super) global_chunks: Vec<RawChunk>,
    /// Byte ranges of each page's outer FORM chunk inside the original
    /// document buffer, in page order. Populated only for bundled DJVM
    /// documents parsed from a contiguous slice; empty otherwise (single-page
    /// DJVU, indirect DJVM, or when offsets were unavailable).
    ///
    /// Used by [`DjVuDocument::page_byte_range`] (#196 Phase 2). Lets a
    /// future HTTP-Range fetcher (#196 Phase 3) request exactly the bytes
    /// for a given page.
    pub(super) page_byte_ranges: Vec<core::ops::Range<u64>>,
    /// Configurable resource limits supplied at parse/open time.
    pub(super) resource_limits: Option<crate::resource_limits::ResourceLimits>,
    /// `ANTa`/`ANTz` chunks of the shared-annotation component (DIRM flag 3),
    /// or empty. DjVuLibre reads document metadata from here (#833).
    pub(super) shared_anno: Vec<RawChunk>,
}

#[cfg(feature = "std")]
pub(super) fn attach_resource_limits(
    mut document: DjVuDocument,
    limits: Option<crate::resource_limits::ResourceLimits>,
) -> DjVuDocument {
    document.resource_limits = limits;
    if let Some(limits) = limits {
        for page in &mut document.pages {
            page.resource_limits = Some(limits);
        }
    }
    document
}

#[cfg(feature = "std")]
pub(super) fn check_parse_limits(
    data: &[u8],
    limits: Option<crate::resource_limits::ResourceLimits>,
) -> Result<(), DocError> {
    if let Some(limits) = limits.filter(|limits| !limits.is_empty()) {
        let _ = crate::validate::check_document_limits(data, &limits, "document.parse")?;
    }
    Ok(())
}

impl DjVuDocument {
    /// Parse a DjVu document from a byte slice.
    ///
    /// For indirect documents (INCL references to external files), a resolver
    /// must be supplied via [`DjVuDocument::parse_with_resolver`].
    ///
    /// # Errors
    ///
    /// Returns `DocError::NoResolver` if the document is indirect and no resolver
    /// was provided.
    pub fn parse(data: &[u8]) -> Result<Self, DocError> {
        #[cfg(feature = "std")]
        {
            Self::parse_with_options(data, &crate::resource_limits::ParseOptions::default())
        }
        #[cfg(not(feature = "std"))]
        {
            Self::parse_with_resolver(data, None::<fn(&str) -> Result<Vec<u8>, DocError>>)
        }
    }

    /// Parse a DjVu document with configurable resource limits.
    ///
    /// When [`ParseOptions::limits`](crate::resource_limits::ParseOptions::limits) is set, header-only estimates are checked
    /// before the document is fully parsed. The same limits are stored on the
    /// returned document and inherited by subsequent render calls unless
    /// overridden via [`render_pixmap_with_limits`](crate::djvu_render::render_pixmap_with_limits).
    #[cfg(feature = "std")]
    pub fn parse_with_options(
        data: &[u8],
        opts: &crate::resource_limits::ParseOptions,
    ) -> Result<Self, DocError> {
        Self::parse_with_resolver_and_options(
            data,
            None::<fn(&str) -> Result<Vec<u8>, DocError>>,
            opts,
        )
    }

    /// Parse with an optional resolver and configurable resource limits.
    #[cfg(feature = "std")]
    pub fn parse_with_resolver_and_options<R>(
        data: &[u8],
        resolver: Option<R>,
        opts: &crate::resource_limits::ParseOptions,
    ) -> Result<Self, DocError>
    where
        R: Fn(&str) -> Result<Vec<u8>, DocError>,
    {
        check_parse_limits(data, opts.limits)?;
        let document = Self::parse_with_resolver(data, resolver)?;
        Ok(attach_resource_limits(document, opts.limits))
    }

    /// Configurable resource limits supplied at parse/open time, if any.
    pub fn resource_limits(&self) -> Option<crate::resource_limits::ResourceLimits> {
        self.resource_limits
    }

    /// Parse from an owned, shared backing store (an owned `Vec<u8>` or an
    /// `Mmap`), constructing **lazy** pages for bundled DJVM documents.
    ///
    /// For a bundled document only the cheap per-page `INFO` header is parsed up
    /// front; each page's chunk bytes are materialised from `backing` on first
    /// access instead of being copied at open time (LAZY_PAGE_CONSTRUCT). This
    /// makes "open a 500-page book, render page 1" O(1) in copies rather than
    /// O(total document bytes). For `mmap` backings the copy is avoided entirely
    /// until a page is touched.
    ///
    /// Single-page, non-DJVM, and indirect documents fall back to the eager
    /// [`parse`](Self::parse) path (they are small or need a resolver), so this
    /// is safe to call for any input. Pages come from the same catalog
    /// assembler as the eager path; only their chunk copy is deferred.
    #[cfg(feature = "std")]
    pub(crate) fn parse_backed_with_options(
        backing: Backing,
        opts: &crate::resource_limits::ParseOptions,
    ) -> Result<Self, DocError> {
        check_parse_limits(backing_bytes(&backing), opts.limits)?;
        let data = backing_bytes(&backing);
        let form = parse_form(data)?;
        if &form.form_type != b"DJVM" {
            return Self::parse_with_resolver_and_options(
                data,
                None::<fn(&str) -> Result<Vec<u8>, DocError>>,
                opts,
            );
        }
        let Some(dirm_chunk) = form.chunks.iter().find(|c| &c.id == b"DIRM") else {
            return Self::parse_with_resolver_and_options(
                data,
                None::<fn(&str) -> Result<Vec<u8>, DocError>>,
                opts,
            );
        };
        let payload = DirmPayload::decode(dirm_chunk.data).map_err(DocError::Malformed)?;
        if !payload.is_bundled() {
            // Indirect: needs a resolver — defer to the eager path (which errors
            // consistently with the previous behaviour).
            return Self::parse_with_resolver_and_options(
                data,
                None::<fn(&str) -> Result<Vec<u8>, DocError>>,
                opts,
            );
        }

        let entries = payload.components();
        let comp_offsets = &payload.offsets;
        let bookmarks = parse_navm_bookmarks(&form.chunks)?;
        let global_chunks: Vec<RawChunk> = form
            .chunks
            .iter()
            .filter(|c| &c.id != b"FORM")
            .map(|c| RawChunk {
                id: c.id,
                data: c.data.to_vec(),
            })
            .collect();

        let mut source = BundledSource {
            sub_forms: form.chunks.iter().filter(|c| &c.id == b"FORM").collect(),
            backing: Some(backing.clone()),
        };
        let assembly::Assembly { pages, shared_anno } = assembly::assemble(&entries, &mut source)?;
        let page_byte_ranges = bundled_page_byte_ranges(&entries, comp_offsets, data, pages.len());

        Ok(attach_resource_limits(
            DjVuDocument {
                pages,
                bookmarks,
                global_chunks,
                page_byte_ranges,
                resource_limits: None,
                shared_anno,
            },
            opts.limits,
        ))
    }

    /// Parse a DjVu document using the typed sync component resolver contract.
    ///
    /// For an indirect `FORM:DJVM`, the resolver is called once for every DIRM
    /// entry in declaration order. That includes `Page`, `Shared`, and
    /// `Thumbnail` components; shared `Djbz` dictionaries referenced by page
    /// `INCL` chunks are attached to the resulting pages just as they are for
    /// bundled documents. Single-page and bundled documents do not call the
    /// resolver.
    ///
    /// The older [`Self::parse_with_resolver`] API remains available for
    /// callers whose resolver is keyed only by a string page name.
    pub fn parse_with_component_resolver<R>(data: &[u8], resolver: &R) -> Result<Self, DocError>
    where
        R: ComponentResolver + ?Sized,
    {
        let form = parse_form(data)?;
        if form.form_type != *b"DJVM" {
            // Preserve the existing single-page and non-DjVu behavior. The
            // resolver is intentionally unused for a standalone FORM:DJVU.
            return Self::parse(data);
        }

        let dirm_chunk = form
            .chunks
            .iter()
            .find(|c| &c.id == b"DIRM")
            .ok_or(DocError::MissingChunk("DIRM"))?;
        let payload = DirmPayload::decode(dirm_chunk.data).map_err(DocError::Malformed)?;
        if payload.is_bundled() {
            // Bundled components are already in the index bytes and therefore
            // do not need an external resolver.
            return Self::parse(data);
        }

        let entries = payload.components();
        let bookmarks = parse_navm_bookmarks(&form.chunks)?;
        let global_chunks: Vec<RawChunk> = form
            .chunks
            .iter()
            .filter(|c| &c.id != b"FORM")
            .map(|c| RawChunk {
                id: c.id,
                data: c.data.to_vec(),
            })
            .collect();

        let assembly::Assembly { pages, shared_anno } =
            assembly::assemble(&entries, &mut TypedSource(resolver))?;

        Ok(DjVuDocument {
            pages,
            bookmarks,
            global_chunks,
            // Indirect component bytes live outside the index buffer.
            page_byte_ranges: Vec::new(),
            resource_limits: None,
            shared_anno,
        })
    }

    /// Parse a DjVu document with an optional resolver for indirect pages.
    ///
    /// The resolver receives the `name` field from each INCL chunk and must
    /// return the raw bytes of that external component file.
    pub fn parse_with_resolver<R>(data: &[u8], resolver: Option<R>) -> Result<Self, DocError>
    where
        R: Fn(&str) -> Result<Vec<u8>, DocError>,
    {
        let form = parse_form(data)?;

        match &form.form_type {
            b"DJVU" => {
                // Single-page document — expose all top-level chunks as global
                let global_chunks: Vec<RawChunk> = form
                    .chunks
                    .iter()
                    .map(|c| RawChunk {
                        id: c.id,
                        data: c.data.to_vec(),
                    })
                    .collect();
                let page = parse_page_from_chunks(&form.chunks, 0, None)?;
                // Single-page document spans the entire buffer.
                #[allow(clippy::single_range_in_vec_init)]
                let page_byte_ranges = vec![0u64..(data.len() as u64)];
                Ok(DjVuDocument {
                    pages: vec![page],
                    bookmarks: vec![],
                    global_chunks,
                    page_byte_ranges,
                    resource_limits: None,
                    shared_anno: Vec::new(),
                })
            }
            b"BM44" | b"PM44" => {
                let page = parse_legacy_iw44_page(&form.form_type, &form.chunks, 0)?;
                #[allow(clippy::single_range_in_vec_init)]
                let page_byte_ranges = vec![0u64..(data.len() as u64)];
                Ok(DjVuDocument {
                    pages: vec![page],
                    bookmarks: vec![],
                    global_chunks: Vec::new(),
                    page_byte_ranges,
                    resource_limits: None,
                    shared_anno: Vec::new(),
                })
            }
            b"DJVM" => {
                // Multi-page document — parse DIRM first
                let dirm_chunk = form
                    .chunks
                    .iter()
                    .find(|c| &c.id == b"DIRM")
                    .ok_or(DocError::MissingChunk("DIRM"))?;

                let payload = DirmPayload::decode(dirm_chunk.data).map_err(DocError::Malformed)?;
                let entries = payload.components();
                let is_bundled = payload.is_bundled();
                let comp_offsets = payload.offsets;

                // Collect NAVM bookmarks (BZZ-compressed)
                let bookmarks = parse_navm_bookmarks(&form.chunks)?;

                // Store non-FORM global chunks (DIRM, NAVM, etc.)
                let global_chunks: Vec<RawChunk> = form
                    .chunks
                    .iter()
                    .filter(|c| &c.id != b"FORM")
                    .map(|c| RawChunk {
                        id: c.id,
                        data: c.data.to_vec(),
                    })
                    .collect();

                if is_bundled {
                    // Bundled: FORM:DJVU / FORM:DJVI sub-forms follow DIRM in sequence.
                    let mut source = BundledSource {
                        sub_forms: form.chunks.iter().filter(|c| &c.id == b"FORM").collect(),
                        #[cfg(feature = "std")]
                        backing: None,
                    };
                    let assembly::Assembly { pages, shared_anno } =
                        assembly::assemble(&entries, &mut source)?;
                    let page_byte_ranges =
                        bundled_page_byte_ranges(&entries, &comp_offsets, data, pages.len());

                    Ok(DjVuDocument {
                        pages,
                        bookmarks,
                        global_chunks,
                        page_byte_ranges,
                        resource_limits: None,
                        shared_anno,
                    })
                } else {
                    // Indirect: components must be resolved by name.
                    let resolver = resolver.ok_or(DocError::NoResolver)?;
                    let assembly::Assembly { pages, shared_anno } =
                        assembly::assemble(&entries, &mut NamedSource(resolver))?;

                    Ok(DjVuDocument {
                        pages,
                        bookmarks,
                        global_chunks,
                        // Indirect: per-page bytes live in external files, not the
                        // index buffer — no meaningful range to expose here.
                        page_byte_ranges: Vec::new(),
                        resource_limits: None,
                        shared_anno,
                    })
                }
            }
            other => Err(DocError::NotDjVu(*other)),
        }
    }

    #[cfg(all(feature = "std", feature = "async"))]
    pub(crate) fn parse_single_page_with_shared(
        data: &[u8],
        index: usize,
        shared_djbz: Option<Arc<SharedDict>>,
    ) -> Result<DjVuPage, DocError> {
        let form = parse_form(data)?;
        if !crate::dirm::is_page_form(&form.form_type) {
            return Err(DocError::NotDjVu(form.form_type));
        }
        parse_component_page(&form.form_type, &form.chunks, index, shared_djbz)
    }

    /// Number of pages.
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Byte range of `page`'s outer FORM chunk inside the original document
    /// buffer (#196 Phase 2).
    ///
    /// Returns `Some(start..end)` where `start` is the absolute offset of the
    /// 4-byte `FORM` magic and `end` is one past the last byte of the chunk
    /// payload. The range is suitable for an HTTP `Range:` request that
    /// fetches exactly the bytes needed to decode that page (assuming any
    /// referenced shared `DJVI` dictionaries are already in hand — those
    /// have their own ranges too, but `page_byte_range` only covers pages).
    ///
    /// Returns `None` for:
    /// - `index >= page_count()`
    /// - Indirect DJVM documents (per-page bytes live in external files)
    /// - Bundled DJVM documents whose DIRM offset table couldn't be matched
    ///   to every page
    ///
    /// Single-page DJVU documents always return the full buffer range.
    pub fn page_byte_range(&self, index: usize) -> Option<core::ops::Range<u64>> {
        self.page_byte_ranges.get(index).cloned()
    }

    /// Speculatively decode page `index`'s render layers (background, mask,
    /// foreground) on a background thread pool, so that a **later**,
    /// synchronous [`crate::djvu_render::render_pixmap`] call at native
    /// resolution finds the caches already warm (the B7 next-page prefetch
    /// lever — e.g. call `doc.prefetch_page(k + 1)` right after rendering
    /// page `k`, while the reader is still looking at it).
    ///
    /// Requires an `Arc<DjVuDocument>` so the spawned task can outlive this
    /// call — the background closure holds its own clone of the `Arc` and
    /// writes into the *same* page's existing `OnceLock`-backed
    /// `PageLayers` cache, so there is no separate
    /// "prefetch buffer" to race against: whichever caller (the background
    /// task or a later foreground render) reaches `get_or_init` first does
    /// the decode, the other observes the cached result. Out-of-range
    /// `index` is a no-op. This is a hint, not a guarantee — if the
    /// background task hasn't finished by the time the page is rendered, the
    /// caller still gets correct output, just without the latency win.
    ///
    /// Requires the `parallel` feature (spawns onto the shared rayon pool).
    #[cfg(all(feature = "std", feature = "parallel"))]
    pub fn prefetch_page(self: &Arc<Self>, index: usize) {
        if index >= self.page_count() {
            return;
        }
        let doc = Arc::clone(self);
        rayon::spawn(move || {
            let Ok(page) = doc.page(index) else {
                return;
            };
            // Mirrors the common full-resolution render path's dependency
            // chain: mask/fg44 are independent of bg; bg_rgb_s1 subsumes the
            // bg44 ZP arithmetic decode (see `PageLayers::bg_rgb_s1`), so this
            // warms every cache slot a native-resolution `render_pixmap` call
            // reads from. (A page too large to hold its background whole
            // gets no RGB pixmap — #811 — but the bg44 slot is still warmed.)
            let _ = page.decoded_mask();
            let _ = page.decoded_fg44();
            let _ = page.decoded_bg_rgb_s1();
        });
    }

    /// Access a page by 0-based index.
    ///
    /// # Errors
    ///
    /// Returns `DocError::PageOutOfRange` if `index >= page_count()`.
    pub fn page(&self, index: usize) -> Result<&DjVuPage, DocError> {
        self.pages.get(index).ok_or(DocError::PageOutOfRange {
            index,
            count: self.pages.len(),
        })
    }

    /// Drop every page's render-tier decode cache, reclaiming all per-page
    /// render memory in one call.
    ///
    /// See [`DjVuPage::evict_render_cache`]: rendered pages memoise their decoded
    /// layers for the document's lifetime, so peak RSS grows linearly with pages
    /// rendered. This frees all of it (each page rebuilds lazily on next render).
    #[cfg(feature = "std")]
    pub fn evict_render_caches(&mut self) {
        for p in &mut self.pages {
            p.evict_render_cache();
        }
    }

    /// Drop the render cache of every page **except** those whose index is in
    /// `keep`, bounding memory to a working set (e.g. the visible pages plus a
    /// small prefetch window) in a long-lived viewer.
    #[cfg(feature = "std")]
    pub fn retain_render_caches(&self, keep: &[usize]) {
        for (i, p) in self.pages.iter().enumerate() {
            if !keep.contains(&i) {
                p.evict_render_cache();
            }
        }
    }

    /// Approximate total resident bytes held by all pages' render caches.
    ///
    /// Sum of [`DjVuPage::render_cache_bytes`]; use it to decide when to call
    /// [`enforce_cache_budget`](Self::enforce_cache_budget).
    #[cfg(feature = "std")]
    pub fn render_cache_bytes(&self) -> usize {
        self.pages.iter().map(|p| p.render_cache_bytes()).sum()
    }

    /// Evict least-recently-rendered pages' caches until the total render-cache
    /// memory is at most `max_bytes`, never evicting a page whose index is in
    /// `protect`. Returns the bytes freed.
    ///
    /// This is the automatic form of [`retain_render_caches`](Self::retain_render_caches):
    /// instead of naming exactly which pages to keep, the caller sets a memory
    /// ceiling and a small protected working set (e.g. the visible pages), and
    /// the least-recently-used cached pages are dropped first (via the per-page
    /// LRU access tick stamped on every render). A viewer can call it after each
    /// page render to hold memory near a fixed budget. No-op (returns 0) when
    /// already under budget. Evicted caches rebuild lazily and identically.
    #[cfg(feature = "std")]
    pub fn enforce_cache_budget(&self, max_bytes: usize, protect: &[usize]) -> usize {
        let mut total = self.render_cache_bytes();
        if total <= max_bytes {
            return 0;
        }
        // Evictable pages (cached, not protected), least-recently-used first.
        let mut cands: Vec<(usize, u64, usize)> = self
            .pages
            .iter()
            .enumerate()
            .filter(|(i, p)| !protect.contains(i) && p.render_cache_bytes() > 0)
            .map(|(i, p)| (i, p.render_cache_access_tick(), p.render_cache_bytes()))
            .collect();
        cands.sort_by_key(|&(_, tick, _)| tick);

        let mut freed = 0usize;
        for (i, _, bytes) in cands {
            if total <= max_bytes {
                break;
            }
            self.pages[i].evict_render_cache();
            freed += bytes;
            total = total.saturating_sub(bytes);
        }
        freed
    }

    /// C5_COMPRESS: like [`downgrade_render_caches`](Self::downgrade_render_caches)
    /// applied to every page — downgrade instead of drop.
    #[cfg(feature = "std")]
    pub fn downgrade_render_caches(&self) {
        for p in &self.pages {
            p.downgrade_render_cache();
        }
    }

    /// Like [`enforce_cache_budget`](Self::enforce_cache_budget), but taking
    /// [`CacheBudgetOptions`] to opt into the C5_COMPRESS downgrade-before-drop
    /// tier. Returns the bytes freed (net of any bytes still held by
    /// downgraded — not fully dropped — pages).
    #[cfg(feature = "std")]
    pub fn enforce_cache_budget_with(
        &self,
        max_bytes: usize,
        protect: &[usize],
        opts: CacheBudgetOptions,
    ) -> usize {
        if !opts.downgrade_before_drop {
            return self.enforce_cache_budget(max_bytes, protect);
        }
        let mut total = self.render_cache_bytes();
        if total <= max_bytes {
            return 0;
        }
        let starting_total = total;

        // Pass 1: downgrade LRU-first (cheap tier) until under budget or no
        // eligible candidates remain.
        let mut cands: Vec<(usize, u64, usize)> = self
            .pages
            .iter()
            .enumerate()
            .filter(|(i, p)| !protect.contains(i) && p.render_cache_bytes() > 0)
            .map(|(i, p)| (i, p.render_cache_access_tick(), p.render_cache_bytes()))
            .collect();
        cands.sort_by_key(|&(_, tick, _)| tick);

        for &(i, _, before) in &cands {
            if total <= max_bytes {
                break;
            }
            self.pages[i].downgrade_render_cache();
            let after = self.pages[i].render_cache_bytes();
            total = total.saturating_sub(before.saturating_sub(after));
        }

        // Pass 2: still over budget (downgrading wasn't enough, e.g. many
        // small pages or nothing left to shrink) — fall back to full drops,
        // LRU-first, same as `enforce_cache_budget`.
        if total > max_bytes {
            let mut cands2: Vec<(usize, u64, usize)> = self
                .pages
                .iter()
                .enumerate()
                .filter(|(i, p)| !protect.contains(i) && p.render_cache_bytes() > 0)
                .map(|(i, p)| (i, p.render_cache_access_tick(), p.render_cache_bytes()))
                .collect();
            cands2.sort_by_key(|&(_, tick, _)| tick);

            for (i, _, bytes) in cands2 {
                if total <= max_bytes {
                    break;
                }
                self.pages[i].evict_render_cache();
                total = total.saturating_sub(bytes);
            }
        }

        starting_total.saturating_sub(total)
    }

    /// The NAVM table of contents, or an empty slice if not present.
    pub fn bookmarks(&self) -> &[DjVuBookmark] {
        &self.bookmarks
    }

    /// Parse document-level metadata.
    ///
    /// Sources, in order:
    /// 1. a root `METz` (BZZ-compressed) or `METa` (plain) chunk, which
    ///    earlier djvu-rs versions wrote and DjVuLibre ignores;
    /// 2. the `(metadata …)` block of the shared-annotation component, where
    ///    DjVuLibre (`djvused set-meta`) and djvu-rs store it for a bundle
    ///    (#833);
    /// 3. for a single-page `FORM:DJVU`, the `(metadata …)` block of its own
    ///    annotation chunk, the only scope such a file has.
    ///
    /// Returns `Ok(None)` if no source carries metadata.
    pub fn metadata(&self) -> Result<Option<DjVuMetadata>, DocError> {
        if let Some(bytes) = self.chunk_payload(b"METz", b"METa")? {
            return Ok(Some(crate::metadata::parse_metadata(&bytes)?));
        }
        let find = |id: &[u8; 4]| {
            self.shared_anno
                .iter()
                .find(|c| &c.id == id)
                .map(|c| c.data.as_slice())
        };
        let bytes = match decode_paired_payload(find(b"ANTz"), find(b"ANTa"))? {
            Some(bytes) => Some(bytes),
            // Root-level ANTz/ANTa exist only in a single-page FORM:DJVU.
            None => self.chunk_payload(b"ANTz", b"ANTa")?,
        };
        let Some(bytes) = bytes else {
            return Ok(None);
        };
        let meta = crate::metadata::parse_metadata(&bytes)?;
        Ok((meta != DjVuMetadata::default()).then_some(meta))
    }

    /// Component directory from the document `DIRM` chunk.
    ///
    /// Returns an empty vector when no `DIRM` is present (typical single-page
    /// `FORM:DJVU`). Kind letters match DjVuLibre `djvused ls`: `P` page,
    /// `I` shared/include, `A` shared annotation, `T` thumbnail. Unlike
    /// `djvused ls`, every thumbnail entry is listed in DIRM order.
    pub fn component_directory(&self) -> Result<Vec<ComponentDirectoryEntry>, DocError> {
        let Some(data) = self.raw_chunk(b"DIRM") else {
            return Ok(Vec::new());
        };
        let payload = DirmPayload::decode(data).map_err(DocError::Malformed)?;
        Ok(payload
            .components()
            .into_iter()
            .map(|component| ComponentDirectoryEntry {
                kind: match component.kind {
                    DirmComponentKind::Page => 'P',
                    DirmComponentKind::Thumbnail => 'T',
                    DirmComponentKind::Shared => 'I',
                    DirmComponentKind::SharedAnno => 'A',
                },
                id: component.id,
            })
            .collect())
    }

    /// Return the raw bytes of the first document-level chunk with the given
    /// 4-byte ID.
    ///
    /// For single-page DJVU files this covers all top-level chunks (INFO,
    /// Sjbz, BG44, …).  For multi-page DJVM files this covers non-page chunks
    /// such as DIRM and NAVM.  Per-page chunks are accessed via
    /// [`DjVuPage::raw_chunk`].
    ///
    /// Returns `None` if no such chunk exists.
    pub fn raw_chunk(&self, id: &[u8; 4]) -> Option<&[u8]> {
        self.global_chunks
            .iter()
            .find(|c| &c.id == id)
            .map(|c| c.data.as_slice())
    }

    /// Return the raw bytes of all document-level chunks with the given ID.
    ///
    /// Returns an empty `Vec` if no such chunk exists.
    pub fn all_chunks(&self, id: &[u8; 4]) -> Vec<&[u8]> {
        self.global_chunks
            .iter()
            .filter(|c| &c.id == id)
            .map(|c| c.data.as_slice())
            .collect()
    }

    /// Return the IDs of all document-level chunks, in order.
    ///
    /// For multi-page DJVM files this is the sequence of non-page chunks
    /// (DIRM, NAVM, …).  Duplicate IDs appear once per chunk.
    pub fn chunk_ids(&self) -> Vec<[u8; 4]> {
        self.global_chunks.iter().map(|c| c.id).collect()
    }

    /// Decode the payload of a paired `*z` (BZZ-compressed) / `*a` (raw)
    /// document-level chunk, e.g. `chunk_payload(b"METz", b"METa")` for
    /// document metadata.
    ///
    /// The document-level counterpart of [`DjVuPage::chunk_payload`]; it owns
    /// the BZZ-or-raw decision once so the format parsers stay pure.
    pub fn chunk_payload(
        &self,
        id_z: &[u8; 4],
        id_a: &[u8; 4],
    ) -> Result<Option<Vec<u8>>, DocError> {
        Ok(decode_paired_payload(
            self.raw_chunk(id_z),
            self.raw_chunk(id_a),
        )?)
    }

    /// Parse an indirect DjVu document from bytes, resolving component files
    /// relative to `base_dir`.
    ///
    /// For bundled documents this is equivalent to [`DjVuDocument::parse`].
    /// For indirect documents, component names from the DIRM are resolved as
    /// paths under `base_dir`, and each referenced file is read from disk.
    ///
    /// # Errors
    ///
    /// Returns `DocError::Io` if a component file cannot be read, or any parse
    /// error from the component data.
    #[cfg(feature = "std")]
    pub fn parse_from_dir(
        data: &[u8],
        base_dir: impl AsRef<std::path::Path>,
    ) -> Result<Self, DocError> {
        Self::parse_from_dir_with_options(
            data,
            base_dir,
            &crate::resource_limits::ParseOptions::default(),
        )
    }

    /// Parse an indirect document from a directory with configurable resource limits.
    #[cfg(feature = "std")]
    pub fn parse_from_dir_with_options(
        data: &[u8],
        base_dir: impl AsRef<std::path::Path>,
        opts: &crate::resource_limits::ParseOptions,
    ) -> Result<Self, DocError> {
        let base = base_dir.as_ref().to_path_buf();
        let resolver = move |name: &str| -> Result<Vec<u8>, DocError> {
            // Strip any "file://" prefix
            let name = name.strip_prefix("file://").unwrap_or(name);
            let path = if std::path::Path::new(name).is_absolute() {
                std::path::PathBuf::from(name)
            } else {
                base.join(name)
            };
            std::fs::read(&path).map_err(|_| DocError::IndirectResolve(name.to_string()))
        };
        Self::parse_with_resolver_and_options(data, Some(resolver), opts)
    }
}
