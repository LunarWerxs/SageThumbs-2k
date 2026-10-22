//! Internal parsing helpers: component sources and page parsers.

use super::*;

// ---- Internal parsing helpers -----------------------------------------------

pub(super) fn component_id_from_dirm(component: &DirmComponent) -> ComponentId {
    let kind = match component.kind {
        DirmComponentKind::Page => ComponentKind::Page,
        DirmComponentKind::Shared | DirmComponentKind::SharedAnno => ComponentKind::Shared,
        DirmComponentKind::Thumbnail => ComponentKind::Thumbnail,
    };
    ComponentId::new(component.id.clone(), kind)
}

/// Whether a resolved component's form type fits its DIRM kind. A page may be
/// a legacy `FORM:BM44`/`FORM:PM44` image as well as `FORM:DJVU`.
pub(crate) fn form_fits_kind(form_type: &[u8; 4], kind: ComponentKind) -> bool {
    match kind {
        ComponentKind::Page => crate::dirm::is_page_form(form_type),
        ComponentKind::Shared => form_type == b"DJVI",
        ComponentKind::Thumbnail => form_type == b"THUM",
    }
}

/// Bundled components: `FORM` children of the index buffer, in DIRM order.
pub(super) struct BundledSource<'a> {
    pub(super) sub_forms: Vec<&'a IffChunk<'a>>,
    /// The buffer behind `sub_forms`, when pages should re-slice it lazily
    /// instead of copying their chunks now.
    #[cfg(feature = "std")]
    pub(super) backing: Option<Backing>,
}

impl<'a> assembly::ComponentSource<'a> for BundledSource<'a> {
    fn fetch(
        &mut self,
        index: usize,
        _entry: &DirmComponent,
    ) -> Result<Option<assembly::ComponentBytes<'a>>, DocError> {
        Ok(self
            .sub_forms
            .get(index)
            .map(|sf| assembly::ComponentBytes::Body(sf.data)))
    }

    #[cfg(feature = "std")]
    fn build_page(
        &mut self,
        page_index: usize,
        bytes: &assembly::ComponentBytes<'a>,
        form_type: &[u8; 4],
        chunks: &[IffChunk<'_>],
        shared: Option<PageSharedDict>,
    ) -> Result<DjVuPage, DocError> {
        // A legacy IW44 image page is small and has no INFO to parse lazily;
        // decode it eagerly like the non-backed path does.
        let lazy = !matches!(form_type, b"BM44" | b"PM44");
        match (&self.backing, bytes) {
            (Some(backing), assembly::ComponentBytes::Body(body)) if lazy => {
                // The page's FORM body is a slice of the backing bytes, so its
                // offset lets the lazy store re-slice and parse it on demand.
                let off = body.as_ptr() as usize - backing_bytes(backing).as_ptr() as usize;
                let range = off..off + body.len();
                parse_page_lazy(chunks, page_index, shared, backing.clone(), range)
            }
            _ => parse_component_page(form_type, chunks, page_index, shared),
        }
    }
}

/// Indirect components fetched by DIRM name through a
/// [`DjVuDocument::parse_with_resolver`] callback.
///
/// A page that cannot be resolved fails the open. A shared component that
/// cannot be resolved is skipped: metadata is optional, and a page that needs
/// a missing dictionary reports it when rendered.
pub(super) struct NamedSource<R>(pub(super) R);

impl<R> assembly::ComponentSource<'static> for NamedSource<R>
where
    R: Fn(&str) -> Result<Vec<u8>, DocError>,
{
    fn fetch(
        &mut self,
        _index: usize,
        entry: &DirmComponent,
    ) -> Result<Option<assembly::ComponentBytes<'static>>, DocError> {
        Ok(match entry.kind {
            DirmComponentKind::Page => {
                Some((self.0)(&entry.id).map_err(|_| DocError::IndirectResolve(entry.id.clone()))?)
            }
            DirmComponentKind::Shared | DirmComponentKind::SharedAnno => (self.0)(&entry.id).ok(),
            DirmComponentKind::Thumbnail => None,
        }
        .map(assembly::ComponentBytes::File))
    }
}

/// Indirect components fetched through a typed [`ComponentResolver`]: every
/// DIRM entry is resolved once, in order, and must have the form its kind
/// declares.
pub(super) struct TypedSource<'r, R: ?Sized>(pub(super) &'r R);

impl<R> assembly::ComponentSource<'static> for TypedSource<'_, R>
where
    R: ComponentResolver + ?Sized,
{
    fn fetch(
        &mut self,
        _index: usize,
        entry: &DirmComponent,
    ) -> Result<Option<assembly::ComponentBytes<'static>>, DocError> {
        let component = component_id_from_dirm(entry);
        let resolved = self
            .0
            .resolve(&component)
            .map_err(DocError::ComponentResolve)?;
        let found = parse_form(&resolved)?.form_type;
        if !form_fits_kind(&found, component.kind) {
            return Err(DocError::ComponentKindMismatch {
                expected: component.kind,
                component,
                found,
            });
        }
        Ok(Some(assembly::ComponentBytes::File(resolved)))
    }
}

/// Byte ranges of the bundled page `FORM`s, or none unless every page has one:
/// a partial table would surprise callers iterating by page index.
pub(super) fn bundled_page_byte_ranges(
    entries: &[DirmComponent],
    offsets: &[u32],
    data: &[u8],
    page_count: usize,
) -> Vec<core::ops::Range<u64>> {
    let ranges: Vec<_> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.kind == DirmComponentKind::Page)
        .filter_map(|(comp_idx, _)| {
            let off = *offsets.get(comp_idx)?;
            let start = off as usize;
            // `start` is an untrusted DIRM offset; `start + 8` overflows
            // `usize` on 32-bit targets for a crafted offset. Guard the slice.
            let size = data.get(start.checked_add(4)?..start.checked_add(8)?)?;
            Some(crate::dirm::form_byte_range(
                off,
                [size[0], size[1], size[2], size[3]],
            ))
        })
        .collect();
    if ranges.len() == page_count {
        ranges
    } else {
        Vec::new()
    }
}

/// Parse a `DjVuPage` from the chunks of a FORM:DJVU.
///
/// `shared_djbz` is the raw `Djbz` data from a referenced DJVI component
/// (resolved from the page's INCL chunk by the caller); pass `None` if no
/// shared dictionary is available.
/// Build a page whose chunk bytes are materialised lazily from `backing`.
///
/// Only the cheap fixed-size `INFO` header is parsed now; the per-chunk copy is
/// deferred to first [`DjVuPage::chunk_slice`] access. `range` is the page's
/// `FORM` sub-form byte range within `backing`.
#[cfg(feature = "std")]
pub(super) fn parse_page_lazy(
    chunks: &[IffChunk<'_>],
    index: usize,
    shared_djbz: Option<Arc<SharedDict>>,
    backing: Backing,
    range: core::ops::Range<usize>,
) -> Result<DjVuPage, DocError> {
    let info_chunk = chunks
        .iter()
        .find(|c| &c.id == b"INFO")
        .ok_or(DocError::MissingChunk("INFO"))?;
    let info = PageInfo::parse(info_chunk.data)?;
    Ok(DjVuPage {
        info,
        chunks: ChunkStore::Lazy {
            backing,
            range,
            cache: std::sync::OnceLock::new(),
        },
        index,
        shared_djbz,
        render_layers: std::sync::OnceLock::new(),
        resource_limits: None,
    })
}

#[cfg(feature = "std")]
pub(super) fn parse_page_from_chunks(
    chunks: &[IffChunk<'_>],
    index: usize,
    shared_djbz: Option<Arc<SharedDict>>,
) -> Result<DjVuPage, DocError> {
    let info_chunk = chunks
        .iter()
        .find(|c| &c.id == b"INFO")
        .ok_or(DocError::MissingChunk("INFO"))?;

    let info = PageInfo::parse(info_chunk.data)?;

    // Copy all chunks to owned storage for lazy decode later.
    let raw_chunks: Vec<RawChunk> = chunks
        .iter()
        .map(|c| RawChunk {
            id: c.id,
            data: c.data.to_vec(),
        })
        .collect();

    Ok(DjVuPage {
        info,
        chunks: ChunkStore::Eager(raw_chunks),
        index,
        shared_djbz,
        render_layers: std::sync::OnceLock::new(),
        resource_limits: None,
    })
}

/// Build [`PageInfo`] from the first IW44 chunk header of a legacy BM44/PM44
/// document (no INFO chunk). DjVuLibre reports 100 dpi for these photo forms.
pub(super) fn page_info_from_iw44_first_chunk(
    form_type: &[u8; 4],
    payload: &[u8],
) -> Result<PageInfo, DocError> {
    if payload.len() < 9 {
        return Err(DocError::Malformed(
            "legacy IW44 first chunk header truncated",
        ));
    }
    let serial = payload[0];
    if serial != 0 {
        return Err(DocError::Malformed(
            "legacy IW44 first chunk must have serial 0",
        ));
    }
    let majver = payload[2];
    let is_grayscale = (majver >> 7) != 0;
    match (form_type, is_grayscale) {
        (b"BM44", true) | (b"PM44", false) => {}
        (b"BM44", false) => {
            return Err(DocError::Malformed(
                "FORM:BM44 requires a grayscale IW44 bitstream",
            ));
        }
        (b"PM44", true) => {
            return Err(DocError::Malformed(
                "FORM:PM44 requires a color IW44 bitstream",
            ));
        }
        _ => {
            return Err(DocError::Malformed("unexpected legacy IW44 form type"));
        }
    }
    let width = u16::from_be_bytes([payload[4], payload[5]]);
    let height = u16::from_be_bytes([payload[6], payload[7]]);
    if width == 0 || height == 0 {
        return Err(DocError::Malformed("legacy IW44 zero dimension"));
    }
    let pixels = u64::from(width) * u64::from(height);
    if pixels > 64 * 1024 * 1024 {
        return Err(DocError::Malformed("legacy IW44 image too large"));
    }
    Ok(PageInfo {
        width,
        height,
        dpi: 100,
        gamma: 2.2,
        rotation: crate::info::Rotation::None,
    })
}

/// Parse a legacy standalone `FORM:BM44` or `FORM:PM44` page.
#[cfg(feature = "std")]
pub(super) type PageSharedDict = Arc<SharedDict>;
#[cfg(not(feature = "std"))]
pub(super) type PageSharedDict = Vec<u8>;

/// Parse one page component of a multi-page document.
///
/// A page component is usually `FORM:DJVU`, but DjVuLibre also bundles a
/// legacy `FORM:BM44`/`FORM:PM44` image file as a page (`djvm -c`), and
/// djvu-rs merge does the same.
pub(super) fn parse_component_page(
    form_type: &[u8],
    chunks: &[IffChunk<'_>],
    index: usize,
    shared_djbz: Option<PageSharedDict>,
) -> Result<DjVuPage, DocError> {
    match form_type {
        b"BM44" => parse_legacy_iw44_page(b"BM44", chunks, index),
        b"PM44" => parse_legacy_iw44_page(b"PM44", chunks, index),
        _ => parse_page_from_chunks(chunks, index, shared_djbz),
    }
}

pub(super) fn parse_legacy_iw44_page(
    form_type: &[u8; 4],
    chunks: &[IffChunk<'_>],
    index: usize,
) -> Result<DjVuPage, DocError> {
    let expected_id = match form_type {
        b"BM44" => *b"BM44",
        b"PM44" => *b"PM44",
        _ => {
            return Err(DocError::Malformed(
                "parse_legacy_iw44_page requires BM44 or PM44",
            ));
        }
    };
    if chunks.is_empty() {
        return Err(DocError::MissingChunk(match form_type {
            b"BM44" => "BM44",
            _ => "PM44",
        }));
    }
    for chunk in chunks {
        if chunk.id != expected_id {
            return Err(DocError::Malformed(
                "legacy IW44 form contains unexpected chunk id",
            ));
        }
    }
    let info = page_info_from_iw44_first_chunk(form_type, chunks[0].data)?;
    let raw_chunks: Vec<RawChunk> = chunks
        .iter()
        .map(|c| RawChunk {
            id: c.id,
            data: c.data.to_vec(),
        })
        .collect();
    #[cfg(feature = "std")]
    {
        Ok(DjVuPage {
            info,
            chunks: ChunkStore::Eager(raw_chunks),
            index,
            shared_djbz: None,
            render_layers: std::sync::OnceLock::new(),
            resource_limits: None,
        })
    }
    #[cfg(not(feature = "std"))]
    {
        Ok(DjVuPage {
            info,
            chunks: raw_chunks,
            index,
            shared_djbz: None,
            resource_limits: None,
        })
    }
}

#[cfg(not(feature = "std"))]
pub(super) fn parse_page_from_chunks(
    chunks: &[IffChunk<'_>],
    index: usize,
    shared_djbz: Option<Vec<u8>>,
) -> Result<DjVuPage, DocError> {
    let info_chunk = chunks
        .iter()
        .find(|c| &c.id == b"INFO")
        .ok_or(DocError::MissingChunk("INFO"))?;

    let info = PageInfo::parse(info_chunk.data)?;

    let raw_chunks: Vec<RawChunk> = chunks
        .iter()
        .map(|c| RawChunk {
            id: c.id,
            data: c.data.to_vec(),
        })
        .collect();

    Ok(DjVuPage {
        info,
        chunks: raw_chunks,
        index,
        shared_djbz,
        resource_limits: None,
    })
}

/// Parse sub-form chunks from the data portion of a FORM chunk.
///
/// The `data` bytes start with a 4-byte form type (e.g. `DJVU`), followed by
/// sequential IFF chunks.
/// Copy the `ANTa`/`ANTz` chunks of a shared-annotation component.
pub(super) fn annotation_chunks(chunks: &[IffChunk<'_>]) -> Vec<RawChunk> {
    chunks
        .iter()
        .filter(|c| &c.id == b"ANTa" || &c.id == b"ANTz")
        .map(|c| RawChunk {
            id: c.id,
            data: c.data.to_vec(),
        })
        .collect()
}

pub(super) fn parse_sub_form(data: &[u8]) -> Result<Vec<IffChunk<'_>>, DocError> {
    if data.len() < 4 {
        return Err(DocError::Malformed("sub-form data too short"));
    }
    // data[0..4] = form type (DJVU / DJVI / THUM …)
    // data[4..] = sequential chunks
    let body = data
        .get(4..)
        .ok_or(DocError::Malformed("sub-form body missing"))?;
    let chunks = parse_form_body(body).map_err(DocError::Iff)?;
    Ok(chunks)
}
