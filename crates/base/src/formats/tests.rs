use super::*;

// The category lists under test are the REAL module-scoped consts that
// `category()` uses — not a copy — so the data lives in exactly one place.
const EBOOK: &[&str] = EBOOK_EXTS;
const DOCUMENT: &[&str] = DOCUMENT_EXTS;
const AUDIO: &[&str] = AUDIO_EXTS;
const RAW: &[&str] = RAW_EXTS;
const VIDEO: &[&str] = VIDEO_EXTS;
const ARCHIVE: &[&str] = ARCHIVE_EXTS;

fn in_formats(ext: &str) -> bool {
    FORMATS.iter().any(|(e, _)| *e == ext)
}

/// Every extension a category list names must actually be a registered
/// `FORMATS` entry — otherwise `category()` classifies a phantom extension we
/// never hook. Catches a typo or a `FORMATS` removal that leaves a stale list.
#[test]
fn category_lists_are_subset_of_formats() {
    for (name, list) in [
        ("EBOOK", EBOOK),
        ("DOCUMENT", DOCUMENT),
        ("AUDIO", AUDIO),
        ("RAW", RAW),
        ("VIDEO", VIDEO),
        ("ARCHIVE", ARCHIVE),
    ] {
        for &ext in list {
            assert!(
                in_formats(ext),
                "{name} list names `{ext}`, which is not in FORMATS",
            );
        }
    }
}

/// `category()` must agree with these lists for every real extension, and must
/// never return a non-`Image` verdict for an extension none of the lists claim.
/// The lists here ARE the consts `category()` reads, so the loop's value is the
/// final guard: a known non-Image ext that fell out of its list would default to
/// `Image`, and any list-less ext that somehow classified as non-Image trips the
/// guard below — either way, drift between the two encodings fails the test.
#[test]
fn category_verdict_matches_lists() {
    for &(ext, _) in FORMATS {
        let expected = if EBOOK.contains(&ext) {
            Category::Ebook
        } else if DOCUMENT.contains(&ext) {
            Category::Document
        } else if AUDIO.contains(&ext) {
            Category::Audio
        } else if RAW.contains(&ext) {
            Category::Raw
        } else if VIDEO.contains(&ext) {
            Category::Video
        } else if ARCHIVE.contains(&ext) {
            Category::Archive
        } else {
            Category::Image
        };
        assert!(
            category(ext) == expected,
            "category(\"{ext}\") disagrees with the category lists",
        );
    }
    // Guard: nothing the lists DON'T claim may classify as non-Image. A known
    // non-Image ext that drops out of a list would default to Image and be
    // caught above; this catches the reverse (a list-less ext wrongly typed).
    for &(ext, _) in FORMATS {
        let claimed = EBOOK.contains(&ext)
            || DOCUMENT.contains(&ext)
            || AUDIO.contains(&ext)
            || RAW.contains(&ext)
            || VIDEO.contains(&ext)
            || ARCHIVE.contains(&ext);
        if !claimed {
            assert!(
                category(ext) == Category::Image,
                "category(\"{ext}\") is non-Image but no list claims it",
            );
        }
    }
}

/// Per-category counts are DERIVED from `FORMATS` (not hardcoded), so adding a
/// format updates the totals automatically. We only assert the partition is
/// exhaustive — every `FORMATS` entry lands in exactly one bucket and the
/// buckets sum back to `FORMATS.len()` — plus that each non-Image bucket equals
/// its list length. No magic "179" lives here; it falls out of the table.
#[test]
fn category_counts_partition_formats() {
    let mut n = [0usize; 7];
    for &(ext, _) in FORMATS {
        n[category(ext) as usize] += 1;
    }
    let total: usize = n.iter().sum();
    assert_eq!(total, FORMATS.len(), "counts must partition FORMATS");

    // Each non-Image bucket equals exactly its list's length (no dupes, no gaps).
    assert_eq!(n[Category::Ebook as usize], EBOOK.len(), "Ebook");
    assert_eq!(n[Category::Document as usize], DOCUMENT.len(), "Document");
    assert_eq!(n[Category::Audio as usize], AUDIO.len(), "Audio");
    assert_eq!(n[Category::Raw as usize], RAW.len(), "Camera RAW");
    assert_eq!(n[Category::Video as usize], VIDEO.len(), "Video");
    assert_eq!(n[Category::Archive as usize], ARCHIVE.len(), "Archive");
    // Image is whatever remains — derived, not asserted to a literal.
    let non_image =
        EBOOK.len() + DOCUMENT.len() + AUDIO.len() + RAW.len() + VIDEO.len() + ARCHIVE.len();
    assert_eq!(
        n[Category::Image as usize],
        FORMATS.len() - non_image,
        "Image is the remainder of FORMATS",
    );
}

/// jbig was dropped from FORMATS 2026-07-08 (see the inline comment above FORMATS'
/// `jfif` entry) but must still be swept by register()/unregister(), or a machine
/// that ran a build where jbig was registered keeps an orphaned shellex hook forever.
#[test]
fn removed_extensions_contains_jbig() {
    assert!(
        REMOVED_EXTENSIONS.contains(&"jbig"),
        "jbig was a registered-then-removed extension and must be in REMOVED_EXTENSIONS \
         so register/unregister sweep the stale hook on upgrade",
    );
}

/// `REMOVED_EXTENSIONS` (the historically-dropped exts we sweep on register/unregister)
/// MUST NOT overlap `FORMATS` — otherwise the cleanup would unhook a LIVE format.
#[test]
fn removed_extensions_disjoint_from_formats() {
    for &ext in REMOVED_EXTENSIONS {
        assert!(
            !FORMATS.iter().any(|&(e, _)| e == ext),
            "REMOVED_EXTENSIONS contains \"{ext}\" which is still a live FORMATS entry — \
             the register/unregister cleanup sweep would unhook it",
        );
    }
}

/// `FORMATS` must have no duplicate extensions — a dupe would double-count in
/// the partition and silently mis-size the Options list.
#[test]
fn formats_has_no_duplicate_extensions() {
    for (i, &(ext, _)) in FORMATS.iter().enumerate() {
        for &(other, _) in &FORMATS[i + 1..] {
            assert!(ext != other, "duplicate FORMATS extension `{ext}`");
        }
    }
}

// ---- Capability (audit E03) -----------------------------------------------------

/// Every `FORMATS` entry answers `capability()` with internally-consistent fields -
/// archives are the only non-convertible, listing-only entries; everything else gets
/// the image verbs and shows one picture, not a listing.
#[test]
fn every_formats_entry_has_a_capability() {
    for &(ext, _) in FORMATS {
        let cap = capability(ext);
        if is_archive(ext) {
            assert!(!cap.convertible, "archive `{ext}` must not be convertible");
            assert!(
                cap.preview_listing,
                "archive `{ext}` must be preview_listing"
            );
            assert_eq!(cap.source, Source::ContainedImages, "archive `{ext}`");
        } else {
            assert!(
                cap.convertible,
                "`{ext}` must be convertible (not an archive)"
            );
            assert!(!cap.preview_listing, "`{ext}` must not be preview_listing");
        }
    }
}

/// `WMPHOTO_EXTS`/`HEIF_OS_CODEC_EXTS` must stay subsets of `FORMATS`, same discipline
/// as the category lists above.
#[test]
fn capability_lists_are_subset_of_formats() {
    for &ext in WMPHOTO_EXTS {
        assert!(
            in_formats(ext),
            "WMPHOTO_EXTS names `{ext}`, which is not in FORMATS"
        );
    }
    for &ext in HEIF_OS_CODEC_EXTS {
        assert!(
            in_formats(ext),
            "HEIF_OS_CODEC_EXTS names `{ext}`, which is not in FORMATS"
        );
    }
    for &ext in AV1_OS_CODEC_EXTS {
        assert!(
            in_formats(ext),
            "AV1_OS_CODEC_EXTS names `{ext}`, which is not in FORMATS"
        );
    }
    for &ext in EMBEDDED_PREVIEW_EXTS {
        assert!(
            in_formats(ext),
            "EMBEDDED_PREVIEW_EXTS names `{ext}`, which is not in FORMATS"
        );
        assert_eq!(
            category(ext),
            Category::Image,
            "EMBEDDED_PREVIEW_EXTS names `{ext}`, which is not Category::Image - it \
             already gets its Source from its own category and needs no override"
        );
    }
}

/// Audit E03 #1: `container/mod.rs`'s `extract_cover` dispatch (`try_creative_app_cover`,
/// `try_ebook_and_cad_cover`, `try_misc_cover`, plus the ZIP-family `project::extract`
/// cascade) serves an EMBEDDED preview for exactly `EMBEDDED_PREVIEW_EXTS`, never a full
/// decode - so every one of them must classify as `Source::EmbeddedPreview`, not the
/// `Source::FullDecode` the rest of `Category::Image` gets.
#[test]
fn embedded_preview_exts_are_classified_embedded_preview() {
    for &ext in EMBEDDED_PREVIEW_EXTS {
        assert_eq!(
            capability(ext).source,
            Source::EmbeddedPreview,
            "container-derived `{ext}` must be Source::EmbeddedPreview, not a claimed \
             full decode of the image itself"
        );
    }
}

/// The flip side of the test above: no RAW extension (embedded-JPEG-preview-first by
/// design) and no `EMBEDDED_PREVIEW_EXTS` container extension is ever misclassified as
/// `Source::FullDecode` - the exact false claim audit E03 #1 found live on the website.
#[test]
fn no_raw_or_container_extension_is_full_decode() {
    for &ext in RAW.iter().chain(EMBEDDED_PREVIEW_EXTS) {
        assert_ne!(
            capability(ext).source,
            Source::FullDecode,
            "`{ext}` rides an embedded/carried preview, not a full decode"
        );
    }
}

/// Every video extension depends on Media Foundation, except the ones whose own codecs
/// are decoded by us: `flv` (VP6/Sorenson Spark, audit E03 #5) and the MPEG-1/2 program
/// and elementary stream family (2026-09-17) - `st2k doctor`'s per-file
/// `video_codec_note` says the same thing.
#[test]
fn video_category_maps_to_media_foundation() {
    for &ext in VIDEO {
        let expected = if SELF_DECODED_VIDEO_EXTS.contains(&ext) {
            None
        } else {
            Some(OsCodec::MediaFoundation)
        };
        assert_eq!(
            capability(ext).os_codec,
            expected,
            "video extension `{ext}` os_codec expectation"
        );
    }
}

/// Audit E03 #5, pinned directly: FLV keeps its default icon claim honest - its own
/// codecs never depend on an OS codec that might be missing. The MPEG family joined it
/// on 2026-09-17 (our own MPEG-1/2 decoder), and every self-decoded extension must be a
/// registered video extension.
#[test]
fn flv_os_codec_is_none() {
    assert_eq!(capability("flv").os_codec, None);
    for ext in ["mpg", "mpeg", "m1v", "m2v", "vob"] {
        assert_eq!(capability(ext).os_codec, None, "{ext}");
    }
    for ext in SELF_DECODED_VIDEO_EXTS {
        assert!(
            VIDEO_EXTS.contains(ext),
            "{ext} is self-decoded but not a video ext"
        );
    }
}

/// Audit E03 #2: AVIF has no in-process AV1 decoder (`Cargo.toml`'s `image` feature list
/// excludes it) - its route is the OS's AV1 codec, named `OsCodec::Av1` even though it
/// can't be probed the way `WmPhoto`/`Heif` can (see that variant's doc).
#[test]
fn avif_os_codec_is_av1() {
    assert_eq!(capability("avif").os_codec, Some(OsCodec::Av1));
}

/// Archives never get the image verbs - `verbs::actions::is_image` excludes them so
/// Convert/Rotate never act on an archive's extracted cover.
#[test]
fn archives_are_not_convertible() {
    for &ext in ARCHIVE {
        let cap = capability(ext);
        assert!(!cap.convertible, "archive `{ext}` must not be convertible");
        assert!(
            cap.preview_listing,
            "archive `{ext}` must list its contents"
        );
    }
}

/// One representative extension per `Source` kind, table-driven, pinning both the
/// source and (where applicable) the OS codec dependency.
#[test]
fn spot_check_one_extension_per_source_kind() {
    let cases: &[(&str, Source, Option<OsCodec>)] = &[
        ("png", Source::FullDecode, None),
        ("jxr", Source::FullDecode, Some(OsCodec::WmPhoto)),
        ("heic", Source::FullDecode, Some(OsCodec::Heif)),
        ("avif", Source::FullDecode, Some(OsCodec::Av1)),
        ("cr2", Source::EmbeddedPreview, None),
        ("psd", Source::EmbeddedPreview, None),
        ("apk", Source::EmbeddedPreview, None),
        ("flv", Source::VideoFrame, None),
        ("mp3", Source::CoverArt, None),
        ("wav", Source::CoverArt, None),
        ("epub", Source::CoverOrFirstPage, None),
        ("pdf", Source::CoverOrFirstPage, None),
        ("mp4", Source::VideoFrame, Some(OsCodec::MediaFoundation)),
        ("zip", Source::ContainedImages, None),
    ];
    for &(ext, expected_source, expected_codec) in cases {
        let cap = capability(ext);
        assert_eq!(cap.source, expected_source, "source for `{ext}`");
        assert_eq!(cap.os_codec, expected_codec, "os_codec for `{ext}`");
    }
}

/// Wire-format strings are stable and lowercase - `st2k formats --json` and the
/// website generator both parse these; a rename here is a silent breaking change.
#[test]
fn wire_strings_are_stable_lowercase() {
    for s in [
        Source::FullDecode.as_str(),
        Source::EmbeddedPreview.as_str(),
        Source::CoverArt.as_str(),
        Source::CoverOrFirstPage.as_str(),
        Source::VideoFrame.as_str(),
        Source::ContainedImages.as_str(),
        OsCodec::MediaFoundation.as_str(),
        OsCodec::WmPhoto.as_str(),
        OsCodec::Heif.as_str(),
        OsCodec::Av1.as_str(),
    ] {
        assert_eq!(s, s.to_ascii_lowercase(), "`{s}` must be lowercase");
        assert!(!s.is_empty());
    }
}
