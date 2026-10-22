//! The memory-mapped document.

use super::*;

// ---- Memory-mapped document -------------------------------------------------

/// A DjVu document backed by a memory-mapped file.
///
/// Instead of copying the entire file into a `Vec<u8>`, this type maps the file
/// into the process address space using the OS virtual-memory subsystem.  The
/// kernel pages data from disk on demand, which can significantly reduce peak
/// memory usage for large multi-volume scans (100+ MB).
///
/// # Safety contract
///
/// **The underlying file must not be modified or truncated while the mapping is
/// alive.**  Mutating a memory-mapped file is undefined behaviour on most
/// platforms (SIGBUS on Linux/macOS, access violation on Windows).  The caller
/// is responsible for ensuring file immutability for the lifetime of this
/// struct.
///
/// Requires the `mmap` feature flag.
#[cfg(feature = "mmap")]
pub struct MmapDocument {
    /// The memory mapping, wrapped in the shared [`Backing`] type and kept alive
    /// for the document's lifetime. For bundled documents the parsed pages are
    /// **lazy** and read their chunk bytes directly from this mapping on demand
    /// (zero-copy open); for single-page / indirect documents the pages own copies
    /// and this simply outlives the parse. Held via the same `Arc` the pages
    /// clone, so the mapping cannot be dropped while a lazy page still needs it.
    pub(super) _backing: Backing,
    /// The same mapping, kept as its concrete type (an extra `Arc` clone of
    /// the identical allocation `_backing` erases) so
    /// [`MmapDocument::advise_page_willneed`] can call `memmap2::Mmap::advise_range`
    /// directly — the type-erased `Backing` alias can't expose that method.
    // SageThumbs 2K vendoring: only the unix-only advise_page_willneed reads this field, so
    // a Windows build (clippy -D warnings) sees it as dead code.
    #[cfg_attr(
        not(unix),
        allow(dead_code, reason = "read only by the unix-only advise_page_willneed")
    )]
    pub(super) mmap: Arc<memmap2::Mmap>,
    pub(super) doc: DjVuDocument,
}

#[cfg(feature = "mmap")]
impl MmapDocument {
    /// Open a DjVu file via memory-mapped I/O.
    ///
    /// # Safety contract
    ///
    /// The file at `path` **must not be modified or truncated** while the
    /// returned `MmapDocument` is alive.  See the struct-level documentation
    /// for details.
    ///
    /// # Errors
    ///
    /// Returns `DocError::Io` if the file cannot be opened or mapped, or any
    /// parse error from [`DjVuDocument::parse`].
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, DocError> {
        let file = std::fs::File::open(path.as_ref())?;

        // SAFETY: The caller guarantees the file is not modified while mapped.
        // memmap2::Mmap provides a &[u8] view of the file contents.
        #[allow(unsafe_code)]
        let mmap = unsafe { memmap2::Mmap::map(&file) }?;

        // Move the mapping into the shared backing; bundled pages read from it
        // lazily (zero-copy open), and the `Arc` keeps it alive for them. Keep a
        // second, concretely-typed clone (same allocation, just another strong
        // ref) for `advise_page_willneed`.
        let mmap = Arc::new(mmap);
        let backing: Backing = mmap.clone();
        let doc = DjVuDocument::parse_backed_with_options(
            backing.clone(),
            &crate::resource_limits::ParseOptions::default(),
        )?;
        Ok(MmapDocument {
            _backing: backing,
            mmap,
            doc,
        })
    }

    /// Open a DjVu file with automatic filesystem resolution for indirect pages.
    ///
    /// For bundled documents this is identical to [`MmapDocument::open`].
    /// For indirect DJVM documents, component files named in the DIRM are
    /// resolved relative to the directory containing `path`.
    ///
    /// # Safety contract
    ///
    /// The file at `path` **must not be modified or truncated** while the
    /// returned `MmapDocument` is alive.
    pub fn open_indirect(path: impl AsRef<std::path::Path>) -> Result<Self, DocError> {
        let path = path.as_ref();
        let file = std::fs::File::open(path)?;
        #[allow(unsafe_code)]
        let mmap = unsafe { memmap2::Mmap::map(&file) }?;

        let base_dir = path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        // Indirect documents resolve external component files, so pages are eager
        // here; the mapping is still held via the shared backing for uniformity.
        let doc = DjVuDocument::parse_from_dir(&mmap, &base_dir)?;
        let mmap = Arc::new(mmap);
        let backing: Backing = mmap.clone();
        Ok(MmapDocument {
            _backing: backing,
            mmap,
            doc,
        })
    }

    /// Access the parsed [`DjVuDocument`].
    pub fn document(&self) -> &DjVuDocument {
        &self.doc
    }

    /// Number of pages in the document.
    pub fn page_count(&self) -> usize {
        self.doc.page_count()
    }

    /// Access a page by 0-based index.
    pub fn page(&self, index: usize) -> Result<&DjVuPage, DocError> {
        self.doc.page(index)
    }

    /// Hint to the OS that page `index`'s own bytes will be needed soon
    /// (`MADV_WILLNEED`). The cold-open (B6) lever: call this right after
    /// [`MmapDocument::open`], before the first render, so the kernel can
    /// start readahead I/O while the caller does anything else (parse
    /// metadata, build UI, etc.) instead of only starting on the page fault
    /// the render's first chunk read triggers.
    ///
    /// # Measured (COLD_OPEN, round 36)
    ///
    /// On a local NVMe SSD (M1) with `pathogenic_bacteria_1896.djvu` (517
    /// pages, 26 MB — small per-page FORM ranges, tens of KB), this hint is a
    /// wash: ±0–2%, inside measurement noise, whether issued for page 0 or a
    /// page deep in the file. Two things account for it: (1) most of a
    /// bundled document's structural cost (walking every page's IFF chunk
    /// headers to build `page_byte_range`) is already paid synchronously
    /// inside [`MmapDocument::open`], before this hint can be issued — there
    /// isn't much cold-read work left to schedule ahead by the time the
    /// caller gets a `MmapDocument` back; (2) a single page's FORM range is
    /// small enough that on fast local storage the demand-fault path is
    /// already close to the readahead path's latency. **An earlier version
    /// of this method advised the whole `0..range.end` prefix (covering the
    /// header/DIRM region too) and *reproducibly regressed cold open by
    /// ~12%*** (low dispersion, not noise) — advising far more than what's
    /// about to be read is actively harmful, not just wasted effort. Scoped
    /// to just `range` (this page's own bytes) it's harmless but unproven on
    /// this host; likely worth revisiting on higher-latency storage (network
    /// mounts, spinning disks) where a real win is more plausible. See
    /// `examples/cold_open_bench.rs --mode madvise`.
    ///
    /// Best-effort — a `madvise` failure (unsupported platform, unmapped
    /// range) is surfaced as `Err` but changes no state; correctness never
    /// depends on the hint landing. A `None` from
    /// [`DjVuDocument::page_byte_range`] (out-of-range index, indirect
    /// document, or an unmatched DIRM offset table) is treated as a no-op
    /// `Ok(())` rather than an error, since there is nothing wrong to report
    /// — there's just no known byte range to advise on.
    ///
    /// Only supported on Unix (the underlying `memmap2::Mmap::advise_range`
    /// is `#[cfg(unix)]`); a no-op stub is not provided for other platforms —
    /// gate calls with `#[cfg(unix)]` if you need to build for Windows too.
    #[cfg(unix)]
    pub fn advise_page_willneed(&self, index: usize) -> std::io::Result<()> {
        let Some(range) = self.doc.page_byte_range(index) else {
            return Ok(());
        };
        let start = (range.start as usize).min(self.mmap.len());
        let end = (range.end as usize).min(self.mmap.len());
        if end <= start {
            return Ok(());
        }
        self.mmap
            .advise_range(memmap2::Advice::WillNeed, start, end - start)
    }

    /// Consume this `MmapDocument`, returning the owned [`DjVuDocument`].
    ///
    /// Bundled documents' lazily-constructed pages (`ChunkStore::Lazy`)
    /// hold their own `Arc` clone of the memory mapping, so it stays mapped
    /// for as long as any page needs it — dropping this wrapper's own
    /// reference here is safe (indirect documents' pages are eager and don't
    /// reference the mapping at all after parsing). Useful to obtain an owned
    /// value to wrap in `Arc<DjVuDocument>`, which [`DjVuDocument::prefetch_page`]
    /// requires so a background task can share ownership of the same page
    /// caches the foreground render uses.
    pub fn into_document(self) -> DjVuDocument {
        self.doc
    }
}

#[cfg(feature = "mmap")]
impl core::ops::Deref for MmapDocument {
    type Target = DjVuDocument;
    fn deref(&self) -> &DjVuDocument {
        &self.doc
    }
}
