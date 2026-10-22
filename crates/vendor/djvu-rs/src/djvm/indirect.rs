//! Indirect (multi-file) documents: converting a bundle and creating an index.

use super::*;

/// An indirect `FORM:DJVM` index and the external component files it resolves.
pub struct IndirectDocument {
    /// The indirect `FORM:DJVM` index file bytes (`DIRM` has no bundled bit or
    /// offset table; document-level chunks such as `NAVM` are retained).
    pub index: Vec<u8>,
    /// One resolver-keyed standalone component file per `DIRM` entry, in
    /// directory order.
    pub components: Vec<(String, Vec<u8>)>,
}

/// Convert a bundled `FORM:DJVM` into its indirect index and standalone
/// component files.
///
/// The returned index retains each document-level non-`FORM` chunk (including
/// `NAVM`). Its `DIRM` is the source directory with only the bundled bit and
/// offset table removed: the BZZ-compressed metadata tail is retained verbatim,
/// so component ids, names, titles, and flags remain stable. Each returned
/// component is a complete `AT&T`-prefixed `FORM:DJVU`, `FORM:DJVI`, or
/// `FORM:THUM` file suitable for [`crate::djvu_document::ComponentResolver`].
pub fn to_indirect(bundled: &[u8]) -> Result<IndirectDocument, DjvmError> {
    let Bundle {
        mut dirm,
        directory,
        forms,
        chunks,
    } = Bundle::parse(bundled)?;
    let expected_count = forms.len();

    // The BZZ metadata tail is opaque here. Decoding it only supplies the
    // resolver keys; the re-emitted DIRM carries the original metadata bytes.
    let components = directory
        .into_iter()
        .zip(forms)
        .map(|(component, form)| (component.id, wrap_sub_form(form)))
        .collect();

    // Bundled DIRM layout is [flags][nfiles][offset table][BZZ metadata].
    // Clear only the bit that selects that layout; `encode` then omits the
    // table while preserving the metadata blob byte-for-byte.
    dirm.flags &= !BUNDLED_FLAG;
    dirm.offsets.clear();
    let indirect_dirm = dirm.encode();

    // Preserve document-level chunks (NAVM and any extensions) in their
    // original order while removing all embedded component FORMs. Re-frame
    // leaves through the IFF emission seam so length and padding are correct.
    let mut index_chunks = Vec::with_capacity(chunks.len() - expected_count);
    let mut replaced_dirm = false;
    for chunk in &chunks {
        match chunk.id {
            id if id == *b"FORM" => {}
            id if id == *b"DIRM" && !replaced_dirm => {
                index_chunks.push(iff::Chunk::Leaf {
                    id: *b"DIRM",
                    data: indirect_dirm.clone(),
                });
                replaced_dirm = true;
            }
            id if id == *b"DIRM" => {}
            id => index_chunks.push(iff::Chunk::Leaf {
                id,
                data: chunk.data.to_vec(),
            }),
        }
    }
    debug_assert!(replaced_dirm, "the DIRM was found above");
    let index_parts = index_chunks
        .iter()
        .map(iff::EmitPart::Chunk)
        .collect::<Vec<_>>();
    let index = iff::partial_emit(*b"DJVM", &index_parts).ok_or(DjvmError::OutputTooLarge)?;

    Ok(IndirectDocument { index, components })
}

/// Create an indirect (non-bundled) DJVM index file that references pages as
/// separate files.
///
/// The returned bytes are a valid `FORM:DJVM` with a DIRM directory chunk whose
/// `is_bundled` flag is **not** set.  Each entry in `page_names` becomes one
/// `Page` component; there are no embedded `FORM:DJVU` sub-forms — the component
/// data lives in separate files that must be passed to a resolver when parsing.
///
/// This helper lists pages only. To list shared `DJVI` components (symbol
/// dictionaries, shared annotations) and thumbnails too, use
/// [`create_indirect_with_components`].
///
/// # Errors
///
/// Returns [`DjvmError::EmptyMerge`] if `page_names` is empty.
pub fn create_indirect(page_names: &[&str]) -> Result<Vec<u8>, DjvmError> {
    if page_names.is_empty() {
        return Err(DjvmError::EmptyMerge);
    }

    let pages = page_names
        .iter()
        .map(|name| DirmComponent {
            kind: DirmComponentKind::Page,
            id: name.to_string(),
            size: 0,
        })
        .collect::<Vec<_>>();

    // Indirect: a single DIRM chunk, no embedded component FORMs. Route the
    // DJVM framing through the emission seam (same path as the bundled build).
    let dirm = iff::Chunk::Leaf {
        id: *b"DIRM",
        data: DirmPayload::build_indirect(&pages).encode(),
    };
    iff::partial_emit(*b"DJVM", &[iff::EmitPart::Chunk(&dirm)]).ok_or(DjvmError::OutputTooLarge)
}

/// Create an indirect `FORM:DJVM` index for a set of standalone component
/// files: pages, shared `DJVI` components and `THUM` thumbnails.
///
/// Each entry is `(name, file bytes)`, in directory order. The name is the
/// DIRM id: the file name a resolver loads and the name a page's `INCL` uses.
/// The FORM type of each file sets its kind: `DJVU` (or legacy `BM44`/`PM44`)
/// is a page, `DJVI` is a shared component, `THUM` is a thumbnail. A `DJVI`
/// that every page includes, that holds annotations and no `Djbz` symbol
/// dictionary, becomes the document's shared annotation (DIRM flag 3), where
/// DjVuLibre reads document metadata; only the first such component does.
///
/// The index records each component's size. Write each component file with the
/// exact bytes given here, next to the index. The index holds only the
/// directory: to keep bookmarks (`NAVM`), split a bundled document with
/// [`to_indirect`] instead.
///
/// # Errors
///
/// - [`DjvmError::EmptyMerge`] when no component is a page;
/// - [`DjvmError::DuplicateComponentName`] when two components share a name;
/// - [`DjvmError::UnsupportedComponentForm`] for any other FORM type;
/// - [`DjvmError::UnresolvedInclude`] when a page includes a name that is not
///   a shared component in the list;
/// - [`DjvmError::TooManyComponents`] above 65 535 components;
/// - [`DjvmError::Iff`] when a component is not a valid IFF file.
pub fn create_indirect_with_components(components: &[(&str, &[u8])]) -> Result<Vec<u8>, DjvmError> {
    use std::collections::{BTreeMap, BTreeSet};

    if components.len() > usize::from(u16::MAX) {
        return Err(DjvmError::TooManyComponents {
            count: components.len(),
        });
    }

    let mut index_of: BTreeMap<&str, usize> = BTreeMap::new();
    let mut entries = Vec::with_capacity(components.len());
    let mut page_includes: Vec<(&str, BTreeSet<String>)> = Vec::new();
    // Shared components that could hold the shared annotation, in order.
    let mut anno_candidates: Vec<(&str, usize)> = Vec::new();
    for (index, &(name, bytes)) in components.iter().enumerate() {
        if index_of.insert(name, index).is_some() {
            return Err(DjvmError::DuplicateComponentName {
                name: name.to_string(),
            });
        }
        let form = iff::parse_form(bytes)?;
        // `parse_form` succeeded, so the FORM header is present.
        let form_bytes = strip_att(bytes);
        let declared =
            u32::from_be_bytes([form_bytes[4], form_bytes[5], form_bytes[6], form_bytes[7]]);
        let kind = match &form.form_type {
            form_type if is_page_form(form_type) => {
                let includes = form
                    .chunks
                    .iter()
                    .filter(|chunk| chunk.id == *b"INCL")
                    .map(|chunk| {
                        crate::dirm::incl_target(chunk.data).map_or_else(
                            || String::from_utf8_lossy(chunk.data.trim_ascii_end()).into_owned(),
                            str::to_owned,
                        )
                    })
                    .collect();
                page_includes.push((name, includes));
                DirmComponentKind::Page
            }
            b"DJVI" => {
                let has = |id: &[u8; 4]| form.chunks.iter().any(|chunk| chunk.id == *id);
                if !has(b"Djbz") && (has(b"ANTa") || has(b"ANTz")) {
                    anno_candidates.push((name, index));
                }
                DirmComponentKind::Shared
            }
            b"THUM" => DirmComponentKind::Thumbnail,
            other => {
                return Err(DjvmError::UnsupportedComponentForm {
                    name: name.to_string(),
                    form_type: *other,
                });
            }
        };
        entries.push(DirmComponent {
            kind,
            id: name.to_string(),
            size: declared.saturating_add(8),
        });
    }
    if page_includes.is_empty() {
        return Err(DjvmError::EmptyMerge);
    }

    for (page, includes) in &page_includes {
        if let Some(include) = includes.iter().find(|include| {
            index_of
                .get(include.as_str())
                .map(|&index| entries[index].kind)
                != Some(DirmComponentKind::Shared)
        }) {
            return Err(DjvmError::UnresolvedInclude {
                page: page.to_string(),
                include: include.clone(),
            });
        }
    }
    if let Some(&(_, index)) = anno_candidates.iter().find(|(name, _)| {
        page_includes
            .iter()
            .all(|(_, includes)| includes.contains(*name))
    }) {
        entries[index].kind = DirmComponentKind::SharedAnno;
    }

    let dirm = iff::Chunk::Leaf {
        id: *b"DIRM",
        data: DirmPayload::build_indirect(&entries).encode(),
    };
    iff::partial_emit(*b"DJVM", &[iff::EmitPart::Chunk(&dirm)]).ok_or(DjvmError::OutputTooLarge)
}
