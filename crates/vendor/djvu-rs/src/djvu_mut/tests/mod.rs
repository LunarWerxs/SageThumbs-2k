use super::*;
use std::path::PathBuf;

pub(super) fn corpus_path(name: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures");
    p.push(name);
    p
}

pub(super) fn read_corpus(name: &str) -> Vec<u8> {
    std::fs::read(corpus_path(name)).expect("corpus fixture missing")
}

/// Walk top-level children of the outer FORM and return their absolute
/// byte ranges (header+payload+pad).
pub(super) fn top_form_ranges(data: &[u8]) -> Vec<core::ops::Range<usize>> {
    assert_eq!(&data[..4], b"AT&T");
    let form_len = u32::from_be_bytes([data[8], data[9], data[10], data[11]]) as usize;
    let body_end = 12 + form_len;
    let mut pos = 16usize; // skip AT&T(4) + FORM(4) + len(4) + secondary_id(4)
    let mut out = Vec::new();
    while pos + 8 <= body_end {
        let len = u32::from_be_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
            as usize;
        let mut next = pos + 8 + len;
        if next & 1 == 1 && next < body_end {
            next += 1;
        }
        out.push(pos..next);
        pos = next;
    }
    out
}

/// #595: `save_patched` must leave the file byte-identical to
/// `try_into_bytes` for clean, same-size-edit, and size-changing-edit
/// saves — and its `bytes_written` must reflect the incremental win.
#[test]
fn save_patched_matches_full_serialization() {
    let original = read_corpus("navm_fgbz.djvu");
    let tmp = tempfile::NamedTempFile::new().unwrap();

    // Clean save: nothing written.
    std::fs::write(tmp.path(), &original).unwrap();
    let doc = DjVuDocumentMut::from_bytes(&original).unwrap();
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(tmp.path())
        .unwrap();
    let stats = doc.save_patched(&mut f).unwrap();
    assert_eq!(stats.bytes_written, 0);
    assert_eq!(std::fs::read(tmp.path()).unwrap(), original);

    // Size-changing edit (bookmarks): file equals the full serialization,
    // and the untouched head is skipped.
    let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
    let bookmarks = vec![DjVuBookmark {
        title: "patched".into(),
        url: "#1".into(),
        children: Vec::new(),
    }];
    doc.set_bookmarks(&bookmarks).unwrap();
    let expected = {
        let mut clone = DjVuDocumentMut::from_bytes(&original).unwrap();
        clone.set_bookmarks(&bookmarks).unwrap();
        clone.try_into_bytes().unwrap()
    };
    std::fs::write(tmp.path(), &original).unwrap();
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(tmp.path())
        .unwrap();
    let stats = doc.save_patched(&mut f).unwrap();
    assert_eq!(std::fs::read(tmp.path()).unwrap(), expected);
    assert_eq!(stats.file_len, expected.len() as u64);
    assert!(
        stats.bytes_written < expected.len() as u64,
        "size-changing edit must still skip the untouched head"
    );

    // Same-size edit (replace a leaf with an equal-length payload): only
    // that component's bytes are written; DIRM stays untouched on disk.
    // Same-size scenario needs an emit-stable base: navm_fgbz.djvu itself
    // lacks the final IFF pad byte (odd root FORM length), which
    // `iff::emit` normalizes (+1 byte). Use the normalized bytes from the
    // bookmark edit above as the on-disk original.
    let original = expected;
    let doc0 = DjVuDocumentMut::from_bytes(&original).unwrap();
    // Find a page leaf to overwrite with same-length data: page 0's INFO.
    let info_path = (0..doc0.root_child_count())
        .find_map(|i| match doc0.chunk_at_path(&[i]) {
            Ok(Chunk::Form {
                secondary_id: [b'D', b'J', b'V', b'U'],
                children,
                ..
            }) => children.iter().enumerate().find_map(|(j, c)| match c {
                Chunk::Leaf {
                    id: [b'I', b'N', b'F', b'O'],
                    ..
                } => Some(vec![i, j]),
                _ => None,
            }),
            _ => None,
        })
        .expect("bundle has a page with INFO");
    let mut new_info = doc0.chunk_at_path(&info_path).unwrap().data().to_vec();
    // Flip the gamma byte (offset 7 = 10*gamma) — same length, real edit.
    new_info[7] ^= 1;
    let mut doc = doc0.clone();
    doc.replace_leaf(&info_path, new_info.clone()).unwrap();
    let expected = {
        let mut clone = doc0.clone();
        clone.replace_leaf(&info_path, new_info).unwrap();
        clone.try_into_bytes().unwrap()
    };
    assert_eq!(expected.len(), original.len(), "edit must be same-size");
    std::fs::write(tmp.path(), &original).unwrap();
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(tmp.path())
        .unwrap();
    let stats = doc.save_patched(&mut f).unwrap();
    assert_eq!(std::fs::read(tmp.path()).unwrap(), expected);
    assert!(
        stats.bytes_written <= 64,
        "same-size single-byte edit must write only the edited span, wrote {}",
        stats.bytes_written
    );

    // Wrong target: refuse before writing anything.
    let doc = DjVuDocumentMut::from_bytes(&original).unwrap();
    std::fs::write(tmp.path(), b"not the original").unwrap();
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(tmp.path())
        .unwrap();
    assert!(matches!(
        doc.save_patched(&mut f),
        Err(MutError::PatchTargetMismatch)
    ));
    assert_eq!(std::fs::read(tmp.path()).unwrap(), b"not the original");
}

/// Round-trip without edits is byte-identical on a single-page document.
#[test]
fn roundtrip_byte_identical_chicken() {
    let original = read_corpus("chicken.djvu");
    let doc = DjVuDocumentMut::from_bytes(&original).unwrap();
    assert!(!doc.is_dirty());
    assert_eq!(doc.into_bytes(), original);
}

/// Round-trip without edits is byte-identical on a bilevel JB2 document.
#[test]
fn roundtrip_byte_identical_boy_jb2() {
    let original = read_corpus("boy_jb2.djvu");
    let doc = DjVuDocumentMut::from_bytes(&original).unwrap();
    assert_eq!(doc.into_bytes(), original);
}

/// Round-trip without edits is byte-identical on a multi-page DJVM bundle.
#[test]
fn roundtrip_byte_identical_djvm_bundle() {
    let original = read_corpus("DjVu3Spec_bundled.djvu");
    let doc = DjVuDocumentMut::from_bytes(&original).unwrap();
    assert_eq!(doc.root_form_type(), Some(b"DJVM"));
    assert_eq!(doc.into_bytes(), original);
}

/// Round-trip without edits is byte-identical on a navm/fgbz document.
#[test]
fn roundtrip_byte_identical_navm() {
    let original = read_corpus("navm_fgbz.djvu");
    let doc = DjVuDocumentMut::from_bytes(&original).unwrap();
    assert_eq!(doc.into_bytes(), original);
}

/// `replace_leaf` mutates in place and the serialised output reflects it.
#[test]
fn replace_leaf_changes_emitted_bytes() {
    let original = read_corpus("chicken.djvu");
    let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();

    // Walk to the first leaf — for chicken.djvu (FORM:DJVU) this is INFO.
    let first = doc.chunk_at_path(&[0]).unwrap();
    let original_first_data = first.data().to_vec();
    assert!(!original_first_data.is_empty());

    // Replace with a marker and serialise.
    let marker = b"PR1_TEST_MARKER".to_vec();
    doc.replace_leaf(&[0], marker.clone()).unwrap();
    assert!(doc.is_dirty());

    let edited = doc.into_bytes();

    // Re-parse the edited bytes and confirm the leaf payload changed.
    let reparsed = DjVuDocumentMut::from_bytes(&edited).unwrap();
    let new_first = reparsed.chunk_at_path(&[0]).unwrap();
    assert_eq!(new_first.data(), marker.as_slice());
}

#[test]
fn single_page_patch_preserves_unedited_child_bytes() {
    let original = read_corpus("chicken.djvu");
    let original_ranges =
        original_single_page_child_ranges(&original).expect("single-page child ranges");
    assert!(
        original_ranges.len() > 2,
        "fixture must have unrelated chunks to preserve"
    );

    let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
    doc.replace_leaf(&[0], b"PATCHED_INFO".to_vec()).unwrap();
    let edited = doc.try_into_bytes().unwrap();
    let edited_ranges = original_single_page_child_ranges(&edited).expect("edited child ranges");
    assert_eq!(edited_ranges.len(), original_ranges.len());

    for (idx, (before, after)) in original_ranges.iter().zip(edited_ranges.iter()).enumerate() {
        if idx == 0 {
            assert_ne!(
                &original[before.range.clone()],
                &edited[after.range.clone()]
            );
            continue;
        }
        assert_eq!(before.id, after.id);
        assert_eq!(
            &original[before.range.clone()],
            &edited[after.range.clone()],
            "unchanged child #{idx} must be copied byte-for-byte"
        );
    }
}

#[test]
fn single_page_patch_falls_back_for_bundled_djvm() {
    let original = read_corpus("DjVu3Spec_bundled.djvu");
    let doc = DjVuDocumentMut::from_bytes(&original).unwrap();
    assert!(
        emit_patched_single_page(&doc.file.root, &original).is_none(),
        "single-page patch path must decline bundled DJVM layouts"
    );
}

#[test]
fn replace_leaf_rejects_empty_path() {
    let original = read_corpus("chicken.djvu");
    let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
    let err = doc.replace_leaf(&[], vec![]).unwrap_err();
    assert!(matches!(err, MutError::EmptyPath));
}

#[test]
fn replace_leaf_rejects_out_of_range() {
    let original = read_corpus("chicken.djvu");
    let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
    let err = doc.replace_leaf(&[9999], vec![]).unwrap_err();
    assert!(matches!(err, MutError::PathOutOfRange { .. }));
}

#[test]
fn replace_leaf_rejects_traversing_leaf() {
    let original = read_corpus("chicken.djvu");
    let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
    // [0] is a leaf (INFO).  [0, 0] tries to descend past it.
    let err = doc.replace_leaf(&[0, 0], vec![]).unwrap_err();
    assert!(matches!(err, MutError::PathTraversesLeaf { .. }));
}

#[test]
fn replace_leaf_rejects_form_target() {
    // For a DJVM bundle, [N] for some N points at a FORM:DJVU page,
    // not a leaf.  Picking the last child of DjVu3Spec_bundled (which
    // is a page FORM) demonstrates NotALeaf.
    let original = read_corpus("DjVu3Spec_bundled.djvu");
    let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
    let last_idx = doc.root_child_count() - 1;
    let err = doc.replace_leaf(&[last_idx], vec![]).unwrap_err();
    assert!(matches!(err, MutError::NotALeaf));
}

#[test]
fn root_form_type_djvu_single_page() {
    let original = read_corpus("chicken.djvu");
    let doc = DjVuDocumentMut::from_bytes(&original).unwrap();
    assert_eq!(doc.root_form_type(), Some(b"DJVU"));
}

// ---- PR2 setters ------------------------------------------------------

#[test]
fn page_count_single_page_djvu_is_one() {
    let original = read_corpus("chicken.djvu");
    let doc = DjVuDocumentMut::from_bytes(&original).unwrap();
    assert_eq!(doc.page_count(), 1);
}

#[test]
fn page_count_djvm_bundle_counts_djvu_components_only() {
    let original = read_corpus("DjVu3Spec_bundled.djvu");
    let doc = DjVuDocumentMut::from_bytes(&original).unwrap();
    // The bundle has multiple FORM:DJVU pages; assert it's > 1 and matches
    // the count of DJVU children at the root.
    let direct: usize = doc
            .file
            .root
            .children()
            .iter()
            .filter(|c| {
                matches!(c, crate::iff::Chunk::Form { secondary_id, .. } if secondary_id == b"DJVU")
            })
            .count();
    assert!(direct >= 2, "expected multi-page bundle, got {direct}");
    assert_eq!(doc.page_count(), direct);
}

#[test]
fn page_mut_out_of_range_errors() {
    let original = read_corpus("chicken.djvu");
    let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
    let err = doc.page_mut(1).err().unwrap();
    assert!(matches!(
        err,
        MutError::PageOutOfRange { index: 1, count: 1 }
    ));
}

#[test]
fn page_mut_djvm_bundle_succeeds_after_pr3() {
    // PR3 enables page_mut on bundled FORM:DJVM. Verify it returns a
    // valid handle for index 0 and rejects out-of-range indices.
    let original = read_corpus("DjVu3Spec_bundled.djvu");
    let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
    assert!(doc.page_mut(0).is_ok());
    let count = doc.page_count();
    let err = doc.page_mut(count).err().unwrap();
    assert!(matches!(err, MutError::PageOutOfRange { .. }));
}

#[test]
fn page_mut_indirect_djvm_returns_unsupported_before_range_check() {
    let mut doc = DjVuDocumentMut::from_bytes(&indirect_djvm_bytes()).unwrap();
    let err = doc.page_mut(0).err().unwrap();
    assert!(matches!(err, MutError::IndirectDjvmUnsupported));
}

pub(super) fn indirect_djvm_bytes() -> Vec<u8> {
    let bzz_meta: &[u8] = &[
        0xff, 0xff, 0xed, 0xbf, 0x8a, 0x1f, 0xbe, 0xad, 0x14, 0x57, 0x10, 0xc9, 0x63, 0x19, 0x11,
        0xf0, 0x85, 0x28, 0x12, 0x8a, 0xbf,
    ];

    let mut dirm_data = Vec::new();
    dirm_data.push(0x00);
    dirm_data.push(0x00);
    dirm_data.push(0x01);
    dirm_data.extend_from_slice(bzz_meta);

    // FORM:DJVM carrying a single (indirect) DIRM chunk, built through the
    // emission seam rather than hand-assembled framing.
    let dirm = Chunk::Leaf {
        id: *b"DIRM",
        data: dirm_data,
    };
    iff::partial_emit(*b"DJVM", &[iff::EmitPart::Chunk(&dirm)]).expect("fits within u32")
}

#[test]
fn set_text_layer_roundtrip_chicken() {
    use crate::text::{Rect, TextLayer, TextZone, TextZoneKind};

    let original = read_corpus("chicken.djvu");
    let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();

    let layer = TextLayer {
        text: "hello world".to_string(),
        zones: vec![TextZone {
            kind: TextZoneKind::Page,
            rect: Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 50,
            },
            text: "hello world".to_string(),
            children: vec![],
        }],
    };
    doc.page_mut(0).unwrap().set_text_layer(&layer).unwrap();
    assert!(doc.is_dirty());
    let edited = doc.into_bytes();

    // Re-parse and confirm a TXTz chunk now exists.
    let reparsed = DjVuDocumentMut::from_bytes(&edited).unwrap();
    let has_txtz = reparsed
        .file
        .root
        .children()
        .iter()
        .any(|c| matches!(c, Chunk::Leaf { id, .. } if id == b"TXTz"));
    assert!(
        has_txtz,
        "TXTz chunk should be present after set_text_layer"
    );
}

#[test]
fn set_annotations_roundtrip_chicken() {
    use crate::annotation::{Annotation, Color};

    let original = read_corpus("chicken.djvu");
    let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();

    let mut ann = Annotation::default();
    ann.background = Some(Color {
        r: 0xFF,
        g: 0xFF,
        b: 0xFF,
    });
    ann.mode = Some("color".to_string());
    doc.page_mut(0).unwrap().set_annotations(&ann, &[]);
    let edited = doc.into_bytes();

    let reparsed = DjVuDocumentMut::from_bytes(&edited).unwrap();
    let antz = reparsed
        .file
        .root
        .children()
        .iter()
        .find(|c| matches!(c, Chunk::Leaf { id, .. } if id == b"ANTz"));
    assert!(antz.is_some(), "ANTz should be inserted");
    let data = antz.unwrap().data();
    let decoded = crate::bzz::bzz_decode(data).expect("ANTz must decompress");
    let (parsed_ann, _areas) =
        crate::annotation::parse_annotations(&decoded).expect("ANTz must round-trip");
    assert_eq!(parsed_ann.mode.as_deref(), Some("color"));
    assert_eq!(
        parsed_ann.background,
        Some(Color {
            r: 0xFF,
            g: 0xFF,
            b: 0xFF
        })
    );
}

#[test]
fn set_metadata_roundtrip_chicken() {
    let original = read_corpus("chicken.djvu");
    let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();

    let mut meta = DjVuMetadata::default();
    meta.title = Some("Test Title".into());
    meta.author = Some("Tester".into());
    doc.page_mut(0).unwrap().set_metadata(&meta).unwrap();
    let edited = doc.into_bytes();

    let reparsed = DjVuDocumentMut::from_bytes(&edited).unwrap();
    let antz = reparsed
        .file
        .root
        .children()
        .iter()
        .find(|c| matches!(c, Chunk::Leaf { id, .. } if id == b"ANTz"))
        .expect("ANTz should hold the metadata");
    let decoded = crate::bzz::bzz_decode(antz.data()).unwrap();
    let parsed = crate::metadata::parse_metadata(&decoded).unwrap();
    assert_eq!(parsed, meta);
    assert!(
        !reparsed
            .file
            .root
            .children()
            .iter()
            .any(|c| matches!(c, Chunk::Leaf { id, .. } if id == b"METz"))
    );
}

#[test]
fn set_metadata_empty_removes_existing_chunk() {
    let original = read_corpus("chicken.djvu");
    let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();

    // Insert one, then clear.
    let mut meta = DjVuMetadata::default();
    meta.title = Some("X".into());
    doc.page_mut(0).unwrap().set_metadata(&meta).unwrap();
    doc.page_mut(0)
        .unwrap()
        .set_metadata(&DjVuMetadata::default())
        .unwrap();

    let edited = doc.into_bytes();
    let reparsed = DjVuDocumentMut::from_bytes(&edited).unwrap();
    let any_meta = reparsed
        .file
        .root
        .children()
        .iter()
        .any(|c| matches!(c, Chunk::Leaf { id, .. } if id == b"METa" || id == b"METz"));
    assert!(!any_meta, "set_metadata(empty) should remove any METa/METz");
}

#[test]
fn set_metadata_replaces_existing_chunk_in_place() {
    let original = read_corpus("chicken.djvu");
    let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();

    let mut m1 = DjVuMetadata::default();
    m1.title = Some("First".into());
    doc.page_mut(0).unwrap().set_metadata(&m1).unwrap();

    let mut m2 = DjVuMetadata::default();
    m2.title = Some("Second".into());
    doc.page_mut(0).unwrap().set_metadata(&m2).unwrap();

    let edited = doc.into_bytes();
    let reparsed = DjVuDocumentMut::from_bytes(&edited).unwrap();
    let ids: Vec<[u8; 4]> = reparsed
        .file
        .root
        .children()
        .iter()
        .filter_map(|c| match c {
            Chunk::Leaf { id, .. } => Some(*id),
            _ => None,
        })
        .collect();
    let count = |want: &[u8; 4]| ids.iter().filter(|id| *id == want).count();
    assert_eq!(count(b"ANTa") + count(b"ANTz"), 1, "one annotation chunk");
    assert_eq!(count(b"METa") + count(b"METz"), 0);
    let meta = crate::djvu_document::DjVuDocument::parse(&edited)
        .unwrap()
        .metadata()
        .unwrap()
        .unwrap();
    assert_eq!(meta.title.as_deref(), Some("Second"));
}

// ---- PR3: bundled DJVM mutation + set_bookmarks -----------------------

/// Helper: parse the FORM:DJVM body, return the DIRM chunk's offset table
/// and the actual file offsets where each component FORM header sits.
pub(super) fn dirm_offsets_and_actual(data: &[u8]) -> (Vec<u32>, Vec<u32>) {
    // Parse top-level FORM
    let form = crate::iff::parse_form(data).expect("parse_form");
    assert_eq!(&form.form_type, b"DJVM");

    let dirm = form
        .chunks
        .iter()
        .find(|c| &c.id == b"DIRM")
        .expect("DIRM present");
    // Decode through the canonical owner instead of hand-parsing bytes.
    let payload = crate::dirm::DirmPayload::decode(dirm.data).expect("decode DIRM");
    let declared = payload.offsets;
    let nfiles = declared.len();

    // Walk the file to find each FORM child's absolute byte offset.
    // Layout: AT&T(4) FORM(4) length(4) DJVM(4) chunks…
    let mut actual = Vec::with_capacity(nfiles);
    let mut pos = 16usize;
    let body_end = 8 + u32::from_be_bytes([data[8], data[9], data[10], data[11]]) as usize;
    while pos < body_end {
        let id = &data[pos..pos + 4];
        let len = u32::from_be_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
            as usize;
        if id == b"FORM" {
            actual.push(pos as u32);
        }
        let mut next = pos + 8 + len;
        if next & 1 == 1 {
            next += 1;
        }
        pos = next;
    }
    (declared, actual)
}

#[test]
fn dirm_offsets_match_actual_after_no_edit() {
    // Sanity: even without edits, the recompute path agrees with the
    // original document layout on a real bundle.
    let original = read_corpus("DjVu3Spec_bundled.djvu");
    let (declared, actual) = dirm_offsets_and_actual(&original);
    assert_eq!(declared, actual);
}

#[test]
fn dirm_offsets_recomputed_after_page_metadata_edit() {
    let original = read_corpus("DjVu3Spec_bundled.djvu");
    let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();

    // Edit page 0's metadata so the page FORM grows.
    let mut meta = DjVuMetadata::default();
    meta.title = Some("PR3 DJVM bundled mutation".into());
    meta.author = Some("djvu-rs PR3 tests".into());
    doc.page_mut(0).unwrap().set_metadata(&meta).unwrap();
    assert!(doc.is_dirty());

    let edited = doc.into_bytes();
    // Sizes must have changed (metadata chunk was inserted).
    assert_ne!(edited.len(), original.len());

    // DIRM offsets in the new bytes must match where the FORM headers
    // actually live.
    let (declared, actual) = dirm_offsets_and_actual(&edited);
    assert_eq!(
        declared, actual,
        "DIRM offsets must point at the new FORM positions after edit"
    );

    // The full document must still parse via DjVuDocument and expose the
    // expected page count.
    let reparsed =
        crate::djvu_document::DjVuDocument::parse(&edited).expect("edited bundle must parse");
    let original_doc =
        crate::djvu_document::DjVuDocument::parse(&original).expect("original bundle parses");
    assert_eq!(reparsed.page_count(), original_doc.page_count());
}

/// Helper: the DIRM metadata size of each component and its actual
/// `FORM` header plus declared length.
pub(super) fn dirm_sizes_and_actual(data: &[u8]) -> (Vec<u32>, Vec<u32>) {
    let form = crate::iff::parse_form(data).expect("parse_form");
    let dirm = form
        .chunks
        .iter()
        .find(|c| &c.id == b"DIRM")
        .expect("DIRM present");
    let payload = crate::dirm::DirmPayload::decode(dirm.data).expect("decode DIRM");
    let declared = payload.components().iter().map(|c| c.size).collect();
    let actual = payload
        .offsets
        .iter()
        .map(|&off| {
            let o = off as usize;
            u32::from_be_bytes([data[o + 4], data[o + 5], data[o + 6], data[o + 7]]) + 8
        })
        .collect();
    (declared, actual)
}

#[test]
fn dirm_sizes_recomputed_after_page_edit() {
    // DjVuLibre reads a bundled component by offset and metadata size; a
    // stale size makes it read a truncated page ("Unexpected End Of File").
    let original = read_corpus("DjVu3Spec_bundled.djvu");
    let (declared, actual) = dirm_sizes_and_actual(&original);
    assert_eq!(declared, actual, "fixture sizes are exact");

    let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
    let mut meta = DjVuMetadata::default();
    meta.title = Some("grow page 1".into());
    doc.page_mut(1).unwrap().set_metadata(&meta).unwrap();
    let edited = doc.into_bytes();

    let (declared, actual) = dirm_sizes_and_actual(&edited);
    assert_eq!(declared, actual, "DIRM sizes must follow the edited FORMs");
    let (orig_declared, _) = dirm_sizes_and_actual(&original);
    let changed: Vec<usize> = (0..declared.len())
        .filter(|&i| declared[i] != orig_declared[i])
        .collect();
    assert_eq!(changed.len(), 1, "only the edited page's size changes");
}

#[test]
fn dirm_offsets_recomputed_after_middle_page_edit() {
    // Editing a non-first page must shift only the trailing offsets.
    let original = read_corpus("DjVu3Spec_bundled.djvu");
    let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
    let count = doc.page_count();
    assert!(count >= 3);

    let mid = count / 2;
    let mut meta = DjVuMetadata::default();
    meta.title = Some("PR3 mid-page edit".into());
    doc.page_mut(mid).unwrap().set_metadata(&meta).unwrap();

    let edited = doc.into_bytes();
    let (declared, actual) = dirm_offsets_and_actual(&edited);
    assert_eq!(declared, actual);

    // Pages before `mid` move only by the DIRM length change (the size
    // table is re-encoded), so they all shift by the same amount.
    let (orig_declared, _) = dirm_offsets_and_actual(&original);
    let shift = i64::from(declared[0]) - i64::from(orig_declared[0]);
    for i in 0..mid {
        assert_eq!(
            i64::from(declared[i]) - i64::from(orig_declared[i]),
            shift,
            "offset for page {i} (before edit) must shift only with DIRM"
        );
    }
}

#[test]
fn set_bookmarks_replaces_navm_in_bundle() {
    use crate::djvu_document::DjVuBookmark;

    let original = read_corpus("DjVu3Spec_bundled.djvu");
    let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();

    let bookmarks = vec![
        DjVuBookmark {
            title: "Front matter".into(),
            url: "#1".into(),
            children: vec![DjVuBookmark {
                title: "Acknowledgments".into(),
                url: "#3".into(),
                children: vec![],
            }],
        },
        DjVuBookmark {
            title: "Body".into(),
            url: "#10".into(),
            children: vec![],
        },
    ];
    doc.set_bookmarks(&bookmarks).unwrap();
    assert!(doc.is_dirty());
    let edited = doc.into_bytes();

    // DIRM offsets must still be correct after the NAVM size change.
    let (declared, actual) = dirm_offsets_and_actual(&edited);
    assert_eq!(declared, actual);

    // Round-trip the bookmarks via the high-level DjVuDocument parser.
    let reparsed = crate::djvu_document::DjVuDocument::parse(&edited)
        .expect("bundle with new bookmarks parses");
    let parsed_bms = reparsed.bookmarks();
    assert_eq!(parsed_bms.len(), 2);
    assert_eq!(parsed_bms[0].title, "Front matter");
    assert_eq!(parsed_bms[0].children.len(), 1);
    assert_eq!(parsed_bms[0].children[0].title, "Acknowledgments");
    assert_eq!(parsed_bms[1].title, "Body");
}

#[test]
fn set_bookmarks_empty_removes_navm() {
    let original = read_corpus("DjVu3Spec_bundled.djvu");
    let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
    // The fixture might or might not have NAVM; either way, calling with
    // an empty slice should result in no NAVM in the output.
    doc.set_bookmarks(&[]).unwrap();
    let edited = doc.into_bytes();

    let form = crate::iff::parse_form(&edited).unwrap();
    let has_navm = form.chunks.iter().any(|c| &c.id == b"NAVM");
    assert!(!has_navm, "set_bookmarks(&[]) must remove NAVM");

    // DIRM offsets still match.
    let (declared, actual) = dirm_offsets_and_actual(&edited);
    assert_eq!(declared, actual);
}

#[test]
fn set_bookmarks_inserts_navm_when_absent() {
    use crate::djvu_document::DjVuBookmark;

    // Build a bundle that has no NAVM by first stripping it, then
    // re-add bookmarks via set_bookmarks.
    let original = read_corpus("DjVu3Spec_bundled.djvu");
    let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
    doc.set_bookmarks(&[]).unwrap();
    let stripped = doc.into_bytes();

    let mut doc = DjVuDocumentMut::from_bytes(&stripped).unwrap();
    let bms = vec![DjVuBookmark {
        title: "Re-added".into(),
        url: "#1".into(),
        children: vec![],
    }];
    doc.set_bookmarks(&bms).unwrap();
    let edited = doc.into_bytes();

    let form = crate::iff::parse_form(&edited).unwrap();
    let navm_pos = form
        .chunks
        .iter()
        .position(|c| &c.id == b"NAVM")
        .expect("NAVM should be inserted");
    let dirm_pos = form.chunks.iter().position(|c| &c.id == b"DIRM").unwrap();
    assert_eq!(
        navm_pos,
        dirm_pos + 1,
        "NAVM should be placed immediately after DIRM"
    );

    let (declared, actual) = dirm_offsets_and_actual(&edited);
    assert_eq!(declared, actual);
}

/// DjVuLibre reads `NAVM` only when it directly follows `DIRM`, so
/// document-level metadata must never land between them, in either order.
#[test]
fn metadata_and_bookmarks_keep_navm_after_dirm() {
    use crate::djvu_document::DjVuBookmark;
    use crate::metadata::DjVuMetadata;

    let bookmarks = vec![DjVuBookmark {
        title: "Start".into(),
        url: "#1".into(),
        children: vec![],
    }];
    let metadata = DjVuMetadata {
        title: Some("Atlas".into()),
        ..DjVuMetadata::default()
    };
    let original = read_corpus("navm_fgbz.djvu");

    let chunk_ids = |bytes: &[u8]| -> Vec<[u8; 4]> {
        crate::iff::parse_form(bytes)
            .unwrap()
            .chunks
            .iter()
            .map(|c| c.id)
            .collect()
    };

    for metadata_first in [true, false] {
        let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
        if metadata_first {
            doc.set_metadata(&metadata).unwrap();
            doc.set_bookmarks(&bookmarks).unwrap();
        } else {
            doc.set_bookmarks(&bookmarks).unwrap();
            doc.set_metadata(&metadata).unwrap();
        }
        let edited = doc.into_bytes();

        let ids = chunk_ids(&edited);
        assert_eq!(
            &ids[..2],
            &[*b"DIRM", *b"NAVM"],
            "metadata_first={metadata_first}"
        );
        assert!(
            !ids.contains(b"METz"),
            "metadata goes to the shared annotation"
        );

        let (declared, actual) = dirm_offsets_and_actual(&edited);
        assert_eq!(declared, actual);
        let reparsed = crate::djvu_document::DjVuDocument::parse(&edited).unwrap();
        assert_eq!(reparsed.bookmarks()[0].title, "Start");
        let meta = reparsed.metadata().unwrap().unwrap();
        assert_eq!(meta.title.as_deref(), Some("Atlas"));
    }

    // Earlier versions wrote DIRM, METz, NAVM; the next edit repairs it.
    let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
    let Chunk::Form { children, .. } = &mut doc.file.root else {
        unreachable!()
    };
    children.insert(
        1,
        Chunk::Leaf {
            id: *b"METz",
            data: crate::metadata::encode_metadata_bzz(&metadata),
        },
    );
    doc.dirty = true;
    let misplaced = doc.into_bytes();
    assert_eq!(&chunk_ids(&misplaced)[..3], &[*b"DIRM", *b"METz", *b"NAVM"]);

    let mut doc = DjVuDocumentMut::from_bytes(&misplaced).unwrap();
    doc.set_bookmarks(&bookmarks).unwrap();
    let repaired = doc.into_bytes();
    assert_eq!(&chunk_ids(&repaired)[..3], &[*b"DIRM", *b"NAVM", *b"METz"]);
    let (declared, actual) = dirm_offsets_and_actual(&repaired);
    assert_eq!(declared, actual);
}

pub(super) fn component_kinds(bytes: &[u8]) -> Vec<DirmComponentKind> {
    let form = crate::iff::parse_form(bytes).unwrap();
    let dirm = form.chunks.iter().find(|c| &c.id == b"DIRM").unwrap();
    DirmPayload::decode(dirm.data)
        .unwrap()
        .components()
        .into_iter()
        .map(|c| c.kind)
        .collect()
}

pub(super) fn atlas() -> DjVuMetadata {
    DjVuMetadata {
        title: Some("Atlas".into()),
        author: Some("Me".into()),
        ..DjVuMetadata::default()
    }
}

/// Without a shared annotation, document metadata adds one the way
/// `djvused set-meta` does, and leaves the pages' own annotations alone.
#[test]
fn document_metadata_adds_a_shared_annotation() {
    let original = read_corpus("navm_fgbz.djvu");
    assert!(!component_kinds(&original).contains(&DirmComponentKind::SharedAnno));
    let before = crate::djvu_document::DjVuDocument::parse(&original).unwrap();

    let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
    doc.set_metadata(&atlas()).unwrap();
    let edited = doc.into_bytes();

    let kinds = component_kinds(&edited);
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == DirmComponentKind::SharedAnno)
            .count(),
        1
    );
    let (declared, actual) = dirm_offsets_and_actual(&edited);
    assert_eq!(declared, actual);

    let after = crate::djvu_document::DjVuDocument::parse(&edited).unwrap();
    assert_eq!(after.metadata().unwrap(), Some(atlas()));
    assert_eq!(after.page_count(), before.page_count());
    for i in 0..after.page_count() {
        let page = after.page(i).unwrap();
        assert_eq!(
            format!("{:?}", page.annotations().unwrap()),
            format!("{:?}", before.page(i).unwrap().annotations().unwrap())
        );
        assert!(page.raw_chunk(b"INCL").is_some(), "page {i} includes it");
    }

    // A second edit reuses the component instead of adding another.
    let mut doc = DjVuDocumentMut::from_bytes(&edited).unwrap();
    doc.set_metadata(&DjVuMetadata {
        title: Some("Second".into()),
        ..DjVuMetadata::default()
    })
    .unwrap();
    let again = doc.into_bytes();
    assert_eq!(component_kinds(&again).len(), kinds.len());
    let meta = crate::djvu_document::DjVuDocument::parse(&again)
        .unwrap()
        .metadata()
        .unwrap()
        .unwrap();
    assert_eq!((meta.title.as_deref(), meta.author), (Some("Second"), None));

    let mut doc = DjVuDocumentMut::from_bytes(&again).unwrap();
    doc.remove_metadata().unwrap();
    let removed = doc.into_bytes();
    let reparsed = crate::djvu_document::DjVuDocument::parse(&removed).unwrap();
    assert_eq!(reparsed.metadata().unwrap(), None);
}

/// An existing shared annotation keeps its other forms; only the
/// `(metadata …)` block changes.
#[test]
fn document_metadata_keeps_shared_annotation_forms() {
    let original = read_corpus("czech.djvu");
    let kinds = component_kinds(&original);
    assert!(kinds.contains(&DirmComponentKind::SharedAnno));

    let shared_forms = |bytes: &[u8]| {
        let doc = DjVuDocumentMut::from_bytes(bytes).unwrap();
        let child = shared_anno_child(&doc.file.root).unwrap().unwrap();
        let data = doc.file.root.children()[child]
            .children()
            .iter()
            .find(|c| matches!(c, Chunk::Leaf { id, .. } if id == b"ANTz"))
            .map(|c| crate::bzz::bzz_decode(c.data()).unwrap())
            .unwrap();
        let (annotation, areas) = parse_annotations(&data).unwrap();
        let kept: Vec<String> = annotation
            .extra
            .iter()
            .filter(|f| !is_metadata_form(f))
            .cloned()
            .collect();
        format!(
            "{:?}",
            (
                annotation.background,
                annotation.zoom,
                annotation.mode,
                kept,
                areas
            )
        )
    };

    let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
    doc.set_metadata(&atlas()).unwrap();
    let edited = doc.into_bytes();

    assert_eq!(component_kinds(&edited), kinds);
    assert_eq!(shared_forms(&edited), shared_forms(&original));
    let meta = crate::djvu_document::DjVuDocument::parse(&edited)
        .unwrap()
        .metadata()
        .unwrap();
    assert_eq!(meta, Some(atlas()));
}

/// Root METa/METz from earlier versions would shadow the new value.
#[test]
fn document_metadata_drops_root_metz() {
    let original = read_corpus("navm_fgbz.djvu");
    let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
    let Chunk::Form { children, .. } = &mut doc.file.root else {
        unreachable!()
    };
    children.insert(
        2,
        Chunk::Leaf {
            id: *b"METz",
            data: crate::metadata::encode_metadata_bzz(&atlas()),
        },
    );
    doc.dirty = true;
    doc.set_metadata(&DjVuMetadata {
        title: Some("New".into()),
        ..DjVuMetadata::default()
    })
    .unwrap();
    let edited = doc.into_bytes();
    let form = crate::iff::parse_form(&edited).unwrap();
    assert!(!form.chunks.iter().any(|c| &c.id == b"METz"));
    let meta = crate::djvu_document::DjVuDocument::parse(&edited)
        .unwrap()
        .metadata()
        .unwrap()
        .unwrap();
    assert_eq!(meta.title.as_deref(), Some("New"));
}

/// A single-page file keeps metadata in its own ANTz, next to its links.
#[test]
fn single_page_metadata_joins_page_annotations() {
    let original = read_corpus("boy.djvu");
    let area = MapArea {
        url: "https://example.org".into(),
        target: None,
        description: String::new(),
        shape: crate::annotation::Shape::Rect(crate::annotation::Rect {
            x: 1,
            y: 2,
            width: 30,
            height: 40,
        }),
        border: None,
        highlight: None,
        extra: Vec::new(),
    };
    let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
    doc.page_mut(0)
        .unwrap()
        .set_annotations(&Annotation::default(), core::slice::from_ref(&area));
    doc.set_metadata(&atlas()).unwrap();
    let edited = doc.into_bytes();

    let form = crate::iff::parse_form(&edited).unwrap();
    assert!(!form.chunks.iter().any(|c| &c.id == b"METz"));
    let reparsed = crate::djvu_document::DjVuDocument::parse(&edited).unwrap();
    assert_eq!(reparsed.metadata().unwrap(), Some(atlas()));
    let (_, areas) = reparsed.page(0).unwrap().annotations().unwrap().unwrap();
    assert_eq!(format!("{areas:?}"), format!("{:?}", [area]));

    let mut doc = DjVuDocumentMut::from_bytes(&edited).unwrap();
    doc.page_mut(0).unwrap().remove_metadata().unwrap();
    let removed = doc.into_bytes();
    let reparsed = crate::djvu_document::DjVuDocument::parse(&removed).unwrap();
    assert_eq!(reparsed.metadata().unwrap(), None);
    assert_eq!(
        reparsed
            .page(0)
            .unwrap()
            .annotations()
            .unwrap()
            .unwrap()
            .1
            .len(),
        1
    );
}

/// Page annotations and metadata share one chunk; replacing or removing
/// the annotations keeps the metadata.
#[test]
fn annotation_edits_keep_page_metadata() {
    let original = read_corpus("boy.djvu");
    let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
    doc.set_metadata(&atlas()).unwrap();
    let zoomed = Annotation {
        zoom: Some(150),
        ..Annotation::default()
    };
    doc.page_mut(0).unwrap().set_annotations(&zoomed, &[]);
    let edited = doc.into_bytes();
    let reparsed = crate::djvu_document::DjVuDocument::parse(&edited).unwrap();
    assert_eq!(reparsed.metadata().unwrap(), Some(atlas()));
    let (annotation, _) = reparsed.page(0).unwrap().annotations().unwrap().unwrap();
    assert_eq!(annotation.zoom, Some(150));

    let mut doc = DjVuDocumentMut::from_bytes(&edited).unwrap();
    doc.page_mut(0).unwrap().remove_annotations();
    let removed = doc.into_bytes();
    let reparsed = crate::djvu_document::DjVuDocument::parse(&removed).unwrap();
    assert_eq!(reparsed.metadata().unwrap(), Some(atlas()));
    let (annotation, areas) = reparsed.page(0).unwrap().annotations().unwrap().unwrap();
    assert_eq!((annotation.zoom, areas.len()), (None, 0));

    // An explicit block in `extra` replaces the stored one.
    let mut doc = DjVuDocumentMut::from_bytes(&removed).unwrap();
    let explicit = Annotation {
        extra: vec!["(metadata (title \"Given\"))".into()],
        ..Annotation::default()
    };
    doc.page_mut(0).unwrap().set_annotations(&explicit, &[]);
    let replaced = doc.into_bytes();
    let meta = crate::djvu_document::DjVuDocument::parse(&replaced)
        .unwrap()
        .metadata()
        .unwrap()
        .unwrap();
    assert_eq!((meta.title.as_deref(), meta.author), (Some("Given"), None));

    // Without metadata, removing annotations still removes the chunk.
    let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
    doc.page_mut(0).unwrap().set_annotations(&zoomed, &[]);
    doc.page_mut(0).unwrap().remove_annotations();
    let plain = doc.into_bytes();
    let form = crate::iff::parse_form(&plain).unwrap();
    assert!(!form.chunks.iter().any(|c| &c.id == b"ANTz"));
}

#[test]
fn metadata_form_detection() {
    assert!(is_metadata_form("(metadata (title \"x\"))"));
    assert!(is_metadata_form("  ( METADATA\n)"));
    assert!(!is_metadata_form("(metadatax)"));
    assert!(!is_metadata_form("(zoom page)"));
    assert!(!is_metadata_form("metadata"));
}

#[test]
fn set_bookmarks_on_single_page_djvu_errors() {
    let original = read_corpus("chicken.djvu");
    let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
    let err = doc.set_bookmarks(&[]).err().unwrap();
    assert!(matches!(err, MutError::BookmarksRequireDjvm));
}

#[test]
fn page_mut_djvm_text_layer_roundtrip() {
    use crate::text::{Rect, TextLayer, TextZone, TextZoneKind};

    let original = read_corpus("DjVu3Spec_bundled.djvu");
    let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
    let layer = TextLayer {
        text: "djvm page-3 text".into(),
        zones: vec![TextZone {
            kind: TextZoneKind::Page,
            rect: Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 50,
            },
            text: "djvm page-3 text".into(),
            children: vec![],
        }],
    };
    doc.page_mut(2).unwrap().set_text_layer(&layer).unwrap();
    let edited = doc.into_bytes();

    let (declared, actual) = dirm_offsets_and_actual(&edited);
    assert_eq!(declared, actual);

    // Re-open and confirm the targeted page now has a TXTz chunk.
    let reparsed = DjVuDocumentMut::from_bytes(&edited).unwrap();
    // The third FORM:DJVU child should have a TXTz leaf.
    let mut djvu_seen = 0usize;
    let mut found_txtz = false;
    for child in reparsed.file.root.children() {
        if let Chunk::Form {
            secondary_id,
            children,
            ..
        } = child
            && secondary_id == b"DJVU"
        {
            if djvu_seen == 2 {
                found_txtz = children
                    .iter()
                    .any(|c| matches!(c, Chunk::Leaf { id, .. } if id == b"TXTz"));
                break;
            }
            djvu_seen += 1;
        }
    }
    assert!(
        found_txtz,
        "TXTz chunk should be present on page 2 after set_text_layer"
    );
}

/// PR4 of #222: editing one page in a bundled DJVM must leave every
/// other page's bytes unchanged. The mutated page itself may grow
/// (e.g. a new METz chunk), but unmutated FORM:DJVU/DJVI components
/// must round-trip byte-identical.
#[test]
fn unmutated_pages_byte_identical_after_metadata_edit() {
    use crate::metadata::DjVuMetadata;

    let original = read_corpus("DjVu3Spec_bundled.djvu");

    let orig_ranges = top_form_ranges(&original);

    let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
    let meta = DjVuMetadata {
        title: Some("PR4 byte-identical probe".into()),
        ..Default::default()
    };
    doc.page_mut(0).unwrap().set_metadata(&meta).unwrap();
    let edited = doc.into_bytes();

    let edited_ranges = top_form_ranges(&edited);
    assert_eq!(orig_ranges.len(), edited_ranges.len());

    // The first FORM:DJVU child corresponds to page 0 (the one we edited);
    // it is allowed to differ. All others must be byte-identical.
    let mut djvu_idx = 0usize;
    for (i, (or, er)) in orig_ranges.iter().zip(edited_ranges.iter()).enumerate() {
        // Only enforce identity on FORM:DJVU/DJVI components — bare leaves
        // (DIRM, NAVM) legitimately change when offsets shift.
        let is_form_djvu = &original[or.start..or.start + 4] == b"FORM"
            && (&original[or.start + 8..or.start + 12] == b"DJVU"
                || &original[or.start + 8..or.start + 12] == b"DJVI");
        if !is_form_djvu {
            continue;
        }
        let is_edited_page = djvu_idx == 0;
        djvu_idx += 1;
        if is_edited_page {
            continue;
        }
        assert_eq!(
            &original[or.clone()],
            &edited[er.clone()],
            "FORM at top-level child #{i} must be byte-identical after edit"
        );
    }
}

// ---- #325: resolver-backed indirect DJVM rebundling -------------------

/// Build an indirect FORM:DJVM index over `page_names` and a resolver that
/// serves each named fixture from `tests/fixtures`.
pub(super) fn indirect_over_fixtures(
    page_names: &[&str],
) -> (
    Vec<u8>,
    impl Fn(&str) -> Result<Vec<u8>, std::io::Error> + use<>,
) {
    let index = crate::djvm::create_indirect(page_names).expect("create_indirect");
    // Snapshot the fixture bytes keyed by name so the resolver is owned.
    let map: std::collections::HashMap<String, Vec<u8>> = page_names
        .iter()
        .map(|n| (n.to_string(), read_corpus(n)))
        .collect();
    let resolver = move |name: &str| -> Result<Vec<u8>, std::io::Error> {
        map.get(name)
            .cloned()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no such component"))
    };
    (index, resolver)
}

#[test]
fn from_indirect_resolved_rebundles_single_page() {
    let (index, resolver) = indirect_over_fixtures(&["chicken.djvu"]);
    let doc = DjVuDocumentMut::from_indirect_resolved(&index, resolver).unwrap();
    assert_eq!(doc.root_form_type(), Some(b"DJVM"));
    assert_eq!(doc.page_count(), 1);
    assert!(!doc.is_dirty());

    // Output must parse as a bundled DJVM without any resolver.
    let bundled = doc.try_into_bytes().unwrap();
    let reparsed =
        crate::djvu_document::DjVuDocument::parse(&bundled).expect("bundled output parses");
    assert_eq!(reparsed.page_count(), 1);
    // The single page's pixel dimensions come from the resolved chicken.djvu.
    assert_eq!(reparsed.page(0).unwrap().width(), 181);
    assert_eq!(reparsed.page(0).unwrap().height(), 240);

    // DIRM offsets must point at the actual component FORM positions.
    let (declared, actual) = dirm_offsets_and_actual(&bundled);
    assert_eq!(declared, actual);
}

#[test]
fn from_indirect_resolved_multi_page_preserves_order() {
    let (index, resolver) = indirect_over_fixtures(&["chicken.djvu", "irish.djvu"]);
    let doc = DjVuDocumentMut::from_indirect_resolved(&index, resolver).unwrap();
    assert_eq!(doc.page_count(), 2);
    let bundled = doc.try_into_bytes().unwrap();

    let reparsed = crate::djvu_document::DjVuDocument::parse(&bundled).expect("parses");
    assert_eq!(reparsed.page_count(), 2);
    // Page 0 == chicken (181x240), page 1 == irish (different size).
    assert_eq!(reparsed.page(0).unwrap().width(), 181);
    let irish_doc = crate::djvu_document::DjVuDocument::parse(&read_corpus("irish.djvu"))
        .expect("irish parses standalone");
    assert_eq!(
        reparsed.page(1).unwrap().dimensions(),
        irish_doc.page(0).unwrap().dimensions()
    );

    let (declared, actual) = dirm_offsets_and_actual(&bundled);
    assert_eq!(declared, actual);
}

#[test]
fn from_indirect_resolved_then_metadata_edit_roundtrips() {
    let (index, resolver) = indirect_over_fixtures(&["chicken.djvu", "irish.djvu"]);
    let mut doc = DjVuDocumentMut::from_indirect_resolved(&index, resolver).unwrap();

    let meta = DjVuMetadata {
        title: Some("rebundled indirect".into()),
        author: Some("djvu-rs #325".into()),
        ..Default::default()
    };
    doc.page_mut(1).unwrap().set_metadata(&meta).unwrap();
    assert!(doc.is_dirty());
    let edited = doc.into_bytes();

    // Offsets stay consistent after the page-1 metadata grows.
    let (declared, actual) = dirm_offsets_and_actual(&edited);
    assert_eq!(declared, actual);

    // Metadata round-trips through the high-level parser on the edited page.
    let reparsed = DjVuDocumentMut::from_bytes(&edited).unwrap();
    let mut djvu_seen = 0usize;
    let mut found = None;
    for child in reparsed.file.root.children() {
        if let Chunk::Form {
            secondary_id,
            children,
            ..
        } = child
            && secondary_id == b"DJVU"
        {
            if djvu_seen == 1 {
                found = children
                    .iter()
                    .find(|c| matches!(c, Chunk::Leaf { id, .. } if id == b"ANTz"))
                    .map(|c| c.data().to_vec());
                break;
            }
            djvu_seen += 1;
        }
    }
    let antz = found.expect("page 1 should have ANTz after edit");
    let decoded = crate::bzz::bzz_decode(&antz).unwrap();
    let parsed = crate::metadata::parse_metadata(&decoded).unwrap();
    assert_eq!(parsed.title.as_deref(), Some("rebundled indirect"));
}

#[test]
fn from_indirect_resolved_then_text_layer_edit() {
    use crate::text::{Rect, TextLayer, TextZone, TextZoneKind};

    let (index, resolver) = indirect_over_fixtures(&["chicken.djvu"]);
    let mut doc = DjVuDocumentMut::from_indirect_resolved(&index, resolver).unwrap();
    let layer = TextLayer {
        text: "rebundled text".into(),
        zones: vec![TextZone {
            kind: TextZoneKind::Page,
            rect: Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 50,
            },
            text: "rebundled text".into(),
            children: vec![],
        }],
    };
    doc.page_mut(0).unwrap().set_text_layer(&layer).unwrap();
    let edited = doc.into_bytes();

    let reparsed = crate::djvu_document::DjVuDocument::parse(&edited).expect("parses");
    let text = reparsed.page(0).unwrap().text_layer().unwrap();
    assert!(text.is_some(), "edited page should expose a text layer");
    assert_eq!(text.unwrap().text, "rebundled text");
}

#[test]
fn from_indirect_resolved_missing_component_errors() {
    // Resolver that never produces bytes ⇒ ComponentResolve.
    let index = crate::djvm::create_indirect(&["missing.djvu"]).expect("create_indirect");
    let err = DjVuDocumentMut::from_indirect_resolved(&index, |_name: &str| {
        Err::<Vec<u8>, _>(std::io::Error::new(std::io::ErrorKind::NotFound, "nope"))
    })
    .unwrap_err();
    match err {
        MutError::ComponentResolve { name } => assert_eq!(name, "missing.djvu"),
        other => panic!("expected ComponentResolve, got {other:?}"),
    }
}

#[test]
fn from_indirect_resolved_malformed_component_errors() {
    let index = crate::djvm::create_indirect(&["garbage.djvu"]).expect("create_indirect");
    let err = DjVuDocumentMut::from_indirect_resolved(&index, |_name: &str| {
        Ok::<Vec<u8>, std::io::Error>(b"not an iff document".to_vec())
    })
    .unwrap_err();
    assert!(
        matches!(err, MutError::ComponentMalformed { .. }),
        "{err:?}"
    );
}

// Lines 293-298: DjVuDocumentMut::from_indirect_resolved with FORM:FAKE component.
#[test]
fn from_indirect_resolved_wrong_form_type_errors() {
    let index = crate::djvm::create_indirect(&["fake.djvu"]).expect("create_indirect");
    let fake = iff::emit(&DjvuFile {
        root: Chunk::Form {
            secondary_id: *b"FAKE",
            length: 0,
            children: vec![],
        },
    });
    let err = DjVuDocumentMut::from_indirect_resolved(&index, move |_name: &str| {
        Ok::<Vec<u8>, std::io::Error>(fake.clone())
    })
    .unwrap_err();
    assert!(
        matches!(err, MutError::ComponentMalformed { .. }),
        "{err:?}"
    );
}

#[test]
fn from_indirect_resolved_rejects_bundled_input() {
    // A genuinely bundled DJVM is not indirect ⇒ NotIndirectDjvm.
    let bundled = read_corpus("DjVu3Spec_bundled.djvu");
    let err = DjVuDocumentMut::from_indirect_resolved(&bundled, |_n: &str| {
        Ok::<Vec<u8>, std::io::Error>(Vec::new())
    })
    .unwrap_err();
    assert!(matches!(err, MutError::NotIndirectDjvm), "{err:?}");
}

#[test]
fn from_indirect_resolved_rejects_single_page_djvu() {
    let chicken = read_corpus("chicken.djvu");
    let err = DjVuDocumentMut::from_indirect_resolved(&chicken, |_n: &str| {
        Ok::<Vec<u8>, std::io::Error>(Vec::new())
    })
    .unwrap_err();
    assert!(matches!(err, MutError::NotIndirectDjvm), "{err:?}");
}

/// Indirect DJVM whose DIRM lists only Shared entries (no Page) fires
/// lines 274-275: DirmMalformed "indirect DIRM lists no page component".
#[test]
fn from_indirect_resolved_no_page_component_returns_dirm_malformed() {
    use crate::dirm::DirmPayload;
    // Build indirect DJVM with 1 Shared entry (flag=0x00)
    let dirm_payload = DirmPayload::build_indirect(&[DirmComponent::new(
        DirmComponentKind::Shared,
        "shared.djvi",
    )]);
    let dirm_chunk = iff::Chunk::Leaf {
        id: *b"DIRM",
        data: dirm_payload.encode(),
    };
    let index = iff::partial_emit(*b"DJVM", &[iff::EmitPart::Chunk(&dirm_chunk)]).expect("fits");

    let err = DjVuDocumentMut::from_indirect_resolved(&index, |_name: &str| {
        Ok::<Vec<u8>, std::io::Error>(Vec::new())
    })
    .unwrap_err();
    assert!(
        matches!(err, MutError::DirmMalformed(_)),
        "expected DirmMalformed, got {err:?}"
    );
}

#[test]
fn from_bytes_on_indirect_still_unsupported_for_page_mut() {
    // The plain entry point keeps the documented unsupported behavior.
    let index = crate::djvm::create_indirect(&["chicken.djvu"]).expect("create_indirect");
    let mut doc = DjVuDocumentMut::from_bytes(&index).unwrap();
    let err = doc.page_mut(0).err().unwrap();
    assert!(matches!(err, MutError::IndirectDjvmUnsupported), "{err:?}");
}

// ---- #326: explicit external-file rewrite plan ------------------------

/// A fresh, empty temp directory unique to `tag` (cleared if it exists).
pub(super) fn fresh_temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("djvu_rs_rewrite_{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn rewrite_plan_commits_full_set_to_dir() {
    let (index, resolver) = indirect_over_fixtures(&["chicken.djvu", "irish.djvu"]);
    let mut plan = IndirectRewritePlan::from_indirect_resolved(&index, resolver).unwrap();
    assert_eq!(plan.page_count(), 2);

    // Edit page 0's metadata in memory only.
    plan.edit_page(0, |doc| {
        let meta = DjVuMetadata {
            title: Some("rewrite path".into()),
            ..Default::default()
        };
        doc.page_mut(0)?.set_metadata(&meta).unwrap();
        Ok(())
    })
    .unwrap();

    // The preview marks page 0 changed, page 1 and root unchanged.
    let preview = plan.plan("index.djvu");
    assert_eq!(preview.len(), 3);
    assert_eq!(preview[0].name, "chicken.djvu");
    assert!(preview[0].changed, "edited page must show changed");
    assert_eq!(preview[1].name, "irish.djvu");
    assert!(!preview[1].changed, "untouched page must be unchanged");
    assert!(preview[2].is_root);
    assert!(!preview[2].changed, "root unchanged for a page-only edit");

    let dir = fresh_temp_dir("commit_full_set");
    let written = plan.commit_to_dir(&dir, "index.djvu").unwrap();
    assert_eq!(written.len(), 3);
    for p in &written {
        assert!(p.exists(), "committed file {p:?} must exist");
    }
    // No stray temp files left behind.
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "temp files must be renamed away");

    // The rewritten directory parses as an indirect document and the edit
    // landed on page 0.
    let index_bytes = std::fs::read(dir.join("index.djvu")).unwrap();
    let doc = crate::djvu_document::DjVuDocument::parse_from_dir(&index_bytes, &dir).unwrap();
    assert_eq!(doc.page_count(), 2);
    let meta_page0 = doc.page(0).unwrap();
    // metadata is read at the document level; confirm the edited component
    // round-trips through the single-page parser.
    let edited_comp = std::fs::read(dir.join("chicken.djvu")).unwrap();
    let reparsed = DjVuDocumentMut::from_bytes(&edited_comp).unwrap();
    let has_antz = reparsed
        .file
        .root
        .children()
        .iter()
        .any(|c| matches!(c, Chunk::Leaf { id, .. } if id == b"ANTz"));
    assert!(has_antz, "edited component file must contain ANTz");
    // The unedited component is byte-identical to the source fixture.
    let irish_src = read_corpus("irish.djvu");
    let irish_out = std::fs::read(dir.join("irish.djvu")).unwrap();
    assert_eq!(irish_out, irish_src, "unedited component copied verbatim");
    let _ = meta_page0;

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rewrite_plan_rejects_duplicate_dirm_names() {
    // Two DIRM entries with the same id ⇒ DuplicateComponentName.
    let index = crate::djvm::create_indirect(&["dup.djvu", "dup.djvu"]).expect("create");
    let err = IndirectRewritePlan::from_indirect_resolved(&index, |_n: &str| {
        Ok::<Vec<u8>, std::io::Error>(read_corpus("chicken.djvu"))
    })
    .unwrap_err();
    match err {
        MutError::DuplicateComponentName { name } => assert_eq!(name, "dup.djvu"),
        other => panic!("expected DuplicateComponentName, got {other:?}"),
    }
}

#[test]
fn rewrite_plan_rejects_unsafe_dirm_names() {
    for bad in [
        "../evil.djvu",
        "/abs.djvu",
        "sub/page.djvu",
        "..",
        "a:b.djvu",
    ] {
        let index = crate::djvm::create_indirect(&[bad]).expect("create");
        let err = IndirectRewritePlan::from_indirect_resolved(&index, |_n: &str| {
            Ok::<Vec<u8>, std::io::Error>(read_corpus("chicken.djvu"))
        })
        .unwrap_err();
        assert!(
            matches!(err, MutError::UnsafeComponentName { .. }),
            "name {bad:?} should be rejected, got {err:?}"
        );
    }
}

#[test]
fn rewrite_plan_unsafe_root_name_leaves_dir_unchanged() {
    let (index, resolver) = indirect_over_fixtures(&["chicken.djvu"]);
    let plan = IndirectRewritePlan::from_indirect_resolved(&index, resolver).unwrap();

    let dir = fresh_temp_dir("unsafe_root");
    // Drop a sentinel file that must survive a failed commit.
    std::fs::write(dir.join("sentinel"), b"keep me").unwrap();

    let err = plan.commit_to_dir(&dir, "../escape.djvu").unwrap_err();
    assert!(
        matches!(err, MutError::UnsafeComponentName { .. }),
        "{err:?}"
    );

    // Nothing was written: only the sentinel remains.
    let entries: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, vec!["sentinel".to_string()]);
    assert_eq!(std::fs::read(dir.join("sentinel")).unwrap(), b"keep me");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rewrite_plan_root_name_collision_rejected() {
    let (index, resolver) = indirect_over_fixtures(&["chicken.djvu"]);
    let plan = IndirectRewritePlan::from_indirect_resolved(&index, resolver).unwrap();
    let dir = fresh_temp_dir("root_collision");
    // Root name equals a component name — would shadow the page file.
    let err = plan.commit_to_dir(&dir, "chicken.djvu").unwrap_err();
    assert!(
        matches!(err, MutError::DuplicateComponentName { .. }),
        "{err:?}"
    );
    // Validation failed before writing: directory is still empty.
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rewrite_plan_set_bookmarks_marks_root_changed() {
    let (index, resolver) = indirect_over_fixtures(&["chicken.djvu"]);
    let mut plan = IndirectRewritePlan::from_indirect_resolved(&index, resolver).unwrap();
    plan.set_bookmarks(&[DjVuBookmark {
        title: "Top".into(),
        url: "#1".into(),
        children: vec![],
    }])
    .unwrap();

    let preview = plan.plan("index.djvu");
    let root = preview.iter().find(|i| i.is_root).unwrap();
    assert!(root.changed, "root index must be marked changed");

    // Commit and confirm the index file carries NAVM bookmarks.
    let dir = fresh_temp_dir("bookmarks");
    plan.commit_to_dir(&dir, "index.djvu").unwrap();
    let index_bytes = std::fs::read(dir.join("index.djvu")).unwrap();
    let form = crate::iff::parse_form(&index_bytes).unwrap();
    assert!(
        form.chunks.iter().any(|c| &c.id == b"NAVM"),
        "committed index must contain NAVM"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rewrite_plan_rejects_bundled_input() {
    let bundled = read_corpus("DjVu3Spec_bundled.djvu");
    let err = IndirectRewritePlan::from_indirect_resolved(&bundled, |_n: &str| {
        Ok::<Vec<u8>, std::io::Error>(Vec::new())
    })
    .unwrap_err();
    assert!(matches!(err, MutError::NotIndirectDjvm), "{err:?}");
}

#[test]
fn chunk_at_path_rejects_empty_path() {
    let original = read_corpus("chicken.djvu");
    let doc = DjVuDocumentMut::from_bytes(&original).unwrap();
    let err = doc.chunk_at_path(&[]).unwrap_err();
    assert!(matches!(err, MutError::EmptyPath));
}

#[test]
fn root_form_type_returns_some_for_form_root() {
    let original = read_corpus("chicken.djvu");
    let doc = DjVuDocumentMut::from_bytes(&original).unwrap();
    let t = doc.root_form_type();
    assert!(t.is_some());
}

// Branch-coverage edge cases, one per uncovered line of the parent module.
mod edge_cases;
