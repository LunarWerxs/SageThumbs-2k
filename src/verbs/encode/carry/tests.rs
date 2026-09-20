#![cfg(test)]

use super::*;

/// Little-endian TIFF with Make and Orientation, so the rewrite has to find
/// the right entry rather than the first one.
fn tiff_with_orientation(o: u16) -> Vec<u8> {
    let mut v = b"II*\0".to_vec();
    v.extend_from_slice(&8u32.to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    // Make, ASCII, count 6, value at 38
    v.extend_from_slice(&0x010Fu16.to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&6u32.to_le_bytes());
    v.extend_from_slice(&38u32.to_le_bytes());
    // Orientation, SHORT, count 1, inline value
    v.extend_from_slice(&TAG_ORIENTATION.to_le_bytes());
    v.extend_from_slice(&3u16.to_le_bytes());
    v.extend_from_slice(&1u32.to_le_bytes());
    v.extend_from_slice(&(o as u32).to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
    v.extend_from_slice(b"SageT\0");
    v
}

fn orientation_of(tiff: &[u8]) -> u16 {
    let e = 8 + 2 + 12; // IFD0 + count + first entry
    u16::from_le_bytes([tiff[e + 8], tiff[e + 9]])
}

#[test]
fn orientation_is_reset_because_the_pixels_are_already_upright() {
    let mut t = tiff_with_orientation(6);
    assert_eq!(orientation_of(&t), 6);
    reset_orientation_to_1(&mut t);
    assert_eq!(orientation_of(&t), 1, "carrying 6 forward double-rotates");
    // The rest of the block is untouched - same length, Make still readable.
    assert_eq!(t.len(), tiff_with_orientation(6).len());
    assert!(t.windows(5).any(|w| w == b"SageT"));
}

#[test]
fn a_block_with_no_orientation_survives_unchanged() {
    let mut v = b"II*\0".to_vec();
    v.extend_from_slice(&8u32.to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes()); // zero entries
    v.extend_from_slice(&0u32.to_le_bytes());
    let before = v.clone();
    reset_orientation_to_1(&mut v);
    drop_ifd1_thumbnail(&mut v);
    assert_eq!(v, before);
}

#[test]
fn garbage_is_not_mangled() {
    for mut junk in [b"not a tiff".to_vec(), b"II".to_vec(), Vec::new()] {
        let before = junk.clone();
        reset_orientation_to_1(&mut junk);
        drop_ifd1_thumbnail(&mut junk);
        assert_eq!(junk, before);
    }
}

/// `tiff_with_orientation` plus an IFD1 (one JPEGInterchangeFormat entry) and a
/// thumbnail blob after it, the layout every camera writes. Returns the block and
/// the offset IFD1 starts at.
fn tiff_with_ifd1_thumbnail() -> (Vec<u8>, usize) {
    let mut v = tiff_with_orientation(6);
    // tiff_with_orientation: header(8) + count(2) + 2 entries(24) + next(4) = 38, then
    // the 6-byte Make value at 38 -> 44. IFD1 goes at 44.
    let ifd1 = v.len();
    let next_ptr_at = 8 + 2 + 2 * 12;
    v[next_ptr_at..next_ptr_at + 4].copy_from_slice(&(ifd1 as u32).to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&0x0201u16.to_le_bytes()); // JPEGInterchangeFormat
    v.extend_from_slice(&4u16.to_le_bytes()); // LONG
    v.extend_from_slice(&1u32.to_le_bytes());
    let thumb_at = (ifd1 + 2 + 12 + 4) as u32;
    v.extend_from_slice(&thumb_at.to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes()); // no IFD2
    v.extend_from_slice(b"\xFF\xD8stale-thumbnail\xFF\xD9");
    (v, ifd1)
}

/// The embedded preview shows the ORIGINAL framing; after a rotate/resize it must go,
/// and IFD0's own values (the out-of-line Make) must survive the cut.
#[test]
fn ifd1_thumbnail_is_dropped_and_ifd0_values_survive() {
    let (mut t, ifd1) = tiff_with_ifd1_thumbnail();
    assert!(t.windows(15).any(|w| w == b"stale-thumbnail"));
    drop_ifd1_thumbnail(&mut t);
    assert_eq!(t.len(), ifd1, "block must end where IFD1 began");
    assert!(
        !t.windows(15).any(|w| w == b"stale-thumbnail"),
        "thumbnail bytes survived"
    );
    assert!(t.windows(5).any(|w| w == b"SageT"), "IFD0's Make was cut");
    let next_ptr_at = 8 + 2 + 2 * 12;
    assert_eq!(&t[next_ptr_at..next_ptr_at + 4], &[0, 0, 0, 0]);
    // A second pass is a no-op.
    let before = t.clone();
    drop_ifd1_thumbnail(&mut t);
    assert_eq!(t, before);
}

/// When an IFD0 value sits PAST IFD1's offset, the block is not truncated (that would
/// cut the value) - only the pointer is cleared.
#[test]
fn ifd1_is_unlinked_but_not_truncated_when_ifd0_data_follows_it() {
    let (mut t, ifd1) = tiff_with_ifd1_thumbnail();
    // Point Make's out-of-line value past IFD1 (into the thumbnail bytes).
    let make_entry = 8 + 2;
    let far = (t.len() - 6) as u32;
    t[make_entry + 8..make_entry + 12].copy_from_slice(&far.to_le_bytes());
    let len_before = t.len();
    drop_ifd1_thumbnail(&mut t);
    assert_eq!(
        t.len(),
        len_before,
        "must not cut through a referenced value"
    );
    let next_ptr_at = 8 + 2 + 2 * 12;
    assert_eq!(&t[next_ptr_at..next_ptr_at + 4], &[0, 0, 0, 0]);
    let _ = ifd1;
}

/// HEIC/AVIF: the `Exif` item is a 4-byte header offset then the TIFF block, and the
/// XMP `mime` item is the packet itself. Both must come out, orientation reset.
#[test]
fn reads_exif_and_xmp_items_from_a_heic() {
    use crate::strip::isobmff::testutil::synth;
    let mut exif_item = 0u32.to_be_bytes().to_vec();
    exif_item.extend_from_slice(&tiff_with_orientation(6));
    let xmp_item = b"<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF/></x:xmpmeta>";
    let (file, _) = synth(&[(1, &exif_item), (2, xmp_item)], &[]);
    let (exif, xmp) = read_isobmff(&file);
    let exif = exif.expect("no Exif item read");
    assert_eq!(
        exif,
        tiff_with_orientation(6),
        "TIFF block must be the item minus its header offset"
    );
    assert_eq!(xmp.as_deref(), Some(&xmp_item[..]));

    let carried = read(&file, "heic").expect("keep-metadata default is on");
    let t = carried.exif.expect("no exif carried");
    assert_eq!(
        orientation_of(&t),
        1,
        "orientation must be reset for the upright pixels"
    );
    assert!(t.windows(5).any(|w| w == b"SageT"));
}

/// End-to-end through the real verb: a JPEG whose EXIF says "rotate 90" is
/// converted to PNG, and the PNG must come out with the camera intact and the
/// orientation neutralised. Get the second half wrong and every phone photo
/// converts sideways.
#[test]
fn convert_carries_exif_and_neutralises_orientation() {
    let dir = std::env::temp_dir().join(format!("st2k_carry_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let jpg = dir.join("shot.jpg");

    let mut base = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(16, 8, image::Rgb([70, 80, 90])))
        .write_to(
            &mut std::io::Cursor::new(&mut base),
            image::ImageFormat::Jpeg,
        )
        .unwrap();

    let mut payload = EXIF_PREFIX.to_vec();
    payload.extend_from_slice(&tiff_with_orientation(6));
    let mut with_exif = base[0..2].to_vec(); // SOI
    with_exif.extend_from_slice(&[0xFF, markers::APP1]);
    with_exif.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
    with_exif.extend_from_slice(&payload);
    with_exif.extend_from_slice(&base[2..]);
    std::fs::write(&jpg, &with_exif).unwrap();

    let out = super::convert_file(
        jpg.to_str().unwrap(),
        Target {
            format: ImageFormat::Png,
            ext: "png",
            webp_quality: None,
        },
    )
    .unwrap();

    let info = crate::strip::read_info(out.to_str().unwrap());
    assert_eq!(
        info.make.as_deref(),
        Some("SageT"),
        "the camera did not survive the conversion"
    );

    let png = Png::from_bytes(Bytes::from(std::fs::read(&out).unwrap())).unwrap();
    let exif = png
        .chunk_by_type(*b"eXIf")
        .expect("no eXIf chunk")
        .contents();
    assert_eq!(
        orientation_of(exif),
        1,
        "orientation was carried through verbatim - the image will double-rotate"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A PNG or WebP metadata chunk has no size limit; a JPEG APP segment does,
/// and `img-parts` enforces it with an `.unwrap()`. With `panic = "abort"` in
/// the shell DLL that is an explorer.exe crash, so an oversized block must be
/// DROPPED rather than handed to the encoder.
#[test]
fn an_oversized_metadata_block_is_dropped_instead_of_panicking() {
    let mut base = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(8, 8, image::Rgb([1, 2, 3])))
        .write_to(
            &mut std::io::Cursor::new(&mut base),
            image::ImageFormat::Jpeg,
        )
        .unwrap();

    let huge = Carried {
        exif: Some(vec![0x41; 90_000]),
        xmp: Some(vec![0x42; 90_000]),
        iptc: None,
        icc: None,
    };
    let out = apply_jpeg(&huge, Bytes::from(base.clone())).expect("must not panic");
    // Re-parseable, and the oversized blocks simply are not in it.
    Jpeg::from_bytes(Bytes::from(out.clone())).expect("output must still be a JPEG");
    assert!(
        out.len() < base.len() + 1000,
        "an oversized block was embedded"
    );
}

/// A JPEG whose profile spans two APP2 chunks is converted to PNG; the PNG must carry
/// the joined profile in an `iCCP` chunk, or a wide-gamut photo converts to sRGB
/// colours.
#[test]
fn convert_carries_the_icc_profile_from_jpeg_to_png() {
    let dir = std::env::temp_dir().join(format!("st2k_carry_icc_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let jpg = dir.join("wide.jpg");

    let mut base = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(16, 8, image::Rgb([70, 80, 90])))
        .write_to(
            &mut std::io::Cursor::new(&mut base),
            image::ImageFormat::Jpeg,
        )
        .unwrap();
    let icc = vec![0x5A; 70_000];
    let mut with_icc = base[0..2].to_vec(); // SOI
    for (i, part) in icc.chunks(ICC_CHUNK_MAX).enumerate() {
        let mut payload = ICC_PREFIX.to_vec();
        payload.push(i as u8 + 1);
        payload.push(2);
        payload.extend_from_slice(part);
        with_icc.extend_from_slice(&[0xFF, markers::APP2]);
        with_icc.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        with_icc.extend_from_slice(&payload);
    }
    with_icc.extend_from_slice(&base[2..]);
    std::fs::write(&jpg, &with_icc).unwrap();

    let out = super::convert_file(
        jpg.to_str().unwrap(),
        Target {
            format: ImageFormat::Png,
            ext: "png",
            webp_quality: None,
        },
    )
    .unwrap();
    let png = Png::from_bytes(Bytes::from(std::fs::read(&out).unwrap())).unwrap();
    assert!(
        png.chunk_by_type(*b"iCCP").is_some(),
        "no iCCP chunk written"
    );
    assert_eq!(
        png.icc_profile().map(|b| b.to_vec()),
        Some(icc),
        "the profile did not survive the conversion intact"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The pure-Rust encoder writes a simple `VP8L` file; grafting anything onto it needs
/// a `VP8X` header first, with the feature bits set, and the chunks in the order the
/// container spec fixes. The result must still decode.
#[test]
fn webp_output_gets_a_vp8x_header_with_the_carried_blocks() {
    let mut base = Vec::new();
    image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
        9,
        5,
        image::Rgba([1, 2, 3, 200]),
    ))
    .write_to(
        &mut std::io::Cursor::new(&mut base),
        image::ImageFormat::WebP,
    )
    .unwrap();
    let plain = WebP::from_bytes(Bytes::from(base.clone())).unwrap();
    assert!(!plain.has_chunk(*b"VP8X"), "expected a simple VP8L file");

    let meta = Carried {
        exif: Some(tiff_with_orientation(1)),
        xmp: Some(b"<x:xmpmeta/>".to_vec()),
        iptc: None,
        icc: Some(b"fake-icc".to_vec()),
    };
    let out = apply_webp(&meta, Bytes::from(base)).expect("graft refused");
    let webp = WebP::from_bytes(Bytes::from(out.clone())).unwrap();
    let ids: Vec<[u8; 4]> = webp.chunks().iter().map(|c| c.id()).collect();
    assert_eq!(&ids[..2], &[*b"VP8X", *b"ICCP"], "header and profile lead");
    assert_eq!(
        &ids[ids.len() - 2..],
        &[*b"EXIF", *b"XMP "],
        "metadata follows the image data"
    );
    let vp8x = webp
        .chunk_by_id(*b"VP8X")
        .unwrap()
        .content()
        .data()
        .unwrap();
    assert_eq!(
        vp8x[0] & (VP8X_ICC | VP8X_EXIF | VP8X_XMP),
        VP8X_ICC | VP8X_EXIF | VP8X_XMP,
        "feature bits"
    );
    // The canvas fields sit at bytes 4..10 of the header (flags, three reserved bytes,
    // then width-1 and height-1 as 24-bit little-endian); the real decoder below is the
    // proof they are read as 9x5.
    assert_eq!(&vp8x[4..7], &[8, 0, 0], "canvas width - 1");
    assert_eq!(&vp8x[7..10], &[4, 0, 0], "canvas height - 1");
    assert_eq!(webp.icc_profile().as_deref(), Some(&b"fake-icc"[..]));
    let decoded = image::load_from_memory(&out).expect("must still decode");
    assert_eq!((decoded.width(), decoded.height()), (9, 5));
}

/// A little-endian TIFF directory at absolute offset `at`: count, the entries (sorted
/// by the caller), no next IFD, then the out-of-line values.
fn le_ifd(at: usize, entries: &[(u16, u16, u32, &[u8])]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    let mut tail: Vec<u8> = Vec::new();
    let tail_base = at + 2 + entries.len() * 12 + 4;
    for (tag, typ, count, data) in entries {
        v.extend_from_slice(&tag.to_le_bytes());
        v.extend_from_slice(&typ.to_le_bytes());
        v.extend_from_slice(&count.to_le_bytes());
        if data.len() <= 4 {
            let mut inline = [0u8; 4];
            inline[..data.len()].copy_from_slice(data);
            v.extend_from_slice(&inline);
        } else {
            if tail.len() % 2 == 1 {
                tail.push(0);
            }
            v.extend_from_slice(&((tail_base + tail.len()) as u32).to_le_bytes());
            tail.extend_from_slice(data);
        }
    }
    v.extend_from_slice(&0u32.to_le_bytes());
    v.extend_from_slice(&tail);
    v
}

/// A TIFF file whose IFD0 mixes pixel-structure entries (width, strip offsets) with
/// the attributes, XMP and ICC tags and an Exif sub-IFD (with a MakerNote to drop).
fn tiff_file() -> Vec<u8> {
    let iso = 400u16.to_le_bytes();
    let exif_entries: [(u16, u16, u32, &[u8]); 3] = [
        (0x8827, 3, 1, &iso),                      // PhotographicSensitivity
        (0x9003, 2, 20, b"2024:05:06 07:08:09\0"), // DateTimeOriginal
        (0x927C, 7, 5, b"maker"),                  // MakerNote
    ];
    let width = 4u16.to_le_bytes();
    let strips = 200u32.to_le_bytes();
    let orientation = 6u16.to_le_bytes();
    let ifd0 = |exif_at: u32| -> Vec<u8> {
        let exif_ptr = exif_at.to_le_bytes();
        let entries: [(u16, u16, u32, &[u8]); 7] = [
            (0x0100, 3, 1, &width),           // ImageWidth
            (0x010F, 2, 6, b"SageT\0"),       // Make
            (0x0111, 4, 1, &strips),          // StripOffsets
            (0x0112, 3, 1, &orientation),     // Orientation
            (0x02BC, 1, 12, b"<x:xmpmeta/>"), // XMP
            (0x8769, 4, 1, &exif_ptr),        // Exif IFD
            (0x8773, 7, 8, b"fake-icc"),      // ICC
        ];
        le_ifd(8, &entries)
    };
    let mut exif_at = 8 + ifd0(0).len();
    if exif_at % 2 == 1 {
        exif_at += 1;
    }
    let mut file = b"II*\0".to_vec();
    file.extend_from_slice(&8u32.to_le_bytes());
    file.extend_from_slice(&ifd0(exif_at as u32));
    file.resize(exif_at, 0);
    file.extend_from_slice(&le_ifd(exif_at, &exif_entries));
    file
}

/// The rebuilt block must parse as EXIF with the attributes, the Exif sub-IFD and a
/// reset orientation, and without a single pixel-structure entry or MakerNote; the
/// XMP and ICC tags come out as their own packets.
#[test]
fn tiff_ifd0_walk_carries_attributes_but_no_pixel_pointers() {
    use exif::{In, Tag, Value};
    let file = tiff_file();
    let carried = read(&file, "tif").expect("keep-metadata default is on");
    assert_eq!(carried.xmp.as_deref(), Some(&b"<x:xmpmeta/>"[..]));
    assert_eq!(carried.icc.as_deref(), Some(&b"fake-icc"[..]));
    let block = carried.exif.expect("no exif block");
    let exif = exif::Reader::new()
        .read_raw(block)
        .expect("the rebuilt block must parse");
    let ascii = |t: Tag| -> String {
        match &exif.get_field(t, In::PRIMARY).expect("field missing").value {
            Value::Ascii(v) => String::from_utf8_lossy(v.first().unwrap()).into_owned(),
            other => panic!("{t}: not ASCII: {other:?}"),
        }
    };
    assert_eq!(ascii(Tag::Make), "SageT");
    assert_eq!(ascii(Tag::DateTimeOriginal), "2024:05:06 07:08:09");
    let uint = |t: Tag| {
        exif.get_field(t, In::PRIMARY)
            .and_then(|f| f.value.get_uint(0))
    };
    assert_eq!(uint(Tag::Orientation), Some(1), "orientation must be reset");
    assert_eq!(uint(Tag::PhotographicSensitivity), Some(400));
    for t in [Tag::ImageWidth, Tag::StripOffsets, Tag::MakerNote] {
        assert!(
            exif.get_field(t, In::PRIMARY).is_none(),
            "{t} must not be carried"
        );
    }
}

#[test]
fn itxt_only_matches_the_xmp_keyword() {
    let mut c = PNG_XMP_KEYWORD.to_vec();
    c.extend_from_slice(&[0, 0, 0, 0, 0]);
    c.extend_from_slice(b"<x:xmpmeta/>");
    assert_eq!(itxt_xmp(&c).as_deref(), Some(&b"<x:xmpmeta/>"[..]));

    let mut other = b"Comment".to_vec();
    other.extend_from_slice(&[0, 0, 0, 0, 0]);
    other.extend_from_slice(b"hello");
    assert!(itxt_xmp(&other).is_none());
}

/// A JPEG with EXIF (orientation 6, Make) and XMP, built the same way
/// `convert_carries_exif_and_neutralises_orientation` does.
fn jpeg_with_exif_and_xmp() -> Vec<u8> {
    let mut base = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(16, 8, image::Rgb([70, 80, 90])))
        .write_to(
            &mut std::io::Cursor::new(&mut base),
            image::ImageFormat::Jpeg,
        )
        .unwrap();

    let mut exif_payload = EXIF_PREFIX.to_vec();
    exif_payload.extend_from_slice(&tiff_with_orientation(6));
    let mut xmp_payload = XMP_PREFIX.to_vec();
    xmp_payload.extend_from_slice(b"<x:xmpmeta/>");

    let mut out = base[0..2].to_vec(); // SOI
    for payload in [&exif_payload, &xmp_payload] {
        out.extend_from_slice(&[0xFF, markers::APP1]);
        out.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(payload);
    }
    out.extend_from_slice(&base[2..]);
    out
}

/// Item 2: `convert_file`'s magick-only targets (AVIF/JXL) used to call
/// `encode_via_magick` with no carried metadata at all, dropping EXIF/XMP that
/// the native-format branch already keeps. This checks the mechanism
/// `verbs::encode::encode_via_magick_carrying` now uses: it PNG-encodes the
/// decoded image, then grafts the carried chunks onto that PNG (via
/// `apply_to_png_bytes` below) before handing it to magick.
///
/// Asserted directly on the intermediate PNG rather than on the AVIF/JXL
/// magick writes: whether ImageMagick's own AVIF/JXL coder re-embeds a PNG's
/// `eXIf`/XMP `iTXt` into its output is a property of the bundled magick
/// binary, not of this code, so it is not a fact this crate can assert.
/// The end-to-end run below (gated on ImageMagick being present) proves the
/// carry step does not break the real convert - the file still comes out a
/// valid AVIF - which is what this crate DOES control.
#[test]
fn magick_branch_grafts_carried_metadata_onto_the_intermediate_png() {
    use exif::{In, Tag, Value};

    let jpg = jpeg_with_exif_and_xmp();
    let carried = read(&jpg, "jpg").expect("keep-metadata default is on");

    let mut png = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(16, 8, image::Rgb([70, 80, 90])))
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();

    let grafted = apply_to_png_bytes(&carried, png);
    let decoded = Png::from_bytes(Bytes::from(grafted)).expect("must still be a valid PNG");

    let exif_chunk = decoded
        .chunk_by_type(*b"eXIf")
        .expect("no eXIf chunk on the intermediate PNG")
        .contents();
    let exif = exif::Reader::new()
        .read_raw(exif_chunk.to_vec())
        .expect("the eXIf chunk must parse");
    let make = match &exif.get_field(Tag::Make, In::PRIMARY).unwrap().value {
        Value::Ascii(v) => String::from_utf8_lossy(v.first().unwrap()).into_owned(),
        other => panic!("Make: not ASCII: {other:?}"),
    };
    assert_eq!(make, "SageT");
    assert_eq!(
        exif.get_field(Tag::Orientation, In::PRIMARY)
            .and_then(|f| f.value.get_uint(0)),
        Some(1),
        "orientation must be reset for the already-upright pixels"
    );

    let xmp_chunk = decoded
        .chunks_by_type(*b"iTXt")
        .find_map(|c| itxt_xmp(c.contents()))
        .expect("no XMP iTXt chunk on the intermediate PNG");
    assert_eq!(xmp_chunk.as_slice(), &b"<x:xmpmeta/>"[..]);

    // End-to-end: the real convert_file magick branch must still produce a
    // valid AVIF once the carry step is wired in (skipped, with a printed
    // reason, when ImageMagick is not installed).
    if !crate::decode::magick_available() {
        eprintln!(
            "SKIPPED magick_branch_grafts_carried_metadata_onto_the_intermediate_png \
             (end-to-end half): no ImageMagick"
        );
        return;
    }
    let dir = std::env::temp_dir().join(format!("st2k_carry_magick_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let jpg_path = dir.join("shot.jpg");
    std::fs::write(&jpg_path, jpeg_with_exif_and_xmp()).unwrap();

    let out = super::convert_file(
        jpg_path.to_str().unwrap(),
        Target {
            format: ImageFormat::Avif,
            ext: "avif",
            webp_quality: None,
        },
    )
    .expect("convert to AVIF via the magick branch must still succeed");
    let bytes = std::fs::read(&out).unwrap();
    assert!(
        bytes.len() > 12 && &bytes[4..8] == b"ftyp",
        "output is not a valid ISOBMFF/AVIF file"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
