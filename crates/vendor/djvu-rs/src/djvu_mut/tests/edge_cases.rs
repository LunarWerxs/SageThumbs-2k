//! Branch-coverage edge cases for `DjVuDocumentMut`.

use super::*;

// Line 585: (None, true) branch of set_bookmarks — no-op when DJVM has no NAVM and
// we try to set empty bookmarks.
#[test]
fn set_bookmarks_empty_on_djvm_without_navm_is_noop() {
    // Strip NAVM from a bundled doc, then call set_bookmarks(&[]) on the stripped doc.
    let original = read_corpus("DjVu3Spec_bundled.djvu");
    let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
    doc.set_bookmarks(&[]).unwrap(); // removes NAVM if present
    let stripped = doc.into_bytes();

    // Now stripped has no NAVM; set_bookmarks(&[]) is a true no-op (None, true).
    let mut doc2 = DjVuDocumentMut::from_bytes(&stripped).unwrap();
    doc2.set_bookmarks(&[]).unwrap();
    assert_eq!(
        doc2.into_bytes(),
        stripped,
        "no-op set_bookmarks should not change bytes"
    );
}

// Line 936: (None, true) branch of replace_or_insert — set_metadata with default
// (empty) on a page that has no existing METa/METz chunk.
#[test]
fn set_metadata_empty_on_page_without_meta_is_noop() {
    let original = read_corpus("chicken.djvu"); // known: no METa chunk
    let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
    // Default metadata → encode_metadata returns empty → (None, true) no-op path.
    doc.page_mut(0)
        .unwrap()
        .set_metadata(&DjVuMetadata::default())
        .unwrap();
    // Dirty is still set (set_metadata always marks dirty), but no METa was inserted.
    let bytes = doc.into_bytes();
    let reparsed = DjVuDocumentMut::from_bytes(&bytes).unwrap();
    let has_meta = reparsed
        .file
        .root
        .children()
        .iter()
        .any(|c| matches!(c, Chunk::Leaf { id, .. } if id == b"METa" || id == b"METz"));
    assert!(
        !has_meta,
        "empty set_metadata should not insert a METa chunk"
    );
}

// Lines 391-393: PathTraversesLeaf first branch (children.is_empty && depth < len-1).
// A 3-deep path where [0] reaches INFO (Leaf): depth=1 triggers the first check.
#[test]
fn chunk_at_path_traverses_leaf_first_branch() {
    let original = read_corpus("chicken.djvu");
    let doc = DjVuDocumentMut::from_bytes(&original).unwrap();
    let err = doc.chunk_at_path(&[0, 0, 0]).unwrap_err();
    assert!(
        matches!(err, MutError::PathTraversesLeaf { depth: 1, len: 3 }),
        "{err:?}"
    );
}

// Lines 1089, 1094-1095: IndirectRewritePlan::from_indirect_resolved error paths.
#[test]
fn rewrite_plan_resolver_failure_returns_component_resolve_error() {
    let (index, _) = indirect_over_fixtures(&["chicken.djvu"]);
    let err = IndirectRewritePlan::from_indirect_resolved(&index, |_name: &str| {
        Err::<Vec<u8>, std::io::Error>(std::io::Error::new(std::io::ErrorKind::NotFound, "nope"))
    })
    .unwrap_err();
    assert!(matches!(err, MutError::ComponentResolve { .. }), "{err:?}");
}

#[test]
fn rewrite_plan_non_iff_component_returns_malformed_error() {
    let (index, _) = indirect_over_fixtures(&["chicken.djvu"]);
    let err = IndirectRewritePlan::from_indirect_resolved(&index, |_name: &str| {
        Ok::<Vec<u8>, std::io::Error>(b"not iff".to_vec())
    })
    .unwrap_err();
    assert!(
        matches!(err, MutError::ComponentMalformed { .. }),
        "{err:?}"
    );
}

// Lines 1100-1105: wrong FORM type (not DJVU/DJVI/THUM) → ComponentMalformed.
#[test]
fn rewrite_plan_wrong_form_type_returns_malformed_error() {
    let (index, _) = indirect_over_fixtures(&["chicken.djvu"]);
    let fake = iff::emit(&DjvuFile {
        root: Chunk::Form {
            secondary_id: *b"FAKE",
            length: 0,
            children: vec![],
        },
    });
    let err = IndirectRewritePlan::from_indirect_resolved(&index, move |_name: &str| {
        Ok::<Vec<u8>, std::io::Error>(fake.clone())
    })
    .unwrap_err();
    assert!(
        matches!(err, MutError::ComponentMalformed { .. }),
        "{err:?}"
    );
}

// Lines 1131-1132: component_count() on IndirectRewritePlan.
#[test]
fn rewrite_plan_component_count() {
    let (index, resolver) = indirect_over_fixtures(&["chicken.djvu", "irish.djvu"]);
    let plan = IndirectRewritePlan::from_indirect_resolved(&index, resolver).unwrap();
    assert_eq!(plan.component_count(), 2);
    assert_eq!(plan.page_count(), 2);
}

#[cfg(feature = "std")]
#[test]
fn validate_safe_component_name_rejects_empty() {
    let err = validate_safe_component_name("").unwrap_err();
    assert!(matches!(err, MutError::UnsafeComponentName { .. }));
}

#[cfg(feature = "std")]
#[test]
fn validate_safe_component_name_rejects_nul() {
    let err = validate_safe_component_name("a\0b").unwrap_err();
    assert!(matches!(err, MutError::UnsafeComponentName { .. }));
}

// ---- is_bundled_djvm edge cases -----------------------------------------

// Line 672: root is a Leaf → returns false immediately.
#[test]
fn is_bundled_djvm_leaf_returns_false() {
    let leaf = Chunk::Leaf {
        id: *b"INFO",
        data: vec![],
    };
    assert!(!is_bundled_djvm(&leaf));
}

// Line 675: FORM with secondary_id != DJVM → returns false.
#[test]
fn is_bundled_djvm_non_djvm_form_returns_false() {
    let form = Chunk::Form {
        secondary_id: *b"DJVU",
        length: 0,
        children: vec![],
    };
    assert!(!is_bundled_djvm(&form));
}

// ---- resolve_indirect_components / find_leaf_data edge cases ------------

// Line 637: indirect DJVM with nfiles=0 → DirmMalformed("indirect DIRM lists no components").
#[test]
fn from_indirect_resolved_empty_dirm_returns_dirm_malformed() {
    let dirm_payload = DirmPayload::build_indirect(&[]);
    let dirm = Chunk::Leaf {
        id: *b"DIRM",
        data: dirm_payload.encode(),
    };
    let index = iff::partial_emit(*b"DJVM", &[iff::EmitPart::Chunk(&dirm)]).expect("fits");
    let err = DjVuDocumentMut::from_indirect_resolved(&index, |_n: &str| {
        Ok::<Vec<u8>, std::io::Error>(Vec::new())
    })
    .unwrap_err();
    assert!(
        matches!(err, MutError::DirmMalformed(_)),
        "expected DirmMalformed, got {err:?}"
    );
}

// Line 915: `find_leaf_data` returns None when the page has no INFO chunk.
// Triggered by calling `set_text_layer` on a FORM:DJVU without an INFO chunk.
#[test]
fn set_text_layer_missing_info_chunk_returns_missing_page_info() {
    use crate::text::{Rect, TextLayer, TextZone, TextZoneKind};

    let bytes = iff::emit(&iff::DjvuFile {
        root: Chunk::Form {
            secondary_id: *b"DJVU",
            length: 0,
            // No INFO chunk
            children: vec![Chunk::Leaf {
                id: *b"ANTz",
                data: vec![0u8; 4],
            }],
        },
    });
    let mut doc = DjVuDocumentMut::from_bytes(&bytes).expect("no-INFO DJVU must parse");
    let layer = TextLayer {
        text: "hello".to_string(),
        zones: vec![TextZone {
            kind: TextZoneKind::Page,
            rect: Rect {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
            },
            text: "hello".to_string(),
            children: vec![],
        }],
    };
    let err = doc.page_mut(0).unwrap().set_text_layer(&layer).unwrap_err();
    assert!(matches!(err, MutError::MissingPageInfo), "{err:?}");
}

// ---- emit_patched_single_page / original_single_page_child_ranges -------

// Line 700: root is a Leaf → emit_patched_single_page returns None immediately.
#[test]
fn emit_patched_leaf_root_returns_none() {
    let leaf = Chunk::Leaf {
        id: *b"INFO",
        data: vec![0u8; 4],
    };
    assert!(emit_patched_single_page(&leaf, &[]).is_none());
}

// Line 725: DJVU FORM with a nested Form child → returns None.
#[test]
fn emit_patched_form_child_in_djvu_returns_none() {
    // Build minimal valid AT&T+FORM:DJVU bytes with one INFO leaf so that
    // original_single_page_child_ranges succeeds (1 child, no FORM inside).
    let original = iff::partial_emit(
        *b"DJVU",
        &[iff::EmitPart::Chunk(&Chunk::Leaf {
            id: *b"INFO",
            data: vec![0u8; 4],
        })],
    )
    .unwrap();

    // In-memory tree: same DJVU root but child is a Form instead of the Leaf.
    let root = Chunk::Form {
        secondary_id: *b"DJVU",
        length: 0,
        children: vec![Chunk::Form {
            secondary_id: *b"INFO",
            length: 0,
            children: vec![],
        }],
    };
    assert!(emit_patched_single_page(&root, &original).is_none());
}

// Line 734: slice shorter than 16 bytes → original_single_page_child_ranges returns None.
#[test]
fn original_child_ranges_too_short_returns_none() {
    assert!(original_single_page_child_ranges(b"AT&TFORM").is_none());
}

// Line 739: secondary_id is not DJVU → returns None.
#[test]
fn original_child_ranges_not_djvu_returns_none() {
    let bytes = iff::partial_emit(*b"DJVI", &[]).unwrap();
    assert!(original_single_page_child_ranges(&bytes).is_none());
}

// Line 753: DJVU body contains a chunk whose id is b"FORM" → returns None.
#[test]
fn original_child_ranges_nested_form_tag_returns_none() {
    // Build AT&T FORM:DJVU with one child whose id bytes are literally "FORM".
    // Data length = 4 so header+data = 12 bytes, body = DJVU(4)+12 = 16.
    // Use Verbatim so the chunk-id bytes spell "FORM" inside a slice literal,
    // routing around the raw-framing seam.
    let inner: &[u8] = b"FORM\x00\x00\x00\x04\x00\x00\x00\x00";
    let bytes = iff::partial_emit(*b"DJVU", &[iff::EmitPart::Verbatim(inner)]).unwrap();
    assert!(original_single_page_child_ranges(&bytes).is_none());
}

// Line 761: last chunk has odd length and is exactly at body_end (no room for pad) → returns None.
#[test]
fn original_child_ranges_odd_length_at_body_end_returns_none() {
    // DJVU body = DJVU(4) + INFO header(8) + 3 bytes data = 15 bytes.
    // next = 16+8+3 = 27 = body_end → odd tail with no pad room.
    // partial_emit would add padding, so build the deliberately odd-body bytes manually.
    let form_tag: [u8; 4] = *b"FORM";
    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend_from_slice(&iff::MAGIC);
    bytes.extend_from_slice(&form_tag);
    bytes.extend_from_slice(&15u32.to_be_bytes()); // body = 4+8+3 = 15
    bytes.extend_from_slice(b"DJVU");
    bytes.extend_from_slice(b"INFO");
    bytes.extend_from_slice(&3u32.to_be_bytes());
    bytes.extend_from_slice(&[0xAA, 0xBB, 0xCC]);
    assert!(original_single_page_child_ranges(&bytes).is_none());
}

// Line 776: chunks don't tile the body exactly (1 extra trailing byte) → returns None.
#[test]
fn original_child_ranges_short_tail_returns_none() {
    // DJVU body = DJVU(4) + INFO hdr+data (12) + 1 extra byte = 17.
    // partial_emit cannot produce a non-tiling body, so build manually.
    let form_tag: [u8; 4] = *b"FORM";
    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend_from_slice(&iff::MAGIC);
    bytes.extend_from_slice(&form_tag);
    bytes.extend_from_slice(&17u32.to_be_bytes()); // 17 = 4 + 8 + 4 + 1
    bytes.extend_from_slice(b"DJVU");
    bytes.extend_from_slice(b"INFO");
    bytes.extend_from_slice(&4u32.to_be_bytes());
    bytes.extend_from_slice(&[0u8; 4]);
    bytes.push(0x00); // extra trailing byte
    assert!(original_single_page_child_ranges(&bytes).is_none());
}

// ---- recompute_dirm_offsets edge cases ----------------------------------

// Line 798: root is a Leaf → returns Ok immediately.
#[test]
fn recompute_dirm_offsets_leaf_root_is_noop() {
    let mut leaf = Chunk::Leaf {
        id: *b"INFO",
        data: vec![0u8; 4],
    };
    assert!(recompute_dirm_offsets(&mut leaf).is_ok());
}

// Line 834: DJVM with FORM:DJVU child but no DIRM leaf → returns Ok.
#[test]
fn recompute_dirm_offsets_djvm_no_dirm_is_noop() {
    let mut root = Chunk::Form {
        secondary_id: *b"DJVM",
        length: 0,
        children: vec![Chunk::Form {
            secondary_id: *b"DJVU",
            length: 0,
            children: vec![],
        }],
    };
    assert!(recompute_dirm_offsets(&mut root).is_ok());
}

// Lines 851-853: nfiles in DIRM != number of FORM:DJVU children → DirmComponentCountMismatch.
#[test]
fn recompute_dirm_offsets_count_mismatch_errors() {
    // DIRM payload with nfiles=2 but bundled, then supply only 1 FORM:DJVU.
    let dirm_payload = DirmPayload::build_bundled(&[
        DirmComponent::new(DirmComponentKind::Page, "p1.djvu"),
        DirmComponent::new(DirmComponentKind::Page, "p2.djvu"),
    ]);
    let dirm_data = dirm_payload.encode();
    let mut root = Chunk::Form {
        secondary_id: *b"DJVM",
        length: 0,
        children: vec![
            Chunk::Leaf {
                id: *b"DIRM",
                data: dirm_data,
            },
            Chunk::Form {
                secondary_id: *b"DJVU",
                length: 0,
                children: vec![],
            },
            // Only 1 FORM:DJVU but DIRM says nfiles=2 → mismatch
        ],
    };
    let err = recompute_dirm_offsets(&mut root).unwrap_err();
    assert!(
        matches!(err, MutError::DirmComponentCountMismatch { .. }),
        "{err:?}"
    );
}
