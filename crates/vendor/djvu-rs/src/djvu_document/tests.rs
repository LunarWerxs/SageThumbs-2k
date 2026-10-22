use super::*;

fn fixture_bytes(name: &str) -> Vec<u8> {
    std::fs::read(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/{name}")),
    )
    .unwrap_or_else(|_| panic!("fixture {name} should exist"))
}

/// #624: a page may carry several `INCL` chunks (czech.djvu: shared
/// annotations + two symbol-dictionary includes). Resolution must scan
/// them all and pick the include that actually holds a `Djbz` — taking
/// only the first INCL left every czech mask undecodable
/// (`MissingSharedDict`). The expected mask is byte-identical to
/// DjVuLibre's `ddjvu -mode=mask` output.
#[test]
fn multi_incl_page_resolves_shared_dict() {
    let path =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/czech.djvu");
    let data = std::fs::read(path).unwrap();
    let doc = DjVuDocument::parse(&data).unwrap();
    assert_czech_catalog(&doc);
}

/// Every czech page has its symbol dictionary, and page 1's mask matches
/// DjVuLibre's `ddjvu -mode=mask` output.
fn assert_czech_catalog(doc: &DjVuDocument) {
    assert_eq!(doc.page_count(), 85);
    let missing: Vec<usize> = (0..doc.page_count())
        .filter(|&i| doc.page(i).unwrap().shared_djbz.is_none())
        .collect();
    assert!(
        missing.is_empty(),
        "pages without a dictionary: {missing:?}"
    );
    let mask = doc
        .page(1)
        .unwrap()
        .extract_mask()
        .expect("mask decode must succeed")
        .expect("page 1 has an Sjbz mask");
    assert_eq!((mask.width, mask.height), (1095, 1750));
    let black: u64 = (0..mask.height)
        .map(|y| (0..mask.width).filter(|&x| mask.get(x, y)).count() as u64)
        .sum();
    assert_eq!(
        black, 308_624,
        "mask content must match the ddjvu reference"
    );
}

/// The lazy backed loader behind `Document::from_bytes` used to read only
/// the first INCL, so every czech page lost its dictionary.
#[test]
fn backed_multi_incl_page_resolves_shared_dict() {
    let data = fixture_bytes("czech.djvu");
    let doc = crate::Document::from_bytes(data).unwrap();
    assert_czech_catalog(doc.inner());
}

/// czech.djvu split into an indirect index and its component files.
fn czech_indirect() -> (Vec<u8>, std::collections::BTreeMap<String, Vec<u8>>) {
    let split = crate::djvm::to_indirect(&fixture_bytes("czech.djvu")).unwrap();
    (split.index, split.components.into_iter().collect())
}

/// The name-resolver path used to give indirect pages no dictionary.
#[test]
fn indirect_named_resolver_attaches_shared_dict() {
    let (index, files) = czech_indirect();
    let doc = DjVuDocument::parse_with_resolver(
        &index,
        Some(|name: &str| {
            files
                .get(name)
                .cloned()
                .ok_or_else(|| DocError::IndirectResolve(name.to_string()))
        }),
    )
    .unwrap();
    assert_czech_catalog(&doc);
}

#[test]
fn parse_from_dir_attaches_shared_dict() {
    let (index, files) = czech_indirect();
    let dir = tempfile::tempdir().unwrap();
    for (name, bytes) in &files {
        std::fs::write(dir.path().join(name), bytes).unwrap();
    }
    let doc = DjVuDocument::parse_from_dir(&index, dir.path()).unwrap();
    assert_czech_catalog(&doc);
}

#[test]
fn indirect_typed_resolver_attaches_shared_dict() {
    let (index, files) = czech_indirect();
    let doc = DjVuDocument::parse_with_component_resolver(&index, &|c: &ComponentId| {
        files
            .get(&c.name)
            .cloned()
            .ok_or_else(|| ComponentResolveError::Missing {
                component: c.clone(),
            })
    })
    .unwrap();
    assert_czech_catalog(&doc);
    assert_eq!(sorted_extra(&doc), czech_expected_metadata());
}

/// A page may INCL a shared component that DIRM lists after it; the
/// assembler waits for the whole catalog before building that page.
#[test]
fn page_before_its_shared_dict_in_dirm_gets_it() {
    let (_, files) = czech_indirect();
    let split = crate::djvm::to_indirect(&fixture_bytes("czech.djvu")).unwrap();
    let (pages, shared): (Vec<_>, Vec<_>) = split
        .components
        .iter()
        .filter(|(_, bytes)| &bytes[12..16] != b"THUM")
        .partition(|(_, bytes)| &bytes[12..16] == b"DJVU");
    let components: Vec<(&str, &[u8])> = pages
        .iter()
        .chain(&shared)
        .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
        .collect();
    let index = crate::djvm::create_indirect_with_components(&components).unwrap();
    let doc = DjVuDocument::parse_with_resolver(
        &index,
        Some(|name: &str| {
            files
                .get(name)
                .cloned()
                .ok_or_else(|| DocError::IndirectResolve(name.to_string()))
        }),
    )
    .unwrap();
    assert_czech_catalog(&doc);
}

/// A legacy BM44/PM44 image is a page component too, so the typed
/// resolver must not reject it as a kind mismatch.
#[test]
fn indirect_typed_resolver_accepts_legacy_iw44_page() {
    let files = [
        ("a.djvu", fixture_bytes("legacy_bm44.djvu")),
        ("b.djvu", fixture_bytes("legacy_pm44.djvu")),
    ];
    let components: Vec<(&str, &[u8])> = files.iter().map(|(n, b)| (*n, b.as_slice())).collect();
    let index = crate::djvm::create_indirect_with_components(&components).unwrap();
    let doc = DjVuDocument::parse_with_component_resolver(&index, &|c: &ComponentId| {
        files
            .iter()
            .find(|(n, _)| *n == c.name)
            .map(|(_, b)| b.clone())
            .ok_or_else(|| ComponentResolveError::Missing {
                component: c.clone(),
            })
    })
    .unwrap();
    assert_eq!(doc.page_count(), 2);
    for (i, (_, bytes)) in files.iter().enumerate() {
        let single = DjVuDocument::parse(bytes).unwrap();
        let (want, got) = (single.page(0).unwrap(), doc.page(i).unwrap());
        assert_eq!((got.width(), got.height()), (want.width(), want.height()));
    }
}

fn czech_expected_metadata() -> Vec<(String, String)> {
    [
        ("HostComputer", "schroeder"),
        ("ModDate", "2017-12-14T22:19:04+00:00"),
        ("Producer", "Aleš Kapica, djvutool 0.8"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

fn sorted_extra(doc: &DjVuDocument) -> Vec<(String, String)> {
    let mut extra = doc
        .metadata()
        .unwrap()
        .expect("shared annotation carries (metadata …)")
        .extra;
    extra.sort();
    extra
}

/// #833: DIRM flag 3 marks the shared annotation. `djvused ls` lists it as
/// `A`, and `djvused print-meta` reads document metadata from its
/// `(metadata …)` block; czech.djvu has no METa/METz chunk.
#[test]
fn shared_annotation_is_listed_and_supplies_metadata() {
    let path =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/czech.djvu");
    let data = std::fs::read(path).unwrap();
    let doc = DjVuDocument::parse(&data).unwrap();
    let annotations: Vec<String> = doc
        .component_directory()
        .unwrap()
        .into_iter()
        .filter(|entry| entry.kind == 'A')
        .map(|entry| entry.id)
        .collect();
    assert_eq!(annotations, ["shared_anno.iff"]);
    assert_eq!(sorted_extra(&doc), czech_expected_metadata());

    // The indirect form resolves the shared annotation by name too.
    let indirect = crate::djvm::to_indirect(&data).unwrap();
    let components = indirect.components;
    let doc = DjVuDocument::parse_with_resolver(
        &indirect.index,
        Some(|name: &str| {
            components
                .iter()
                .find(|(id, _)| id == name)
                .map(|(_, bytes)| bytes.clone())
                .ok_or(DocError::IndirectResolve(name.to_string()))
        }),
    )
    .unwrap();
    assert_eq!(sorted_extra(&doc), czech_expected_metadata());
}

/// #833: rewriting a bundled document keeps DIRM flag 3; it used to be
/// saved as a plain include (flag 0).
#[test]
fn page_removal_keeps_shared_annotation_flag() {
    let path =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/czech.djvu");
    let data = std::fs::read(path).unwrap();
    let removal =
        crate::djvm::remove_pages(&data, &[0], crate::djvm::UnreachablePolicy::Preserve).unwrap();
    let doc = DjVuDocument::parse(&removal.document).unwrap();
    assert!(
        doc.component_directory()
            .unwrap()
            .iter()
            .any(|entry| entry.kind == 'A' && entry.id == "shared_anno.iff")
    );
    assert_eq!(sorted_extra(&doc), czech_expected_metadata());
}

/// A document without METa/METz or a shared annotation has no metadata.
#[test]
fn metadata_is_none_without_shared_annotation() {
    let path =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/carte.djvu");
    let doc = DjVuDocument::parse(&std::fs::read(path).unwrap()).unwrap();
    assert!(doc.metadata().unwrap().is_none());
}

/// A NAVM bookmark chain nested far deeper than `MAX_NAVM_DEPTH` must error,
/// not recurse until the stack overflows (security finding). Drives the
/// internal entry parser directly with a crafted decoded buffer.
#[test]
fn deeply_nested_bookmarks_are_rejected_not_overflow() {
    // [total_count u16 = 1] then one entry that is a 400-deep single-child
    // chain: each node = [n_children=1][title len3=0][url len3=0]; deepest =
    // [n_children=0][..][..].
    let mut decoded = vec![0x00, 0x01];
    for _ in 0..400 {
        decoded.push(1); // n_children
        decoded.extend_from_slice(&[0, 0, 0]); // empty title (3-byte len)
        decoded.extend_from_slice(&[0, 0, 0]); // empty url
    }
    decoded.push(0); // deepest: no children
    decoded.extend_from_slice(&[0, 0, 0]);
    decoded.extend_from_slice(&[0, 0, 0]);

    let mut pos = 2usize;
    let mut counter = 0usize;
    let r = parse_bookmark_entry(&decoded, &mut pos, &mut counter, 0);
    assert!(r.is_err(), "deep bookmark chain must error, not overflow");
}

fn assets_path() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("references/djvujs/library/assets")
}

// ---- TDD: failing tests written first (Red phase) -----------------------

/// Single-page FORM:DJVU — basic parse, page count, dimensions, DPI.
#[test]
fn single_page_parse_and_metadata() {
    let data = std::fs::read(assets_path().join("chicken.djvu")).expect("chicken.djvu must exist");
    let doc = DjVuDocument::parse(&data).expect("parse should succeed");

    assert_eq!(doc.page_count(), 1);
    let page = doc.page(0).expect("page 0 must exist");
    assert_eq!(page.width(), 181);
    assert_eq!(page.height(), 240);
    assert_eq!(page.dpi(), 100);
    assert!((page.gamma() - 2.2).abs() < 0.01, "gamma should be ~2.2");
}

/// Single-page document: page index out of range.
#[test]
fn single_page_out_of_range() {
    let data = std::fs::read(assets_path().join("chicken.djvu")).expect("chicken.djvu must exist");
    let doc = DjVuDocument::parse(&data).expect("parse should succeed");
    let err = doc.page(1).expect_err("page 1 should be out of range");
    assert!(
        matches!(err, DocError::PageOutOfRange { index: 1, count: 1 }),
        "unexpected error: {err:?}"
    );
}

// ---- #342: chunk-payload dispatch (compressed / raw / missing) ----------
//
// These exercise the single BZZ-or-raw seam directly, decoupled from any
// format parser: `decode_paired_payload` (the free function) and the
// `DjVuPage::chunk_payload` accessor built on it.

#[test]
fn paired_payload_prefers_compressed_z_chunk() {
    let raw = b"the quick brown fox".as_slice();
    let z = crate::bzz_encode::bzz_encode(raw);
    // Both present: the compressed `*z` chunk wins.
    let out =
        decode_paired_payload(Some(&z), Some(b"ignored raw")).expect("bzz decode should succeed");
    assert_eq!(out.as_deref(), Some(raw));
}

#[test]
fn paired_payload_falls_back_to_raw_a_chunk() {
    let raw = b"plain uncompressed payload".as_slice();
    let out = decode_paired_payload(None, Some(raw)).expect("raw passthrough");
    assert_eq!(out.as_deref(), Some(raw));
}

#[test]
fn paired_payload_missing_both_is_none() {
    assert_eq!(decode_paired_payload(None, None).expect("none"), None);
}

#[test]
fn paired_payload_empty_chunk_is_placeholder_none() {
    // DjVu uses a zero-length chunk as a "no payload" placeholder for both
    // the compressed and raw variants.
    assert_eq!(
        decode_paired_payload(Some(&[]), None).expect("empty z"),
        None
    );
    assert_eq!(
        decode_paired_payload(None, Some(&[])).expect("empty a"),
        None
    );
}

#[test]
fn paired_payload_invalid_bzz_errors() {
    // A non-empty `*z` chunk that is not valid BZZ must surface the error,
    // not be silently treated as missing.
    let result = decode_paired_payload(Some(&[0xff, 0x00, 0x13, 0x37]), None);
    assert!(result.is_err(), "invalid BZZ must error, got {result:?}");
}

/// Build a minimal valid INFO chunk payload (10 bytes) for the given size.
fn make_info(width: u16, height: u16) -> Vec<u8> {
    let mut v = Vec::with_capacity(10);
    v.extend_from_slice(&width.to_be_bytes());
    v.extend_from_slice(&height.to_be_bytes());
    v.extend_from_slice(&[0, 0]); // version bytes (unused here)
    v.extend_from_slice(&100u16.to_le_bytes()); // dpi (little-endian)
    v.push(22); // gamma byte → 2.2
    v.push(0); // flags → no rotation
    v
}

/// Build a `DjVuPage` directly from hand-made chunks (INFO + extras), so the
/// accessor can be tested without a full file round-trip through a parser.
/// #605: repeated metadata access returns identical results through the
/// cache, and the shared handles point at one allocation.
#[test]
fn metadata_cache_repeated_access_is_consistent() {
    let data = std::fs::read(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/links.djvu"),
    )
    .unwrap();
    let doc = DjVuDocument::parse(&data).unwrap();
    let page = doc.page(0).unwrap();

    let a1 = page.annotations().unwrap();
    let a2 = page.annotations().unwrap();
    assert_eq!(
        a1.as_ref().map(|(_, m)| m.len()),
        a2.as_ref().map(|(_, m)| m.len())
    );
    let s1 = page.annotations_shared().unwrap();
    let s2 = page.annotations_shared().unwrap();
    if let (Some(s1), Some(s2)) = (s1, s2) {
        assert!(
            std::sync::Arc::ptr_eq(&s1, &s2),
            "warm hits must share one decode"
        );
    }
    let h1 = page.hyperlinks().unwrap();
    let h2 = page.hyperlinks().unwrap();
    assert_eq!(h1.len(), h2.len());
}

/// #605: malformed TXTz keeps erroring on every call (errors are not
/// cached), matching the pre-cache behaviour.
#[test]
fn metadata_cache_does_not_cache_errors() {
    // TXTz payload that BZZ-decodes but fails structured parse — or fails
    // BZZ outright; either way both calls must return Err.
    let bogus = [0xFFu8, 0x00, 0x12, 0x34, 0x56];
    let page = page_with_chunks(&[(b"TXTz", &bogus)]);
    assert!(page.text_layer().is_err());
    assert!(
        page.text_layer().is_err(),
        "second call must error identically"
    );
}

fn page_with_chunks(extra: &[(&[u8; 4], &[u8])]) -> DjVuPage {
    let info = make_info(64, 48);
    let mut chunks = Vec::new();
    chunks.push(IffChunk {
        id: *b"INFO",
        data: &info,
    });
    for (id, data) in extra {
        chunks.push(IffChunk { id: **id, data });
    }
    parse_page_from_chunks(&chunks, 0, None).expect("page should build")
}

#[test]
fn parse_with_options_rejects_exceeded_page_count_before_decode() {
    let data = fixture_bytes("boy.djvu");
    let err = DjVuDocument::parse_with_options(
        &data,
        &crate::resource_limits::ParseOptions {
            limits: Some(crate::resource_limits::ResourceLimits {
                max_pages: Some(0),
                ..Default::default()
            }),
        },
    )
    .expect_err("parse should fail on page-count limit");
    assert!(matches!(err, DocError::ResourceLimit(_)));
}

#[test]
fn parse_with_options_stores_limits_for_render_inheritance() {
    let data = fixture_bytes("boy.djvu");
    let limits = crate::resource_limits::ResourceLimits {
        max_render_pixels: Some(100_000),
        ..Default::default()
    };
    let doc = DjVuDocument::parse_with_options(
        &data,
        &crate::resource_limits::ParseOptions {
            limits: Some(limits),
        },
    )
    .expect("parse should succeed");
    assert_eq!(doc.resource_limits(), Some(limits));
    assert_eq!(doc.page(0).unwrap().resource_limits(), Some(limits));
}

#[test]
fn chunk_payload_decodes_compressed_txtz() {
    let raw = b"decoded text-layer payload".as_slice();
    let z = crate::bzz_encode::bzz_encode(raw);
    let page = page_with_chunks(&[(b"TXTz", &z)]);
    let out = page
        .chunk_payload(b"TXTz", b"TXTa")
        .expect("chunk_payload should succeed");
    assert_eq!(out.as_deref(), Some(raw));
}

#[test]
fn chunk_payload_passes_through_raw_txta() {
    let raw = b"raw text-layer payload".as_slice();
    let page = page_with_chunks(&[(b"TXTa", raw)]);
    let out = page
        .chunk_payload(b"TXTz", b"TXTa")
        .expect("chunk_payload should succeed");
    assert_eq!(out.as_deref(), Some(raw));
}

#[test]
fn chunk_payload_missing_chunk_is_none() {
    let page = page_with_chunks(&[]); // INFO only, no TXT* chunks
    let out = page
        .chunk_payload(b"TXTz", b"TXTa")
        .expect("chunk_payload should succeed");
    assert_eq!(out, None);
}

/// Single-page document: no thumbnails expected.
#[test]
fn single_page_no_thumbnail() {
    let data = std::fs::read(assets_path().join("chicken.djvu")).expect("chicken.djvu must exist");
    let doc = DjVuDocument::parse(&data).expect("parse should succeed");
    let page = doc.page(0).expect("page 0 must exist");
    // Data is not decoded until thumbnail() is called — verify lazy contract
    let thumb = page.thumbnail().expect("thumbnail() should not error");
    assert!(
        thumb.is_none(),
        "single-page chicken.djvu has no TH44 chunks"
    );
}

/// Single-page: dimensions helper.
#[test]
fn single_page_dimensions() {
    let data = std::fs::read(assets_path().join("chicken.djvu")).expect("chicken.djvu must exist");
    let doc = DjVuDocument::parse(&data).expect("parse should succeed");
    let page = doc.page(0).unwrap();
    assert_eq!(page.dimensions(), (181, 240));
}

/// Bundled multi-page FORM:DJVM — page count and DIRM parsing.
#[test]
fn multipage_bundled_page_count() {
    let data = std::fs::read(assets_path().join("DjVu3Spec_bundled.djvu"))
        .expect("DjVu3Spec_bundled.djvu must exist");
    let doc = DjVuDocument::parse(&data).expect("bundled parse should succeed");
    // The bundled spec PDF has many pages — just check > 1
    assert!(
        doc.page_count() > 1,
        "bundled document should have more than 1 page, got {}",
        doc.page_count()
    );
}

/// Bundled multi-page: each page should have valid metadata.
#[test]
fn multipage_bundled_page_metadata() {
    let data = std::fs::read(assets_path().join("DjVu3Spec_bundled.djvu"))
        .expect("DjVu3Spec_bundled.djvu must exist");
    let doc = DjVuDocument::parse(&data).expect("bundled parse should succeed");

    let page0 = doc.page(0).expect("page 0 must exist");
    assert!(page0.width() > 0, "page width must be non-zero");
    assert!(page0.height() > 0, "page height must be non-zero");
    assert!(page0.dpi() > 0, "page dpi must be non-zero");
}

/// NAVM bookmarks from a document that contains them.
#[test]
fn navm_bookmarks_present() {
    let data =
        std::fs::read(assets_path().join("navm_fgbz.djvu")).expect("navm_fgbz.djvu must exist");
    let doc = DjVuDocument::parse(&data).expect("parse should succeed");
    // navm_fgbz.djvu has NAVM chunk — should return at least one bookmark
    let bm = doc.bookmarks();
    assert!(
        !bm.is_empty(),
        "navm_fgbz.djvu should have at least one bookmark"
    );
}

/// Documents without NAVM should return empty bookmark list.
#[test]
fn no_navm_returns_empty_bookmarks() {
    let data = std::fs::read(assets_path().join("chicken.djvu")).expect("chicken.djvu must exist");
    let doc = DjVuDocument::parse(&data).expect("parse should succeed");
    assert!(
        doc.bookmarks().is_empty(),
        "chicken.djvu has no NAVM — bookmarks should be empty"
    );
}

/// Indirect document: parse with resolver callback.
///
/// We simulate an indirect document by constructing a DJVM DIRM that marks
/// entries as non-bundled and supplying a resolver that returns the bytes of
/// the real chicken.djvu page.
#[test]
fn indirect_document_with_resolver() {
    // Load chicken.djvu — we'll use it as the "resolved" page.
    let chicken_data =
        std::fs::read(assets_path().join("chicken.djvu")).expect("chicken.djvu must exist");
    // Build a minimal indirect DJVM document referencing "chicken.djvu"
    let djvm_data = build_indirect_djvm_bytes("chicken.djvu");

    let resolver = |name: &str| -> Result<Vec<u8>, DocError> {
        if name == "chicken.djvu" {
            Ok(chicken_data.clone())
        } else {
            Err(DocError::IndirectResolve(name.to_string()))
        }
    };

    let doc = DjVuDocument::parse_with_resolver(&djvm_data, Some(resolver))
        .expect("indirect parse should succeed");

    assert_eq!(doc.page_count(), 1);
    let page = doc.page(0).unwrap();
    assert_eq!(page.width(), 181);
    assert_eq!(page.height(), 240);
}

/// Indirect document without resolver must return NoResolver error.
#[test]
fn indirect_document_no_resolver_returns_error() {
    let djvm_data = build_indirect_djvm_bytes("chicken.djvu");
    let err = DjVuDocument::parse(&djvm_data).expect_err("should fail without resolver");
    assert!(
        matches!(err, DocError::NoResolver),
        "expected NoResolver, got {err:?}"
    );
}

/// Page must not decode image data before thumbnail() is called.
///
/// We verify laziness by confirming that constructing the document and
/// accessing `page()` without calling `thumbnail()` does not involve
/// any IW44 decoder side-effects.  We test this by calling thumbnail()
/// on a page with no TH44 chunks and verifying we get Ok(None).
#[test]
fn page_is_lazy_no_decode_before_thumbnail() {
    let data = std::fs::read(assets_path().join("boy_jb2.djvu")).expect("boy_jb2.djvu must exist");
    let doc = DjVuDocument::parse(&data).expect("parse should succeed");
    let page = doc.page(0).expect("page 0 must exist");

    // Chunks are available (materialised on access for lazy pages) but no
    // IW44 decoding has happened yet.
    assert!(!page.chunk_slice().is_empty(), "chunks must be available");

    // thumbnail() triggers decode — but there's no TH44 chunk in boy_jb2.djvu
    let thumb = page.thumbnail().expect("thumbnail() should not error");
    assert!(thumb.is_none());
}

/// Non-DjVu file returns NotDjVu error.
#[test]
fn not_djvu_returns_error() {
    // Construct a valid IFF with a non-DjVu form type ("XXXX" + 4 dummy
    // bytes), routed through the emission seam.
    let data = crate::iff::partial_emit(*b"XXXX", &[crate::iff::EmitPart::Verbatim(b"XXXX")])
        .expect("fits within u32");
    let err = DjVuDocument::parse(&data).expect_err("should fail");
    assert!(
        matches!(err, DocError::NotDjVu(_) | DocError::Iff(_)),
        "expected NotDjVu or Iff error, got {err:?}"
    );
}

// ---- Helpers: build minimal DJVM documents for indirect tests -----------

/// Build a minimal indirect FORM:DJVM with 1 page component named "chicken.djvu".
///
/// DIRM format: flags=0x00 (not bundled), nfiles=1, followed by BZZ-compressed
/// metadata. The BZZ bytes below were pre-computed using the reference `bzz -e`
/// tool encoding the metadata:
///   `\x00\x00\x00` (size, 3 bytes) + `\x01` (Page flag) + `chicken.djvu\x00`
fn build_indirect_djvm_bytes(_page_name: &str) -> Vec<u8> {
    // BZZ-encoded DIRM metadata for 1 Page component named "chicken.djvu".
    // Generated with: printf '\x00\x00\x00\x01chicken.djvu\x00' | bzz -e - file.bzz
    // Verified to decode back to the original 17-byte meta block.
    let bzz_meta: &[u8] = &[
        0xff, 0xff, 0xed, 0xbf, 0x8a, 0x1f, 0xbe, 0xad, 0x14, 0x57, 0x10, 0xc9, 0x63, 0x19, 0x11,
        0xf0, 0x85, 0x28, 0x12, 0x8a, 0xbf,
    ];

    let mut dirm_data = Vec::new();
    dirm_data.push(0x00); // flags: not bundled (is_bundled bit = 0)
    dirm_data.push(0x00); // nfiles high byte
    dirm_data.push(0x01); // nfiles low byte = 1
    dirm_data.extend_from_slice(bzz_meta);

    build_djvm_with_dirm(&dirm_data)
}

fn build_djvm_with_dirm(dirm_data: &[u8]) -> Vec<u8> {
    // A FORM:DJVM carrying a single DIRM chunk, built through the seam.
    let dirm = crate::iff::Chunk::Leaf {
        id: *b"DIRM",
        data: dirm_data.to_vec(),
    };
    crate::iff::partial_emit(*b"DJVM", &[crate::iff::EmitPart::Chunk(&dirm)])
        .expect("fits within u32")
}

/// Sub-FORM with < 4 bytes of data: parse_sub_form returns Malformed (line 1225).
#[test]
fn parse_bundled_djvm_with_short_sub_form_returns_malformed() {
    use crate::dirm::DirmPayload;
    // Bundled DIRM with 1 Page entry (flags=0x80 = bundled, flag=0x01=Page)
    let dirm_payload = DirmPayload::build_bundled(&[crate::dirm::DirmComponent::new(
        crate::dirm::DirmComponentKind::Page,
        "p0001.djvu",
    )]);
    let dirm = crate::iff::Chunk::Leaf {
        id: *b"DIRM",
        data: dirm_payload.encode(),
    };
    // Short sub-FORM: FORM ID (4 bytes) + length=2 (4 bytes) + 2 data bytes
    // When the IFF parser reads this, data.len() = 2 < 4 → parse_sub_form Err
    let short_form_bytes: &[u8] = b"FORM\x00\x00\x00\x02AB";
    let djvm = crate::iff::partial_emit(
        *b"DJVM",
        &[
            crate::iff::EmitPart::Chunk(&dirm),
            crate::iff::EmitPart::Verbatim(short_form_bytes),
        ],
    )
    .expect("fits within u32");

    let err = DjVuDocument::parse(&djvm).expect_err("short sub-form must error");
    assert!(
        matches!(err, DocError::Malformed(_)),
        "expected Malformed, got {err:?}"
    );
}

// ── raw chunk API (Issue #43) ────────────────────────────────────────────

/// `DjVuPage::raw_chunk` returns bytes for known chunk types.
#[test]
fn page_raw_chunk_info_present() {
    let data = std::fs::read(assets_path().join("chicken.djvu")).expect("chicken.djvu must exist");
    let doc = DjVuDocument::parse(&data).expect("parse must succeed");
    let page = doc.page(0).expect("page 0 must exist");

    // INFO chunk must be present
    let info = page.raw_chunk(b"INFO").expect("INFO chunk must be present");
    assert_eq!(info.len(), 10, "INFO chunk is always 10 bytes");
}

/// `DjVuPage::raw_chunk` returns None for absent chunk types.
#[test]
fn page_raw_chunk_absent() {
    let data = std::fs::read(assets_path().join("chicken.djvu")).expect("chicken.djvu must exist");
    let doc = DjVuDocument::parse(&data).expect("parse must succeed");
    let page = doc.page(0).expect("page 0 must exist");

    assert!(
        page.raw_chunk(b"XXXX").is_none(),
        "unknown chunk type must return None"
    );
}

/// `DjVuPage::all_chunks` returns multiple BG44 chunks in order.
#[test]
fn page_all_chunks_bg44_multiple() {
    // big-scanned-page.djvu has 4 progressive BG44 chunks
    let data = std::fs::read(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/big-scanned-page.djvu"),
    )
    .expect("big-scanned-page.djvu must exist");
    let doc = DjVuDocument::parse(&data).expect("parse must succeed");
    let page = doc.page(0).expect("page 0 must exist");

    let bg44 = page.all_chunks(b"BG44");
    assert!(
        bg44.len() >= 2,
        "colour page must have ≥2 BG44 chunks, got {}",
        bg44.len()
    );

    // Chunks must be non-empty
    for (i, chunk) in bg44.iter().enumerate() {
        assert!(!chunk.is_empty(), "BG44 chunk {i} must not be empty");
    }
}

/// `DjVuPage::chunk_ids` lists all chunk IDs in order.
#[test]
fn page_chunk_ids_includes_info() {
    let data = std::fs::read(assets_path().join("chicken.djvu")).expect("chicken.djvu must exist");
    let doc = DjVuDocument::parse(&data).expect("parse must succeed");
    let page = doc.page(0).expect("page 0 must exist");

    let ids = page.chunk_ids();
    assert!(!ids.is_empty(), "chunk_ids must not be empty");
    assert!(
        ids.contains(b"INFO"),
        "chunk_ids must include INFO, got: {:?}",
        ids.iter()
            .map(|id| std::str::from_utf8(id).unwrap_or("????"))
            .collect::<Vec<_>>()
    );
}

/// `DjVuDocument::raw_chunk` works for single-page DJVU files.
#[test]
fn document_raw_chunk_single_page() {
    let data = std::fs::read(assets_path().join("chicken.djvu")).expect("chicken.djvu must exist");
    let doc = DjVuDocument::parse(&data).expect("parse must succeed");

    // Single-page DJVU exposes all top-level chunks at document level too
    let info = doc
        .raw_chunk(b"INFO")
        .expect("document must expose INFO chunk");
    assert_eq!(info.len(), 10);
}

// ── DJVI shared dictionary / INCL chunks (Issue #45) ────────────────────

/// DjVu3Spec_bundled.djvu has shared DJVI symbol dictionaries.
/// Parsing must succeed and pages with INCL references must carry the dict.
#[test]
fn djvi_shared_dict_parsed_from_bundled_djvm() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/DjVu3Spec_bundled.djvu");
    let data = std::fs::read(&path).expect("DjVu3Spec_bundled.djvu must exist");
    let doc = DjVuDocument::parse(&data).expect("parse must succeed");

    assert!(doc.page_count() > 0, "document must have pages");

    // At least one page should have a shared dict loaded (shared_djbz Some)
    let pages_with_dict = doc.pages.iter().filter(|p| p.shared_djbz.is_some()).count();
    assert!(
        pages_with_dict > 0,
        "at least one page must have a resolved shared DJVI dict"
    );
}

/// Pages with INCL references must render their mask without error.
#[test]
fn djvi_incl_page_mask_renders_ok() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/DjVu3Spec_bundled.djvu");
    let data = std::fs::read(&path).expect("DjVu3Spec_bundled.djvu must exist");
    let doc = DjVuDocument::parse(&data).expect("parse must succeed");

    // Find first page with a shared dict and render its mask
    let page = doc
        .pages
        .iter()
        .find(|p| p.shared_djbz.is_some())
        .expect("at least one page must have a shared dict");

    let mask = page
        .extract_mask()
        .expect("extract_mask must succeed for INCL page");
    assert!(mask.is_some(), "INCL page must have a JB2 mask");
    let bm = mask.unwrap();
    assert!(
        bm.width > 0 && bm.height > 0,
        "mask must have non-zero dimensions"
    );
}

/// `extract_mask_sub4` must be bit-for-bit identical to decoding the full
/// mask and then max-pool-downsampling it by 4 (round 89 follow-up: this
/// is what lets the thumbnail path skip the full-resolution JB2 canvas).
#[test]
fn mask_sub4_matches_extract_mask_then_downsample() {
    let data = std::fs::read(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/boy_jb2.djvu"),
    )
    .expect("boy_jb2.djvu must exist");
    let doc = DjVuDocument::parse(&data).expect("parse must succeed");
    let page = doc.page(0).expect("page 0 must exist");

    let full = page
        .extract_mask()
        .expect("extract_mask must succeed")
        .expect("boy_jb2.djvu page must have a JB2 mask");
    let expected = crate::djvu_render::downsample_mask_4x(&full);

    let actual = page
        .extract_mask_sub4()
        .expect("extract_mask_sub4 must succeed")
        .expect("boy_jb2.djvu page must have a JB2 mask");

    assert_eq!(expected.width, actual.width);
    assert_eq!(expected.height, actual.height);
    assert_eq!(expected.data, actual.data, "sub4 mask mismatch");
}

/// Same equivalence check on a page with a shared dictionary (INCL /
/// Djbz), which `extract_mask_sub4` resolves the same way `extract_mask`
/// does before decoding.
#[test]
fn mask_sub4_matches_extract_mask_then_downsample_shared_dict() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/DjVu3Spec_bundled.djvu");
    let data = std::fs::read(&path).expect("DjVu3Spec_bundled.djvu must exist");
    let doc = DjVuDocument::parse(&data).expect("parse must succeed");
    let page = doc
        .pages
        .iter()
        .find(|p| p.shared_djbz.is_some())
        .expect("at least one page must have a shared dict");

    let full = page
        .extract_mask()
        .expect("extract_mask must succeed")
        .expect("page must have a JB2 mask");
    let expected = crate::djvu_render::downsample_mask_4x(&full);

    let actual = page
        .extract_mask_sub4()
        .expect("extract_mask_sub4 must succeed")
        .expect("page must have a JB2 mask");

    assert_eq!(expected.width, actual.width);
    assert_eq!(expected.height, actual.height);
    assert_eq!(expected.data, actual.data, "sub4 mask mismatch");
}

/// Pages without INCL still render correctly (no regression).
#[test]
fn no_regression_non_incl_pages() {
    // boy_jb2.djvu has a Sjbz mask and no INCL reference
    let data = std::fs::read(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/boy_jb2.djvu"),
    )
    .expect("boy_jb2.djvu must exist");
    let doc = DjVuDocument::parse(&data).expect("parse must succeed");
    let page = doc.page(0).expect("page 0 must exist");
    assert!(
        page.shared_djbz.is_none(),
        "single-page DJVU has no shared dict"
    );
    let mask = page.extract_mask().expect("extract_mask must succeed");
    assert!(mask.is_some(), "boy_jb2.djvu page must have a JB2 mask");
}

/// `carte.djvu` has a 5-byte INFO chunk (width, height, version byte —
/// no dpi/gamma/flags) instead of the canonical 10-byte layout. The file
/// itself is intact (byte-exact IFF framing; `djvudump`/`ddjvu` from
/// DjVuLibre parse and render it without complaint), so `DjVuDocument::parse`
/// rejecting it as `Iff(Truncated)` was a parser-strictness bug, not a
/// corrupt fixture. Regression test for that bug (see `info.rs`'s
/// `carte_style_five_byte_info_parses_with_defaults` for the unit-level
/// check).
#[test]
fn parse_carte_with_short_info_chunk() {
    let data = std::fs::read(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/carte.djvu"),
    )
    .expect("carte.djvu must exist");
    let doc = DjVuDocument::parse(&data).expect("carte.djvu must parse despite short INFO");
    assert_eq!(doc.page_count(), 1);
    let page = doc.page(0).expect("page 0 must exist");
    assert_eq!(page.width(), 4200);
    assert_eq!(page.height(), 2556);
}

/// Round-trip: bytes from `raw_chunk` re-parse to the same metadata.
#[test]
fn page_raw_chunk_info_roundtrip() {
    let data = std::fs::read(assets_path().join("chicken.djvu")).expect("chicken.djvu must exist");
    let doc = DjVuDocument::parse(&data).expect("parse must succeed");
    let page = doc.page(0).expect("page 0 must exist");

    let raw_info = page.raw_chunk(b"INFO").expect("INFO chunk must be present");
    let reparsed = crate::info::PageInfo::parse(raw_info).expect("re-parse must succeed");
    assert_eq!(reparsed.width, page.width() as u16);
    assert_eq!(reparsed.height, page.height() as u16);
    assert_eq!(reparsed.dpi, page.dpi());
}

// ── #196 Phase 2: page_byte_range ────────────────────────────────────────

/// Single-page DJVU: byte range covers the entire input buffer.
#[test]
fn page_byte_range_single_page_covers_full_buffer() {
    let data = std::fs::read(assets_path().join("chicken.djvu")).expect("chicken.djvu must exist");
    let doc = DjVuDocument::parse(&data).expect("parse must succeed");

    let r = doc.page_byte_range(0).expect("page 0 must have a range");
    assert_eq!(r.start, 0);
    assert_eq!(r.end, data.len() as u64);

    assert!(
        doc.page_byte_range(1).is_none(),
        "out-of-range index returns None"
    );
}

/// Bundled DJVM: every page's byte range is non-empty, in-bounds,
/// non-overlapping with neighbours, and re-parseable as a FORM.
#[test]
fn page_byte_range_bundled_djvm_round_trips() {
    let path = assets_path().join("DjVu3Spec_bundled.djvu");
    let Ok(data) = std::fs::read(&path) else {
        eprintln!("skip: {} missing", path.display());
        return;
    };
    let doc = DjVuDocument::parse(&data).expect("bundled DJVM parse must succeed");

    let mut prev_end = 0u64;
    for i in 0..doc.page_count() {
        let r = doc
            .page_byte_range(i)
            .unwrap_or_else(|| panic!("page {i} must have a range"));
        assert!(r.end <= data.len() as u64, "page {i} range OOB");
        assert!(r.start < r.end, "page {i} range empty");
        assert!(r.start >= prev_end, "page {i} overlaps previous");
        prev_end = r.end;

        // The range must start with `b"FORM"` magic.
        let slice = &data[r.start as usize..r.end as usize];
        assert_eq!(&slice[..4], b"FORM", "page {i} range must start with FORM");
    }
}

#[test]
fn page_thumbnail_with_th44_data() {
    // Extract real TH44 chunk bytes from carte.djvu (which contains TH44 data)
    // and embed them in a synthetic page to cover the thumbnail decode path.
    let carte = std::fs::read(assets_path().join("carte.djvu")).unwrap();
    // Find TH44 in the raw bytes and extract chunk payload
    let th44_pos = carte.windows(4).position(|w| w == b"TH44");
    if let Some(pos) = th44_pos
        && pos + 8 <= carte.len()
    {
        let chunk_len = u32::from_be_bytes([
            carte[pos + 4],
            carte[pos + 5],
            carte[pos + 6],
            carte[pos + 7],
        ]) as usize;
        let chunk_data = carte.get(pos + 8..pos + 8 + chunk_len).unwrap_or(&[]);
        if !chunk_data.is_empty() {
            let page = page_with_chunks(&[(b"TH44", chunk_data)]);
            // This should decode successfully (covers lines 298-303)
            let thumb = page.thumbnail();
            assert!(thumb.is_ok(), "thumbnail decode should not error");
            // The thumbnail may or may not be Some depending on IW44 data validity
        }
    }
}

#[test]
fn extract_mask_from_smmr_chunk() {
    // Build a page with an Smmr chunk (G4/MMR-encoded mask). This covers the
    // Smmr decode path in extract_mask() (lines 545-546).
    use crate::chunk_encode::{ChunkEncoder, SmmrChunk};
    let mut bm = crate::bitmap::Bitmap::new(8, 8);
    bm.set_black(2, 2);
    let smmr_chunk = SmmrChunk(&bm).encode_chunk().unwrap();
    let page = page_with_chunks(&[(b"Smmr", &smmr_chunk.payload)]);
    let result = page.extract_mask().unwrap();
    assert!(result.is_some(), "Smmr page should have a mask");
    assert_eq!(result.unwrap().width, 8);
}

#[test]
fn extract_background_returns_none_for_jb2_only_page() {
    // A page with only Sjbz (no BG44) → extract_background returns Ok(None)
    // This covers lines 638-641 in djvu_document.rs.
    let jb2_data = std::fs::read(assets_path().join("boy_jb2.djvu")).unwrap();
    let doc = DjVuDocument::parse(&jb2_data).unwrap();
    let page = doc.page(0).unwrap();
    let bg = page.extract_background().unwrap();
    assert!(bg.is_none(), "JB2-only page should have no background");
}

#[test]
fn extract_mask_indexed_smmr_path() {
    // Page with Smmr chunk: extract_mask_indexed takes the Smmr path (lines 570-575).
    use crate::chunk_encode::{ChunkEncoder, SmmrChunk};
    let mut bm = crate::bitmap::Bitmap::new(4, 4);
    bm.set_black(1, 1);
    let smmr_chunk = SmmrChunk(&bm).encode_chunk().unwrap();
    let page = page_with_chunks(&[(b"Smmr", &smmr_chunk.payload)]);
    let result = page.extract_mask_indexed().unwrap();
    assert!(result.is_some());
    let (mask, indices) = result.unwrap();
    assert_eq!(mask.width, 4);
    assert_eq!(indices.len(), 4 * 4);
}

#[test]
fn extract_mask_indexed_no_chunks_returns_none() {
    // Page with no Sjbz or Smmr → Ok(None) (line 588).
    let page = page_with_chunks(&[]);
    let result = page.extract_mask_indexed().unwrap();
    assert!(result.is_none());
}

#[test]
fn extract_background_decodes_iw44_from_color_page() {
    // chicken.djvu has BG44 → extract_background decodes IW44 (lines 644-649).
    let data = std::fs::read(assets_path().join("chicken.djvu")).unwrap();
    let doc = DjVuDocument::parse(&data).unwrap();
    let page = doc.page(0).unwrap();
    let bg = page.extract_background().unwrap();
    assert!(bg.is_some(), "chicken.djvu page should have a background");
    let pm = bg.unwrap();
    assert!(pm.width > 0 && pm.height > 0);
}

#[test]
fn djvu_page_debug_impl_does_not_panic() {
    let data = std::fs::read(assets_path().join("chicken.djvu")).unwrap();
    let doc = DjVuDocument::parse(&data).unwrap();
    let page = doc.page(0).unwrap();
    let s = format!("{page:?}");
    assert!(s.contains("DjVuPage"));
}

#[test]
fn page_index_returns_zero_for_first_page() {
    let data = std::fs::read(assets_path().join("chicken.djvu")).unwrap();
    let doc = DjVuDocument::parse(&data).unwrap();
    let page = doc.page(0).unwrap();
    assert_eq!(page.index(), 0);
}

#[test]
fn page_text_returns_some_for_text_page() {
    let data = std::fs::read(assets_path().join("colorbook.djvu")).unwrap();
    let doc = DjVuDocument::parse(&data).unwrap();
    let page = doc.page(0).unwrap();
    let t = page.text().unwrap();
    assert!(t.is_some(), "colorbook page 0 should have text");
}

/// Out-of-range page index returns None.
#[test]
fn page_byte_range_out_of_range() {
    let data = std::fs::read(assets_path().join("chicken.djvu")).expect("chicken.djvu must exist");
    let doc = DjVuDocument::parse(&data).expect("parse must succeed");
    assert!(doc.page_byte_range(99).is_none());
}

/// MmapDocument opens a file and parses identically to in-memory parse.
#[test]
#[cfg(feature = "mmap")]
fn mmap_document_matches_parse() {
    let path = assets_path().join("chicken.djvu");
    let mmap_doc = MmapDocument::open(&path).expect("mmap open should succeed");
    let data = std::fs::read(&path).expect("read should succeed");
    let mem_doc = DjVuDocument::parse(&data).expect("parse should succeed");

    assert_eq!(mmap_doc.page_count(), mem_doc.page_count());
    for i in 0..mmap_doc.page_count() {
        let mp = mmap_doc.page(i).unwrap();
        let pp = mem_doc.page(i).unwrap();
        assert_eq!(mp.width(), pp.width());
        assert_eq!(mp.height(), pp.height());
        assert_eq!(mp.dpi(), pp.dpi());
    }
}

#[test]
fn extract_foreground_returns_none_when_no_fg44() {
    // JB2-only page has no FG44 chunks — extract_foreground returns Ok(None).
    let data = std::fs::read(assets_path().join("boy_jb2.djvu")).unwrap();
    let doc = DjVuDocument::parse(&data).unwrap();
    let fg = doc.page(0).unwrap().extract_foreground().unwrap();
    assert!(fg.is_none());
}

#[test]
fn metadata_returns_none_for_doc_without_meta_chunk() {
    let data = std::fs::read(assets_path().join("chicken.djvu")).unwrap();
    let doc = DjVuDocument::parse(&data).unwrap();
    let meta = doc.metadata().unwrap();
    // chicken.djvu has no METa/METz chunk
    assert!(meta.is_none());
}

#[test]
fn all_chunks_returns_matching_chunks() {
    let data = std::fs::read(assets_path().join("chicken.djvu")).unwrap();
    let doc = DjVuDocument::parse(&data).unwrap();
    // INFO is a global chunk for single-page DJVU
    let info = doc.all_chunks(b"INFO");
    assert!(!info.is_empty());
    // Non-existent chunk returns empty
    let none = doc.all_chunks(b"XXXX");
    assert!(none.is_empty());
}

#[test]
fn chunk_ids_returns_nonempty_for_djvu() {
    let data = std::fs::read(assets_path().join("chicken.djvu")).unwrap();
    let doc = DjVuDocument::parse(&data).unwrap();
    let ids = doc.chunk_ids();
    assert!(!ids.is_empty());
}

#[test]
#[cfg(feature = "mmap")]
fn mmap_open_indirect_on_bundled_doc_succeeds() {
    let path = assets_path().join("chicken.djvu");
    let doc = MmapDocument::open_indirect(&path).expect("open_indirect should work on bundled");
    assert!(doc.page_count() > 0);
}

#[test]
#[cfg(feature = "mmap")]
fn mmap_document_method_and_deref_are_reachable() {
    let path = assets_path().join("chicken.djvu");
    let mmap_doc = MmapDocument::open(&path).expect("mmap open should succeed");
    // document() accessor (line 1128-1129)
    assert!(mmap_doc.document().page_count() > 0);
    // Deref to &DjVuDocument (lines 1146-1147)
    let inner: &DjVuDocument = &mmap_doc;
    assert!(inner.page_count() > 0);
}

/// `advise_page_willneed` is a best-effort hint: it must not error on a
/// real bundled document and must be a harmless no-op for an
/// out-of-range index (COLD_OPEN B6).
#[test]
#[cfg(all(feature = "mmap", unix))]
fn mmap_advise_page_willneed_in_range_and_out_of_range() {
    let path = assets_path().join("chicken.djvu");
    let mmap_doc = MmapDocument::open(&path).expect("mmap open should succeed");
    mmap_doc
        .advise_page_willneed(0)
        .expect("advise on page 0 should not error");
    // Out of range: page_byte_range returns None, so this must be a no-op
    // Ok(()), not an error.
    mmap_doc
        .advise_page_willneed(9_999)
        .expect("advise on an out-of-range page must be a harmless no-op");
}

/// `into_document` must yield a document that still renders correctly —
/// the lazily-constructed pages hold their own `Arc` clone of the
/// mapping, so dropping `MmapDocument`'s own reference must not unmap the
/// file out from under them (COLD_OPEN B6/B7 prerequisite).
#[test]
#[cfg(feature = "mmap")]
fn mmap_into_document_pages_still_render_after_wrapper_dropped() {
    let path = assets_path().join("chicken.djvu");
    let mmap_doc = MmapDocument::open(&path).expect("mmap open should succeed");
    let doc = mmap_doc.into_document();
    let page = doc.page(0).expect("page 0 should exist");
    let pm = crate::djvu_render::render_pixmap(
        page,
        &crate::djvu_render::RenderOptions {
            width: page.width() as u32,
            height: page.height() as u32,
            ..crate::djvu_render::RenderOptions::default()
        },
    )
    .expect("render after into_document should succeed");
    assert!(pm.width > 0 && pm.height > 0);
}

/// `prefetch_page` must actually warm the page's render caches before the
/// caller does a synchronous render (COLD_OPEN B7). Not a timing
/// assertion (that's what the `cold_open_bench` example measures) — just
/// correctness: the background decode must land in the same cache a
/// subsequent `render_pixmap` reads, and out-of-range indices must be a
/// no-op rather than a panic.
#[test]
#[cfg(all(feature = "mmap", feature = "parallel"))]
fn prefetch_page_warms_cache_and_ignores_out_of_range() {
    let path = assets_path().join("chicken.djvu");
    let mmap_doc = MmapDocument::open(&path).expect("mmap open should succeed");
    let doc = Arc::new(mmap_doc.into_document());

    doc.prefetch_page(9_999); // out of range: must not panic
    doc.prefetch_page(0);

    // Give the background task a moment to finish (this test only checks
    // correctness, not latency — a generous sleep avoids flakiness).
    std::thread::sleep(std::time::Duration::from_millis(200));

    let page = doc.page(0).unwrap();
    // Cache should already be warm: render_layers() bytes > 0 without us
    // having called any decoded_* accessor on this thread ourselves.
    assert!(
        page.render_cache_bytes() > 0,
        "prefetch_page should have populated the render cache"
    );

    // A subsequent render must still succeed and be unaffected.
    let pm = crate::djvu_render::render_pixmap(
        page,
        &crate::djvu_render::RenderOptions {
            width: page.width() as u32,
            height: page.height() as u32,
            ..crate::djvu_render::RenderOptions::default()
        },
    )
    .expect("render after prefetch should succeed");
    assert!(pm.width > 0 && pm.height > 0);
}

#[test]
fn metadata_returns_some_for_doc_with_meta_chunk() {
    // Build a synthetic FORM:DJVU containing an INFO chunk and a METa chunk.
    use crate::iff::{Chunk, DjvuFile, emit};
    use crate::metadata::{DjVuMetadata, encode_metadata};

    let info = make_info(100, 100);
    let meta = DjVuMetadata {
        author: Some("TestAuthor".into()),
        ..DjVuMetadata::default()
    };
    let meta_bytes = encode_metadata(&meta);
    if meta_bytes.is_empty() {
        return; // encode returned empty — nothing to test
    }

    let file = DjvuFile {
        root: Chunk::Form {
            secondary_id: *b"DJVU",
            length: 0, // emit recalculates
            children: vec![
                Chunk::Leaf {
                    id: *b"INFO",
                    data: info,
                },
                Chunk::Leaf {
                    id: *b"METa",
                    data: meta_bytes,
                },
            ],
        },
    };
    let bytes = emit(&file);
    let doc = DjVuDocument::parse(&bytes).expect("parse should succeed");
    let m = doc.metadata().expect("metadata() should not error");
    assert!(
        m.is_some(),
        "metadata should be Some for a doc with METa chunk"
    );
    assert_eq!(m.unwrap().author.as_deref(), Some("TestAuthor"));
}

#[test]
fn extract_mask_uses_inline_djbz_when_present() {
    // Build a page with both Sjbz (using shared shapes) and an inline Djbz.
    // This hits the `find_chunk(b"Djbz")` branch in extract_mask (lines 535-537).
    use crate::jb2_encode::{cluster_shared_symbols, encode_jb2_dict_with_shared, encode_jb2_djbz};

    let mut shape = crate::bitmap::Bitmap::new(8, 8);
    shape.set_black(2, 2);
    shape.set_black(3, 3);
    let shapes = cluster_shared_symbols(&[shape.clone(), shape.clone()], 2);
    if shapes.is_empty() {
        return; // no shared shapes; skip
    }
    let djbz_data = encode_jb2_djbz(&shapes);
    let sjbz_data = encode_jb2_dict_with_shared(&shape, &shapes);

    let page = page_with_chunks(&[(b"Djbz", &djbz_data), (b"Sjbz", &sjbz_data)]);
    let result = page.extract_mask();
    assert!(
        result.is_ok(),
        "extract_mask with inline Djbz should succeed"
    );
}

#[test]
fn extract_mask_indexed_uses_inline_djbz_when_present() {
    // Same as above but for extract_mask_indexed (lines 561-563).
    use crate::jb2_encode::{cluster_shared_symbols, encode_jb2_dict_with_shared, encode_jb2_djbz};

    let mut shape = crate::bitmap::Bitmap::new(8, 8);
    shape.set_black(2, 2);
    shape.set_black(3, 3);
    let shapes = cluster_shared_symbols(&[shape.clone(), shape.clone()], 2);
    if shapes.is_empty() {
        return;
    }
    let djbz_data = encode_jb2_djbz(&shapes);
    let sjbz_data = encode_jb2_dict_with_shared(&shape, &shapes);

    let page = page_with_chunks(&[(b"Djbz", &djbz_data), (b"Sjbz", &sjbz_data)]);
    let result = page.extract_mask_indexed();
    assert!(
        result.is_ok(),
        "extract_mask_indexed with inline Djbz should succeed"
    );
}

/// NAVM with BZZ-decoded payload shorter than 2 bytes returns Ok([]).
#[test]
fn parse_navm_bookmarks_short_decoded_returns_empty() {
    use crate::bzz_encode::bzz_encode;
    // Encode a single byte — decoded is 1 byte < 2 → line 1248
    let bzz = bzz_encode(b"x");
    let chunk = crate::iff::IffChunk {
        id: *b"NAVM",
        data: &bzz,
    };
    let result = parse_navm_bookmarks(&[chunk]).unwrap();
    assert!(
        result.is_empty(),
        "NAVM with decoded < 2 bytes must yield empty bookmarks"
    );
}

/// NAVM with total_count > 0 but no actual entries → truncated entry error.
#[test]
fn parse_navm_bookmarks_truncated_entry_returns_error() {
    use crate::bzz_encode::bzz_encode;
    // Declare total_count = 1 (2 bytes) but no bookmark data follows → line 1281
    let payload = vec![0x00, 0x01]; // total_count = 1
    let bzz = bzz_encode(&payload);
    let chunk = crate::iff::IffChunk {
        id: *b"NAVM",
        data: &bzz,
    };
    let result = parse_navm_bookmarks(&[chunk]);
    assert!(
        result.is_err(),
        "NAVM with declared count > 0 but no entry data must error"
    );
}

/// NAVM bookmark title with CP1252 bytes (0x96 en dash — DjVuLibre on
/// Windows) must decode leniently instead of aborting the open (#524).
#[test]
fn parse_navm_bookmarks_cp1252_title_is_lenient() {
    use crate::bzz_encode::bzz_encode;
    // [total_count u16 = 1][n_children u8 = 0]
    // [title: u24 len + bytes][url: u24 len + bytes]
    let title = b"Chapter 1 \x96 Intro";
    let mut payload = vec![0x00, 0x01, 0x00];
    payload.extend_from_slice(&[0x00, 0x00, title.len() as u8]);
    payload.extend_from_slice(title);
    payload.extend_from_slice(&[0x00, 0x00, 0x02]);
    payload.extend_from_slice(b"#1");
    let bzz = bzz_encode(&payload);
    let chunk = crate::iff::IffChunk {
        id: *b"NAVM",
        data: &bzz,
    };
    let bookmarks = parse_navm_bookmarks(&[chunk]).expect("CP1252 title must not abort");
    assert_eq!(bookmarks.len(), 1);
    assert_eq!(bookmarks[0].title, "Chapter 1 \u{2013} Intro");
    assert_eq!(bookmarks[0].url, "#1");
}

/// NAVM entry whose n_children byte is present but the title string's 3-byte
/// length prefix is cut off → read_navm_str returns Malformed (line 1313).
#[test]
fn parse_navm_bookmarks_string_length_truncated_returns_error() {
    use crate::bzz_encode::bzz_encode;
    // Decoded layout: [total_count u16 = 1][n_children u8 = 0]
    // After reading n_children (pos=3), read_navm_str needs 3 more bytes
    // for the length prefix but data.len()=3 → 3+3>3 → Malformed (line 1313).
    let payload = vec![0x00, 0x01, 0x00]; // total_count=1, n_children=0
    let bzz = bzz_encode(&payload);
    let chunk = crate::iff::IffChunk {
        id: *b"NAVM",
        data: &bzz,
    };
    let result = parse_navm_bookmarks(&[chunk]);
    assert!(
        result.is_err(),
        "NAVM with truncated string length must error"
    );
}

/// Indirect DJVM with a shared DJVI component entry: the shared entry must
/// be skipped (line 876 `continue`) and the page resolved via the resolver.
#[test]
fn indirect_djvm_with_shared_djvi_entry_skips_to_page() {
    use crate::dirm::DirmPayload;
    let chicken_data =
        std::fs::read(assets_path().join("chicken.djvu")).expect("chicken.djvu must exist");

    // Build DIRM: entry 0 = Shared (flag=0x00), entry 1 = Page (flag=0x01)
    let dirm_payload = DirmPayload::build_indirect(&[
        crate::dirm::DirmComponent::new(crate::dirm::DirmComponentKind::Shared, "shared.djvi"),
        crate::dirm::DirmComponent::new(crate::dirm::DirmComponentKind::Page, "page.djvu"),
    ]);
    let dirm_data = dirm_payload.encode();
    let djvm_data = build_djvm_with_dirm(&dirm_data);

    let resolver = |name: &str| -> Result<Vec<u8>, DocError> {
        if name == "page.djvu" {
            Ok(chicken_data.clone())
        } else {
            Err(DocError::IndirectResolve(name.to_string()))
        }
    };

    let doc = DjVuDocument::parse_with_resolver(&djvm_data, Some(resolver))
        .expect("indirect DJVM with shared entry must parse");
    assert_eq!(doc.page_count(), 1);
    let page = doc.page(0).unwrap();
    assert_eq!(page.width(), 181);
}

/// The typed resolver sees shared entries as well as pages, and a resolved
/// DJVI dictionary is connected to the page through its INCL reference.
#[test]
fn typed_indirect_resolver_loads_shared_djvi_component() {
    use std::cell::RefCell;

    use crate::dirm::DirmPayload;
    use crate::iff::{Chunk, EmitPart};

    let chicken_data =
        std::fs::read(assets_path().join("chicken.djvu")).expect("chicken.djvu exists");

    // Add an INCL reference to the otherwise ordinary fixture page.
    let mut page_file = crate::iff::parse(&chicken_data).expect("parse page fixture");
    match &mut page_file.root {
        Chunk::Form {
            secondary_id,
            children,
            ..
        } if secondary_id == b"DJVU" => {
            children.insert(
                1,
                Chunk::Leaf {
                    id: *b"INCL",
                    data: b"shared.djvi".to_vec(),
                },
            );
        }
        _ => panic!("fixture must be FORM:DJVU"),
    }
    let page_bytes = crate::iff::emit(&page_file);

    let dict_chunk = Chunk::Leaf {
        id: *b"Djbz",
        data: vec![0x01, 0x02],
    };
    let shared_bytes = crate::iff::partial_emit(*b"DJVI", &[EmitPart::Chunk(&dict_chunk)])
        .expect("shared component fits");
    let thumbnail_bytes = crate::iff::partial_emit(*b"THUM", &[]).expect("thumbnail fits");

    let dirm = DirmPayload::build_indirect(&[
        crate::dirm::DirmComponent::new(crate::dirm::DirmComponentKind::Shared, "shared.djvi"),
        crate::dirm::DirmComponent::new(crate::dirm::DirmComponentKind::Page, "page.djvu"),
        crate::dirm::DirmComponent::new(crate::dirm::DirmComponentKind::Thumbnail, "thumb.thum"),
    ]);
    let dirm_chunk = Chunk::Leaf {
        id: *b"DIRM",
        data: dirm.encode(),
    };
    let djvm =
        crate::iff::partial_emit(*b"DJVM", &[EmitPart::Chunk(&dirm_chunk)]).expect("index fits");

    let seen = RefCell::new(Vec::new());
    let resolver = |component: &ComponentId| {
        seen.borrow_mut().push(component.clone());
        match component.name.as_str() {
            "shared.djvi" => Ok(shared_bytes.clone()),
            "page.djvu" => Ok(page_bytes.clone()),
            "thumb.thum" => Ok(thumbnail_bytes.clone()),
            _ => Err(ComponentResolveError::Missing {
                component: component.clone(),
            }),
        }
    };

    let doc = DjVuDocument::parse_with_component_resolver(&djvm, &resolver)
        .expect("typed indirect parse");
    assert_eq!(doc.page_count(), 1);
    assert!(doc.pages[0].shared_djbz.is_some());
    assert_eq!(
        seen.borrow().as_slice(),
        &[
            ComponentId::new("shared.djvi", ComponentKind::Shared),
            ComponentId::new("page.djvu", ComponentKind::Page),
            ComponentId::new("thumb.thum", ComponentKind::Thumbnail),
        ]
    );
}

/// parse_from_dir with a DIRM component named as an absolute path (line 1040).
#[test]
fn parse_from_dir_resolves_absolute_component_path() {
    use crate::dirm::DirmPayload;
    use crate::iff::{self as iff_mod, Chunk, EmitPart};

    // Write a single-page DJVU to a temp file at an absolute path.
    let chicken =
        std::fs::read(assets_path().join("chicken.djvu")).expect("chicken.djvu must exist");
    let tmp_dir = std::env::temp_dir();
    let abs_name = tmp_dir.join("djvu_rs_test_abs_component.djvu");
    std::fs::write(&abs_name, &chicken).expect("write tmp component");
    let abs_name_str = abs_name.to_str().unwrap().to_string();

    let dirm_payload = DirmPayload::build_indirect(&[crate::dirm::DirmComponent::new(
        crate::dirm::DirmComponentKind::Page,
        &abs_name_str,
    )]);
    let dirm = Chunk::Leaf {
        id: *b"DIRM",
        data: dirm_payload.encode(),
    };
    let djvm = iff_mod::partial_emit(*b"DJVM", &[EmitPart::Chunk(&dirm)]).expect("fits within u32");

    let doc = DjVuDocument::parse_from_dir(&djvm, &tmp_dir)
        .expect("absolute-path component must resolve");
    assert_eq!(doc.page_count(), 1);
    let _ = std::fs::remove_file(&abs_name);
}

/// parse_single_page_with_shared: form type is not DJVU → NotDjVu error (line 908).
#[cfg(all(feature = "std", feature = "async"))]
#[test]
fn parse_single_page_with_shared_wrong_form_type_returns_not_djvu() {
    use crate::iff::{self as iff_mod, Chunk, DjvuFile};

    let bytes = iff_mod::emit(&DjvuFile {
        root: Chunk::Form {
            secondary_id: *b"DJVI",
            length: 0,
            children: vec![],
        },
    });
    let err = DjVuDocument::parse_single_page_with_shared(&bytes, 0, None)
        .expect_err("FORM:DJVI must not be accepted as a page");
    assert!(
        matches!(err, DocError::NotDjVu(_)),
        "expected NotDjVu, got {err:?}"
    );
}

/// DIRM offset points outside the file bytes, so the byte-range lookup
/// for the page fails and `page_byte_ranges.clear()` (line 859) fires.
/// The document still parses successfully (the IFF tree is intact); the
/// page is accessible but `page_byte_range` returns None.
#[test]
fn bundled_djvm_out_of_bounds_dirm_offset_clears_page_byte_ranges() {
    use crate::dirm::DirmPayload;
    use crate::iff::{self as iff_mod, Chunk, EmitPart};

    let chicken =
        std::fs::read(assets_path().join("chicken.djvu")).expect("chicken.djvu must exist");

    // Build a bundled DIRM with one Page entry but set its offset to a value
    // far beyond the end of the file so the byte-range lookup fails.
    let mut dirm_payload = DirmPayload::build_bundled(&[crate::dirm::DirmComponent::new(
        crate::dirm::DirmComponentKind::Page,
        "p.djvu",
    )]);
    dirm_payload.offsets[0] = 0xFFFF_FFFF; // points well outside the file
    let dirm_data = dirm_payload.encode();

    let dirm = Chunk::Leaf {
        id: *b"DIRM",
        data: dirm_data,
    };
    // Strip the 4-byte AT&T magic from chicken.djvu to get the bare FORM bytes.
    let form_bytes = chicken
        .strip_prefix(b"AT&T")
        .expect("chicken.djvu must start with AT&T");

    let djvm = iff_mod::partial_emit(
        *b"DJVM",
        &[EmitPart::Chunk(&dirm), EmitPart::Verbatim(form_bytes)],
    )
    .expect("fits within u32");

    let doc = DjVuDocument::parse(&djvm).expect("DJVM with bad offset must still parse");
    assert_eq!(doc.page_count(), 1, "page must still be accessible");
    // page_byte_range is cleared because the offset was out of bounds.
    assert!(
        doc.page_byte_range(0).is_none(),
        "page_byte_range must be None when DIRM offset is out of bounds"
    );
}

/// Legacy FORM:BM44 parses as a one-page grayscale IW44 document (#683).
#[test]
fn legacy_bm44_parses_as_one_page() {
    let data = std::fs::read(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/legacy_bm44.djvu"),
    )
    .expect("legacy_bm44.djvu must exist");
    let doc = DjVuDocument::parse(&data).expect("BM44 must parse");
    assert_eq!(doc.page_count(), 1);
    let page = doc.page(0).unwrap();
    assert_eq!(page.dimensions(), (32, 32));
    assert_eq!(page.dpi(), 100);
    assert_eq!(page.bg44_chunks().len(), 3);
}

/// Legacy FORM:PM44 parses as a one-page color IW44 document (#683).
#[test]
fn legacy_pm44_parses_as_one_page() {
    let data = std::fs::read(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/legacy_pm44.djvu"),
    )
    .expect("legacy_pm44.djvu must exist");
    let doc = DjVuDocument::parse(&data).expect("PM44 must parse");
    assert_eq!(doc.page_count(), 1);
    let page = doc.page(0).unwrap();
    assert_eq!(page.dimensions(), (181, 240));
    assert_eq!(page.dpi(), 100);
    assert!(!page.bg44_chunks().is_empty());
}

/// Empty BM44 body is a typed missing-chunk error, not a panic.
#[test]
fn legacy_bm44_empty_is_typed_error() {
    let data = crate::iff::partial_emit(*b"BM44", &[]).expect("fits within u32");
    let err = DjVuDocument::parse(&data).expect_err("empty BM44 must fail");
    assert!(matches!(err, DocError::MissingChunk("BM44")), "got {err:?}");
}

/// Truncated first IW44 header fails closed.
#[test]
fn legacy_bm44_truncated_header_is_typed_error() {
    use crate::iff::{Chunk, EmitPart};
    let chunk = Chunk::Leaf {
        id: *b"BM44",
        data: vec![0, 1, 0x81],
    };
    let data =
        crate::iff::partial_emit(*b"BM44", &[EmitPart::Chunk(&chunk)]).expect("fits within u32");
    let err = DjVuDocument::parse(&data).expect_err("truncated BM44 must fail");
    assert!(matches!(err, DocError::Malformed(_)), "got {err:?}");
}

/// FORM:BM44 with a color IW44 bitstream is rejected.
#[test]
fn legacy_bm44_rejects_color_bitstream() {
    use crate::iff::{Chunk, EmitPart};
    // Color major byte 0x01, 8x8.
    let payload = vec![0, 1, 0x01, 2, 0, 8, 0, 8, 0];
    let chunk = Chunk::Leaf {
        id: *b"BM44",
        data: payload,
    };
    let data =
        crate::iff::partial_emit(*b"BM44", &[EmitPart::Chunk(&chunk)]).expect("fits within u32");
    let err = DjVuDocument::parse(&data).expect_err("color BM44 must fail");
    assert!(matches!(err, DocError::Malformed(_)), "got {err:?}");
}
