//! Page assembly for a multi-page `FORM:DJVM` catalog.
//!
//! Every sync loader turns the DIRM catalog into pages the same way: walk the
//! components in order, index each shared component's `Djbz` symbol
//! dictionary and the document's shared annotation, then build each page and
//! attach the dictionary its `INCL` chunks name. Only *where the component
//! bytes come from* differs, so that is the seam: a [`ComponentSource`] per
//! loader (bundled bytes, a name resolver, a typed resolver).
//!
//! Dictionary policy, the same for every source:
//!
//! - A page may `INCL` several components (shared annotations *and* the symbol
//!   dictionary — czech.djvu carries three, #624). The first target that holds
//!   a `Djbz` is the page's dictionary.
//! - An `INCL` name that is not UTF-8 or names no shared component is ignored.
//! - A shared component the source cannot supply is skipped; the page opens
//!   and a render that needs the dictionary fails with `MissingSharedDict`.

#[cfg(not(feature = "std"))]
use alloc::{collections::BTreeMap, string::String, vec::Vec};
#[cfg(feature = "std")]
use std::collections::BTreeMap;

use super::{
    DjVuPage, DocError, PageSharedDict, RawChunk, annotation_chunks, parse_component_page,
    parse_sub_form,
};
use crate::{
    dirm::{DirmComponent, DirmComponentKind, incl_target},
    iff::{IffChunk, parse_form},
};

/// Bytes of one DIRM component, as a source supplies them.
pub(super) enum ComponentBytes<'a> {
    /// A `FORM` body borrowed from a bundled document: the 4-byte form type
    /// followed by the chunks.
    Body(&'a [u8]),
    /// A standalone component file (`AT&T` magic optional).
    File(Vec<u8>),
}

impl ComponentBytes<'_> {
    fn parse(&self) -> Result<([u8; 4], Vec<IffChunk<'_>>), DocError> {
        match self {
            Self::Body(body) => {
                let form_type = body
                    .get(..4)
                    .and_then(|t| <[u8; 4]>::try_from(t).ok())
                    .ok_or(DocError::Malformed("sub-form data too short"))?;
                Ok((form_type, parse_sub_form(body)?))
            }
            Self::File(bytes) => {
                let form = parse_form(bytes)?;
                Ok((form.form_type, form.chunks))
            }
        }
    }
}

/// Where the component bytes of a DJVM catalog come from.
pub(super) trait ComponentSource<'a> {
    /// Supply DIRM component `index`, or `None` to skip it.
    ///
    /// The assembler asks for every component once, in DIRM order. A page
    /// must be supplied or fail; `None` for a page is a malformed document.
    fn fetch(
        &mut self,
        index: usize,
        entry: &DirmComponent,
    ) -> Result<Option<ComponentBytes<'a>>, DocError>;

    /// Build page `page_index` from its parsed component.
    ///
    /// The default parses the chunks eagerly; the backed bundled source
    /// overrides it to defer the chunk copy.
    fn build_page(
        &mut self,
        page_index: usize,
        bytes: &ComponentBytes<'a>,
        form_type: &[u8; 4],
        chunks: &[IffChunk<'_>],
        shared: Option<PageSharedDict>,
    ) -> Result<DjVuPage, DocError> {
        let _ = bytes;
        parse_component_page(form_type, chunks, page_index, shared)
    }
}

/// Pages and document-level shared data of an assembled catalog.
pub(super) struct Assembly {
    pub pages: Vec<DjVuPage>,
    pub shared_anno: Vec<RawChunk>,
}

/// Build the pages of a DJVM catalog from `source`.
pub(super) fn assemble<'a, S>(
    entries: &[DirmComponent],
    source: &mut S,
) -> Result<Assembly, DocError>
where
    S: ComponentSource<'a>,
{
    // DIRM position of each shared component, to tell whether a page's INCL
    // target is still ahead in the catalog.
    let include_pos: BTreeMap<&str, usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.kind.is_include())
        .map(|(i, e)| (e.id.as_str(), i))
        .collect();

    let mut dicts: BTreeMap<String, PageSharedDict> = BTreeMap::new();
    let mut shared_anno = Vec::new();
    let mut pages: Vec<Option<DjVuPage>> = Vec::new();
    // Pages whose INCL names a shared component later in DIRM wait until the
    // whole catalog is indexed. Writers put shared components first, so this
    // is empty for real documents and pages are built as they stream by.
    let mut deferred: Vec<(usize, ComponentBytes<'a>)> = Vec::new();

    for (index, entry) in entries.iter().enumerate() {
        let fetched = source.fetch(index, entry)?;
        match entry.kind {
            DirmComponentKind::Page => {
                let bytes = fetched.ok_or(DocError::Malformed(
                    "DIRM entry count exceeds FORM children",
                ))?;
                let page_index = pages.len();
                let waits = {
                    let (_, chunks) = bytes.parse()?;
                    incl_targets(&chunks)
                        .any(|name| include_pos.get(name).is_some_and(|&pos| pos > index))
                };
                if waits {
                    pages.push(None);
                    deferred.push((page_index, bytes));
                } else {
                    let page = build(source, page_index, &bytes, &dicts)?;
                    pages.push(Some(page));
                }
            }
            DirmComponentKind::Shared | DirmComponentKind::SharedAnno => {
                let Some(bytes) = fetched else { continue };
                let Ok((_, chunks)) = bytes.parse() else {
                    continue;
                };
                if entry.kind == DirmComponentKind::SharedAnno {
                    shared_anno = annotation_chunks(&chunks);
                }
                if let Some(djbz) = chunks.iter().find(|c| &c.id == b"Djbz") {
                    dicts.insert(entry.id.clone(), new_dict(djbz.data));
                }
            }
            DirmComponentKind::Thumbnail => {}
        }
    }

    for (page_index, bytes) in deferred {
        pages[page_index] = Some(build(source, page_index, &bytes, &dicts)?);
    }

    Ok(Assembly {
        pages: pages.into_iter().flatten().collect(),
        shared_anno,
    })
}

fn build<'a, S>(
    source: &mut S,
    page_index: usize,
    bytes: &ComponentBytes<'a>,
    dicts: &BTreeMap<String, PageSharedDict>,
) -> Result<DjVuPage, DocError>
where
    S: ComponentSource<'a>,
{
    let (form_type, chunks) = bytes.parse()?;
    let shared = incl_targets(&chunks)
        .find_map(|name| dicts.get(name))
        .cloned();
    source.build_page(page_index, bytes, &form_type, &chunks, shared)
}

/// The `INCL` target names of a component, in chunk order.
pub(crate) fn incl_targets<'c>(chunks: &'c [IffChunk<'_>]) -> impl Iterator<Item = &'c str> {
    chunks
        .iter()
        .filter(|c| &c.id == b"INCL")
        .filter_map(|c| incl_target(c.data))
}

#[cfg(feature = "std")]
fn new_dict(djbz: &[u8]) -> PageSharedDict {
    std::sync::Arc::new(super::SharedDict::new(djbz.to_vec()))
}

#[cfg(not(feature = "std"))]
fn new_dict(djbz: &[u8]) -> PageSharedDict {
    djbz.to_vec()
}
