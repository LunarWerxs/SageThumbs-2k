//! Finding the preview already inside a RAW or JPEG.
//! A camera file carries several JPEGs and only one of them is the picture
//! a person wants; these pin which one wins and that hostile input cannot
//! panic the parser.

use super::*;

#[test]
fn jpeg_span_len_ignores_ffd9_in_metadata() {
    // The APP0 payload contains a stray `FF D9` (looks like EOI). The span must
    // still reach the REAL EOI at the end, not stop early inside the metadata.
    let jpg = mini_jpeg(&[0xFF, 0xD9, 0x11, 0x22], 8);
    assert_eq!(jpeg_span_len(&jpg, 0), Some(jpg.len()));
    // Not a JPEG at all → None (no panic).
    assert!(jpeg_span_len(&[0u8, 1, 2, 3], 0).is_none());
}

/// A structurally perfect JPEG that nothing can decode must never win the pick.
///
/// This is the Canon CR2 shape, and it cost the whole RAW fast tier before it was found: a
/// CR2 stores its compressed sensor data as a ~20 MB LOSSLESS JPEG (SOF3) with a valid SOI,
/// a valid marker chain and a valid EOI. "Largest embedded JPEG" therefore picked the sensor
/// data over the real display preview, and both the `image` crate and WIC rejected it ("the
/// image header is unrecognized"), so `decode_raw_preview` failed outright and a Canon RAW
/// fell all the way through to a ~900 ms WIC demosaic. Measured after the fix: 76 ms.
#[test]
fn largest_embedded_jpeg_skips_lossless_frames_like_a_cr2_sensor_stream() {
    let preview = mini_jpeg_sof(0xC0, 40 * 1024); // baseline — the real display preview
    let sensor = mini_jpeg_sof(0xC3, 400 * 1024); // lossless — ten times bigger, undecodable
    let mut raw = vec![0u8; 32];
    let off = raw.len();
    raw.extend_from_slice(&preview);
    raw.extend_from_slice(&[0xAB; 16]);
    raw.extend_from_slice(&sensor);
    let pick = largest_embedded_jpeg(&raw, MIN_RAW_PREVIEW).expect("the baseline preview");
    assert_eq!(
        pick,
        &raw[off..off + preview.len()],
        "the lossless frame is larger and must still lose — size is not the tiebreak when the \
         bigger candidate cannot be decoded at all"
    );

    // And when the sensor stream is the ONLY thing there, the tier must decline rather than
    // hand a decoder bytes it will reject: declining lets WIC/magick demosaic properly.
    let mut sensor_only = vec![0u8; 32];
    sensor_only.extend_from_slice(&mini_jpeg_sof(0xC3, 400 * 1024));
    assert!(
        largest_embedded_jpeg(&sensor_only, MIN_RAW_PREVIEW).is_none(),
        "a RAW with no decodable preview must fall through to the demosaic tiers"
    );

    // The span itself is still measured for a skipped frame — that is what lets the scan step
    // OVER it rather than re-entering its entropy data looking for more SOI markers.
    let lossless = mini_jpeg_sof(0xC3, 1024);
    assert_eq!(
        crate::container::jpeg_span(&lossless, 0),
        Some((lossless.len(), Some(0xC3)))
    );
    assert!(!crate::container::jpeg_sof_is_decodable(0xC3));
    for good in [0xC0u8, 0xC1, 0xC2] {
        assert!(crate::container::jpeg_sof_is_decodable(good));
    }
}

#[test]
fn largest_embedded_jpeg_prefers_the_real_preview() {
    // A fake RAW: leading header junk, a tiny thumb (< MIN_RAW_PREVIEW), junk, then
    // a real preview (≥ MIN_RAW_PREVIEW). The big one must win; bytes must match.
    let thumb = mini_jpeg(&[], 64); // ~80 B
    let preview = mini_jpeg(&[], 20 * 1024); // ~20 KB
    let mut raw = vec![0u8; 48]; // TIFF-ish header bytes (no 0xFF)
    raw.extend_from_slice(&thumb);
    raw.extend_from_slice(&[0xAB; 32]); // inter-image junk
    let off = raw.len();
    raw.extend_from_slice(&preview);
    raw.extend_from_slice(&[0xCD; 16]); // trailing junk
    let pick = largest_embedded_jpeg(&raw, MIN_RAW_PREVIEW).expect("should find the preview");
    assert_eq!(pick, &raw[off..off + preview.len()]);
}

#[test]
fn largest_embedded_jpeg_rejects_thumb_only_raw() {
    // A RAW whose ONLY embedded JPEG is a tiny thumb → None, so decode_any falls
    // through to the WIC/magick demosaic for a full-resolution result.
    let mut raw = vec![0u8; 48];
    raw.extend_from_slice(&mini_jpeg(&[], 64));
    assert!(largest_embedded_jpeg(&raw, MIN_RAW_PREVIEW).is_none());
}

#[test]
fn largest_embedded_jpeg_prefers_capped_over_fullres() {
    // RAW with a screen-size preview (≤ cap) AND a full-res preview (> cap): the
    // capped one wins — fast to decode, ample for a thumbnail/convert. (This is the
    // .pef/.cr2 case: don't decode a 35 MP monster to make a 256px icon.)
    let medium = mini_jpeg(&[], 100 * 1024); // ~100 KB, within range
    let fullres = mini_jpeg(&[], PREVIEW_SOFT_MAX + 64 * 1024); // over the cap
    let mut raw = vec![0u8; 32];
    let moff = raw.len();
    raw.extend_from_slice(&medium);
    raw.extend_from_slice(&[0xAB; 16]);
    raw.extend_from_slice(&fullres);
    let pick =
        largest_embedded_jpeg(&raw, MIN_RAW_PREVIEW).expect("should pick the capped preview");
    assert_eq!(pick, &raw[moff..moff + medium.len()]);
}

#[test]
fn largest_embedded_jpeg_falls_back_to_oversized() {
    // When the ONLY real preview is over the cap, use it anyway (still beats a
    // demosaic, and correctness over speed).
    let fullres = mini_jpeg(&[], PREVIEW_SOFT_MAX + 32 * 1024);
    let mut raw = vec![0u8; 32];
    let off = raw.len();
    raw.extend_from_slice(&fullres);
    let pick = largest_embedded_jpeg(&raw, MIN_RAW_PREVIEW).expect("oversized preview still used");
    assert_eq!(pick, &raw[off..off + fullres.len()]);
}

#[test]
fn raw_corpus_samples_show_via_embedded_jpeg() {
    // The clean-box guarantee: every camera-RAW the corpus ships should yield a
    // thumbnail from its EMBEDDED JPEG alone — pure-Rust, no WIC / Microsoft RAW
    // Image Extension / ImageMagick — once the lenient last-resort floor is allowed.
    // Diagnostic (prints per-format coverage); skips when no corpus is present.
    // Prefer the REAL-content corpus (`test-corpus-real`) — the plain `test-corpus`
    // RAW entries are synthetic stubs with no embedded preview, which would mislead.
    let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let dir = ["test-corpus-real", "test-corpus"]
        .into_iter()
        .map(|d| base.join(d))
        .find(|p| p.exists());
    let Some(dir) = dir else {
        eprintln!("no test corpus present — skipping RAW coverage check");
        return;
    };
    eprintln!("RAW coverage from: {}", dir.display());
    let exts = [
        "cr2", "cr3", "nef", "arw", "raf", "orf", "rw2", "dng", "pef", "srw", "3fr", "dcr", "fff",
        "iiq", "kdc", "mos", "mrw", "nrw", "x3f",
    ];
    let mut no_preview = Vec::new();
    for ext in exts {
        let p = dir.join(format!("sample.{ext}"));
        let Ok(bytes) = std::fs::read(&p) else {
            continue;
        };
        let strict = largest_embedded_jpeg(&bytes, MIN_RAW_PREVIEW).map(|s| s.len());
        let lenient = largest_embedded_jpeg(&bytes, LENIENT_RAW_PREVIEW).map(|s| s.len());
        eprintln!("  .{ext:<4} strict={strict:?} lenient={lenient:?}");
        // Invariant: the lenient floor is below the strict one, so it must find every
        // preview the strict tier does. A regression in the scanner would break this.
        assert!(
            strict.is_none() || lenient.is_some(),
            ".{ext}: lenient lost a strict preview"
        );
        if lenient.is_none() {
            no_preview.push(ext);
        }
    }
    // Anything left blank is a true no-embedded-preview RAW (needs a real demosaic via
    // WIC/the Microsoft RAW extension) — list it so we know exactly what's NOT covered
    // pure-Rust on a clean install, rather than silently assuming full coverage.
    if !no_preview.is_empty() {
        eprintln!("RAW with NO embedded JPEG (need WIC/demosaic on a clean box): {no_preview:?}");
    }
}

#[test]
fn raw_preview_parsers_are_panic_safe_on_hostile_input() {
    // These parsers run in Explorer's host under panic=abort, so a panic on a
    // malformed file would abort the shell. None of these may panic OR hang
    // (the test completing is the assertion); they may return None or Some.
    let mut many_sois = Vec::new();
    for _ in 0..300 {
        many_sois.extend_from_slice(&[0xFF, 0xD8, 0xFF, 0x00]); // fake SOIs → 64-cap
    }
    let cases: Vec<Vec<u8>> = vec![
        vec![],                                   // empty
        vec![0xFF],                               // single byte
        vec![0xFF, 0xD8, 0xFF],                   // SOI then nothing
        vec![0xFF; 8192],                         // 0xFF storm (marker fill)
        vec![0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x02], // SOS, no entropy/EOI (truncated)
        vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x00], // APP0 declared len = 0 (invalid)
        vec![0xFF, 0xD8, 0xFF, 0xE0, 0xFF, 0xFF], // APP0 len overruns the buffer
        vec![0xFF, 0xD8, 0xFF, 0xDA, 0xFF, 0xFF], // SOS len overruns
        many_sois,
    ];
    for c in &cases {
        let _ = jpeg_span_len(c, 0);
        let _ = largest_embedded_jpeg(c, MIN_RAW_PREVIEW);
        let _ = largest_embedded_jpeg(c, LENIENT_RAW_PREVIEW);
        let _ = decode_raw_preview(c, None); // full path (Err on all of these)
    }
}

#[test]
fn full_decode_defers_raw_preview_until_real_decoders_fail() {
    // The fast embedded-JPEG shortcut only runs for files whose signature says RAW
    // container, so a valid 2x2 TGA carrying a large trailing JPEG decodes as the TGA on
    // the thumbnail path and at full fidelity alike: the trailing JPEG is never mistaken
    // for the picture.
    let bytes = tiny_tga_with_trailing_jpeg();
    let early =
        decode_any_with_wic_target(&bytes, RawPreviewOrder::BeforeExternal, true, None).unwrap();
    assert_eq!((early.width(), early.height()), (2, 2));
    let full = decode_full(&bytes).unwrap();
    assert_eq!((full.width(), full.height()), (2, 2));

    // A RAW-looking container (little-endian TIFF magic, an unreadable directory) that no
    // real decoder can open still yields its embedded JPEG on the thumbnail path.
    let mut raw = vec![0x49, 0x49, 0x2A, 0x00];
    raw.extend_from_slice(&[0u8; 60]);
    raw.extend_from_slice(&noisy_jpeg_bytes(192, 192));
    let early =
        decode_any_with_wic_target(&raw, RawPreviewOrder::BeforeExternal, true, None).unwrap();
    assert_eq!((early.width(), early.height()), (192, 192));
}

#[test]
fn embedded_extractor_rejects_non_and_plain_jpegs() {
    // PNG is not a JPEG → no EXIF thumbnail.
    assert!(exif_thumbnail_jpeg(&png_bytes(8, 8, [1, 2, 3, 255])).is_none());
    // Garbage → None, no panic.
    assert!(exif_thumbnail_jpeg(&[0xFF, 0xD8, 0, 1, 2, 3]).is_none());

    // A plain JPEG (no embedded thumbnail) → extractor None, and the
    // use_embedded path falls back to a correct full decode.
    let mut jpg = Vec::new();
    image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
        120,
        80,
        image::Rgba([200, 40, 40, 255]),
    ))
    .write_to(
        &mut std::io::Cursor::new(&mut jpg),
        image::ImageFormat::Jpeg,
    )
    .unwrap();
    assert!(exif_thumbnail_jpeg(&jpg).is_none());
    let d = decode_thumbnail_opts(&jpg, 64, true).unwrap();
    assert!(d.width <= 64 && d.height <= 64 && d.width > 0);
    assert_eq!(d.rgba.len(), (d.width * d.height * 4) as usize);
}

#[test]
fn embedded_extractor_does_not_panic_on_short_exif_segment() {
    // Crafted APP1 whose declared length (6) is too short to hold the full
    // "Exif\0\0" id: the id bytes legitimately exist only by reading past the
    // segment. The pre-fix code raw-sliced &bytes[body_start+6..seg_end] =
    // [12..10] and panicked (start > end), aborting the host under
    // panic=abort. It must now return None cleanly.
    let crafted = [
        0xFF, 0xD8, // SOI
        0xFF, 0xE1, // APP1 marker
        0x00, 0x06, // segment length = 6 (too short for "Exif\0\0")
        b'E', b'x', b'i', b'f', 0x00, 0x00, // id bytes (last two past seg_end)
        0x00, 0x00, // trailer
    ];
    assert!(exif_thumbnail_jpeg(&crafted).is_none());
    // And the full thumbnail path tolerates it (falls back / fails cleanly).
    assert!(decode_thumbnail_opts(&crafted, 64, true).is_err());
}
