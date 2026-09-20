#![cfg(test)]

use super::*;

#[test]
fn atomic_overwrite_notifies_the_rewritten_item_only_after_success() {
    let dir = std::env::temp_dir().join(format!("st2k_strip_notify_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("rewritten.jpg");
    std::fs::write(&path, b"old").unwrap();

    let mut notified = None;
    atomic_overwrite_with(&path, b"new", |updated| {
        notified = Some(updated.to_path_buf())
    })
    .unwrap();

    assert_eq!(std::fs::read(&path).unwrap(), b"new");
    assert_eq!(notified.as_deref(), Some(path.as_path()));

    let missing = dir.join("missing").join("never-written.jpg");
    let mut failed_notify = false;
    assert!(atomic_overwrite_with(&missing, b"new", |_| failed_notify = true).is_err());
    assert!(!failed_notify, "a failed rewrite must not notify Explorer");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The in-place rewrite must keep the file's identity: a plain temp+rename gave the
/// name to a brand-new file, which dropped the Hidden attribute and every alternate
/// data stream (the Zone.Identifier mark-of-the-web among them). `ReplaceFileW` keeps
/// both, and the content is still the new bytes.
#[test]
fn atomic_overwrite_keeps_attributes_and_alternate_streams() {
    use windows::Win32::Storage::FileSystem::{
        GetFileAttributesW, SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_NORMAL,
    };
    let dir = std::env::temp_dir().join(format!("st2k_strip_attrs_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("marked.jpg");
    std::fs::write(&path, b"old").unwrap();
    // An NTFS alternate data stream; a non-NTFS temp volume cannot hold one, in which
    // case only the attribute half is checked.
    let ads = format!("{}:Zone.Identifier", path.display());
    let has_ads = std::fs::write(&ads, b"[ZoneTransfer]\r\nZoneId=3\r\n").is_ok();
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(once(0)).collect();
    unsafe { SetFileAttributesW(PCWSTR(wide.as_ptr()), FILE_ATTRIBUTE_HIDDEN) }.unwrap();

    atomic_overwrite_with(&path, b"new", |_| {}).unwrap();

    assert_eq!(std::fs::read(&path).unwrap(), b"new");
    let attrs = unsafe { GetFileAttributesW(PCWSTR(wide.as_ptr())) };
    assert_ne!(
        attrs & FILE_ATTRIBUTE_HIDDEN.0,
        0,
        "the Hidden attribute was lost across the rewrite"
    );
    if has_ads {
        assert!(
            std::fs::read(&ads)
                .map(|b| b.starts_with(b"[ZoneTransfer]"))
                .unwrap_or(false),
            "the alternate data stream was lost across the rewrite"
        );
    }
    assert!(
        !path.with_extension("jpg.st2ktmp").exists(),
        "temp file left behind"
    );

    unsafe { SetFileAttributesW(PCWSTR(wide.as_ptr()), FILE_ATTRIBUTE_NORMAL) }.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A Multi-Picture Format JPEG (APP2 `MPF\0`) is refused whole: its index names byte
/// offsets that stripping would move, and the result is written over the original.
#[test]
fn refuses_to_strip_a_multi_picture_mpf_jpeg() {
    let dir = std::env::temp_dir().join(format!("st2k_strip_mpf_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let jpg = dir.join("hdr.jpg");

    let mut base = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        16,
        12,
        image::Rgb([40, 90, 160]),
    ))
    .write_to(
        &mut std::io::Cursor::new(&mut base),
        image::ImageFormat::Jpeg,
    )
    .unwrap();
    let mut out = base[0..2].to_vec(); // SOI
    for (marker, payload) in [
        (markers::APP1, &b"Exif\0\0sometagdata"[..]),
        (
            markers::APP2,
            &b"MPF\0II*\0\x08\0\0\0secondary-image-index"[..],
        ),
    ] {
        out.extend_from_slice(&[0xFF, marker]);
        out.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(payload);
    }
    out.extend_from_slice(&base[2..]);
    out.extend_from_slice(b"\xFF\xD8appended-gain-map\xFF\xD9");
    std::fs::write(&jpg, &out).unwrap();

    assert!(
        strip_metadata(jpg.to_str().unwrap()).is_err(),
        "MPF must refuse"
    );
    assert_eq!(
        std::fs::read(&jpg).unwrap(),
        out,
        "a refused strip must leave the file byte-for-byte as it was"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A minimal little-endian TIFF/EXIF block carrying a single `Make` tag.
/// Layout: header(8) | entry count(2) | one 12-byte entry | next-IFD(4) |
/// the ASCII value at offset 26.
fn tiny_exif(make: &[u8; 6]) -> Vec<u8> {
    let mut v = b"II*\0".to_vec();
    v.extend_from_slice(&8u32.to_le_bytes()); // IFD0 offset
    v.extend_from_slice(&1u16.to_le_bytes()); // one entry
    v.extend_from_slice(&0x010Fu16.to_le_bytes()); // Make
    v.extend_from_slice(&2u16.to_le_bytes()); // ASCII
    v.extend_from_slice(&6u32.to_le_bytes()); // count
    v.extend_from_slice(&26u32.to_le_bytes()); // value offset
    v.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
    v.extend_from_slice(make);
    v
}

/// PNG has carried real EXIF in an `eXIf` chunk since the 2017 spec change,
/// and the competitor sweep flagged it as something we might be ignoring.
/// We are not: `kamadak-exif` reads the chunk, so `read_info` fills in from a
/// PNG exactly as it does from a JPEG. This test is what proves it stays true.
#[test]
fn reads_exif_from_a_png_exif_chunk() {
    let dir = std::env::temp_dir().join(format!("st2k_png_exif_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let png_path = dir.join("e.png");

    let mut base = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(4, 4, image::Rgb([9, 9, 9])))
        .write_to(
            &mut std::io::Cursor::new(&mut base),
            image::ImageFormat::Png,
        )
        .unwrap();

    let mut png = Png::from_bytes(Bytes::from(base)).unwrap();
    let chunk = img_parts::png::PngChunk::new(*b"eXIf", Bytes::from(tiny_exif(b"SageT\0")));
    png.chunks_mut().insert(1, chunk);
    std::fs::write(&png_path, png.encoder().bytes()).unwrap();

    let info = read_info(png_path.to_str().unwrap());
    assert_eq!(info.width, 4);
    assert_eq!(
        info.make.as_deref(),
        Some("SageT"),
        "PNG eXIf chunk was not read"
    );

    // ...and Strip removes it, which the eXIf entry in the PNG arm covers.
    strip_metadata(png_path.to_str().unwrap()).unwrap();
    let after = read_info(png_path.to_str().unwrap());
    assert_eq!(after.make, None, "PNG eXIf survived the strip");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The whole point of the APP11 work: a C2PA manifest goes, a JPEG XT layer
/// wearing the same marker stays, and the pixels are untouched either way.
#[test]
fn strips_c2pa_app11_but_keeps_a_jpeg_xt_layer() {
    let dir = std::env::temp_dir().join(format!("st2k_c2pa_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let jpg = dir.join("c.jpg");

    let mut base = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        16,
        12,
        image::Rgb([10, 20, 30]),
    ))
    .write_to(
        &mut std::io::Cursor::new(&mut base),
        image::ImageFormat::Jpeg,
    )
    .unwrap();

    // `JP` + box instance + packet sequence + LBox + TBox + payload.
    let app11 = |tbox: &[u8; 4], tail: &[u8]| {
        let mut v = b"JP".to_vec();
        v.extend_from_slice(&[0, 1, 0, 0, 0, 1]);
        v.extend_from_slice(&64u32.to_be_bytes());
        v.extend_from_slice(tbox);
        v.extend_from_slice(tail);
        v
    };
    let mut out = base[0..2].to_vec(); // SOI
    for payload in [
        app11(b"jumb", b"c2pa-manifest-store"),
        app11(b"xtld", b"jpegxt-hdr-layer"),
    ] {
        out.extend_from_slice(&[0xFF, markers::APP11]);
        out.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(&payload);
    }
    out.extend_from_slice(&base[2..]);
    std::fs::write(&jpg, &out).unwrap();

    let path = jpg.to_str().unwrap();
    assert!(
        has_content_credentials(path),
        "setup must carry a C2PA manifest"
    );

    strip_metadata(path).unwrap();

    let after = std::fs::read(&jpg).unwrap();
    assert!(
        !after.windows(19).any(|w| w == b"c2pa-manifest-store"),
        "C2PA manifest survived the strip"
    );
    assert!(
        after.windows(16).any(|w| w == b"jpegxt-hdr-layer"),
        "the JPEG XT layer was collateral damage"
    );
    assert!(!has_content_credentials(path));
    let d = image::open(&jpg).unwrap();
    assert_eq!(
        (d.width(), d.height()),
        (16, 12),
        "pixels must be untouched"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A C2PA manifest past ~64KB spans more than one APP11 segment: only the FIRST
/// packet (sequence 1) carries the `LBox`/`TBox` header `is_jumbf_app11` matches on,
/// so a per-segment-only filter left later packets (sequence > 1, same box instance)
/// behind - the manifest fragment survived even though `has_content_credentials`
/// reported `false`.
#[test]
fn strips_every_continuation_packet_of_a_multi_segment_c2pa_manifest() {
    let dir = std::env::temp_dir().join(format!("st2k_c2pa_multi_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let jpg = dir.join("c.jpg");

    let mut base = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        16,
        12,
        image::Rgb([10, 20, 30]),
    ))
    .write_to(
        &mut std::io::Cursor::new(&mut base),
        image::ImageFormat::Jpeg,
    )
    .unwrap();

    // Packet 1 of box instance 1: carries the LBox/TBox header, TBox == "jumb".
    let mut first = b"JP".to_vec();
    first.extend_from_slice(&[0, 1]); // box instance 1
    first.extend_from_slice(&[0, 0, 0, 1]); // sequence 1
    first.extend_from_slice(&64u32.to_be_bytes()); // LBox
    first.extend_from_slice(b"jumb"); // TBox
    first.extend_from_slice(b"manifest-part-one");
    // Packet 2 of the SAME box instance: sequence 2, no LBox/TBox of its own - a real
    // continuation packet, exactly what `is_jumbf_app11` can never match directly.
    let mut second = b"JP".to_vec();
    second.extend_from_slice(&[0, 1]); // same box instance
    second.extend_from_slice(&[0, 0, 0, 2]); // sequence 2
    second.extend_from_slice(b"manifest-part-two");

    let mut out = base[0..2].to_vec(); // SOI
    for payload in [first, second] {
        out.extend_from_slice(&[0xFF, markers::APP11]);
        out.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(&payload);
    }
    out.extend_from_slice(&base[2..]);
    std::fs::write(&jpg, &out).unwrap();

    let path = jpg.to_str().unwrap();
    assert!(
        has_content_credentials(path),
        "setup must carry a C2PA manifest"
    );

    strip_metadata(path).unwrap();

    let after = std::fs::read(&jpg).unwrap();
    assert!(
        !after.windows(18).any(|w| w == b"manifest-part-one"),
        "the box-defining packet survived the strip"
    );
    assert!(
        !after.windows(18).any(|w| w == b"manifest-part-two"),
        "the continuation packet survived the strip"
    );
    assert!(!has_content_credentials(path));

    let _ = std::fs::remove_dir_all(&dir);
}

/// Before this fix, the match arm `"svg" | "svgz" if ext == "svg"` could only ever
/// be true for `ext == "svg"`, so a real `.svgz` always fell through to the
/// unsupported case and `strip_metadata` refused every compressed SVG.
#[test]
fn strips_metadata_from_a_gzip_compressed_svgz_file() {
    use std::io::Write;

    let dir = std::env::temp_dir().join(format!("st2k_svgz_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("logo.svgz");

    let svg = concat!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\">\n",
        "  <title>Company logo FINAL v3</title>\n",
        "  <path d=\"M0 0h10v10z\"/>\n</svg>\n"
    );
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(svg.as_bytes()).unwrap();
    std::fs::write(&path, gz.finish().unwrap()).unwrap();

    strip_metadata(path.to_str().unwrap()).unwrap();

    let rewritten = std::fs::read(&path).unwrap();
    let inflated = gunzip_bounded(&rewritten).expect("output must still be valid gzip");
    let text = String::from_utf8(inflated).unwrap();
    assert!(!text.contains("Company logo"), "{text}");
    assert!(
        text.contains("<path d=\"M0 0h10v10z\"/>"),
        "art damaged: {text}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn strips_jpeg_app1_exif_losslessly() {
    let dir = std::env::temp_dir().join(format!("st2k_strip_exif_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let jpg = dir.join("e.jpg");

    // A baseline JPEG, then splice a fake APP1 "Exif" segment in after SOI.
    let mut base = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        16,
        12,
        image::Rgb([40, 90, 160]),
    ))
    .write_to(
        &mut std::io::Cursor::new(&mut base),
        image::ImageFormat::Jpeg,
    )
    .unwrap();
    let payload = b"Exif\0\0sometagdata".to_vec();
    let len = (payload.len() + 2) as u16;
    let mut with_exif = Vec::new();
    with_exif.extend_from_slice(&base[0..2]); // SOI
    with_exif.extend_from_slice(&[0xFF, 0xE1]); // APP1
    with_exif.extend_from_slice(&len.to_be_bytes());
    with_exif.extend_from_slice(&payload);
    with_exif.extend_from_slice(&base[2..]);
    std::fs::write(&jpg, &with_exif).unwrap();
    assert!(
        with_exif.windows(4).any(|w| w == b"Exif"),
        "setup must contain Exif"
    );

    strip_metadata(jpg.to_str().unwrap()).unwrap();

    let after = std::fs::read(&jpg).unwrap();
    assert!(
        !after.windows(4).any(|w| w == b"Exif"),
        "Exif should be stripped"
    );
    let d = image::open(&jpg).unwrap();
    assert_eq!(
        (d.width(), d.height()),
        (16, 12),
        "pixels must be untouched"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn formats_exif_datetime_filename_safe() {
    assert_eq!(
        format_exif_datetime("2023:05:01 14:30:09"),
        Some("2023-05-01 14.30.09".to_string())
    );
    // Subsecond/odd separators tolerated; reject the never-set clock + junk.
    assert_eq!(format_exif_datetime("0000:00:00 00:00:00"), None);
    assert_eq!(format_exif_datetime("not a date"), None);
    assert_eq!(format_exif_datetime("2023:05 14:30:00"), None);
}

#[test]
fn read_info_returns_dimensions() {
    let dir = std::env::temp_dir().join(format!("st2k_info_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("i.png");
    image::DynamicImage::ImageRgb8(image::RgbImage::new(33, 22))
        .save(&png)
        .unwrap();
    let info = read_info(png.to_str().unwrap());
    assert_eq!((info.width, info.height), (33, 22));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bounded_info_reads_psd_dimensions_from_the_header() {
    let dir = std::env::temp_dir().join(format!("st2k_bounded_info_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let psd = dir.join("large-looking.psd");

    // PSD dimensions live in the fixed 26-byte header. The deliberately large tail models
    // a document that must not be read or decoded merely to fill Explorer's Details pane.
    let mut bytes = Vec::with_capacity(8 * 1024 * 1024);
    bytes.extend_from_slice(b"8BPS");
    bytes.extend_from_slice(&[0, 1]);
    bytes.extend_from_slice(&[0; 6]);
    bytes.extend_from_slice(&3u16.to_be_bytes());
    bytes.extend_from_slice(&4321u32.to_be_bytes()); // height @ 14
    bytes.extend_from_slice(&8765u32.to_be_bytes()); // width @ 18
    bytes.extend_from_slice(&[0; 8]);
    bytes.resize(8 * 1024 * 1024, 0);
    std::fs::write(&psd, bytes).unwrap();

    let info = read_info_bounded(psd.to_str().unwrap());
    assert_eq!((info.width, info.height), (8765, 4321));
    let _ = std::fs::remove_dir_all(&dir);
}
