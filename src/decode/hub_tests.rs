#![cfg(test)]

use super::*;

/// Issue #41's arithmetic: a preview under a quarter of the declared picture on its
/// longer edge is refused for a full-fidelity caller; at or above it (the 4096-edge
/// ImageMagick cap on a 16000 px file), or with no header to compare against, it is
/// returned as before.
#[test]
fn a_preview_a_fraction_of_the_declared_picture_is_refused_with_both_sizes_named() {
    let preview = |w, h| DynamicImage::new_rgb8(w, h);
    let judge = refuse_a_preview_standing_in_for_the_picture;
    let err = judge(preview(107, 160), Some((5464, 8192)))
        .expect_err("the reporter's shape: a 107x160 stand-in for a 5464x8192 photograph");
    let msg = err.message();
    assert!(
        msg.contains("107x160") && msg.contains("5464x8192"),
        "{msg}"
    );
    // A RAW whose IFD0 declares only its thumbnail: the preview is BIGGER than declared.
    assert!(judge(preview(1632, 1080), Some((160, 120))).is_ok());
    // The ImageMagick 4096 cap on a 16000 px picture is a real decode that hit a guard we
    // set, not a stand-in - even though it is far under a quarter.
    assert!(judge(preview(4096, 3072), Some((16000, 12000))).is_ok());
    // A Hasselblad .fff's 1217x913 preview of a 40 MP sensor: under a quarter, and still a
    // picture. This is the case the absolute floor exists for.
    assert!(judge(preview(1217, 913), Some((8176, 6132))).is_ok());
    // Exactly a quarter AND thumbnail-sized is a stand-in; one pixel over the quarter is not.
    assert!(judge(preview(500, 375), Some((2000, 1500))).is_err());
    assert!(judge(preview(501, 375), Some((2000, 1500))).is_ok());
    // Over the floor, whatever the ratio says.
    assert!(judge(preview(513, 384), Some((20000, 15000))).is_ok());
    // No readable header: nothing to compare against, so nothing is refused.
    assert!(judge(preview(160, 120), None).is_ok());
}

/// Issue #41 through the real header reader and the real last-resort carve: a PNG whose
/// header declares 5000x5000 and whose pixel data is garbage, with a real 160x107 JPEG
/// appended after IEND. The carve finds the stamp, and the rule every file-writing caller
/// applies refuses exactly that result, naming both sizes.
///
/// Deliberately NOT driven through `decode_full_for_output` end to end. Which tiers can
/// read this file depends on the test PROCESS: once any other test has initialised COM,
/// WIC "decodes" the corrupt IDAT as a black 5000x5000 canvas, so the full-fidelity decode
/// never reaches the stand-in (green alone, red in the full suite - measured). That black
/// canvas is a full-size decode and outside this rule by design.
#[test]
fn a_full_fidelity_decode_refuses_the_postage_stamp_a_thumbnail_may_show() {
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        let mut crc = 0xFFFF_FFFFu32;
        for &b in kind.iter().chain(body) {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        out.extend_from_slice(&(!crc).to_be_bytes());
    }
    let mut file = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&5000u32.to_be_bytes());
    ihdr.extend_from_slice(&5000u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit RGB, deflate, no interlace
    chunk(&mut file, b"IHDR", &ihdr);
    chunk(&mut file, b"IDAT", &[0x55; 64]); // not a zlib stream: every decoder refuses it
    chunk(&mut file, b"IEND", &[]);
    let mut jpeg = Vec::new();
    let stamp = image::RgbImage::from_fn(160, 107, |x, y| image::Rgb([x as u8, y as u8, 90]));
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut std::io::Cursor::new(&mut jpeg), 80)
        .encode_image(&stamp)
        .expect("encode the stamp");
    assert!(
        jpeg.len() >= tiers::LENIENT_RAW_PREVIEW,
        "the stamp must clear the lenient floor"
    );
    file.extend_from_slice(&jpeg);

    let declared = declared_dimensions(&file);
    assert_eq!(declared, Some((5000, 5000)));
    let stand_in = try_embedded_jpeg_last_resort(&file).expect("the appended stamp is found");
    assert_eq!((stand_in.width(), stand_in.height()), (160, 107));
    let err = refuse_a_preview_standing_in_for_the_picture(stand_in, declared)
        .expect_err("a decode headed for a FILE refuses it");
    let msg = err.message();
    assert!(
        msg.contains("160x107") && msg.contains("5000x5000"),
        "{msg}"
    );
}

#[test]
fn exceeds_alloc_budget_flags_a_max_dim_legal_hdr_frame_that_blows_the_alloc_cap() {
    // 16000x16000 clears MAX_DIM (16384) — the only guard `HdrDecoder::set_limits`
    // actually applies, since it never overrides the trait's dimension-only default —
    // but at Rgb32F's 12 bytes/px that is ~2.86 GiB, more than 5x MAX_ALLOC.
    assert!(exceeds_alloc_budget(16_000, 16_000, 12, limits::MAX_ALLOC));
}

#[test]
fn exceeds_alloc_budget_allows_an_ordinary_photo_sized_rgba_frame() {
    assert!(!exceeds_alloc_budget(4_000, 3_000, 4, limits::MAX_ALLOC));
}

#[test]
fn exceeds_alloc_budget_is_exact_at_the_boundary() {
    // Saturating arithmetic must not round the boundary away in either direction.
    assert!(!exceeds_alloc_budget(1, 1, 1, 1));
    assert!(exceeds_alloc_budget(1, 1, 2, 1));
}

/// `try_raw_preview_tier`'s gate must accept every RAW shape it used to scan
/// unconditionally, and decline the container formats the fix exists to stop
/// scanning for (an O(file) embedded-JPEG walk those never carry a preview in).
#[test]
fn looks_raw_container_accepts_raw_signatures_and_declines_isobmff() {
    assert!(looks_raw_container(
        b"II\x2A\0rest of a little-endian TIFF/CR2/NEF/ARW"
    ));
    assert!(looks_raw_container(
        b"MM\0\x2Arest of a big-endian TIFF/DNG"
    ));
    assert!(looks_raw_container(b"II\x2B\0rest of a BigTIFF"));
    assert!(looks_raw_container(b"MM\0\x2Brest of a big-endian BigTIFF"));
    // 2026-09-05 audit F38: Canon CRW (CIFF, not TIFF) starts `II 1A 00`, one byte off
    // from the TIFF magic `II 2A 00` above, and used to fall through this gate entirely.
    assert!(looks_raw_container(b"II\x1A\0rest of a Canon CRW"));
    assert!(looks_raw_container(b"FUJIFILMCCD-RAW rest of a Fuji RAF"));
    assert!(looks_raw_container(b"FFF\0rest of a Hasselblad 3FR"));
    assert!(looks_raw_container(b"IIU\0rest of a Panasonic RW2"));
    assert!(looks_raw_container(
        &[b"    ftypcrx ".as_slice(), b"rest of a Canon CR3"].concat()
    ));
    // HEIC/AVIF share the same `ftyp` box shape but a different brand — must not match.
    assert!(!looks_raw_container(
        &[b"    ftypheic".as_slice(), b"rest of an HEIC"].concat()
    ));
    assert!(!looks_raw_container(b"\x89PNG\r\n\x1a\nrest of a PNG"));
    assert!(!looks_raw_container(b""));
    assert!(!looks_raw_container(b"short"));
}

/// `decode_menu_preview` must hand a real target edge down through
/// `decode_cheap` (`MENU_PREVIEW_TARGET_EDGE`, not `None`) — that is what lets the
/// DDS tier's mip selection engage for a mipless texture, and it is directly
/// observable here for an ordinary large image via `try_image_tier`'s pre-reduce:
/// with `None` the source would come back at full resolution.
#[test]
fn decode_menu_preview_passes_a_target_edge_so_a_large_image_is_pre_reduced() {
    let big = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(1200, 1200, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
    }));
    let mut bytes = Vec::new();
    big.write_to(
        &mut std::io::Cursor::new(&mut bytes),
        image::ImageFormat::Png,
    )
    .expect("encode synthetic PNG");
    let out = decode_menu_preview(&bytes).expect("must still decode");
    assert!(
        out.width() < 1200 && out.height() < 1200,
        "a large source must be pre-reduced toward MENU_PREVIEW_TARGET_EDGE, not \
         decoded at full resolution: got {}x{}",
        out.width(),
        out.height()
    );
}

const TEST_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"></svg>"#;

fn gzip(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    enc.write_all(bytes).expect("in-memory gzip write");
    enc.finish().expect("in-memory gzip finish")
}

#[test]
fn decode_svg_if_svg_decodes_a_bare_svg_and_reports_no_inflated_bytes() {
    let (img, inner) = decode_svg_if_svg(TEST_SVG);
    assert!(img.is_some());
    assert!(inner.is_none());
}

#[test]
fn decode_svg_if_svg_decodes_an_svgz_and_still_hands_back_the_inflated_bytes() {
    // The three call sites this was extracted from (decode_menu_preview, decode_cover,
    // decode_image_with_raw_order) differ only in whether they use the second element;
    // the SVGZ case must keep returning it even when decode already succeeded, since
    // `decode_image_with_raw_order` doesn't consult it unless `img` is `None`.
    let (img, inner) = decode_svg_if_svg(&gzip(TEST_SVG));
    assert!(img.is_some());
    assert!(inner.is_some());
}

#[test]
fn decode_svg_if_svg_declines_plain_non_svg_bytes() {
    let (img, inner) = decode_svg_if_svg(b"not an svg");
    assert!(img.is_none());
    assert!(inner.is_none());
}

#[test]
fn decode_svg_if_svg_hands_back_inflated_bytes_for_a_non_svg_gzip_like_emz() {
    // `.emz` (gzipped EMF/WMF): not SVG, but `decode_image_with_raw_order` needs the
    // inflated bytes to try a raster decode on them without inflating twice.
    let (img, inner) = decode_svg_if_svg(&gzip(b"not svg either"));
    assert!(img.is_none());
    assert_eq!(inner.as_deref(), Some(&b"not svg either"[..]));
}
