//! Unit tests for the decode pipeline (extracted verbatim from decode.rs).

use super::*;
// The tests exercise internals that now live in the sibling children; glob them in so
// every assertion below still names them exactly as it did pre-split.
use super::readers::*;
// Not imported by the hub any more: only these tests still name the length-only wrapper.
use crate::container::jpeg_span_len;

fn png_bytes(w: u32, h: u32, color: [u8; 4]) -> Vec<u8> {
    let mut img = image::RgbaImage::new(w, h);
    for p in img.pixels_mut() {
        *p = image::Rgba(color);
    }
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
    bytes
}

fn noisy_jpeg_bytes(w: u32, h: u32) -> Vec<u8> {
    let mut img = image::RgbImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let r = ((x * 37 + y * 11) & 0xFF) as u8;
            let g = ((x * 13 + y * 53) & 0xFF) as u8;
            let b = ((x * 97 + y * 3) & 0xFF) as u8;
            img.put_pixel(x, y, image::Rgb([r, g, b]));
        }
    }
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgb8(img)
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Jpeg,
        )
        .unwrap();
    assert!(
        bytes.len() >= MIN_RAW_PREVIEW,
        "test JPEG must be large enough to be a RAW preview"
    );
    bytes
}

fn tiny_tga_with_trailing_jpeg() -> Vec<u8> {
    let mut tga = vec![0u8; 18];
    tga[2] = 2; // uncompressed true-color
    tga[12..14].copy_from_slice(&2u16.to_le_bytes());
    tga[14..16].copy_from_slice(&2u16.to_le_bytes());
    tga[16] = 24; // BGR
    tga[17] = 0x20; // top-left origin
    tga.extend_from_slice(&[
        0, 0, 255, // red
        0, 255, 0, // green
        255, 0, 0, // blue
        255, 255, 255, // white
    ]);
    tga.extend_from_slice(&noisy_jpeg_bytes(192, 192));
    tga
}

#[test]
fn gzip_wrapped_svg_decodes_natively() {
    // `.svgz` (and `.emz`) arrive gzip-wrapped; `decode_image` must inflate and
    // decode the inner bytes. SVG goes through resvg (pure-Rust, no magick), so
    // this exercises the gunzip path end-to-end without the ImageMagick tier.
    use std::io::Write;
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><rect width="16" height="16" fill="red"/></svg>"#;
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(svg).unwrap();
    let gz = enc.finish().unwrap();
    assert_eq!(&gz[..2], &[0x1f, 0x8b], "test payload must be gzip");
    let img = decode_image(&gz).expect("gzipped SVG should decode");
    assert!(
        img.width() > 0 && img.height() > 0,
        "decoded image must be non-empty"
    );
}

#[test]
fn gunzip_bounded_rejects_non_gzip() {
    assert!(gunzip_bounded(b"not gzip at all").is_none());
}

#[test]
fn magick_time_limits_agree() {
    // ImageMagick's own `-limit time` is ELAPSED seconds, so it has to track the external
    // watchdog's WALL backstop, not its CPU budget — pinning it to the CPU number would let
    // magick self-abort a merely-starved decode and reintroduce issue #9 from inside the
    // child. Bump one, this test catches the others (the silent "watchdog waits 120s but
    // magick still kills at 20s" trap).
    assert_eq!(
        limits::MAGICK_TIME_LIMIT.parse::<u64>().unwrap(),
        limits::MAGICK_WALL_SECS,
        "MAGICK_TIME_LIMIT string must equal MAGICK_WALL_SECS",
    );
    assert_eq!(
        MAGICK_TIMEOUT,
        std::time::Duration::from_secs(limits::MAGICK_WALL_SECS)
    );
    assert_eq!(
        MAGICK_CPU_BUDGET,
        std::time::Duration::from_secs(limits::MAGICK_CPU_SECS)
    );
    // That the CPU budget is tighter than the wall backstop is pinned at compile time,
    // by the `const _: () = assert!(...)` beside the constants in decode.rs.
}

#[test]
fn magick_limits_match_policy_xml() {
    // policy.xml ships to disk beside magick.exe, so it can't read the consts at
    // runtime — pin it here. Change a magick `-limit` and you must change
    // scripts/packaging/imagemagick-policy.xml to match (and vice-versa).
    let policy = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/scripts/packaging/imagemagick-policy.xml"
    ))
    .expect("imagemagick-policy.xml must be readable");
    for (name, value) in [
        ("memory", limits::MAGICK_MEMORY_LIMIT),
        ("map", limits::MAGICK_MAP_LIMIT),
        ("time", limits::MAGICK_TIME_LIMIT),
    ] {
        let needle = format!("name=\"{name}\" value=\"{value}\"");
        assert!(
            policy.contains(&needle),
            "imagemagick-policy.xml is missing `{needle}` — it drifted from decode::limits",
        );
    }
}

#[test]
fn fits_box_and_preserves_aspect() {
    // 200x100 -> must fit in 96x96, longest side fills the box -> 96x48.
    let d = decode_thumbnail_opts(&png_bytes(200, 100, [255, 0, 0, 255]), 96, false).unwrap();
    assert!(d.width <= 96 && d.height <= 96);
    assert_eq!((d.width, d.height), (96, 48));
    assert_eq!(d.rgba.len(), (d.width * d.height * 4) as usize);
    assert!(d.rgba[0] > 200 && d.rgba[3] == 255); // still red, opaque
}

#[test]
fn midsize_images_are_enlarged_smoothly_not_nearest() {
    // 100×50 in a 256 box is above the pixel-art threshold (>64px), so it is enlarged to fill
    // the box (issue #25 — Explorer centres an undersized tile rather than scaling it up), and
    // with Lanczos3 rather than the Nearest reserved for sprites: a small PHOTO nearest-scaled
    // is visibly blocky, which is the reason the two paths are separate.
    let d = decode_thumbnail_opts(&png_bytes(100, 50, [0, 255, 0, 255]), 256, false).unwrap();
    assert_eq!((d.width, d.height), (256, 128));
    assert_eq!(d.rgba.len(), (d.width * d.height * 4) as usize);
}

#[test]
fn garbage_bytes_fail_cleanly() {
    assert!(decode_thumbnail_opts(&[0u8, 1, 2, 3, 4, 5, 6, 7], 96, false).is_err());
}

/// A minimal valid JPEG: SOI + APPn (length-prefixed) + SOS + `entropy` bytes of
/// payload (no 0xFF) + EOI. Used to exercise the RAW-preview carver.
fn mini_jpeg(app_payload: &[u8], entropy: usize) -> Vec<u8> {
    let mut v = vec![0xFF, 0xD8]; // SOI
                                  // APP0 segment carrying `app_payload` (length covers the 2 length bytes too).
    let app_len = (app_payload.len() + 2) as u16;
    v.extend_from_slice(&[0xFF, 0xE0]);
    v.extend_from_slice(&app_len.to_be_bytes());
    v.extend_from_slice(app_payload);
    v.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02]); // SOS, header length = 2 (none)
    v.extend(std::iter::repeat_n(0x55, entropy)); // entropy data (contains no 0xFF)
    v.extend_from_slice(&[0xFF, 0xD9]); // EOI
    v
}

#[test]
fn jpeg_span_len_ignores_ffd9_in_metadata() {
    // The APP0 payload contains a stray `FF D9` (looks like EOI). The span must
    // still reach the REAL EOI at the end, not stop early inside the metadata.
    let jpg = mini_jpeg(&[0xFF, 0xD9, 0x11, 0x22], 8);
    assert_eq!(jpeg_span_len(&jpg, 0), Some(jpg.len()));
    // Not a JPEG at all → None (no panic).
    assert!(jpeg_span_len(&[0u8, 1, 2, 3], 0).is_none());
}

/// [`mini_jpeg`] carrying an explicit SOFn frame header, so a test can build the difference
/// between a picture and a pile of sensor readings. `sof` is the marker's low byte: 0xC0 is
/// baseline, 0xC3 is LOSSLESS — the encoding Canon CR2 uses for raw sensor data.
fn mini_jpeg_sof(sof: u8, entropy: usize) -> Vec<u8> {
    let mut v = vec![0xFF, 0xD8]; // SOI
                                  // SOFn. The declared length (0x0B = 11) counts its own two bytes, so exactly 9 must
                                  // follow: precision, height, width, component count, then one 3-byte component spec. Get
                                  // that wrong and `jpeg_span` walks off into the next marker and rejects the whole frame —
                                  // a fixture bug that reads exactly like a code bug.
    v.extend_from_slice(&[
        0xFF, sof, 0x00, 0x0B, // marker + segment length
        0x08, 0x00, 0x10, 0x00, 0x10, 0x01, // 8-bit, 16x16, one component
        0x00, 0x11, 0x00, // component 0: id, sampling 1x1, quant table 0
    ]);
    v.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02]); // SOS, header length = 2 (none)
    v.extend(std::iter::repeat_n(0x55, entropy)); // entropy data (contains no 0xFF)
    v.extend_from_slice(&[0xFF, 0xD9]); // EOI
    v
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
fn metafile_detector_matches_wmf_emf_only() {
    assert!(looks_like_metafile(&[0xD7, 0xCD, 0xC6, 0x9A, 0, 0])); // placeable WMF
    assert!(looks_like_metafile(&[0x01, 0x00, 0x09, 0x00, 0x00, 0x03])); // memory WMF
    let mut emf = vec![0u8; 44];
    emf[0] = 1;
    emf[40..44].copy_from_slice(b" EMF");
    assert!(looks_like_metafile(&emf)); // EMF
                                        // Real rasters must NOT be treated as metafiles (they keep the full budget).
    assert!(!looks_like_metafile(&[0xFF, 0xD8, 0xFF, 0])); // JPEG
    assert!(!looks_like_metafile(&[0x89, b'P', b'N', b'G'])); // PNG
    assert!(!looks_like_metafile(&[0x01, 0x00, 0x09, 0x00, 0x99])); // WMF-ish prefix, wrong byte 4/5
}

#[test]
fn full_decode_defers_raw_preview_until_real_decoders_fail() {
    // The fast RAW-preview tier scans for embedded JPEGs before expensive
    // external decoders on the thumbnail path. Full-fidelity callers must not
    // take that shortcut ahead of a real decoder: this valid 2x2 TGA carries a
    // large trailing JPEG that the early path would otherwise prefer.
    let bytes = tiny_tga_with_trailing_jpeg();
    let early = decode_any(&bytes, RawPreviewOrder::BeforeExternal, true).unwrap();
    assert_eq!((early.width(), early.height()), (192, 192));

    let full = decode_full(&bytes).unwrap();
    assert_eq!((full.width(), full.height()), (2, 2));
}

#[test]
fn animated_gif_decodes_first_frame() {
    use image::codecs::gif::GifEncoder;
    use image::Frame;
    let mut bytes = Vec::new();
    {
        let mut enc = GifEncoder::new(&mut bytes);
        let red = image::RgbaImage::from_pixel(20, 20, image::Rgba([220, 30, 30, 255]));
        let blue = image::RgbaImage::from_pixel(20, 20, image::Rgba([30, 30, 220, 255]));
        enc.encode_frame(Frame::new(red)).unwrap();
        enc.encode_frame(Frame::new(blue)).unwrap();
    }
    let d = decode_thumbnail_opts(&bytes, 96, false).unwrap();
    // 20px sprite Nearest-upscales by an integer factor (96/20 -> 4x = 80px).
    assert_eq!((d.width, d.height), (80, 80));
    assert!(
        d.rgba[0] > 180 && d.rgba[2] < 90,
        "expected first (red) frame, got {:?}",
        &d.rgba[0..4]
    );
}

#[test]
fn decodes_svg_to_thumbnail() {
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="60"><rect width="100" height="60" fill="rgb(220,30,40)"/></svg>"#;
    let d = decode_thumbnail_opts(svg, 96, false).unwrap();
    // 100x60 fits the 96 box as 96x(~58); longest side fills it.
    assert_eq!(d.width, 96);
    assert!(d.height <= 96);
    // A center pixel should be the rect's red.
    let i = (((d.height / 2) * d.width + d.width / 2) * 4) as usize;
    assert!(
        d.rgba[i] > 180 && d.rgba[i + 1] < 90 && d.rgba[i + 3] == 255,
        "center should be red, got {:?}",
        &d.rgba[i..i + 4]
    );
}

#[test]
fn menu_preview_now_renders_svg_but_still_skips_pdf() {
    // The in-explorer context-menu tile used to skip SVG (caption-only). It now
    // renders it via resvg (pure-Rust, in-process, time-bounded) — while video /
    // PDF / ImageMagick stay excluded so a right-click can never freeze the shell.
    // A 40px SVG is below SVG_MIN_DIM (512), so render_svg scales the vector UP to a usable
    // 512px long edge (crisp — see `svg_small_scales_up_to_min`); the menu path shares that.
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="40"><rect width="40" height="40" fill="rgb(10,200,90)"/></svg>"#;
    let img = decode_menu_preview(svg).expect("menu preview should now decode a plain SVG");
    assert_eq!((img.width(), img.height()), (512, 512));

    // `.svgz` (gzipped SVG) must inflate + render on the menu path too.
    let mut gz = Vec::new();
    {
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(&mut gz, flate2::Compression::default());
        enc.write_all(svg).unwrap();
    }
    let img = decode_menu_preview(&gz).expect("menu preview should decode gzipped .svgz");
    assert_eq!((img.width(), img.height()), (512, 512));

    // A PDF stays deliberately excluded from the in-explorer menu tier — no
    // WinRT rasterizer here — so it must still fail out to a caption-only tile.
    let fake_pdf = b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n";
    assert!(
        decode_menu_preview(fake_pdf).is_err(),
        "PDF must remain excluded from the in-explorer menu preview"
    );
}

#[test]
fn contact_sheet_composes_svg_covers() {
    // A .7z/.zip of SVG logos (every cover an .svg): the contact-sheet compositor
    // must rasterize each SVG (resvg — safe in the isolated thumbnail/preview host
    // that calls it) and compose a sheet. Before the cover decoder learned SVG, all
    // covers failed to decode and the archive fell back to the stock icon.
    let red = br#"<svg xmlns="http://www.w3.org/2000/svg" width="32" height="32"><rect width="32" height="32" fill="rgb(220,30,40)"/></svg>"#.to_vec();
    let green = br#"<svg xmlns="http://www.w3.org/2000/svg" width="32" height="32"><rect width="32" height="32" fill="rgb(10,200,90)"/></svg>"#.to_vec();
    // Two covers -> the real 2-cell contact sheet (not the single-cover fallback,
    // and definitely not Err). `.expect` succeeding IS the proof the SVGs decoded.
    let sheet = thumbnail_from_covers(&[red, green], 128).expect("svg covers compose a sheet");
    assert_eq!((sheet.width, sheet.height), (128, 128));
}

#[test]
fn icc_color_management_to_srgb() {
    use image::{DynamicImage, GenericImageView, Rgb, RgbImage};
    // No embedded profile → the image must come back byte-for-byte unchanged.
    let img = DynamicImage::ImageRgb8(RgbImage::from_pixel(2, 2, Rgb([30, 150, 80])));
    assert_eq!(
        apply_icc_to_srgb(img.clone(), None).to_rgb8(),
        img.to_rgb8(),
        "no profile must pass through untouched"
    );
    // A real Display-P3 profile (encoded via moxcms) must color-manage a saturated
    // color toward sRGB — values change, dimensions preserved, never blanked.
    let p3 = moxcms::ColorProfile::new_display_p3()
        .encode()
        .expect("encode P3");
    let managed = apply_icc_to_srgb(img.clone(), Some(p3));
    assert_eq!(managed.dimensions(), (2, 2));
    assert_ne!(
        managed.to_rgb8(),
        img.to_rgb8(),
        "a Display-P3 pixel must be transformed, not passed through"
    );
    // A CMYK-space profile must be left alone (we only handle RGB profiles).
    let cmyk_unhandled = apply_icc_to_srgb(img.clone(), Some(vec![0u8; 4])); // junk ICC
    assert_eq!(
        cmyk_unhandled.to_rgb8(),
        img.to_rgb8(),
        "bad ICC → unchanged"
    );
}

#[test]
fn colr_box_profile_extraction() {
    // Embedded ICC: `prof` / `rICC` colour types → the raw profile bytes.
    assert_eq!(
        colr_profile(&[&b"prof"[..], &[1, 2, 3, 4]].concat()),
        Some(vec![1, 2, 3, 4])
    );
    assert_eq!(
        colr_profile(&[&b"rICC"[..], &[9, 9]].concat()),
        Some(vec![9, 9])
    );
    // CICP nclx Display-P3 (primaries = 12, sRGB transfer = 13) → built-in profile.
    assert!(
        colr_profile(&[b'n', b'c', b'l', b'x', 0, 12, 0, 13, 0, 1, 0])
            .is_some_and(|v| !v.is_empty()),
        "nclx Display-P3 maps to a profile"
    );
    // P3 primaries alone are insufficient: a different transfer curve must never be
    // interpreted through the sRGB curve baked into the Display-P3 ICC profile.
    assert_eq!(
        colr_profile(&[b'n', b'c', b'l', b'x', 0, 12, 0, 1, 0, 1, 0]),
        None,
        "P3 primaries with BT.709 transfer are not Display P3"
    );
    assert_eq!(
        colr_profile(&[b'n', b'c', b'l', b'x', 0, 12, 0, 16, 0, 9, 0x80]),
        None,
        "P3 primaries with PQ transfer are not Display P3"
    );
    assert_eq!(
        colr_profile(&[b'n', b'c', b'l', b'x', 0, 12]),
        None,
        "truncated nclx is ignored"
    );
    // nclx BT.709/sRGB (primaries = 1) is a no-op; junk / empty → None.
    assert_eq!(
        colr_profile(&[b'n', b'c', b'l', b'x', 0, 1, 0, 13, 0, 1, 0]),
        None
    );
    assert_eq!(colr_profile(b"prof"), None, "empty profile");
    assert_eq!(colr_profile(b"xxxxyyyy"), None, "unknown colour_type");
}

#[test]
fn isobmff_colr_box_walk() {
    // Minimal AVIF-ish tree: ftyp + meta(FullBox){ iprp{ ipco{ colr(prof + ICC) }}}.
    fn bx(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let size = (8 + body.len()) as u32;
        [&size.to_be_bytes()[..], &typ[..], body].concat()
    }
    let icc = vec![7u8; 32];
    let colr = bx(b"colr", &[&b"prof"[..], &icc].concat());
    let ipco = bx(b"ipco", &colr);
    let iprp = bx(b"iprp", &ipco);
    let meta = bx(b"meta", &[&[0u8; 4][..], &iprp].concat()); // meta FullBox: 4-byte ver/flags
    let file = [bx(b"ftyp", b"avif"), meta].concat();
    assert_eq!(
        isobmff_color_icc(&file),
        Some(icc),
        "ICC pulled from the nested colr box"
    );
    // A non-ISOBMFF buffer (no leading `ftyp`) is never walked.
    assert_eq!(isobmff_color_icc(&[0xFFu8; 64]), None);
}

#[test]
fn heic_auxiliary_alpha_box_walk() {
    fn bx(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let size = u32::try_from(8 + body.len()).unwrap();
        [&size.to_be_bytes()[..], &typ[..], body].concat()
    }
    fn heic_with_auxc(aux_type: &[u8], associated_item: u16, auxl_target: Option<u16>) -> Vec<u8> {
        let mut auxc_body = vec![0u8; 4]; // FullBox version + flags
        auxc_body.extend_from_slice(aux_type);
        let auxc = bx(b"auxC", &auxc_body);
        // auxC is property #2. Item 2 is the auxiliary image, and item 1 is
        // the primary — the same topology as the pinned libheif HEIC fixture.
        let ipco = bx(b"ipco", &[bx(b"ispe", &[0u8; 12]), auxc].concat());
        let ipma = bx(
            b"ipma",
            &[
                &[0u8; 4][..], // FullBox version + flags
                &1u32.to_be_bytes(),
                &associated_item.to_be_bytes(),
                &[1, 0x82], // one essential association to property #2
            ]
            .concat(),
        );
        let iprp = bx(b"iprp", &[ipco, ipma].concat());
        let pitm = bx(b"pitm", &[&[0u8; 4][..], &1u16.to_be_bytes()].concat());
        let iref = auxl_target.map(|target| {
            let auxl = bx(
                b"auxl",
                &[
                    &2u16.to_be_bytes()[..],
                    &1u16.to_be_bytes(),
                    &target.to_be_bytes(),
                ]
                .concat(),
            );
            bx(b"iref", &[&[0u8; 4][..], &auxl].concat())
        });
        let mut meta_body = [&[0u8; 4][..], &pitm, &iprp].concat();
        if let Some(iref) = iref {
            meta_body.extend(iref);
        }
        let meta = bx(b"meta", &meta_body);
        [bx(b"ftyp", b"heic\0\0\0\0mif1"), meta].concat()
    }

    let alpha = heic_with_auxc(b"urn:mpeg:hevc:2015:auxid:1\0", 2, Some(1));
    assert!(
        isobmff_has_hevc_aux_alpha(&alpha),
        "an HEVC alpha auxC property associated with an auxl item is detected"
    );
    assert!(
        !isobmff_has_hevc_aux_alpha(&heic_with_auxc(b"urn:mpeg:hevc:2015:auxid:2\0", 2, Some(1))),
        "a non-alpha HEVC auxiliary type is ignored"
    );
    assert!(
        !isobmff_has_hevc_aux_alpha(&heic_with_auxc(b"urn:mpeg:hevc:2015:auxid:1", 2, Some(1))),
        "the aux type must be NUL-terminated"
    );
    assert!(
        !isobmff_has_hevc_aux_alpha(&heic_with_auxc(b"urn:mpeg:hevc:2015:auxid:1\0", 1, Some(1))),
        "an auxC property assigned to the wrong item cannot affect routing"
    );
    assert!(
        !isobmff_has_hevc_aux_alpha(&heic_with_auxc(b"urn:mpeg:hevc:2015:auxid:1\0", 2, None)),
        "an associated auxC without an auxl relationship cannot affect routing"
    );
    assert!(
        !isobmff_has_hevc_aux_alpha(&heic_with_auxc(b"urn:mpeg:hevc:2015:auxid:1\0", 2, Some(3))),
        "an auxl relationship to a non-primary item cannot affect routing"
    );

    let loose = [
        bx(b"ftyp", b"heic\0\0\0\0mif1"),
        bx(b"free", b"urn:mpeg:hevc:2015:auxid:1\0"),
    ]
    .concat();
    assert!(
        !isobmff_has_hevc_aux_alpha(&loose),
        "the identifier outside meta/iprp/ipco/auxC cannot affect routing"
    );

    let mut truncated = alpha;
    truncated.pop();
    assert!(
        !isobmff_has_hevc_aux_alpha(&truncated),
        "truncated declared boxes are rejected"
    );
}

/// Issue #9: which AVIFs must bypass WIC because its AV1 codec misreads their colour.
///
/// The expectations here are not a guess about the spec — each one is a case measured
/// against libavif AND ImageMagick, worst-channel error out of 255, by
/// `scripts/repro-avif-color.ps1`. WIC was correct in only ONE configuration, so this is a
/// whitelist: anything not proven good is routed to ImageMagick, and anything unparseable
/// is too, so a future WIC that behaves differently cannot silently reintroduce the shift.
#[test]
fn avif_colour_routing_matches_what_wic_actually_gets_wrong() {
    use super::color::{avif_wic_verdict, AvifWicVerdict};

    fn bx(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let size = u32::try_from(8 + body.len()).unwrap();
        [&size.to_be_bytes()[..], &typ[..], body].concat()
    }

    /// `high_bitdepth` sets the av1C bit that marks 10/12-bit; `matrix` writes an nclx
    /// `colr` box with that CICP matrix coefficient (None writes no `colr` at all).
    fn avif(high_bitdepth: bool, matrix: Option<u16>) -> Vec<u8> {
        // av1C: marker+version, profile/level, then the flags byte whose bit 6 is
        // high_bitdepth. Trailing byte is the (unused here) config OBU space.
        let av1c = bx(
            b"av1C",
            &[0x81, 0x00, if high_bitdepth { 0x4c } else { 0x0c }, 0x00],
        );
        let mut props = vec![bx(b"ispe", &[0u8; 12]), av1c];
        if let Some(m) = matrix {
            let mut nclx = b"nclx".to_vec();
            nclx.extend_from_slice(&1u16.to_be_bytes()); // colour_primaries: BT.709
            nclx.extend_from_slice(&13u16.to_be_bytes()); // transfer: sRGB
            nclx.extend_from_slice(&m.to_be_bytes()); // matrix_coefficients
            nclx.push(0x80); // full_range_flag
            props.push(bx(b"colr", &nclx));
        }
        let iprp = bx(b"iprp", &bx(b"ipco", &props.concat()));
        let meta = bx(b"meta", &[&[0u8; 4][..], &iprp].concat());
        [bx(b"ftyp", b"avif\0\0\0\0mif1"), meta].concat()
    }

    // The ONE measured-correct case, and the only one that keeps the cheap WIC path:
    // ordinary 8-bit BT.709, which is what Chrome and Squoosh emit. Measured error 1-3.
    assert_eq!(
        avif_wic_verdict(&avif(false, Some(1))),
        AvifWicVerdict::Trusted,
        "8-bit BT.709 is measurably correct through WIC and must stay on the fast path"
    );
    // Identity leaves RGB alone, so there is no conversion for WIC to get wrong.
    assert_eq!(
        avif_wic_verdict(&avif(false, Some(0))),
        AvifWicVerdict::Trusted
    );

    // avifenc's DEFAULT matrix. Greys hold, saturated colour shifts. Measured error 19.
    assert_eq!(
        avif_wic_verdict(&avif(false, Some(6))),
        AvifWicVerdict::Untrusted,
        "8-bit BT.601 (avifenc's default) must route to ImageMagick: WIC clips while converting          with the wrong matrix, so the error is NOT recoverable after the fact"
    );
    // High bit depth WITH an nclx box is wrong for every matrix in the same way — a transfer
    // curve, not a matrix error (mid grey 128 reads as 138). That is invertible in-process, so
    // it must NOT cost a subprocess.
    for matrix in [0u16, 1, 6, 9] {
        assert_eq!(
            avif_wic_verdict(&avif(true, Some(matrix))),
            AvifWicVerdict::NeedsHighDepthCurve,
            "10/12-bit AVIF with colour signalling is curve-correctable, not magick-bound              (matrix {matrix})"
        );
    }
    // ...but high bit depth with NO colour box at all fails a DIFFERENT way (a full-vs-limited
    // range error, 0 -> 15 and 255 -> 233) which this curve does not fix. It stays on magick.
    assert_eq!(
        avif_wic_verdict(&avif(true, None)),
        AvifWicVerdict::Untrusted,
        "high-bit-depth AVIF with no nclx fails on RANGE, not transfer - the curve must not claim it"
    );
    // No colour signalling at all: WIC assumes BT.709 where libaom encoded BT.601.
    // Measured error 19 at 8-bit, so an absent nclx is NOT a licence to trust WIC.
    assert_eq!(
        avif_wic_verdict(&avif(false, None)),
        AvifWicVerdict::Untrusted,
        "an 8-bit AVIF with no nclx box must route to ImageMagick"
    );

    // HEIC carries hvcC, not av1C, and is routed by the auxiliary-alpha rule instead.
    // Give it an nclx with a matrix that WOULD trip the AVIF rule, to prove the av1C gate
    // is what decides rather than the colour box.
    let heic = {
        let mut nclx = b"nclx".to_vec();
        nclx.extend_from_slice(&1u16.to_be_bytes());
        nclx.extend_from_slice(&13u16.to_be_bytes());
        nclx.extend_from_slice(&6u16.to_be_bytes());
        nclx.push(0x80);
        let ipco = bx(
            b"ipco",
            &[bx(b"hvcC", &[0u8; 4]), bx(b"colr", &nclx)].concat(),
        );
        let meta = bx(b"meta", &[&[0u8; 4][..], &bx(b"iprp", &ipco)].concat());
        [bx(b"ftyp", b"heic\0\0\0\0mif1"), meta].concat()
    };
    assert_eq!(
        avif_wic_verdict(&heic),
        AvifWicVerdict::Trusted,
        "HEIC is not an AVIF and must not be routed by this rule"
    );
    // Not ISOBMFF at all, and a truncated container: decline rather than chew through it.
    assert_eq!(
        avif_wic_verdict(b"not an isobmff file at all"),
        AvifWicVerdict::Trusted
    );
    let mut truncated = avif(true, Some(6));
    truncated.truncate(12);
    assert_eq!(avif_wic_verdict(&truncated), AvifWicVerdict::Trusted);
}

/// The inverse of the transfer WIC applies to high-bit-depth AV1. Pinned against the MEASURED
/// curve, not against itself: the right-hand column is what Microsoft's AV1 codec 2.0.24.0
/// actually returned for a 17-step grey ramp encoded at 10-bit, so this test fails if the
/// correction stops undoing the thing it was built to undo.
#[test]
fn high_depth_curve_undoes_what_wic_measurably_does() {
    use super::color::undo_wic_high_depth_curve;
    use image::{DynamicImage, Rgba, RgbaImage};

    // (true value, what WIC handed back for it). Measured on a 10-bit AVIF grey ramp.
    const MEASURED: [(u8, u8); 17] = [
        (0, 0),
        (16, 29),
        (32, 46),
        (48, 62),
        (64, 77),
        (80, 93),
        (96, 108),
        (112, 123),
        (128, 138),
        (143, 153),
        (159, 167),
        (175, 182),
        (191, 197),
        (207, 211),
        (223, 225),
        (239, 240),
        (255, 254),
    ];

    let mut img = RgbaImage::new(MEASURED.len() as u32, 1);
    for (x, (_, wic)) in MEASURED.iter().enumerate() {
        // Alpha deliberately mid-range: the curve must leave it ALONE, or every semi-
        // transparent pixel silently changes opacity.
        img.put_pixel(x as u32, 0, Rgba([*wic, *wic, *wic, 128]));
    }
    let fixed = undo_wic_high_depth_curve(DynamicImage::ImageRgba8(img)).to_rgba8();

    let mut worst = 0i32;
    for (x, (truth, _)) in MEASURED.iter().enumerate() {
        let px = fixed.get_pixel(x as u32, 0).0;
        assert_eq!(
            px[3], 128,
            "alpha must pass through the colour curve untouched"
        );
        assert_eq!(
            px[0], px[1],
            "the curve must be per-channel identical on a grey"
        );
        worst = worst.max((i32::from(px[0]) - i32::from(*truth)).abs());
    }
    // Uncorrected, this ramp is off by up to 14. The analytic inverse tracks the measured
    // curve to within 2, so anything above that means the correction has drifted.
    assert!(
        worst <= 2,
        "high-bit-depth correction left a worst-channel error of {worst} (expected <= 2)"
    );
}

/// The curve must be monotonic and keep the endpoints, or it would crush highlights/shadows
/// and shift the black/white points of every corrected thumbnail.
#[test]
fn high_depth_curve_is_monotonic_and_keeps_endpoints() {
    use super::color::undo_wic_high_depth_curve;
    use image::{DynamicImage, Rgba, RgbaImage};

    let mut img = RgbaImage::new(256, 1);
    for v in 0u32..256 {
        let b = v as u8;
        img.put_pixel(v, 0, Rgba([b, b, b, 255]));
    }
    let out = undo_wic_high_depth_curve(DynamicImage::ImageRgba8(img)).to_rgba8();
    assert_eq!(out.get_pixel(0, 0).0[0], 0, "black must stay black");
    assert_eq!(out.get_pixel(255, 0).0[0], 255, "white must stay white");
    for v in 1u32..256 {
        assert!(
            out.get_pixel(v, 0).0[0] >= out.get_pixel(v - 1, 0).0[0],
            "curve must be monotonic; it is not at {v}"
        );
    }
}

/// The WebP WIC-eligibility sniffer. Every branch is a routing decision with a correctness
/// stake, so every branch is pinned: an animated WebP through WIC could pick a different
/// FRAME, and an ICC WebP through WIC would skip verified colour management.
#[test]
fn webp_wic_routing_excludes_exactly_the_risky_cases() {
    use super::webp_prefers_wic;

    fn webp(fourcc: &[u8; 4], flags: Option<u8>) -> Vec<u8> {
        let mut b = b"RIFF\x00\x01\x00\x00WEBP".to_vec();
        b.extend_from_slice(fourcc);
        b.extend_from_slice(&10u32.to_le_bytes()); // chunk size
        b.push(flags.unwrap_or(0));
        b.extend_from_slice(&[0u8; 12]); // rest of the VP8X payload / stub data
        b
    }

    // Simple stills: no feature flags exist at all, so nothing to be wrong about.
    assert!(webp_prefers_wic(&webp(b"VP8 ", None)));
    assert!(webp_prefers_wic(&webp(b"VP8L", None)));

    // Extended stills: alpha, EXIF and XMP are fine (alpha survives the shared 32bppRGBA
    // conversion; EXIF orientation is our own pipeline's job either way).
    assert!(webp_prefers_wic(&webp(b"VP8X", Some(0x10)))); // alpha
    assert!(webp_prefers_wic(&webp(b"VP8X", Some(0x0C)))); // EXIF | XMP

    // The two exclusions this sniffer exists for.
    assert!(
        !webp_prefers_wic(&webp(b"VP8X", Some(0x02))),
        "animated WebP must stay on the pure-Rust path: frame choice is pinned there"
    );
    assert!(
        !webp_prefers_wic(&webp(b"VP8X", Some(0x30))),
        "ICC-tagged WebP must stay on the verified colour-management path"
    );

    // Not WebP, unknown first chunk, truncated: decline, keeping the existing tier order.
    assert!(!webp_prefers_wic(b"RIFF\x00\x00\x00\x00WAVEfmt "));
    assert!(!webp_prefers_wic(&webp(b"ANMF", None)));
    assert!(!webp_prefers_wic(&b"RIFF\x00\x01\x00\x00WEBPVP8X"[..]));
    assert!(!webp_prefers_wic(b""));
}

/// The BT.601-AVIF Media Foundation path, minus Media Foundation: the eligibility gates,
/// the YUV maths, and the mini-MP4 all verify without the codec, so CI (which has no AV1
/// extension) still pins everything except the decode itself. The decode is pinned by the
/// corpus fixture `sample-avif-601.avif` + `_expected-colors.txt` on machines that have it.
#[test]
fn avif_mf_eligibility_takes_exactly_the_measured_buckets() {
    use super::avifmf::eligible_bt601_still;

    fn bx(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let size = u32::try_from(8 + body.len()).unwrap();
        [&size.to_be_bytes()[..], &typ[..], body].concat()
    }
    // A configurable AVIF skeleton: ipco with av1C(s), optional nclx, ispe, optional auxC.
    struct Cfg {
        matrix: Option<u16>,
        primaries: u16,
        av1c_count: usize,
        profile_byte: u8, // av1C byte 1: seq_profile in the top 3 bits
        flags2: u8,       // av1C byte 2: high_bitdepth bit 6, monochrome bit 4
        aux_c: bool,
    }
    fn avif(c: &Cfg) -> Vec<u8> {
        let mut props = Vec::new();
        let mut ispe = vec![0u8; 4];
        ispe.extend_from_slice(&320u32.to_be_bytes());
        ispe.extend_from_slice(&240u32.to_be_bytes());
        props.push(bx(b"ispe", &ispe));
        for _ in 0..c.av1c_count {
            props.push(bx(b"av1C", &[0x81, c.profile_byte, c.flags2, 0x00]));
        }
        if let Some(m) = c.matrix {
            let mut nclx = b"nclx".to_vec();
            nclx.extend_from_slice(&c.primaries.to_be_bytes());
            nclx.extend_from_slice(&2u16.to_be_bytes());
            nclx.extend_from_slice(&m.to_be_bytes());
            nclx.push(0x00);
            props.push(bx(b"colr", &nclx));
        }
        if c.aux_c {
            props.push(bx(
                b"auxC",
                b"urn:mpeg:mpegB:cicp:systems:auxiliary:alpha\0",
            ));
        }
        let iprp = bx(b"iprp", &bx(b"ipco", &props.concat()));
        let meta = bx(b"meta", &[&[0u8; 4][..], &iprp].concat());
        [bx(b"ftyp", b"avif\0\0\0\0mif1"), meta].concat()
    }
    let base = Cfg {
        matrix: Some(6),
        primaries: 1,
        av1c_count: 1,
        profile_byte: 0x00,
        flags2: 0x0c,
        aux_c: false,
    };

    // The three eligible matrices: explicit BT.601 (5/6) and unspecified (2, decoded as 601
    // by the ecosystem reference — measured, worst error 1).
    for m in [2u16, 5, 6] {
        let c = Cfg {
            matrix: Some(m),
            ..base
        };
        assert!(
            eligible_bt601_still(&avif(&c)).is_some(),
            "matrix {m} is a measured BT.601 bucket and must be eligible"
        );
    }
    // The dims come from ispe, verbatim.
    let s = eligible_bt601_still(&avif(&base)).unwrap();
    assert_eq!((s.width, s.height), (320, 240));
    assert!(!s.full_range, "range bit clear must read as limited");

    // Everything below must DECLINE (fall back to ImageMagick, never decode wrongly):
    let cases: &[(&str, Cfg)] = &[
        (
            "BT.709 belongs to the WIC fast path, not here",
            Cfg {
                matrix: Some(1),
                ..base
            },
        ),
        (
            "BT.2020 and friends stay with magick's full CICP handling",
            Cfg {
                matrix: Some(9),
                ..base
            },
        ),
        (
            "no colr box at all is not a licence to guess",
            Cfg {
                matrix: None,
                ..base
            },
        ),
        (
            "wide-gamut primaries stay with magick",
            Cfg {
                primaries: 12,
                ..base
            },
        ),
        (
            "a second av1C means an auxiliary (alpha) item - magick composites those",
            Cfg {
                av1c_count: 2,
                ..base
            },
        ),
        (
            "an auxC property is an alpha plane even with one av1C visible",
            Cfg {
                aux_c: true,
                ..base
            },
        ),
        (
            "High profile (4:4:4) is outside the Main-profile gate",
            Cfg {
                profile_byte: 0x20,
                ..base
            },
        ),
        (
            "high bit depth belongs to the WIC+curve path",
            Cfg {
                flags2: 0x4c,
                ..base
            },
        ),
        (
            "monochrome is untested territory - decline",
            Cfg {
                flags2: 0x1c,
                ..base
            },
        ),
    ];
    for (why, c) in cases {
        assert!(eligible_bt601_still(&avif(c)).is_none(), "{why}");
    }
    // Full-range flag reaches the conversion.
    let mut f = avif(&base);
    let i = f.windows(4).position(|w| w == b"nclx").unwrap();
    f[i + 10] = 0x80;
    assert!(eligible_bt601_still(&f).unwrap().full_range);
}

/// The YUV maths against published BT.601 anchor vectors, both ranges. Wrong coefficients
/// here would ship exactly the colour shift this path exists to eliminate.
#[test]
fn avif_mf_yuv_conversion_matches_bt601_anchors() {
    use super::avifmf::nv12_to_srgb_bt601;
    use crate::video::Nv12Frame;

    // One 2x2 frame, all four pixels the same YUV triple.
    fn frame(y: u8, cb: u8, cr: u8) -> Nv12Frame {
        Nv12Frame {
            data: vec![y, y, y, y, cb, cr],
            width: 2,
            height: 2,
            stride: 2,
        }
    }
    // (y, cb, cr, full_range, expected rgb, tolerance)
    type Anchor = (u8, u8, u8, bool, (i32, i32, i32), i32);
    let anchors: &[Anchor] = &[
        (16, 128, 128, false, (0, 0, 0), 0),        // limited black
        (235, 128, 128, false, (255, 255, 255), 0), // limited white
        (126, 128, 128, false, (128, 128, 128), 1), // limited mid grey
        (82, 90, 240, false, (255, 0, 0), 2),       // limited saturated red
        (145, 54, 34, false, (0, 255, 0), 2),       // limited saturated green
        (41, 240, 110, false, (0, 0, 255), 2),      // limited saturated blue
        (0, 128, 128, true, (0, 0, 0), 0),          // full black
        (255, 128, 128, true, (255, 255, 255), 0),  // full white
        (200, 128, 128, true, (200, 200, 200), 0),  // full grey passes through untouched
    ];
    for &(y, cb, cr, full, (er, eg, eb), tol) in anchors {
        let img = nv12_to_srgb_bt601(&frame(y, cb, cr), 2, 2, full, None).unwrap();
        let px = img.to_rgba8().get_pixel(0, 0).0;
        for (got, want) in px[..3].iter().zip([er, eg, eb]) {
            assert!(
                (i32::from(*got) - want).abs() <= tol,
                "yuv({y},{cb},{cr}) full={full}: got {:?}, wanted ({er},{eg},{eb}) +/-{tol}",
                &px[..3]
            );
        }
        assert_eq!(px[3], 255, "this path never carries alpha");
    }
}

/// Target-aware subsampling: asking for a small thumbnail must convert FEWER pixels, without
/// changing what those pixels are. This is the fix for the 12 MP AVIF running 2.95x Windows.
#[test]
fn avif_mf_conversion_subsamples_for_a_small_target() {
    use super::avifmf::nv12_to_srgb_bt601;
    use crate::video::Nv12Frame;

    // A 1200x900 flat mid-grey frame: flat so subsampling cannot change the answer.
    let (w, h) = (1200usize, 900usize);
    let mut data = vec![126u8; w * h];
    data.resize(w * h + w * h / 2, 128u8);
    let frame = Nv12Frame {
        data,
        width: w as u32,
        height: h as u32,
        stride: w as u32,
    };

    // No target: full resolution, as the full-fidelity callers still get.
    let full = nv12_to_srgb_bt601(&frame, w as u32, h as u32, false, None).unwrap();
    assert_eq!((full.width(), full.height()), (1200, 900));

    // A 100 px target wants >= 300 px of intermediate, so step = 1200/300 = 4.
    let small = nv12_to_srgb_bt601(&frame, w as u32, h as u32, false, Some(100)).unwrap();
    assert_eq!(
        (small.width(), small.height()),
        (300, 225),
        "must subsample to >= 3x the target edge, not to the target itself (that would alias)"
    );

    // Never UPSAMPLE, and never subsample when the source is already small enough.
    let big_target = nv12_to_srgb_bt601(&frame, w as u32, h as u32, false, Some(4096)).unwrap();
    assert_eq!((big_target.width(), big_target.height()), (1200, 900));

    // The colour is identical either way - this is a work reduction, not a quality change.
    for img in [&full, &small] {
        let px = img.to_rgba8().get_pixel(1, 1).0;
        assert!(
            px[..3].iter().all(|c| (i32::from(*c) - 128).abs() <= 1),
            "subsampling must not shift colour; got {:?}",
            &px[..3]
        );
    }
}

/// The one-frame MP4 the path builds must be one OUR OWN mp4 parser recognises as av01 —
/// a cheap structural round-trip that needs no codec.
#[test]
fn avif_mf_mini_mp4_roundtrips_through_the_mp4_parser() {
    use super::avifmf::{build_av01_mp4, Av1Still};
    let still = Av1Still {
        av1c: {
            let body = [0x81u8, 0x00, 0x0c, 0x00];
            let mut b = 12u32.to_be_bytes().to_vec();
            b.extend_from_slice(b"av1C");
            b.extend_from_slice(&body);
            b
        },
        colr: {
            let mut payload = b"nclx".to_vec();
            payload.extend_from_slice(&[0, 1, 0, 2, 0, 6, 0]);
            let mut b = (8 + payload.len() as u32).to_be_bytes().to_vec();
            b.extend_from_slice(b"colr");
            b.extend_from_slice(&payload);
            b
        },
        width: 64,
        height: 48,
        full_range: false,
    };
    let mini = build_av01_mp4(&still, &[0u8; 32]).expect("muxer must accept a plain still");
    let fourcc = crate::mp4::video_codec_fourcc(&mut std::io::Cursor::new(&mini));
    assert_eq!(
        fourcc.as_ref(),
        Some(b"av01"),
        "the mini-MP4 must advertise an av01 track our own parser can read back"
    );
}

/// The BMP WIC-eligibility sniffer. Every branch is a routing decision that could change what
/// the user SEES, so every branch is pinned.
#[test]
fn bmp_wic_routing_excludes_the_ambiguous_cases() {
    use super::bmp_prefers_wic;

    /// A BMP head: BITMAPFILEHEADER(14) + BITMAPINFOHEADER(40), enough for the sniffer.
    fn bmp(dib_size: u32, bitcount: u16, compression: u32) -> Vec<u8> {
        let mut b = b"BM".to_vec();
        b.extend_from_slice(&0u32.to_le_bytes()); // file size
        b.extend_from_slice(&0u32.to_le_bytes()); // reserved
        b.extend_from_slice(&54u32.to_le_bytes()); // pixel offset
        b.extend_from_slice(&dib_size.to_le_bytes());
        b.extend_from_slice(&64u32.to_le_bytes()); // width
        b.extend_from_slice(&64u32.to_le_bytes()); // height
        b.extend_from_slice(&1u16.to_le_bytes()); // planes
        b.extend_from_slice(&bitcount.to_le_bytes());
        b.extend_from_slice(&compression.to_le_bytes());
        b.resize(64, 0);
        b
    }

    // The plain memory layouts this optimisation is for.
    for bits in [1u16, 4, 8, 16, 24] {
        assert!(
            bmp_prefers_wic(&bmp(40, bits, 0)),
            "{bits}-bit BI_RGB is a plain layout and must take the fast path"
        );
    }
    assert!(
        bmp_prefers_wic(&bmp(40, 16, 3)),
        "BI_BITFIELDS is still a plain layout"
    );
    // A BITMAPV5HEADER is just a longer header over the same layout.
    assert!(bmp_prefers_wic(&bmp(124, 24, 0)));

    // 32-bit: the alpha byte is alpha in some writers and garbage in others, so the two
    // decoders are entitled to disagree. Stay on the pinned one.
    assert!(
        !bmp_prefers_wic(&bmp(40, 32, 0)),
        "32-bit BMP alpha is ambiguous - it must not change decoder for a speed win"
    );
    // Compressed variants are their own decoders with their own quirks.
    for comp in [1u32, 2, 4, 5] {
        assert!(
            !bmp_prefers_wic(&bmp(40, 8, comp)),
            "compression {comp} is not the plain layout this targets"
        );
    }
    // A BITMAPCOREHEADER (12) has no compression field at all - decline rather than misread.
    assert!(!bmp_prefers_wic(&bmp(12, 24, 0)));
    // Not a BMP, and truncated.
    assert!(!bmp_prefers_wic(b"RIFF\x00\x00\x00\x00WEBPVP8 "));
    assert!(!bmp_prefers_wic(&bmp(40, 24, 0)[..20]));
    assert!(!bmp_prefers_wic(b""));
}

#[test]
fn detects_cmyk_jpeg_by_component_count() {
    // Minimal JPEG: SOI + SOF0 declaring `nf` components + EOI. CMYK/YCCK are 4-component.
    fn jpeg_with_components(nf: u8) -> Vec<u8> {
        let len = 8 + 3 * nf as usize; // SOF0 length field
        let mut b = vec![0xFF, 0xD8]; // SOI
        b.extend_from_slice(&[0xFF, 0xC0, (len >> 8) as u8, len as u8, 8, 0, 1, 0, 1, nf]);
        b.extend(std::iter::repeat_n(0u8, 3 * nf as usize)); // component specs
        b.extend_from_slice(&[0xFF, 0xD9]); // EOI
        b
    }
    assert!(
        is_cmyk_jpeg(&jpeg_with_components(4)),
        "4-component JPEG = CMYK/YCCK"
    );
    assert!(
        !is_cmyk_jpeg(&jpeg_with_components(3)),
        "3-component = YCbCr/RGB"
    );
    assert!(
        !is_cmyk_jpeg(&jpeg_with_components(1)),
        "1-component = grayscale"
    );
    assert!(
        !is_cmyk_jpeg(&[0x89, b'P', b'N', b'G', 0, 0, 0, 0]),
        "PNG is not a CMYK JPEG"
    );
    assert!(!is_cmyk_jpeg(&[]), "empty input");
}

#[test]
fn fully_transparent_thumbnail_is_rejected_blank() {
    // A fully-transparent decode is invisible → reject so Explorer shows the icon.
    let clear = png_bytes(32, 32, [0, 0, 0, 0]);
    assert!(
        decode_thumbnail_opts(&clear, 256, false).is_err(),
        "fully-transparent thumbnail must be rejected as blank"
    );
    // Anything with visible pixels is fine.
    let opaque = png_bytes(32, 32, [10, 20, 30, 255]);
    assert!(decode_thumbnail_opts(&opaque, 256, false).is_ok());
}

/// Issue #17's case, and the behaviour we deliberately KEPT after trying the alternative.
///
/// A PNG whose alpha is entirely zero but whose RGB still holds a picture is shown OPAQUE,
/// so the user sees their artwork. It looks wrong when the hidden RGB is black, which is what
/// was reported — but gating this off for PNG was tried and reverted: it replaced an ugly
/// thumbnail with no thumbnail, and a tile you can recognise beats a generic file icon.
#[test]
fn a_zeroed_alpha_png_is_shown_opaque_not_rejected() {
    let hidden = png_bytes(32, 32, [255, 210, 0, 0]);
    let d = decode_thumbnail_opts(&hidden, 256, false)
        .expect("a zeroed-alpha PNG with real colour must still produce a thumbnail");
    assert!(
        d.rgba.chunks_exact(4).all(|px| px[3] == 255),
        "it must come back fully opaque, or the shell composites it away to nothing"
    );
    assert!(
        d.rgba.chunks_exact(4).any(|px| px[0] != 0 || px[1] != 0),
        "and it must still carry the picture that was hidden under the zeroed alpha"
    );
}

#[test]
fn tiny_sprite_nearest_upscales_and_midsize_fills_the_box() {
    // 16×16 sprite in a 256 box → integer Nearest upscale to 16× = 256 (crisp).
    let sprite = png_bytes(16, 16, [10, 20, 30, 255]);
    let d = decode_thumbnail_opts(&sprite, 256, false).unwrap();
    assert_eq!(
        (d.width, d.height),
        (256, 256),
        "16px sprite should nearest-upscale to 256"
    );
    // 200×200 in a 256 box now FILLS the box (issue #25). This assertion used to read "must
    // stay native", on the belief that Explorer would scale the tile up for us. It does not —
    // it centres what we hand it — so a source under the requested size drew as a visibly
    // smaller tile than its neighbours. Photoshop files showed it worst, because the size of
    // the preview Photoshop bakes into a PSD varies by writing app and version, so two PSDs
    // side by side got different tile sizes for no reason the user could see.
    let mid = png_bytes(200, 200, [10, 20, 30, 255]);
    let d2 = decode_thumbnail_opts(&mid, 256, false).unwrap();
    assert_eq!(
        (d2.width, d2.height),
        (256, 256),
        "a mid-size source must be enlarged to fill the requested box"
    );
    // Aspect ratio survives the enlargement — the long edge lands on cx, the short one scales.
    let wide = png_bytes(200, 100, [10, 20, 30, 255]);
    let d4 = decode_thumbnail_opts(&wide, 256, false).unwrap();
    assert_eq!(
        (d4.width, d4.height),
        (256, 128),
        "enlarging must preserve aspect ratio, not stretch to a square"
    );
    // But there IS a ceiling: past MAX_UPSCALE_FACTOR the source has no detail to give, so a
    // soft full-size rectangle would be worse than an honestly small tile. 100px into a 1024
    // box is 10×, well over the limit, so it stays native.
    let small_for_huge = png_bytes(100, 100, [10, 20, 30, 255]);
    let d5 = decode_thumbnail_opts(&small_for_huge, 1024, false).unwrap();
    assert_eq!(
        (d5.width, d5.height),
        (100, 100),
        "beyond MAX_UPSCALE_FACTOR the source is left native rather than blown up"
    );
    // A large image still shrinks to fit.
    let big = png_bytes(800, 600, [10, 20, 30, 255]);
    let d3 = decode_thumbnail_opts(&big, 256, false).unwrap();
    assert!(d3.width <= 256 && d3.height <= 256 && d3.width.max(d3.height) == 256);
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

#[test]
#[ignore] // needs ImageMagick (magick.exe) installed; run explicitly
fn magick_subprocess_decodes() {
    // Feed a PNG straight to the ImageMagick tier (bypassing the image-first
    // tier) to prove the stdin->stdout subprocess plumbing works end-to-end.
    let png = png_bytes(50, 40, [30, 200, 90, 255]);
    let img = decode_via_magick_capped(&png, None).expect("magick should decode the PNG");
    assert_eq!((img.width(), img.height()), (50, 40));
}

#[test]
fn decode_full_rgba_order_and_orientation() {
    // The companion app's eyedropper samples `decode_full(...).to_rgba8()` and
    // its color readout hinges on the bytes being in **RGBA order, top row
    // first**. Verify with a 2×2 image of four known, distinct colors so a
    // channel swap or vertical flip would be caught. (Moved here from the
    // now-removed `lib::decode_to_rgba8`, a thin wrapper over this.)
    let mut img = image::RgbaImage::new(2, 2);
    img.put_pixel(0, 0, image::Rgba([200, 40, 30, 255])); // top-left red-ish
    img.put_pixel(1, 0, image::Rgba([20, 180, 90, 255])); // top-right green-ish
    img.put_pixel(0, 1, image::Rgba([30, 60, 210, 255])); // bottom-left blue-ish
    img.put_pixel(1, 1, image::Rgba([240, 230, 10, 255])); // bottom-right yellow
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();

    let rgba = decode_full(&bytes).unwrap().to_rgba8();
    assert_eq!((rgba.width(), rgba.height()), (2, 2));
    let px = rgba.as_raw();
    // Row 0 first (top-down), each pixel RGBA in order.
    assert_eq!(&px[0..4], &[200, 40, 30, 255], "top-left");
    assert_eq!(&px[4..8], &[20, 180, 90, 255], "top-right");
    assert_eq!(
        &px[8..12],
        &[30, 60, 210, 255],
        "bottom-left (top-down row order)"
    );
    assert_eq!(&px[12..16], &[240, 230, 10, 255], "bottom-right");
}

#[test]
fn wic_path_decodes() {
    // Exercise the WIC plumbing directly (PNG is decodable by WIC even
    // though in production WIC is only the fallback). Needs COM on-thread.
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
        );
    }
    let bytes = png_bytes(40, 20, [10, 20, 200, 255]);
    let img = unsafe { wic_decode(&bytes) }.expect("WIC should decode PNG");
    assert_eq!((img.width(), img.height()), (40, 20));
}

/// Do the pure-Rust tier and the WIC tier AGREE on a plain JPEG?
///
/// This is the gating question for issue 3 (`docs/ISSUES.md`): routing large JPEGs through WIC
/// would let the codec scale during decode, which is the single biggest preview speed-up
/// available. It is only acceptable if the picture does not visibly change, and the two tiers
/// do not manage colour identically, so the answer has to be measured rather than assumed.
///
/// Reports the worst per-channel difference. Failing loudly with the number is the point: if
/// the tiers ever diverge, whoever reads this needs to see by how much, not just "differs".
#[test]
fn pure_rust_and_wic_agree_on_a_plain_jpeg() {
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
        );
    }
    // Smooth gradients plus hard edges: the combination that exposes both colour-management
    // differences and chroma-upsampling differences between two JPEG decoders.
    let src = image::RgbImage::from_fn(256, 256, |x, y| {
        if (x / 16 + y / 16) % 2 == 0 {
            image::Rgb([x as u8, y as u8, 200])
        } else {
            image::Rgb([220, (x ^ y) as u8, (255 - y) as u8])
        }
    });
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 92)
        .encode_image(&image::DynamicImage::ImageRgb8(src))
        .expect("encode jpeg");

    let pure = decode_preview(&jpeg).expect("pure-Rust tier decodes the jpeg");
    let via_wic =
        unsafe { wic::wic_decode_with_thumbnail(&jpeg, None) }.expect("WIC decodes the same jpeg");
    assert_eq!(
        (pure.width(), pure.height()),
        (via_wic.width(), via_wic.height()),
        "tiers must at least agree on size"
    );

    let (a, b) = (pure.to_rgb8(), via_wic.to_rgb8());
    let mut worst = 0u8;
    let mut total = 0u64;
    for (pa, pb) in a.pixels().zip(b.pixels()) {
        for c in 0..3 {
            let d = pa.0[c].abs_diff(pb.0[c]);
            worst = worst.max(d);
            total += d as u64;
        }
    }
    let mean = total as f64 / (a.pixels().len() * 3) as f64;
    println!("  pure-Rust vs WIC: worst channel delta {worst}, mean {mean:.2}");
    // A few levels is ordinary IDCT/upsampling disagreement between two conforming decoders.
    // A large delta would mean a real colour-management difference, and would make swapping
    // tiers a visible change rather than an invisible speed-up.
    assert!(
        worst <= 24 && mean <= 2.0,
        "tiers disagree too much to swap freely: worst {worst}, mean {mean:.2}"
    );
}

/// The capability probe has to actually DISTINGUISH, or it is a no-op that costs a file open.
///
/// A JPEG decoder reduces in the DCT domain and must be accepted; a PNG decoder has no such
/// mode and must be declined, because for a caller using this as a pre-pass ahead of a full
/// decode, accepting PNG means decoding the image twice. Measured before this existed: a 24 MP
/// PNG "scaled" decode cost 605 ms against 690 ms for the full one, i.e. it WAS the full one.
///
/// Both halves are asserted deliberately. A probe that says no to everything would pass a
/// JPEG-only test by accident and silently disable the optimisation everywhere.
///
/// **The target edge is deliberately NOT a clean fraction of the source.** The first version of
/// this test used 1024 -> 256, an exact quarter, and passed against a probe that was broken:
/// it asked the codec "can you produce exactly this size", and JPEG only offers halvings, so
/// every real photo (4000 px asked for 2048, offered 2000) was rejected and the fast path
/// silently died everywhere except at power-of-two ratios. A benchmark caught it, this test did
/// not. 1024 -> 400 is the awkward ratio that reproduces it.
#[test]
fn the_scaled_pre_pass_takes_jpeg_and_declines_png() {
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
        );
    }
    let dir = std::env::temp_dir();
    let pid = std::process::id();

    // Noise, not flat colour: a uniform image can compress to something a codec handles
    // unusually, and the question here is about the codec's capability, not the content.
    let mut img = image::RgbImage::new(1024, 768);
    for (x, y, p) in img.enumerate_pixels_mut() {
        *p = image::Rgb([(x % 251) as u8, (y % 241) as u8, ((x ^ y) % 239) as u8]);
    }
    let jpg = dir.join(format!("st2k_scaleprobe_{pid}.jpg"));
    image::DynamicImage::ImageRgb8(img)
        .save_with_format(&jpg, image::ImageFormat::Jpeg)
        .expect("stage temp jpeg");
    let png = dir.join(format!("st2k_scaleprobe_{pid}.png"));
    std::fs::write(&png, png_bytes(1024, 768, [10, 20, 200, 255])).expect("stage temp png");

    let jpg_p = jpg.to_string_lossy().into_owned();
    let png_p = png.to_string_lossy().into_owned();

    let scaled = super::wic_scaled_from_path_if_codec_scales(&jpg_p, 400)
        .expect("a JPEG codec reduces in the DCT domain and must be accepted");
    assert_eq!(scaled.width().max(scaled.height()), 400, "scaled to target");
    assert!(
        super::wic_scaled_from_path_if_codec_scales(&png_p, 400).is_none(),
        "a PNG codec cannot decode reduced, so the pre-pass must decline it rather than \
         decode the image a second time"
    );
    // The unconditional entry point still serves PNG — this probe narrows the PRE-pass only,
    // and the oversized rescue depends on WIC opening anything it can.
    assert!(
        super::wic_scaled_from_path(&png_p, 400).is_some(),
        "the plain scaled decode must still handle PNG"
    );

    // The BYTES twin, which is the one the shell provider can actually reach: it is handed an
    // IStream and never a filename, so the by-path probe above was unreachable from the surface
    // that draws every thumbnail in Explorer. Same three answers, over buffers.
    // Big enough to clear the size floor: the 1024x768 file above encodes to a few hundred KB,
    // which this path is SUPPOSED to decline. Noise so it cannot compress its way back under.
    let mut big = image::RgbImage::new(3000, 2000);
    for (x, y, p) in big.enumerate_pixels_mut() {
        *p = image::Rgb([
            (x.wrapping_mul(7) % 251) as u8,
            (y.wrapping_mul(13) % 241) as u8,
            ((x ^ y).wrapping_mul(3) % 239) as u8,
        ]);
    }
    let mut big_jpg = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(big)
        .write_to(&mut big_jpg, image::ImageFormat::Jpeg)
        .expect("encode big jpeg");
    let jpg_bytes = big_jpg.into_inner();
    assert!(
        jpg_bytes.len() >= 512 * 1024,
        "the fixture must clear the size floor to test the path at all, got {} bytes",
        jpg_bytes.len()
    );
    let png_file_bytes = std::fs::read(&png).expect("read staged png");
    let scaled_b = super::wic_scaled_from_bytes_if_codec_scales(&jpg_bytes, 400)
        .expect("a buffered JPEG must take the DCT-scaled path");
    assert_eq!(scaled_b.width().max(scaled_b.height()), 400);
    assert!(
        super::wic_scaled_from_bytes_if_codec_scales(&png_file_bytes, 400).is_none(),
        "PNG must decline: it has no reduced-size mode, so this would decode it twice"
    );
    // The size floor is load-bearing, not decoration. Below it a full decode is already fast
    // and the COM round trip could cost more than it saves, so a small JPEG must NOT be
    // diverted here — and a tiny one is exactly what most folders are full of.
    let mut small = image::RgbImage::new(64, 48);
    for (x, y, p) in small.enumerate_pixels_mut() {
        *p = image::Rgb([(x % 251) as u8, (y % 241) as u8, ((x ^ y) % 239) as u8]);
    }
    let mut small_jpg = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(small)
        .write_to(&mut small_jpg, image::ImageFormat::Jpeg)
        .expect("encode small jpeg");
    assert!(
        super::wic_scaled_from_bytes_if_codec_scales(small_jpg.get_ref(), 400).is_none(),
        "a JPEG under the size floor must stay on the pure-Rust tier"
    );

    let _ = std::fs::remove_file(&jpg);
    let _ = std::fs::remove_file(&png);
}

#[test]
fn wic_by_path_decodes_and_scales_without_buffering() {
    // The oversized rescue: WIC opens the FILE itself, so a document past
    // `limits::MAX_INPUT_BYTES` still thumbnails instead of getting the stock icon.
    // Staging a >256 MB file in a unit test is absurd, so this exercises the decode
    // (`wic_scaled_from_path`, which carries no size gate — its two callers apply their
    // own) and leaves the threshold itself to `oversized_wic_rescue`.
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
        );
    }
    let bytes = png_bytes(400, 200, [10, 20, 200, 255]);
    // Process-id suffixed so concurrent `cargo test` runs cannot race each other.
    let path = std::env::temp_dir().join(format!("st2k_wicpath_{}.png", std::process::id()));
    std::fs::write(&path, &bytes).expect("stage temp png");
    let p = path.to_string_lossy().into_owned();

    let img = super::wic_scaled_from_path(&p, 64).expect("WIC should decode a PNG off the file");
    // Scaled DURING decode: the long edge lands on the requested target, and the aspect
    // ratio survives. This is what makes the memory cost the thumbnail, not the document.
    assert_eq!(
        img.width().max(img.height()),
        64,
        "long edge scaled to target"
    );
    assert_eq!(img.height(), 32, "aspect ratio preserved");

    // A path WIC cannot open must decline rather than panic — that `None` is what lets the
    // caller fall through to its existing refusal.
    let missing = path.with_extension("does-not-exist");
    assert!(super::wic_scaled_from_path(&missing.to_string_lossy(), 64).is_none());

    let _ = std::fs::remove_file(&path);
}

#[test]
fn wic_thumbnail_scaling_keeps_rgba_channel_order() {
    // A SCALED WIC decode must come back in the same channel order as an unscaled one.
    // `IWICBitmapScaler` returns WIC's native BGRA rather than the 32bppRGBA it was fed,
    // so the raw bytes used to reach `RgbaImage::from_raw` transposed: every Explorer tile
    // smaller than its source (HEIC/AVIF/JPEG XR) rendered with red and blue swapped, while
    // the full-fidelity paths — which pass no target edge, hence no scaler — stayed correct.
    // The colour is deliberately asymmetric in R vs B so a swap cannot pass.
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
        );
    }
    const RGBA: [u8; 4] = [10, 20, 200, 255];
    let bytes = png_bytes(64, 64, RGBA);

    let unscaled = unsafe { wic_decode_with_thumbnail(&bytes, Some(64)) }
        .expect("WIC should decode PNG without scaling");
    assert_eq!((unscaled.width(), unscaled.height()), (64, 64));
    assert_eq!(
        unscaled.to_rgba8().get_pixel(32, 32).0,
        RGBA,
        "the no-scaler path must return the source colour"
    );

    let scaled = unsafe { wic_decode_with_thumbnail(&bytes, Some(16)) }
        .expect("WIC should decode PNG with scaling");
    assert_eq!((scaled.width(), scaled.height()), (16, 16));
    assert_eq!(
        scaled.to_rgba8().get_pixel(8, 8).0,
        RGBA,
        "a scaled WIC decode must keep RGBA order (a swap here reads [200, 20, 10, 255])"
    );
}

#[test]
fn read_preview_capped_rescues_head_preview_containers() {
    // Over the cap + BLENDER magic -> a bounded prefix; over the cap without the
    // magic -> the hard refusal; under the cap -> the whole file. Caps shrunk via
    // the `_at` variant so the test doesn't stage multi-hundred-MB files.
    let dir = std::env::temp_dir().join(format!("st2k_head_preview_test_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let blend = dir.join("big.blend");
    let mut data = b"BLENDER-v277".to_vec();
    data.resize(2048, 0);
    std::fs::write(&blend, &data).unwrap();
    let got = read_preview_capped_at(blend.to_str().unwrap(), 1024, 1536).unwrap();
    assert_eq!(
        got.len(),
        1536,
        "oversized blend must yield the bounded prefix"
    );
    assert!(got.starts_with(b"BLENDER"));

    // Prefix cap larger than the file: return everything there is.
    let got = read_preview_capped_at(blend.to_str().unwrap(), 1024, 8192).unwrap();
    assert_eq!(got.len(), 2048);

    let plain = dir.join("big.jpg");
    let mut data = vec![0xFFu8, 0xD8, 0xFF, 0xE0];
    data.resize(2048, 0);
    std::fs::write(&plain, &data).unwrap();
    assert!(
        read_preview_capped_at(plain.to_str().unwrap(), 1024, 1536).is_err(),
        "oversized non-head-preview file keeps the hard refusal"
    );

    // Under the cap: identical to read_capped (whole file, any format).
    let got = read_preview_capped_at(plain.to_str().unwrap(), 4096, 1536).unwrap();
    assert_eq!(got.len(), 2048);
}

#[test]
fn read_preview_capped_under_cap_psd_reads_only_the_head() {
    // UNDER-cap opaque PSD with a baked thumbnail and a fat layer-data tail:
    // the fast path returns the exact head prefix. The same document without
    // the thumbnail, or with alpha, falls back to the whole file.
    let dir = std::env::temp_dir().join(format!("st2k_psd_head_fast_test_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let (psd, head_len) = crate::container::psd_testutil::synthetic_psd(3, true, 512 * 1024);
    let path = dir.join("big.psd");
    std::fs::write(&path, &psd).unwrap();
    let got = read_preview_capped_at(path.to_str().unwrap(), 100 << 20, 16 << 20).unwrap();
    assert_eq!(
        got.len(),
        head_len,
        "opaque PSD must read only the head prefix"
    );
    assert_eq!(got, &psd[..head_len]);

    let (bare, _) = crate::container::psd_testutil::synthetic_psd(3, false, 64 * 1024);
    let path = dir.join("bare.psd");
    std::fs::write(&path, &bare).unwrap();
    let got = read_preview_capped_at(path.to_str().unwrap(), 100 << 20, 16 << 20).unwrap();
    assert_eq!(got.len(), bare.len(), "no baked thumbnail -> whole file");

    let (alpha, _) = crate::container::psd_testutil::synthetic_psd(4, true, 64 * 1024);
    let path = dir.join("alpha.psd");
    std::fs::write(&path, &alpha).unwrap();
    let got = read_preview_capped_at(path.to_str().unwrap(), 100 << 20, 16 << 20).unwrap();
    assert_eq!(
        got.len(),
        alpha.len(),
        "transparent PSD -> whole file for the composite"
    );
}

#[test]
fn read_preview_capped_under_cap_dwg_and_gcode_read_only_the_head() {
    let dir = std::env::temp_dir().join(format!("st2k_head_fast_more_test_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // DWG: exact prefix = through the preview record's payload.
    let (dwg, head_len) = crate::container::dwg_testutil::synthetic_dwg(true, 512 * 1024);
    let path = dir.join("big.dwg");
    std::fs::write(&path, &dwg).unwrap();
    let got = read_preview_capped_at(path.to_str().unwrap(), 100 << 20, 16 << 20).unwrap();
    assert_eq!(
        got.len(),
        head_len,
        "DWG must read only through the preview record"
    );

    // G-code: the fast path is keyed on EXTENSION (no magic bytes) and uses
    // gcode::SCAN_LIMIT, which the extractor already clamps to — so the
    // shortened read must be byte-identical in RESULT to the whole file.
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(image::RgbaImage::new(8, 8))
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &png);
    let mut g = String::from("; generated by PrusaSlicer\n; thumbnail begin 16x16 999\n");
    for chunk in b64.as_bytes().chunks(78) {
        g.push_str("; ");
        g.push_str(std::str::from_utf8(chunk).unwrap());
        g.push('\n');
    }
    g.push_str("; thumbnail end\n");
    let head_bytes = g.len();
    // ~5 MB of toolpath behind the preview, pushing the file past SCAN_LIMIT.
    g.push_str(&"G1 X10 Y10 E1\n".repeat(380_000));
    let path = dir.join("big.gcode");
    std::fs::write(&path, g.as_bytes()).unwrap();
    assert!(std::fs::metadata(&path).unwrap().len() > (4 << 20) + head_bytes as u64);
    let got = read_preview_capped_at(path.to_str().unwrap(), 100 << 20, 16 << 20).unwrap();
    assert_eq!(
        got.len(),
        4 << 20,
        "G-code must read only gcode::SCAN_LIMIT"
    );
    assert!(
        crate::container::extract_cover(&got).is_some(),
        "the SCAN_LIMIT prefix must still yield the slicer thumbnail"
    );

    // A SMALL G-code file (under SCAN_LIMIT) gets the ordinary whole read —
    // the prefix would not be smaller, so the fast path declines.
    let small = dir.join("small.gcode");
    let body = b"G28\nG1 X0 Y0\n";
    std::fs::write(&small, body).unwrap();
    let got = read_preview_capped_at(small.to_str().unwrap(), 100 << 20, 16 << 20).unwrap();
    assert_eq!(got.len(), body.len());
}

#[test]
fn read_preview_capped_rescues_oversized_clip() {
    // A .clip past the byte cap must yield its embedded preview PNG via the
    // tail-database seek (the CLI twin of the provider's IStream rescue) —
    // not the hard refusal, and not a head prefix (the preview is NOT in
    // the head; the db sits after the layer-data padding).
    let dir = std::env::temp_dir().join(format!("st2k_clip_preview_test_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let png = [0x89u8, b'P', b'N', b'G', 42, 42, 42, 42];
    let clip = crate::container::clip_testutil::synthetic_clip(&png, 64 * 1024, false);
    let path = dir.join("big.clip");
    std::fs::write(&path, &clip).unwrap();
    let got = read_preview_capped_at(path.to_str().unwrap(), 1024, 1536).unwrap();
    assert_eq!(got, &png[..]);
}

#[test]
fn tone_map_rescues_all_zero_alpha_float() {
    // A VFX render pass (emission/AOV EXR) can carry real RGB with the whole
    // alpha channel at 0. Tone-mapping must surface the RGB opaque instead of
    // producing a fully-transparent image the blank-thumbnail watchdog rejects.
    let mut buf = image::Rgba32FImage::new(2, 2);
    for p in buf.pixels_mut() {
        *p = image::Rgba([0.5f32, 0.25, 1.5, 0.0]);
    }
    let out = tone_map_float(&DynamicImage::ImageRgba32F(buf)).to_rgba8();
    assert!(
        out.pixels().all(|p| p.0[3] == 255),
        "all-zero alpha must be rescued to opaque"
    );
    assert!(out.pixels().all(|p| p.0[0] > 0), "RGB content must survive");

    // PARTIAL alpha is compositing intent and must be preserved verbatim.
    let mut buf = image::Rgba32FImage::new(2, 1);
    buf.put_pixel(0, 0, image::Rgba([1.0f32, 1.0, 1.0, 1.0]));
    buf.put_pixel(1, 0, image::Rgba([1.0f32, 1.0, 1.0, 0.0]));
    let out = tone_map_float(&DynamicImage::ImageRgba32F(buf)).to_rgba8();
    assert_eq!(out.get_pixel(0, 0).0[3], 255);
    assert_eq!(
        out.get_pixel(1, 0).0[3],
        0,
        "partial transparency must survive untouched"
    );
}

#[test]
fn zero_alpha_exr_thumbnails_end_to_end() {
    // The full chain for a real all-transparent EXR: image-crate decode ->
    // Rgba32F -> tone_map_float rescue -> fit_to_box -> the fully-transparent
    // watchdog must NOT fire (this exact shape showed a default icon before).
    let mut buf = image::Rgba32FImage::new(8, 8);
    for p in buf.pixels_mut() {
        *p = image::Rgba([0.8f32, 0.2, 0.1, 0.0]);
    }
    let mut exr = Vec::new();
    DynamicImage::ImageRgba32F(buf)
        .write_to(
            &mut std::io::Cursor::new(&mut exr),
            image::ImageFormat::OpenExr,
        )
        .unwrap();
    let out = decode_thumbnail_opts(&exr, 64, false)
        .expect("zero-alpha EXR must thumbnail, not be rejected as blank");
    assert!(out.rgba.chunks_exact(4).any(|px| px[3] != 0));
}

/// A positional-ramp EXR: red encodes the column, green the row.
pub(crate) fn ramp_exr_bytes(w: u32, h: u32) -> Vec<u8> {
    let buf =
        image::Rgba32FImage::from_fn(w, h, |x, y| image::Rgba([x as f32, y as f32, 0.25, 1.0]));
    let mut out = Vec::new();
    DynamicImage::ImageRgba32F(buf)
        .write_to(
            &mut std::io::Cursor::new(&mut out),
            image::ImageFormat::OpenExr,
        )
        .expect("write ramp exr");
    out
}

#[test]
fn exr_paths_decode_scaled_off_the_file_handle() {
    let dir = std::env::temp_dir();
    let exr = dir.join(format!("st2k_exr_path_{}.exr", std::process::id()));
    std::fs::write(&exr, ramp_exr_bytes(600, 400)).expect("write temp exr");
    let path = exr.to_string_lossy().into_owned();

    // step = floor(600 / 64) = 9 -> ceil(600/9) x ceil(400/9) = 67x45.
    let img = decode_preview_streamed(&path, 64).expect("EXR must take the streaming path");
    assert_eq!((img.width(), img.height()), (67, 45));
    // Tone-mapped to 8-bit sRGB, like the `image` tier's float output.
    assert!(matches!(img, DynamicImage::ImageRgba8(_)));
    // `decode_preview_path` agrees with it.
    let same = decode_preview_path(&path, 64).expect("path decode");
    assert_eq!((same.width(), same.height()), (67, 45));
    let _ = std::fs::remove_file(&exr);

    // A non-EXR is left entirely to the ordinary bounded read + tiered decode.
    let png = dir.join(format!("st2k_exr_path_{}.png", std::process::id()));
    image::RgbaImage::new(9, 7)
        .save(&png)
        .expect("write temp png");
    let png_path = png.to_string_lossy().into_owned();
    assert!(decode_preview_streamed(&png_path, 64).is_none());
    assert_eq!(
        decode_preview_path(&png_path, 64)
            .map(|i| (i.width(), i.height()))
            .ok(),
        Some((9, 7))
    );
    let _ = std::fs::remove_file(&png);
}

#[test]
fn metafile_min_density_bumps_small_emf_only() {
    // Minimal EMF header: iType=1 (EMR_HEADER), rclBounds(16), rclFrame(16, .01mm), " EMF".
    let mut emf = vec![0u8; 88];
    emf[0..4].copy_from_slice(&1i32.to_le_bytes());
    emf[40..44].copy_from_slice(b" EMF");
    let set_frame = |b: &mut [u8], w: i32, h: i32| {
        b[24..28].copy_from_slice(&0i32.to_le_bytes()); // left
        b[28..32].copy_from_slice(&0i32.to_le_bytes()); // top
        b[32..36].copy_from_slice(&w.to_le_bytes()); // right
        b[36..40].copy_from_slice(&h.to_le_bytes()); // bottom
    };
    // ~0.67 inch (1693 units of .01 mm) → ~64px at 96 DPI → bump toward a 512px long edge.
    set_frame(&mut emf, 1693, 1000);
    let d = metafile_min_density(&emf).expect("small metafile → density bump");
    assert!((760..=772).contains(&d), "density ~768, got {d}");
    // A 10-inch frame (~960px at 96 DPI) is already large → no override.
    set_frame(&mut emf, 25400, 20000);
    assert_eq!(metafile_min_density(&emf), None, "large metafile untouched");
    // A tiny declared frame would compute a huge density; it must be CAPPED so magick's reader
    // can't be handed a value it chokes on (the pre-1.0.1 WMF crash class).
    set_frame(&mut emf, 100, 80); // ~0.04 in → uncapped would be ~13000
    assert_eq!(
        metafile_min_density(&emf),
        Some(1200),
        "tiny-frame density is capped"
    );
    // Placeable WMF is deliberately NOT bumped — its header bbox/Inch can disagree with the
    // metafile body, which is exactly what made a crafted WMF crash magick.
    let mut wmf = vec![0u8; 22];
    wmf[0..4].copy_from_slice(&[0xD7, 0xCD, 0xC6, 0x9A]);
    wmf[10..12].copy_from_slice(&72i16.to_le_bytes()); // bbox right
    wmf[12..14].copy_from_slice(&54i16.to_le_bytes()); // bbox bottom
    wmf[14..16].copy_from_slice(&1440u16.to_le_bytes()); // Inch
    assert_eq!(
        metafile_min_density(&wmf),
        None,
        "WMF left at intrinsic size"
    );
    assert_eq!(metafile_min_density(b"not a metafile at all ......"), None);
}

#[test]
fn svg_small_scales_up_to_min() {
    let svg = |w: u32, h: u32| {
        format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}"><rect width="{w}" height="{h}" fill="rgb(20,120,200)"/></svg>"#
            )
            .into_bytes()
    };
    // Small icon/logo → vector rendered UP to the 512px long edge (crisp), aspect preserved.
    let img = render_svg(&svg(24, 24)).expect("small svg renders");
    assert_eq!((img.width(), img.height()), (512, 512));
    let img = render_svg(&svg(48, 24)).expect("small wide svg renders");
    assert_eq!((img.width(), img.height()), (512, 256));
    // Already-large-enough SVG is left at its intrinsic size.
    let img = render_svg(&svg(800, 600)).expect("normal svg renders");
    assert_eq!((img.width(), img.height()), (800, 600));
    // Oversized SVG still clamps down to the 2048 ceiling.
    let img = render_svg(&svg(4000, 3000)).expect("huge svg renders");
    assert_eq!(img.width(), 2048);
}

/// A JPEG XL whose colour encoding is **AdobeRGB**, encoded with the exact cjxl 0.12
/// parameters from issue #9 (`-d 1.0 -m 1 -e 9 -p --faster_decoding 2 --brotli_effort 11`).
/// 256x256, in-gamut patches only, so every correct decoder must agree on the answer and
/// the assertion below can't be a gamut-mapping coin flip. Regenerate with:
///   cjxl g_adobe.png adobergb_modular.jxl -d 1.0 -m 1 -e 9 -p --faster_decoding 2 --brotli_effort 11
const JXL_ADOBERGB: &[u8] = include_bytes!("../../tests/fixtures/jxl/adobergb_modular.jxl");

#[test]
fn jxl_applies_its_embedded_color_profile() {
    // Issue #9: the jxl tier decoded correctly but never colour-managed, unlike the `image`
    // and WIC tiers. A wide-gamut jxl therefore reached Explorer with its raw AdobeRGB
    // numbers treated as sRGB, which is a visible shift on every saturated colour.
    let img = super::tiers::decode_jxl(JXL_ADOBERGB).expect("decode the AdobeRGB jxl");
    let rgb = img.to_rgb8();
    let px = rgb.get_pixel(16, 16).0;

    // The file's raw stored value. Seeing THIS is the bug: it means no profile was applied.
    assert_ne!(
        [px[0], px[1], px[2]],
        [180, 80, 80],
        "jxl decoded to its raw AdobeRGB numbers - the embedded profile was ignored"
    );
    // AdobeRGB(180,80,80) converted to sRGB. Cross-checked against djxl + LittleCMS, which
    // land on (206,79,79); allow a small delta for a different CMS's rounding.
    for (got, want) in px.iter().zip([206u8, 79, 79]) {
        assert!(
            (i32::from(*got) - i32::from(want)).abs() <= 4,
            "colour-managed jxl pixel {px:?} is not close to the expected [206,79,79]"
        );
    }
}

/// The PDF raster edge has been wrong twice, in opposite directions, so pin the rule.
///
/// It must never render SMALLER than the historical fixed 1024 (that would be a quality
/// regression), never render LARGER than the tile actually asked for (that was the red-team
/// finding: deriving it from the user's global ceiling made a 32 px icon request rasterize a
/// 2560 px page), and must follow a genuinely large request up so PDFs are not the one format
/// that upscales a too-small source once the ceiling exceeds 1024.
#[test]
fn pdf_raster_edge_follows_the_request_but_never_drops_below_1024() {
    use super::pdf_raster_edge;

    // Small icon views ask for far less than 1024; rasterizing lower would look worse than
    // the behaviour that shipped, so the floor holds.
    for cx in [1, 32, 96, 256, 768, 1024] {
        assert_eq!(
            pdf_raster_edge(Some(cx)),
            1024,
            "cx={cx} must still rasterize at the historical 1024 floor",
        );
    }
    // A genuinely large request is followed, which is the issue #26.5 half of the fix.
    assert_eq!(pdf_raster_edge(Some(1025)), 1025);
    assert_eq!(pdf_raster_edge(Some(2560)), 2560);
    // Full-fidelity callers (Convert, Image info) pass None and keep the historical edge.
    assert_eq!(pdf_raster_edge(None), 1024);
    // And it must NOT track the user's global ceiling: at the top setting, a tiny request is
    // still a tiny request. This is the assertion that fails if the regression comes back.
    assert!(
        pdf_raster_edge(Some(32)) < crate::settings::THUMB_MAX,
        "a small request must not rasterize at the global ceiling",
    );
}

/// MEASUREMENT INSTRUMENT — what the codec-scaled pre-pass costs PER FORMAT, against what
/// that format's thumbnail decode costs today. Prints a table; asserts nothing.
///
/// The pre-pass ([`wic_scaled_from_bytes_if_codec_scales`]) is gated to JPEG, and that gate's
/// doc comment says widening it is a RE-MEASUREMENT rather than a relaxed magic test. This is
/// that measurement, banked in the repo so the next person reads a number instead of rebuilding
/// the rig. Point it at a folder of LARGE samples — a 256 px thumbnail of a 256 px image proves
/// nothing, and every format here has a small file for which the answer is "don't bother".
///
/// Columns:
///   * `scales` — what `IWICBitmapSourceTransform::GetClosestSize` answers, plus the size it
///     offers. `no` ends the discussion for that format: there is nothing to win.
///   * `probe` — factory + decoder + `GetFrame` + `GetClosestSize` and no decode. This is the
///     pure cost a DECLINED probe adds to every file of that format, and therefore what the
///     `MIN_SCALED_BYTES` floor is really buying.
///   * `pre-pass` — the scaled decode itself, magic gate bypassed.
///   * `today` — [`decode_preview_capped`], i.e. exactly what ships.
///
/// Widening pays only where `pre-pass` is materially under `today`. A format whose tier already
/// lifts an embedded preview (camera RAW, PSD) shows the OPPOSITE, which is precisely why the
/// gate cannot be a magic-byte list.
///
/// ```text
/// $env:ST2K_FMT_DIR = "...\samples"
/// cargo test --release --lib scaled_pre_pass_sweep -- --ignored --nocapture
/// ```
#[test]
#[ignore = "measurement over a folder of large samples; set ST2K_FMT_DIR and run --release"]
fn scaled_pre_pass_sweep_by_format() {
    let Ok(dir) = std::env::var("ST2K_FMT_DIR") else {
        println!("set ST2K_FMT_DIR to a folder of large samples first");
        return;
    };
    let edge: u32 = std::env::var("ST2K_FMT_EDGE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(256);
    // Best-of, not mean: this box regularly sits at high background load, and the minimum is
    // the closest thing to "what the work actually costs" that a noisy machine will give up.
    let reps: usize = std::env::var("ST2K_FMT_REPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
        );
    }

    let mut files: Vec<std::path::PathBuf> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .map(|e| e.path())
            .collect(),
        Err(e) => {
            println!("cannot read {dir}: {e}");
            return;
        }
    };
    files.sort();

    /// Fastest of `reps` runs, in microseconds, plus whether the work ever succeeded.
    fn best_us(reps: usize, mut f: impl FnMut() -> bool) -> (u128, bool) {
        let (mut best, mut ok) = (u128::MAX, false);
        for _ in 0..reps.max(1) {
            let t = std::time::Instant::now();
            ok |= f();
            best = best.min(t.elapsed().as_micros());
        }
        (best, ok)
    }
    fn ms(us: u128) -> String {
        format!("{:.1}", us as f64 / 1000.0)
    }

    println!("\nscaled pre-pass sweep — target edge {edge} px, best of {reps}");
    println!("dir: {dir}\n");
    println!(
        "{:<14} {:>7} {:>12} {:>18} {:>8} {:>10} {:>10} {:>8}  MAD/out",
        "file", "MB", "pixels", "scales", "probe", "pre-pass", "today", "ratio"
    );
    println!("{}", "-".repeat(112));

    for p in &files {
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let Ok(bytes) = std::fs::read(p) else {
            continue;
        };
        let mb = bytes.len() as f64 / (1024.0 * 1024.0);

        // Three distinct answers, kept distinct on purpose. An earlier version of this sweep
        // printed "wic declines" for both "no codec" and "opened it, exposes no transform
        // interface", which reported TIFF as unreadable when WIC reads it perfectly well.
        let (dims, scales) = match unsafe { wic::wic_scaling_answer(&bytes) } {
            wic::ScalingAnswer::CannotOpen => ("-".to_string(), "wic cannot open".to_string()),
            wic::ScalingAnswer::NoTransform { w, h } => {
                (format!("{w}x{h}"), "no transform iface".to_string())
            }
            wic::ScalingAnswer::Offers { w, h, cw, ch } => (
                format!("{w}x{h}"),
                if cw < w || ch < h {
                    format!("yes {cw}x{ch}")
                } else {
                    "no (full size back)".to_string()
                },
            ),
        };
        let (probe_us, _) = best_us(reps, || {
            !matches!(
                unsafe { wic::wic_scaling_answer(&bytes) },
                wic::ScalingAnswer::CannotOpen
            )
        });

        let head = &bytes[..bytes.len().min(COLOR_HEAD_BYTES)];
        let (pre_us, pre_ok) = best_us(reps, || unsafe {
            wic::wic_decode_bytes_if_codec_scales(&bytes, edge, head).is_ok()
        });
        let (today_us, today_ok) = best_us(reps, || decode_preview_capped(&bytes, edge).is_ok());

        // FIDELITY, not just speed — the column without which this table is a trap. Several
        // codecs answer `GetClosestSize` with a size far BELOW the request (a HEIF `thmb` item,
        // a tile count), and a scaler that takes such an offer and upscales is enormously fast
        // and completely wrong. Compare the two decodes on a common grid: a real reduced-
        // resolution decode differs from the reference by resampling noise, a upscaled
        // postage stamp differs by a mile.
        let fidelity = match (
            unsafe { wic::wic_decode_bytes_if_codec_scales(&bytes, edge, head) },
            decode_preview_capped(&bytes, edge),
        ) {
            (Ok(pre), Ok(reference)) => {
                let (pw, ph) = (pre.width(), pre.height());
                let a = pre.resize_exact(64, 64, image::imageops::FilterType::Triangle);
                let b = reference.resize_exact(64, 64, image::imageops::FilterType::Triangle);
                let (a, b) = (a.to_rgb8(), b.to_rgb8());
                let sum: u64 = a
                    .pixels()
                    .zip(b.pixels())
                    .map(|(x, y)| (0..3).map(|c| x.0[c].abs_diff(y.0[c]) as u64).sum::<u64>())
                    .sum();
                let mad = sum as f64 / (64.0 * 64.0 * 3.0);
                if let Ok(dir) = std::env::var("ST2K_FMT_OUT") {
                    let _ = std::fs::create_dir_all(&dir);
                    let _ = pre.save(std::path::Path::new(&dir).join(format!("{name}.pre.png")));
                    let _ =
                        reference.save(std::path::Path::new(&dir).join(format!("{name}.ref.png")));
                }
                format!("{mad:.1} {pw}x{ph}")
            }
            _ => "-".to_string(),
        };

        // A JPEG over the floor ALREADY takes the pre-pass, so its `today` is the fast number
        // and the ratio is 1.0 by construction. Mark it rather than let the table read as
        // "JPEG gains nothing".
        let shipped = bytes.starts_with(&[0xFF, 0xD8, 0xFF]) && bytes.len() >= 512 * 1024;
        let ratio = match (pre_ok, today_ok) {
            (true, true) if shipped => "(wired)".to_string(),
            (true, true) => format!("{:.1}x", today_us as f64 / pre_us.max(1) as f64),
            (false, _) => "declined".to_string(),
            (_, false) => "no decode".to_string(),
        };
        println!(
            "{:<14} {:>7.1} {:>12} {:>18} {:>8} {:>10} {:>10} {:>8}  {}",
            name,
            mb,
            dims,
            scales,
            ms(probe_us),
            if pre_ok { ms(pre_us) } else { "-".into() },
            if today_ok { ms(today_us) } else { "-".into() },
            ratio,
            fidelity
        );
    }
    println!(
        "\n(times ms; `probe` is what a DECLINED probe adds per file. MAD is mean absolute \n\
         per-channel difference from the shipping decode on a common 64x64 grid — single \n\
         digits are resampling noise, tens mean the pre-pass returned a different picture.)"
    );
}

/// The WIC bomb guard bounds what we COPY OUT, and for a thumbnail that is not the source.
///
/// The rule this pins cost three real files to find: a 24000x14160 PNG (309 MB) was refused
/// outright, even though the requested thumbnail was 256x151 and WIC produces it by streaming
/// rows into the scaler — measured at 2.1 s with no measurable growth in the process working
/// set. Two of the three were UNDER the byte cap, so they were read and decoded for 12-24 s
/// before this guard threw the result away and Explorer drew the stock icon.
#[test]
fn the_wic_guard_bounds_the_output_when_scaling_and_the_source_when_not() {
    use super::limits::{MAX_DIM, MAX_PIXELS, MAX_SCALED_SOURCE_PIXELS};
    use super::wic::wic_source_within_limits;

    // The file that started it: past MAX_DIM on both edges and past MAX_PIXELS in total.
    assert!(
        wic_source_within_limits(24_000, 14_160, Some(256)),
        "a 340 MP source must be accepted for a 256 px thumbnail — the scaler decides what we \
         copy out, and that is 256x151 whatever arrives"
    );
    // ...and the SAME source is still refused when the caller wants every pixel, because then
    // the source really is what we materialize. This half is what keeps Convert/Image-info
    // bounded, and it is why the guard could not simply be relaxed.
    assert!(
        !wic_source_within_limits(24_000, 14_160, None),
        "a full-fidelity decode of the same source must stay refused"
    );

    // A source already within the requested edge is a full decode wearing a thumbnail's
    // clothes: no scaler engages, so it gets the full decode's ceiling.
    assert!(!wic_source_within_limits(20_000, 20_000, Some(32_768)));

    // The scaled ceiling is real, not absent — a decompression bomb costs time even when it
    // costs no memory, and time is what an isolated host actually runs out of.
    let half = (MAX_SCALED_SOURCE_PIXELS / 2) as u32;
    assert!(
        wic_source_within_limits(half, 2, Some(256)),
        "exactly AT the ceiling is allowed — the boundary belongs to the accepted side"
    );
    assert!(
        !wic_source_within_limits(half + 1, 2, Some(256)),
        "past MAX_SCALED_SOURCE_PIXELS a scaled decode must still be refused"
    );

    // Degenerate sizes are refused on either path.
    for cx in [None, Some(256)] {
        assert!(!wic_source_within_limits(0, 100, cx));
        assert!(!wic_source_within_limits(100, 0, cx));
    }

    // The unscaled path is untouched: exactly MAX_DIM square is the historical boundary.
    assert!(wic_source_within_limits(MAX_DIM, MAX_DIM, None));
    assert!(!wic_source_within_limits(MAX_DIM + 1, 1, None));
    assert_eq!(MAX_PIXELS * 4, MAX_SCALED_SOURCE_PIXELS);
}

/// The widened scaled-decode ceiling must NOT be reachable from inside `explorer.exe`.
///
/// `MAX_SCALED_SOURCE_PIXELS` lets a thumbnail request stream a source four times past
/// `MAX_PIXELS`, which is safe because thumbnails are drawn in an ISOLATED host — a hostile
/// file costs a throwaway `dllhost`, and the measured worst case there is ~4 s. The classic
/// context menu's preview tile is the one decode that runs in-process on Explorer's own UI
/// thread under `panic = "abort"`, so it must keep the strict guard.
///
/// It does, but only because `decode_menu_preview` -> `decode_cheap` -> `decode_any` passes
/// `None` for the target edge. That is a property of the call graph, and call graphs get
/// refactored, so this pins the BEHAVIOUR instead.
///
/// **The fixture is 20000x20, and the shape is the whole point.** It is past `MAX_DIM` on one
/// edge (so the strict guard refuses it) while being 0.4 MP in total (so the widened guard
/// accepts it AND a real decode is instant). An earlier version of this test used a 20000x20000
/// bomb with a deliberately invalid payload, reasoning that every guard rejects on the declared
/// size before inflating anything — and it passed even when `decode_any` was edited to pass
/// `Some(256)`, because the garbage payload failed to decode either way. It was asserting
/// nothing. Valid pixels are what make the failure mode observable: if the guard ever lets this
/// through, the decode SUCCEEDS and the assertion below fires.
#[test]
fn the_in_process_menu_path_never_gets_the_widened_ceiling() {
    use super::limits::{MAX_DIM, MAX_SCALED_SOURCE_PIXELS};

    const W: u32 = 20_000;
    const H: u32 = 20;
    // The fixture's two properties are decidable at compile time, so a bad edit should fail the
    // BUILD rather than wait for someone to run the tests — the same call `settings` makes for
    // its constant relationships.
    const _: () = assert!(
        W > MAX_DIM,
        "fixture must exceed the strict per-edge guard, or the split is not being tested"
    );
    const _: () = assert!(
        (W as u64) * (H as u64) < MAX_SCALED_SOURCE_PIXELS,
        "fixture must sit UNDER the widened ceiling, or this proves nothing about the split"
    );

    // The split itself, stated as the pure rule both paths consult.
    assert!(
        wic::wic_source_within_limits(W, H, Some(256)),
        "a thumbnail request may stream this source — it runs in an isolated host"
    );
    assert!(
        !wic::wic_source_within_limits(W, H, None),
        "a full decode of the same source must stay refused"
    );

    // Real, decodable pixels — see the note above about why an invalid payload proved nothing.
    let png = png_bytes(W, H, [20, 140, 90, 255]);
    // The `image` tier declines it first (its own Limits carry MAX_DIM), so this genuinely
    // reaches the WIC tier's guard rather than being rejected earlier for an unrelated reason.
    assert!(
        decode_with_image(&png).is_err(),
        "the image tier must decline it, or the WIC guard is not what this test measures"
    );
    assert!(
        decode_menu_preview(&png).is_err(),
        "the in-process context-menu preview must refuse a source past the strict guard — it          runs inside explorer.exe under panic=abort, where there is no isolated host to lose"
    );
}

/// Wrap a JPEG's bytes with an EXIF APP1 declaring `orientation` (1..=8).
///
/// Hand-assembled rather than pulled from a corpus file so the test states exactly what it
/// depends on: one IFD0 entry, tag 0x0112, little-endian TIFF.
fn with_exif_orientation(jpeg: &[u8], orientation: u16) -> Vec<u8> {
    assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "fixture must be a JPEG");
    let mut app1: Vec<u8> = Vec::new();
    app1.extend_from_slice(b"Exif\0\0");
    app1.extend_from_slice(b"II\x2A\x00"); // little-endian TIFF magic
    app1.extend_from_slice(&8u32.to_le_bytes()); // IFD0 begins at offset 8
    app1.extend_from_slice(&1u16.to_le_bytes()); // one entry
    app1.extend_from_slice(&0x0112u16.to_le_bytes()); // Orientation
    app1.extend_from_slice(&3u16.to_le_bytes()); // type SHORT
    app1.extend_from_slice(&1u32.to_le_bytes()); // count
    app1.extend_from_slice(&(orientation as u32).to_le_bytes()); // value, left-packed
    app1.extend_from_slice(&0u32.to_le_bytes()); // no next IFD

    let mut out = Vec::with_capacity(jpeg.len() + app1.len() + 4);
    out.extend_from_slice(&jpeg[..2]); // SOI
    out.extend_from_slice(&[0xFF, 0xE1]);
    out.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(&app1);
    out.extend_from_slice(&jpeg[2..]);
    out
}

/// A large camera JPEG must come out of the DCT-scaled fast path the right way up.
///
/// The fast path added for large-JPEG thumbnail performance returns early, and an early
/// return is a return PAST `apply_exif_orientation`. WIC hands back the codec's stored
/// pixels and never orients them itself, so every portrait phone photo over the 512 KiB
/// floor thumbnailed on its side, and Explorer then cached it that way.
///
/// The test is two-sided ON PURPOSE. Asserting only that the final image is portrait would
/// also pass if WIC declined and the ordinary tiers (which always oriented correctly) ran
/// instead, i.e. it would pass without ever measuring the path it exists to measure. So it
/// first pins that the fast path is genuinely taken AND that its raw output is unrotated,
/// and only then that the public entry point rotates it.
#[test]
fn the_scaled_jpeg_fast_path_still_applies_exif_orientation() {
    // The fast path IS WIC, so it needs COM on this thread like every other WIC test here.
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
        );
    }
    // Landscape, and noisy enough to clear the 512 KiB floor that gates the fast path.
    let base = noisy_jpeg_bytes(1400, 900);
    let bytes = with_exif_orientation(&base, 6); // 6 = rotate 90 CW
    assert!(
        bytes.len() >= 512 * 1024,
        "fixture is {} bytes, under the fast path's floor: it would prove nothing",
        bytes.len()
    );
    assert_eq!(
        exif_orientation(&bytes),
        Some(6),
        "the APP1 must be readable"
    );

    // 1) The fast path really runs for this input, and really returns unrotated pixels.
    let raw = wic_scaled_from_bytes_if_codec_scales(&bytes, 256)
        .expect("WIC must take the scaled path for a >512 KiB JPEG");
    assert!(
        raw.width() > raw.height(),
        "WIC is expected to return the stored, unoriented landscape pixels"
    );

    // 2) The public thumbnail entry point must nonetheless hand back a PORTRAIT tile.
    let out = decode_preview_thumbnail(&bytes, 256).expect("thumbnail decode must succeed");
    assert!(
        out.height() > out.width(),
        "orientation 6 must rotate the landscape source to portrait, got {}x{}",
        out.width(),
        out.height()
    );
}

/// A genuine, minimal PSX TIM: the 8-byte header (magic `0x10`, mode 2 = 16-bit
/// direct colour, no CLUT) followed by one image block. Built in code rather than
/// committed as a binary so the assertion runs on every machine, and verified
/// against ImageMagick itself before it was written down: `magick identify` reports
/// `TIM 4x4` and every pixel decodes to pure red.
///
/// TIM is the sharp end of the name-selected-coder problem. It carries no signature
/// ImageMagick can sniff, so a nameless stream is undecodable — and unlike RLA/MDC
/// it also refuses a forced `tim:-` coder prefix ("insufficient image data"), which
/// is why [`decode_by_extension`] stages a real file instead.
fn synthetic_tim() -> Vec<u8> {
    const W: u16 = 4;
    const H: u16 = 4;
    const RED_BGR555: u16 = 0x001F;
    let mut px = Vec::new();
    for _ in 0..(W as usize * H as usize) {
        px.extend_from_slice(&RED_BGR555.to_le_bytes());
    }
    let mut out = Vec::new();
    out.extend_from_slice(&0x10u32.to_le_bytes()); // TIM magic
    out.extend_from_slice(&2u32.to_le_bytes()); // flags: 16-bit direct
    out.extend_from_slice(&(12 + px.len() as u32).to_le_bytes()); // block length
    out.extend_from_slice(&0u16.to_le_bytes()); // frame-buffer x
    out.extend_from_slice(&0u16.to_le_bytes()); // frame-buffer y
    out.extend_from_slice(&W.to_le_bytes());
    out.extend_from_slice(&H.to_le_bytes());
    out.extend_from_slice(&px);
    out
}

/// The premise of the whole fallback: a TIM reaches the end of the tiers undecoded.
/// If this ever starts passing, some tier learned to read TIM and
/// [`decode_by_extension`] is no longer load-bearing for it — check before deleting.
#[test]
fn a_tim_is_declined_by_every_ordinary_tier() {
    assert!(
        decode_preview(&synthetic_tim()).is_err(),
        "TIM has no sniffable signature, so the nameless tiers cannot decode it"
    );
}

#[test]
fn naming_the_coder_decodes_a_tim_to_the_right_colour() {
    if !magick_available() {
        // Loud, because a skip that reads as a pass is worse than no test at all.
        eprintln!("SKIPPED naming_the_coder_decodes_a_tim_to_the_right_colour: no ImageMagick");
        return;
    }
    let img = decode_by_extension(&synthetic_tim(), "tim", None)
        .expect("naming the coder must let ImageMagick read a real TIM");
    assert_eq!((img.width(), img.height()), (4, 4));
    let px = img.to_rgba8();
    let [r, g, b, _] = px.get_pixel(2, 2).0;
    assert!(
        r > 200 && g < 60 && b < 60,
        "the TIM is pure red; got ({r},{g},{b}) — a decode that returns the wrong \
         pixels is the failure this asserts against, not merely a decode that errors"
    );
}

/// The routing gate. Naming a coder skips ImageMagick's own detection, so it is
/// offered ONLY for the formats that cannot be sniffed at all. A sniffable format
/// must never be force-routed, however plausible the extension looks.
#[test]
fn only_unsniffable_formats_are_offered_a_named_coder() {
    for ext in [
        "tim", "rla", "cut", "mac", "pix", "jnx", "scr", "nef", "mdc", "TIM", ".tim",
    ] {
        assert!(
            extension_has_named_coder(ext),
            "{ext} has a name-selected ImageMagick coder and no other tier"
        );
    }
    for ext in [
        "png", "jpg", "gif", "webp", "psd", "xcf", "bmp", "tiff", "svg", "rle",
    ] {
        assert!(
            !extension_has_named_coder(ext),
            "{ext} is sniffable — forcing a coder would bypass ImageMagick's detection"
        );
    }
}

/// The extension only ever becomes part of a temp file NAME, so it must not be able
/// to steer that name. Refused, not escaped.
#[test]
fn a_crafted_extension_cannot_steer_the_staged_file() {
    let tim = synthetic_tim();
    for ext in [
        "../../evil",
        "a/b",
        r"a\b",
        "",
        "tim.exe",
        "waytoolongextension",
        "ti m",
        "t:m",
    ] {
        assert!(
            decode_by_extension(&tim, ext, None).is_err(),
            "{ext:?} must be refused outright"
        );
    }
}
