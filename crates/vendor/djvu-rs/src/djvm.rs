//! DJVM document merge and split operations.
//!
//! Provides [`merge`] to combine multiple DjVu documents into a single
//! bundled DJVM, and [`split`] to extract page ranges from a document.
//!
//! [`merge`]: crate::djvm::merge
//! [`split`]: crate::djvm::split

#[cfg(not(feature = "std"))]
use alloc::{format, string::String, vec, vec::Vec};

use crate::dirm::DirmPayload;
use crate::error::IffError;
use crate::iff;

#[cfg(test)]
use crate::djvu_document::DjVuDocument;

/// Error type for merge/split operations.
#[derive(Debug, thiserror::Error)]
pub enum DjvmError {
    /// IFF container parse error.
    #[error("IFF parse error: {0}")]
    Iff(#[from] IffError),

    /// Document model error.
    #[error("document error: {0}")]
    Doc(#[from] crate::djvu_document::DocError),

    /// No pages to merge.
    #[error("no pages to merge")]
    EmptyMerge,

    /// Page range is out of bounds.
    #[error("page range {start}..{end} is out of bounds (document has {count} pages)")]
    PageRangeOutOfBounds {
        start: usize,
        end: usize,
        count: usize,
    },

    /// The assembled document's FORM payload would exceed `u32::MAX` (4 GiB).
    #[error("merged document exceeds the 4 GiB IFF FORM limit")]
    OutputTooLarge,
}

/// Re-serialize a sub-FORM child — the raw `data` of a `FORM` chunk, which
/// begins with its 4-byte form type — back into a standalone `AT&T`-prefixed
/// FORM document. Inverse of [`strip_att`].
fn wrap_sub_form(form_data: &[u8]) -> Vec<u8> {
    // `form_data` is a FORM body: it begins with the 4-byte secondary id
    // (DJVU/DJVI/…) followed by the chunks. Route the AT&T/FORM/length framing
    // through the emission seam rather than hand-assembling it. A well-formed
    // FORM body is even-length (every inner chunk is word-aligned), so the seam
    // reproduces the original bytes exactly; a malformed odd body merely gains a
    // trailing pad, which re-parses identically.
    let split = form_data.len().min(4);
    let (id_bytes, body) = form_data.split_at(split);
    let mut secondary_id = *b"    ";
    secondary_id[..id_bytes.len()].copy_from_slice(id_bytes);
    iff::partial_emit(secondary_id, &[iff::EmitPart::Verbatim(body)])
        .expect("sub-FORM fits within the 4 GiB IFF FORM limit")
}

/// Strip a leading `AT&T` magic from a standalone FORM document, yielding the
/// `FORM`-chunk bytes to embed inside a DJVM bundle. Inverse of [`wrap_sub_form`].
fn strip_att(form: &[u8]) -> &[u8] {
    if form.len() >= 4 && &form[..4] == b"AT&T" {
        &form[4..]
    } else {
        form
    }
}

/// Merge multiple DjVu documents (raw bytes) into a single bundled DJVM.
///
/// Each input document contributes all its pages to the output.
/// Shared dictionaries (DJVI components) are included and INCL
/// references are preserved within each source document's pages.
pub fn merge(documents: &[&[u8]]) -> Result<Vec<u8>, DjvmError> {
    if documents.is_empty() {
        return Err(DjvmError::EmptyMerge);
    }

    let mut components: Vec<Vec<u8>> = Vec::new();
    let mut component_ids: Vec<String> = Vec::new();
    let mut component_flags: Vec<u8> = Vec::new();

    for (doc_idx, &doc_data) in documents.iter().enumerate() {
        let form = iff::parse_form(doc_data)?;

        if &form.form_type == b"DJVU" {
            // Single-page document — the whole file is one page
            components.push(doc_data.to_vec());
            component_ids.push(format!("p{:04}.djvu", components.len()));
            component_flags.push(1); // page
        } else if &form.form_type == b"DJVM" {
            // Multi-page bundled document — extract each FORM child
            for chunk in &form.chunks {
                if &chunk.id == b"FORM" && chunk.data.len() >= 4 {
                    let child_form_type = &chunk.data[..4];

                    let flag = if child_form_type == b"DJVI" { 0 } else { 1 }; // 0 = shared, 1 = page

                    components.push(wrap_sub_form(chunk.data));
                    component_ids.push(format!("d{}p{:04}.djvu", doc_idx, components.len()));
                    component_flags.push(flag);
                }
            }
        }
    }

    if components.is_empty() {
        return Err(DjvmError::EmptyMerge);
    }

    build_djvm(&components, &component_ids, &component_flags)
}

/// Split a document, extracting pages in the given range (0-based, exclusive end).
///
/// Returns raw DjVu bytes for a new document containing only the requested pages.
pub fn split(doc_data: &[u8], start: usize, end: usize) -> Result<Vec<u8>, DjvmError> {
    let form = iff::parse_form(doc_data)?;

    // Page count derived from the same FORM walk used for extraction below, so
    // the bounds check can never disagree with what is actually present (a
    // DIRM-based page count and the FORM:DJVU children can diverge).
    let count = match &form.form_type {
        b"DJVU" => 1,
        b"DJVM" => form
            .chunks
            .iter()
            .filter(|c| &c.id == b"FORM" && c.data.len() >= 4 && &c.data[..4] == b"DJVU")
            .count(),
        _ => 0,
    };

    if start >= count || end > count || start >= end {
        return Err(DjvmError::PageRangeOutOfBounds { start, end, count });
    }

    // Single-page document: just return the whole thing
    if &form.form_type == b"DJVU" && start == 0 && end == 1 {
        return Ok(doc_data.to_vec());
    }

    // For a single page extraction from a multi-page document
    if end - start == 1 && &form.form_type == b"DJVM" {
        let mut page_idx = 0;
        for chunk in &form.chunks {
            if &chunk.id == b"FORM" && chunk.data.len() >= 4 && &chunk.data[..4] == b"DJVU" {
                if page_idx == start {
                    return Ok(wrap_sub_form(chunk.data));
                }
                page_idx += 1;
            }
        }
    }

    // Multiple pages: build a new DJVM bundle with the requested range
    let mut components: Vec<Vec<u8>> = Vec::new();
    let mut component_ids: Vec<String> = Vec::new();
    let mut component_flags: Vec<u8> = Vec::new();

    // First pass: collect shared components (DJVI) that might be needed
    for chunk in &form.chunks {
        if &chunk.id == b"FORM" && chunk.data.len() >= 4 && &chunk.data[..4] == b"DJVI" {
            components.push(wrap_sub_form(chunk.data));
            component_ids.push(format!("shared{}.djvi", components.len()));
            component_flags.push(0); // shared
        }
    }

    // Second pass: collect pages in the requested range
    let mut page_idx = 0;
    for chunk in &form.chunks {
        if &chunk.id == b"FORM" && chunk.data.len() >= 4 && &chunk.data[..4] == b"DJVU" {
            if page_idx >= start && page_idx < end {
                components.push(wrap_sub_form(chunk.data));
                component_ids.push(format!("p{:04}.djvu", page_idx + 1));
                component_flags.push(1); // page
            }
            page_idx += 1;
        }
    }

    build_djvm(&components, &component_ids, &component_flags)
}

/// Build a bundled DJVM file from components.
///
/// The IFF framing — `FORM:DJVM` header, the `DIRM` chunk header, and the
/// even-byte padding between components — is delegated to [`iff::partial_emit`]
/// so this writer shares the one emission seam (#367). The DIRM goes through as
/// a re-framed [`iff::Chunk`]; each component is copied verbatim (its AT&T magic
/// stripped, since it is embedded, not a standalone file).
fn build_djvm(components: &[Vec<u8>], ids: &[String], flags: &[u8]) -> Result<Vec<u8>, DjvmError> {
    let n = components.len();

    // Each component includes the AT&T prefix — strip it for embedding. Its
    // remaining length (FORM header + payload) is the DIRM size-table entry.
    let stripped: Vec<&[u8]> = components.iter().map(|c| strip_att(c)).collect();
    let sizes: Vec<u32> = stripped
        .iter()
        .map(|s| u32::try_from(s.len()).unwrap_or(0))
        .collect();

    // Two-pass emission (the `partial_emit_with_offsets` contract): pass 1
    // learns where each component lands, pass 2 re-emits with the DIRM offset
    // table filled in. The offset table is fixed-width and the size table is
    // final from pass 1, so the layout cannot shift between passes. A zeroed
    // offset table is rejected by DjVuLibre's DjVmDir ("no indirect entries
    // allowed in bundled document", #657).
    let mut payload = DirmPayload::build_bundled(n, flags, ids, &sizes);
    let emit = |payload: &DirmPayload| -> Result<(Vec<u8>, Vec<usize>), DjvmError> {
        let dirm = iff::Chunk::Leaf {
            id: *b"DIRM",
            data: payload.encode(),
        };
        let mut parts: Vec<iff::EmitPart> = Vec::with_capacity(1 + n);
        parts.push(iff::EmitPart::Chunk(&dirm));
        parts.extend(stripped.iter().map(|s| iff::EmitPart::Verbatim(s)));
        iff::partial_emit_with_offsets(*b"DJVM", &parts).ok_or(DjvmError::OutputTooLarge)
    };

    let (_, offsets) = emit(&payload)?;
    payload.offsets = offsets[1..] // parts[0] is the DIRM itself
        .iter()
        .map(|&o| u32::try_from(o).map_err(|_| DjvmError::OutputTooLarge))
        .collect::<Result<_, _>>()?;
    let (bytes, second_offsets) = emit(&payload)?;
    debug_assert_eq!(offsets, second_offsets, "two-pass layout must be stable");
    Ok(bytes)
}

/// Create an indirect (non-bundled) DJVM index file that references pages as
/// separate files.
///
/// The returned bytes are a valid `FORM:DJVM` with a DIRM directory chunk whose
/// `is_bundled` flag is **not** set.  Each entry in `page_names` becomes one
/// `Page` component; there are no embedded `FORM:DJVU` sub-forms — the component
/// data lives in separate files that must be passed to a resolver when parsing.
///
/// Shared-dictionary (DJVI) components are not supported by this helper; use
/// [`merge`] to build a bundled document that includes them.
///
/// # Errors
///
/// Returns [`DjvmError::EmptyMerge`] if `page_names` is empty.
pub fn create_indirect(page_names: &[&str]) -> Result<Vec<u8>, DjvmError> {
    if page_names.is_empty() {
        return Err(DjvmError::EmptyMerge);
    }

    let count = page_names.len();
    let ids: Vec<String> = page_names.iter().map(|s| s.to_string()).collect();
    // All entries are pages (flag = 1)
    let flags: Vec<u8> = vec![1u8; count];

    // Indirect: a single DIRM chunk, no embedded component FORMs. Route the
    // DJVM framing through the emission seam (same path as the bundled build).
    let dirm = iff::Chunk::Leaf {
        id: *b"DIRM",
        data: DirmPayload::build_indirect(count, &flags, &ids).encode(),
    };
    iff::partial_emit(*b"DJVM", &[iff::EmitPart::Chunk(&dirm)]).ok_or(DjvmError::OutputTooLarge)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_path(name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    #[test]
    fn merge_empty_returns_error() {
        let result = merge(&[]);
        assert!(result.is_err());
    }

    /// #657: merged bundles must carry a DjVuLibre-acceptable DIRM — version
    /// byte 0x81 (bundled, directory version 1), every offset non-zero and
    /// pointing at a component `FORM` tag, and the 24-bit size table matching
    /// each component's actual byte span. A zeroed offset table or version 0
    /// is rejected by DjVmDir ("no indirect entries allowed in bundled
    /// document").
    #[test]
    fn merge_dirm_offsets_sizes_and_version_are_djvulibre_clean() {
        let a = std::fs::read(fixture_path("navm_fgbz.djvu")).unwrap();
        let bytes = merge(&[&a, &a]).unwrap();

        assert_eq!(&bytes[16..20], b"DIRM");
        let dirm_len = u32::from_be_bytes(bytes[20..24].try_into().unwrap()) as usize;
        let payload = &bytes[24..24 + dirm_len];
        assert_eq!(payload[0], 0x81, "bundled bit + directory version 1");

        let nfiles = u16::from_be_bytes(payload[1..3].try_into().unwrap()) as usize;
        assert!(nfiles > 0);
        let dirm = DirmPayload::decode(payload).unwrap();
        let components = dirm.components();
        assert_eq!(components.len(), nfiles);
        for (c, &off) in components.iter().zip(&dirm.offsets) {
            assert_ne!(off, 0, "component {} has a zeroed offset", c.id);
            let off = off as usize;
            assert_eq!(&bytes[off..off + 4], b"FORM", "offset must hit a FORM tag");
            let form_len = u32::from_be_bytes(bytes[off + 4..off + 8].try_into().unwrap()) as u64;
            assert_eq!(
                c.size as u64,
                form_len + 8,
                "size table must match component {}'s FORM span",
                c.id
            );
        }
    }

    #[test]
    fn split_single_page_from_multipage() {
        let path = fixture_path("DjVu3Spec_bundled.djvu");
        if !path.exists() {
            // Skip if fixture not available
            return;
        }
        let data = std::fs::read(&path).expect("read fixture");
        let doc = DjVuDocument::parse(&data).expect("parse");
        let count = doc.page_count();
        assert!(count > 1, "need multipage fixture");

        // Split out page 0
        let page0 = split(&data, 0, 1).expect("split page 0");
        // Verify the result is parseable
        let form = iff::parse_form(&page0).expect("parse split page");
        assert_eq!(&form.form_type, b"DJVU");
    }

    #[test]
    fn merge_two_single_page_files() {
        let path = fixture_path("irish.djvu");
        if !path.exists() {
            return;
        }
        let irish = std::fs::read(&path).expect("read fixture");
        let data = merge(&[&irish, &irish]).expect("merge");
        // Verify the result has the right FORM type
        let form = iff::parse_form(&data).expect("parse merged");
        assert_eq!(&form.form_type, b"DJVM");
    }

    #[test]
    fn split_out_of_bounds() {
        let path = fixture_path("irish.djvu");
        if !path.exists() {
            return;
        }
        let data = std::fs::read(&path).expect("read fixture");
        let result = split(&data, 0, 5);
        assert!(result.is_err());
    }

    #[test]
    fn create_indirect_empty_returns_error() {
        let result = create_indirect(&[]);
        assert!(result.is_err());
    }

    #[test]
    fn create_indirect_parses_with_resolver() {
        // Build an indirect DJVM that references "chicken.djvu"
        let indirect_bytes = create_indirect(&["chicken.djvu"]).expect("create_indirect");

        // Verify it parses as FORM:DJVM
        let form = iff::parse_form(&indirect_bytes).expect("parse form");
        assert_eq!(&form.form_type, b"DJVM");

        // Verify DIRM chunk has is_bundled = 0
        let dirm = form.chunks.iter().find(|c| &c.id == b"DIRM").expect("DIRM");
        let payload = crate::dirm::DirmPayload::decode(dirm.data).expect("decode DIRM");
        assert!(
            !payload.is_bundled(),
            "indirect DIRM must not have bundled bit set"
        );

        // Parse with a resolver that supplies chicken.djvu
        let chicken_path = fixture_path("chicken.djvu");
        if !chicken_path.exists() {
            return;
        }
        let chicken_data = std::fs::read(&chicken_path).expect("read chicken.djvu");
        let doc = DjVuDocument::parse_with_resolver(
            &indirect_bytes,
            Some(
                move |name: &str| -> Result<Vec<u8>, crate::djvu_document::DocError> {
                    if name == "chicken.djvu" {
                        Ok(chicken_data.clone())
                    } else {
                        Err(crate::djvu_document::DocError::IndirectResolve(
                            name.to_string(),
                        ))
                    }
                },
            ),
        )
        .expect("parse indirect with resolver");

        assert_eq!(doc.page_count(), 1);
        let page = doc.page(0).unwrap();
        assert_eq!(page.width(), 181);
        assert_eq!(page.height(), 240);
    }

    #[test]
    fn create_indirect_multipage() {
        // 3-page indirect document
        let indirect_bytes =
            create_indirect(&["page1.djvu", "page2.djvu", "page3.djvu"]).expect("create_indirect");
        let form = iff::parse_form(&indirect_bytes).expect("parse");
        assert_eq!(&form.form_type, b"DJVM");

        // Component count = 3 in DIRM
        let dirm = form.chunks.iter().find(|c| &c.id == b"DIRM").expect("DIRM");
        let payload = crate::dirm::DirmPayload::decode(dirm.data).expect("decode DIRM");
        assert_eq!(payload.nfiles, 3);
    }

    #[test]
    fn merge_with_djvm_input_extracts_all_pages() {
        let path = fixture_path("DjVu3Spec_bundled.djvu");
        if !path.exists() {
            return;
        }
        let data = std::fs::read(&path).expect("read");
        let doc = DjVuDocument::parse(&data).expect("parse");
        let expected_pages = doc.page_count();

        // merge(&[djvm]) should expand the DJVM into its component pages
        let merged = merge(&[&data]).expect("merge DJVM");
        let form = iff::parse_form(&merged).expect("parse merged DJVM");
        assert_eq!(&form.form_type, b"DJVM");
        let page_count = form
            .chunks
            .iter()
            .filter(|c| &c.id == b"FORM" && c.data.len() >= 4 && &c.data[..4] == b"DJVU")
            .count();
        assert_eq!(page_count, expected_pages);
    }

    #[test]
    fn split_single_page_djvu_returns_original_bytes() {
        let path = fixture_path("chicken.djvu");
        if !path.exists() {
            return;
        }
        let data = std::fs::read(&path).expect("read");
        let result = split(&data, 0, 1).expect("split single-page");
        assert_eq!(
            result, data,
            "splitting a single-page doc must return original bytes"
        );
    }

    #[test]
    fn split_unknown_form_type_is_out_of_bounds() {
        // A valid AT&T FORM with an unknown form type has 0 pages → always OOB
        let fake = iff::partial_emit(*b"UNKN", &[]).unwrap();
        let result = split(&fake, 0, 1);
        assert!(
            result.is_err(),
            "unknown form type must yield PageRangeOutOfBounds"
        );
    }

    #[test]
    fn split_range_from_multipage_djvm_builds_new_djvm() {
        let path = fixture_path("DjVu3Spec_bundled.djvu");
        if !path.exists() {
            return;
        }
        let data = std::fs::read(&path).expect("read");
        let doc = DjVuDocument::parse(&data).expect("parse");
        let count = doc.page_count();
        if count < 3 {
            return;
        }
        // Extract pages 1..3 — a multi-page range → hits build_djvm path
        let extracted = split(&data, 1, 3).expect("split range");
        let form = iff::parse_form(&extracted).expect("parse extracted");
        assert_eq!(&form.form_type, b"DJVM");
        let page_count = form
            .chunks
            .iter()
            .filter(|c| &c.id == b"FORM" && c.data.len() >= 4 && &c.data[..4] == b"DJVU")
            .count();
        assert_eq!(page_count, 2);
    }

    #[test]
    fn merge_unknown_form_type_returns_empty_merge_error() {
        // All docs are unknown type → components stays empty → EmptyMerge
        let fake = iff::partial_emit(*b"UNKN", &[]).unwrap();
        let result = merge(&[&fake]);
        assert!(matches!(result, Err(DjvmError::EmptyMerge)));
    }

    #[test]
    fn split_second_page_from_djvm_skips_first() {
        // Extracting page at index 1 forces page_idx to increment past index 0,
        // covering the page_idx += 1 path in the single-page DJVM loop.
        let path = fixture_path("DjVu3Spec_bundled.djvu");
        if !path.exists() {
            return;
        }
        let data = std::fs::read(&path).expect("read");
        let doc = DjVuDocument::parse(&data).expect("parse");
        if doc.page_count() < 2 {
            return;
        }
        let result = split(&data, 1, 2).expect("split page 1");
        let form = iff::parse_form(&result).expect("parse split page");
        assert_eq!(&form.form_type, b"DJVU");
    }

    #[test]
    fn parse_from_dir_indirect() {
        // Write an indirect DJVM index and chicken.djvu to a temp directory,
        // then open it via parse_from_dir.
        let chicken_path = fixture_path("chicken.djvu");
        if !chicken_path.exists() {
            return;
        }
        let tmp = std::env::temp_dir().join("djvu_indirect_test");
        std::fs::create_dir_all(&tmp).unwrap();

        // Copy chicken.djvu as the component
        let component_name = "p0001.djvu";
        std::fs::copy(&chicken_path, tmp.join(component_name)).unwrap();

        // Build indirect index
        let index_bytes = create_indirect(&[component_name]).expect("create_indirect");
        let index_path = tmp.join("index.djvu");
        std::fs::write(&index_path, &index_bytes).unwrap();

        // Open via parse_from_dir
        let index_data = std::fs::read(&index_path).unwrap();
        let doc = DjVuDocument::parse_from_dir(&index_data, &tmp).expect("parse_from_dir");
        assert_eq!(doc.page_count(), 1);
        assert_eq!(doc.page(0).unwrap().width(), 181);
    }
}
