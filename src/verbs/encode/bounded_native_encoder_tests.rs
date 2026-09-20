#![cfg(test)]

use super::*;
use exr::prelude::{Compression, MetaData, SampleType, Vec2};
use image::GenericImageView;
use std::io::Cursor;

fn hdr_fixture(width: u32, height: u32) -> DynamicImage {
    DynamicImage::ImageRgba32F(image::Rgba32FImage::from_fn(width, height, |x, y| {
        if x < width / 2 {
            image::Rgba([100_000.0, 2.0, 0.5, 0.25])
        } else {
            image::Rgba([
                1.0 + x as f32 / width as f32,
                0.25 + y as f32 / height as f32,
                4.0,
                0.75,
            ])
        }
    }))
}

#[test]
fn bounded_native_encoders_have_valid_headers_roundtrip_and_sizes() {
    let img = hdr_fixture(64, 32);
    let pixel_count = u64::from(img.width()) * u64::from(img.height());

    let mut exr = Cursor::new(Vec::new());
    encode_exr_bounded(&mut exr, &img).unwrap();
    let exr = exr.into_inner();
    assert!(exr.starts_with(&[0x76, 0x2f, 0x31, 0x01]));
    assert!(
        exr.len() < (pixel_count * 16) as usize,
        "constant-heavy tiled f32 EXR should compress below its raw f32 pixels"
    );
    let metadata = MetaData::read_from_buffered(Cursor::new(&exr), true).unwrap();
    let header = &metadata.headers[0];
    assert_eq!(header.compression, Compression::PIZ);
    assert!(
        header
            .channels
            .list
            .iter()
            .all(|channel| channel.sample_type == SampleType::F32),
        "all EXR output channels must be f32"
    );
    match header.blocks {
        exr::meta::BlockDescription::Tiles(description) => {
            assert_eq!(description.tile_size, Vec2(256, 256));
        }
        _ => panic!("EXR output must use bounded tiles"),
    }
    let decoded_exr = image::load_from_memory_with_format(&exr, ImageFormat::OpenExr).unwrap();
    let decoded_exr = decoded_exr.to_rgba32f();
    let exr_pixel = decoded_exr.get_pixel(0, 0).0;
    assert!(
        (exr_pixel[0] - 100_000.0).abs() < 1.0,
        "f32 EXR value above f16::MAX was clipped"
    );
    assert!((exr_pixel[3] - 0.25).abs() < 0.01, "EXR alpha changed");

    let mut hdr = Vec::new();
    encode_hdr_bounded(&mut hdr, &img).unwrap();
    let hdr_header = format!(
        "#?RADIANCE\n# Rust HDR encoder\nFORMAT=32-bit_rle_rgbe\n\n-Y {} +X {}\n",
        img.height(),
        img.width()
    );
    assert!(hdr.starts_with(hdr_header.as_bytes()));
    assert_eq!(
        &hdr[hdr_header.len()..hdr_header.len() + 4],
        &[2, 2, 0, 64],
        "new Radiance per-component RLE marker is missing"
    );
    assert!(
        hdr.len() < hdr_header.len() + (pixel_count * 4) as usize,
        "constant-heavy HDR should be smaller than raw RGBE"
    );
    let decoded_hdr = image::load_from_memory_with_format(&hdr, ImageFormat::Hdr).unwrap();
    let decoded_hdr = decoded_hdr.to_rgb32f();
    assert!(
        decoded_hdr.get_pixel(0, 0).0[0] > 99_000.0,
        "float HDR range was clipped before RGBE encoding"
    );

    let mut farbfeld = Vec::new();
    encode_farbfeld_streaming(&mut farbfeld, &img).unwrap();
    assert!(farbfeld.starts_with(b"farbfeld"));
    assert_eq!(farbfeld.len(), 16 + (pixel_count * 8) as usize);
    let decoded_farbfeld =
        image::load_from_memory_with_format(&farbfeld, ImageFormat::Farbfeld).unwrap();
    assert_eq!(decoded_farbfeld.dimensions(), img.dimensions());

    let mut pam = Vec::new();
    encode_pam_streaming(&mut pam, &img).unwrap();
    let pam_header_end = pam
        .windows(b"ENDHDR\n".len())
        .position(|window| window == b"ENDHDR\n")
        .map(|index| index + b"ENDHDR\n".len())
        .unwrap();
    assert!(pam.starts_with(b"P7\n"));
    assert!(pam[..pam_header_end]
        .windows(b"MAXVAL 65535".len())
        .any(|window| window == b"MAXVAL 65535"));
    assert_eq!(pam.len(), pam_header_end + (pixel_count * 8) as usize);
    let decoded_pam = image::load_from_memory_with_format(&pam, ImageFormat::Pnm).unwrap();
    assert_eq!(decoded_pam.dimensions(), img.dimensions());
    assert_eq!(decoded_pam.to_rgba16().get_pixel(0, 0).0[3], 16_384);

    let mut ppm = Vec::new();
    encode_ppm_streaming(&mut ppm, &img).unwrap();
    let ppm_header = format!("P6\n{} {}\n65535\n", img.width(), img.height());
    assert!(ppm.starts_with(ppm_header.as_bytes()));
    assert_eq!(ppm.len(), ppm_header.len() + (pixel_count * 6) as usize);
    let decoded_ppm = image::load_from_memory_with_format(&ppm, ImageFormat::Pnm).unwrap();
    assert_eq!(decoded_ppm.dimensions(), img.dimensions());
}

#[test]
fn hdr_short_scanlines_use_raw_compatible_fallback() {
    let img = hdr_fixture(7, 2);
    let mut hdr = Vec::new();
    encode_hdr_bounded(&mut hdr, &img).unwrap();
    let header = b"#?RADIANCE\n# Rust HDR encoder\nFORMAT=32-bit_rle_rgbe\n\n-Y 2 +X 7\n";
    assert!(hdr.starts_with(header));
    assert_eq!(hdr.len(), header.len() + 7 * 2 * 4);
    assert_ne!(&hdr[header.len()..header.len() + 4], &[2, 2, 0, 7]);
    let decoded = image::load_from_memory_with_format(&hdr, ImageFormat::Hdr).unwrap();
    assert_eq!(decoded.dimensions(), (7, 2));
}

#[test]
fn pam_and_ppm_preserve_16_bit_samples_and_pam_channel_models() {
    let fixtures = [
        (
            DynamicImage::ImageLuma16(image::ImageBuffer::from_pixel(1, 1, image::Luma([0x1234]))),
            1usize,
            "GRAYSCALE",
            vec![0x12, 0x34],
        ),
        (
            DynamicImage::ImageLumaA16(image::ImageBuffer::from_pixel(
                1,
                1,
                image::LumaA([0x1234, 0xABCD]),
            )),
            2,
            "GRAYSCALE_ALPHA",
            vec![0x12, 0x34, 0xAB, 0xCD],
        ),
        (
            DynamicImage::ImageRgb16(image::ImageBuffer::from_pixel(
                1,
                1,
                image::Rgb([0x1234, 0x5678, 0x9ABC]),
            )),
            3,
            "RGB",
            vec![0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC],
        ),
        (
            DynamicImage::ImageRgba16(image::ImageBuffer::from_pixel(
                1,
                1,
                image::Rgba([0x1234, 0x5678, 0x9ABC, 0xDEF0]),
            )),
            4,
            "RGB_ALPHA",
            vec![0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0],
        ),
    ];

    for (img, depth, tuple_type, expected_body) in fixtures {
        let mut pam = Vec::new();
        encode_pam_streaming(&mut pam, &img).unwrap();
        let header = format!(
            "P7\nWIDTH 1\nHEIGHT 1\nDEPTH {depth}\nMAXVAL 65535\nTUPLTYPE {tuple_type}\nENDHDR\n"
        );
        assert!(pam.starts_with(header.as_bytes()));
        assert_eq!(&pam[header.len()..], expected_body);
        let decoded = image::load_from_memory_with_format(&pam, ImageFormat::Pnm).unwrap();
        assert_eq!(decoded.dimensions(), (1, 1));
    }

    let rgba = DynamicImage::ImageRgba16(image::ImageBuffer::from_pixel(
        1,
        1,
        image::Rgba([0x1234, 0x5678, 0x9ABC, 0xDEF0]),
    ));
    let mut ppm = Vec::new();
    encode_ppm_streaming(&mut ppm, &rgba).unwrap();
    let header = b"P6\n1 1\n65535\n";
    assert!(ppm.starts_with(header));
    assert_eq!(&ppm[header.len()..], &[0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC]);
    let decoded = image::load_from_memory_with_format(&ppm, ImageFormat::Pnm)
        .unwrap()
        .to_rgb16();
    assert_eq!(decoded.get_pixel(0, 0).0, [0x1234, 0x5678, 0x9ABC]);
}

#[test]
fn hdr_rle_width_boundary_and_raw_marker_escape_are_valid() {
    let rle_img = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        32_767,
        1,
        image::Rgb([64, 32, 16]),
    ));
    let mut rle = Vec::new();
    encode_hdr_bounded(&mut rle, &rle_img).unwrap();
    let rle_header = b"#?RADIANCE\n# Rust HDR encoder\nFORMAT=32-bit_rle_rgbe\n\n-Y 1 +X 32767\n";
    assert_eq!(
        &rle[rle_header.len()..rle_header.len() + 4],
        &[2, 2, 127, 255]
    );
    assert_eq!(
        image::load_from_memory_with_format(&rle, ImageFormat::Hdr)
            .unwrap()
            .dimensions(),
        (32_767, 1)
    );

    let raw_img = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        32_768,
        1,
        image::Rgb([64, 32, 16]),
    ));
    let mut raw = Vec::new();
    encode_hdr_bounded(&mut raw, &raw_img).unwrap();
    let raw_header = b"#?RADIANCE\n# Rust HDR encoder\nFORMAT=32-bit_rle_rgbe\n\n-Y 1 +X 32768\n";
    assert_eq!(raw.len(), raw_header.len() + 32_768 * 4);
    assert_ne!(
        &raw[raw_header.len()..raw_header.len() + 4],
        &[2, 2, 128, 0]
    );
    assert_eq!(
        image::load_from_memory_with_format(&raw, ImageFormat::Hdr)
            .unwrap()
            .dimensions(),
        (32_768, 1)
    );

    assert_eq!(escape_raw_rgbe_marker([1, 1, 1, 42], true), [1, 1, 2, 42]);
    assert_eq!(escape_raw_rgbe_marker([2, 2, 7, 99], true), [2, 3, 7, 99]);
    assert_eq!(escape_raw_rgbe_marker([2, 2, 7, 99], false), [2, 2, 7, 99]);
}

#[test]
fn float_to_integer_samples_preserve_saturation_semantics() {
    assert_eq!(f32_to_u8(f32::NAN), u8::MAX);
    assert_eq!(f32_to_u8(f32::INFINITY), u8::MAX);
    assert_eq!(f32_to_u8(f32::NEG_INFINITY), 0);
    assert_eq!(f32_to_u8(-0.25), 0);
    assert_eq!(f32_to_u8(0.5), 128);
    assert_eq!(f32_to_u8(1.25), u8::MAX);

    assert_eq!(f32_to_u16(f32::NAN), u16::MAX);
    assert_eq!(f32_to_u16(f32::INFINITY), u16::MAX);
    assert_eq!(f32_to_u16(f32::NEG_INFINITY), 0);
    assert_eq!(f32_to_u16(-0.25), 0);
    assert_eq!(f32_to_u16(0.5), 32_768);
    assert_eq!(f32_to_u16(1.25), u16::MAX);
}

#[test]
fn hdr_non_finite_and_out_of_range_samples_saturate_safely() {
    assert_eq!(
        float_rgb_to_rgbe([f32::NAN, f32::NEG_INFINITY, -1.0]),
        [0, 0, 0, 0]
    );
    assert_eq!(
        float_rgb_to_rgbe([f32::INFINITY, 0.0, 0.0]),
        [255, 0, 0, 255]
    );
    assert_eq!(
        float_rgb_to_rgbe([f32::MAX, f32::MAX, f32::MAX]),
        [255, 255, 255, 255]
    );
    assert_eq!(
        float_rgb_to_rgbe([f32::from_bits(1), 0.0, 0.0]),
        [0, 0, 0, 0],
        "unrepresentable subnormal radiance should underflow to black"
    );

    let img = DynamicImage::ImageRgb32F(
        image::Rgb32FImage::from_raw(
            5,
            1,
            vec![
                f32::NAN,
                f32::NEG_INFINITY,
                -1.0,
                f32::INFINITY,
                0.0,
                0.0,
                f32::MAX,
                f32::MAX,
                f32::MAX,
                1.0,
                0.5,
                0.25,
                f32::from_bits(1),
                0.0,
                0.0,
            ],
        )
        .unwrap(),
    );
    let mut hdr = Vec::new();
    encode_hdr_bounded(&mut hdr, &img).unwrap();
    let decoded = image::load_from_memory_with_format(&hdr, ImageFormat::Hdr)
        .unwrap()
        .to_rgb32f();
    assert_eq!(decoded.dimensions(), (5, 1));
    assert_eq!(decoded.get_pixel(0, 0).0, [0.0, 0.0, 0.0]);
    assert_eq!(decoded.get_pixel(4, 0).0, [0.0, 0.0, 0.0]);
    assert!(decoded
        .pixels()
        .flat_map(|pixel| pixel.0)
        .all(|component| component.is_finite() && component >= 0.0));
    assert!(decoded.get_pixel(1, 0).0[0] > 1.0e38);
    assert!(decoded.get_pixel(2, 0).0[0] > 1.0e38);
}

#[test]
fn output_extension_routing_is_explicit_and_honest() {
    for ext in [
        "avif", "jxl", "psd", "dds", "jp2", "pcx", "sgi", "pfm", "dpx", "fits", "xpm", "pict",
        "ras", "palm",
    ] {
        assert!(ext_needs_magick(ext), "{ext} must route through Magick");
        assert_eq!(edit_output_ext(ext), ext);
    }
    for (ext, format) in [
        ("png", ImageFormat::Png),
        ("jpg", ImageFormat::Jpeg),
        ("jpeg", ImageFormat::Jpeg),
        ("jpe", ImageFormat::Jpeg),
        ("jfif", ImageFormat::Jpeg),
        ("gif", ImageFormat::Gif),
        ("webp", ImageFormat::WebP),
        ("pam", ImageFormat::Pnm),
        ("ppm", ImageFormat::Pnm),
        ("pnm", ImageFormat::Pnm),
        ("tiff", ImageFormat::Tiff),
        ("tif", ImageFormat::Tiff),
        ("tga", ImageFormat::Tga),
        ("bmp", ImageFormat::Bmp),
        ("ico", ImageFormat::Ico),
        ("hdr", ImageFormat::Hdr),
        ("exr", ImageFormat::OpenExr),
        ("ff", ImageFormat::Farbfeld),
        ("qoi", ImageFormat::Qoi),
    ] {
        assert_eq!(native_output_format(ext), Some(format), "{ext}");
        assert_eq!(edit_output_ext(ext), ext);
    }
    for ext in ["", "heic", "svg", "pbm", "pgm", "unknown"] {
        assert_eq!(native_output_format(ext), None, "{ext}");
        assert!(!ext_needs_magick(ext), "{ext}");
        assert_eq!(edit_output_ext(ext), "png", "{ext}");
    }
}

/// A solid-colour RGB PNG input fixture written into `dir` — the "source.png" /
/// "source.heic" every convert/transform/resize test below starts from.
fn solid_png(dir: &Path, name: &str, w: u32, h: u32, rgb: [u8; 3]) -> PathBuf {
    let path = dir.join(name);
    DynamicImage::ImageRgb8(image::RgbImage::from_pixel(w, h, image::Rgb(rgb)))
        .save_with_format(&path, ImageFormat::Png)
        .unwrap();
    path
}

/// The output contract shared by the edit/resize tests below: the path carries
/// the extension the verb promised, and the file really holds that format's
/// magic bytes (an unknown source must not become PNG bytes under a `.heic`
/// name, and a `.psd` output must really be a PSD).
fn assert_ext_and_magic(path: &Path, ext: &str, magic: &[u8]) {
    assert_eq!(path.extension().and_then(|e| e.to_str()), Some(ext));
    assert!(std::fs::read(path).unwrap().starts_with(magic));
}

#[test]
fn exact_unknown_conversion_rejects_without_replacing_destination() {
    let dir = scratch_dir("exact-unknown");
    let input = solid_png(&dir, "source.png", 3, 2, [20, 80, 160]);
    let output = dir.join("existing.unknown");
    std::fs::write(&output, b"original destination").unwrap();

    assert!(convert_to(input.to_str().unwrap(), &output, 90, None, Resize::None).is_err());
    assert_eq!(std::fs::read(&output).unwrap(), b"original destination");
    assert!(staging_leftovers(&output).is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn unknown_source_edits_use_png_name_and_signature() {
    let dir = scratch_dir("edit-fallback");
    let input = solid_png(&dir, "source.heic", 12, 8, [20, 80, 160]);

    let edited = transform_file(input.to_str().unwrap(), Transform::Right90).unwrap();
    assert_ext_and_magic(&edited, "png", b"\x89PNG\r\n\x1a\n");
    assert_eq!(image::open(&edited).unwrap().dimensions(), (8, 12));

    let resized = resize_file(input.to_str().unwrap(), Resize::Fit(6, 4)).unwrap();
    assert_ext_and_magic(&resized, "png", b"\x89PNG\r\n\x1a\n");
    assert_eq!(image::open(&resized).unwrap().dimensions(), (6, 4));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
#[ignore = "needs ImageMagick (bundled on a full install, or on PATH); run with --ignored"]
fn exact_psd_and_magick_backed_edits_have_psd_signatures() {
    if !decode::magick_available() {
        return;
    }
    let dir = scratch_dir("magick-routing");
    let input = solid_png(&dir, "source.png", 40, 30, [30, 160, 90]);

    let psd = dir.join("existing.psd");
    std::fs::write(&psd, b"old destination").unwrap();
    convert_to(input.to_str().unwrap(), &psd, 90, None, Resize::None).unwrap();
    assert!(std::fs::read(&psd).unwrap().starts_with(b"8BPS"));
    assert!(staging_leftovers(&psd).is_empty());

    let edited = transform_file(psd.to_str().unwrap(), Transform::Right90).unwrap();
    assert_ext_and_magic(&edited, "psd", b"8BPS");

    let resized = resize_file(psd.to_str().unwrap(), Resize::Fit(20, 15)).unwrap();
    assert_eq!(
        resized.extension().and_then(|ext| ext.to_str()),
        Some("psd")
    );
    assert!(std::fs::read(&resized).unwrap().starts_with(b"8BPS"));
    let _ = std::fs::remove_dir_all(dir);
}

/// A `SystemTime`+pid-suffixed scratch dir, matching the pattern the other `transform_file`/
/// `resize_file` tests in this module already use.
fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "st2k-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Little-endian TIFF IFD0 with one entry, Orientation (tag 0x0112) as SHORT — same
/// shape `carry`'s own (private, cross-file-inaccessible) test fixture uses, rebuilt here
/// because a private helper in a sibling module cannot be imported across the file
/// boundary.
fn tiff_with_orientation(o: u16) -> Vec<u8> {
    let mut v = b"II*\0".to_vec();
    v.extend_from_slice(&8u32.to_le_bytes()); // IFD0 at offset 8
    v.extend_from_slice(&1u16.to_le_bytes()); // one entry
    v.extend_from_slice(&0x0112u16.to_le_bytes()); // Orientation
    v.extend_from_slice(&3u16.to_le_bytes()); // type SHORT
    v.extend_from_slice(&1u32.to_le_bytes()); // count
    v.extend_from_slice(&(o as u32).to_le_bytes()); // value, left-packed inline
    v.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
    v
}

fn orientation_of(tiff: &[u8]) -> u16 {
    let e = 8 + 2; // IFD0 offset(8) + entry count(2) -> first (only) entry
    u16::from_le_bytes([tiff[e + 8], tiff[e + 9]])
}

/// Little-endian TIFF IFD0 with one ASCII entry, Make (tag 0x010F) = "SageT\0" (6 bytes,
/// out-of-line since ASCII > 4 bytes doesn't fit the inline value field).
fn tiff_with_make() -> Vec<u8> {
    // header(8) + count(2) + one 12-byte entry + next-IFD(4) = where the out-of-line
    // value lands.
    let value_offset: u32 = 8 + 2 + 12 + 4;
    let mut v = b"II*\0".to_vec();
    v.extend_from_slice(&8u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&0x010Fu16.to_le_bytes()); // Make
    v.extend_from_slice(&2u16.to_le_bytes()); // type ASCII
    v.extend_from_slice(&6u32.to_le_bytes()); // count, incl. the NUL
    v.extend_from_slice(&value_offset.to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
    v.extend_from_slice(b"SageT\0");
    assert_eq!(
        v.len() as u32,
        value_offset + 6,
        "offset math must match the actual layout"
    );
    v
}

fn jpeg_with_exif(base: &[u8], tiff: &[u8]) -> Vec<u8> {
    let mut payload = b"Exif\0\0".to_vec();
    payload.extend_from_slice(tiff);
    let mut out = base[0..2].to_vec(); // SOI
    out.extend_from_slice(&[0xFF, img_parts::jpeg::markers::APP1]);
    out.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(&payload);
    out.extend_from_slice(&base[2..]);
    out
}

fn png_with_exif(w: u32, h: u32, tiff: &[u8]) -> Vec<u8> {
    let mut base = Vec::new();
    image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
        w,
        h,
        image::Rgba([20, 80, 160, 255]),
    ))
    .write_to(&mut std::io::Cursor::new(&mut base), ImageFormat::Png)
    .unwrap();
    let mut png = img_parts::png::Png::from_bytes(img_parts::Bytes::from(base)).unwrap();
    png.chunks_mut().insert(
        1, // straight after IHDR, matching carry::apply_png's own placement
        img_parts::png::PngChunk::new(*b"eXIf", img_parts::Bytes::from(tiff.to_vec())),
    );
    png.encoder().bytes().to_vec()
}

/// Pixel-level apply of a `Dihedral`, independent of the jpegtran code path.
fn dihedral_apply(img: &DynamicImage, d: Dihedral) -> DynamicImage {
    let mut out = if d.transpose {
        img.rotate90().fliph() // rotate90 = transpose then flip-H
    } else {
        img.clone()
    };
    if d.flip_h {
        out = out.fliph();
    }
    if d.flip_v {
        out = out.flipv();
    }
    out
}

/// What a viewer does with EXIF Orientation `o`, spelled out per the EXIF spec.
fn viewer_upright(img: &DynamicImage, o: u32) -> DynamicImage {
    match o {
        2 => img.fliph(),
        3 => img.rotate180(),
        4 => img.flipv(),
        5 => img.rotate90().fliph(),
        6 => img.rotate90(),
        7 => img.rotate270().fliph(),
        8 => img.rotate270(),
        _ => img.clone(),
    }
}

/// `Transform` derives no `Debug`; a name for the assertion messages.
fn transform_name(t: Transform) -> &'static str {
    match t {
        Transform::Right90 => "Right90",
        Transform::Left90 => "Left90",
        Transform::Rotate180 => "Rotate180",
        Transform::FlipH => "FlipH",
        Transform::FlipV => "FlipV",
    }
}

/// A small non-square image with no symmetry at all, so every one of the eight
/// dihedral results is distinguishable from every other.
fn asymmetric(w: u32, h: u32) -> DynamicImage {
    DynamicImage::ImageRgb8(image::RgbImage::from_fn(w, h, |x, y| {
        image::Rgb([
            (x * 37 % 256) as u8,
            (y * 91 % 256) as u8,
            ((x * x + 3 * y) % 256) as u8,
        ])
    }))
}

const TRANSFORMS: [Transform; 5] = [
    Transform::Right90,
    Transform::Left90,
    Transform::Rotate180,
    Transform::FlipH,
    Transform::FlipV,
];

/// The group arithmetic behind the lossless path: for every EXIF orientation and every
/// menu request, the composed operation applied to the STORED pixels must equal the
/// request applied to what the viewer shows.
#[test]
fn dihedral_composition_matches_viewer_then_request() {
    let stored = asymmetric(6, 4);
    for o in 1..=8u32 {
        for t in TRANSFORMS {
            let name = transform_name(t);
            let want = apply_transform(&viewer_upright(&stored, o), t).to_rgb8();
            let composed = Dihedral::from_exif_orientation(o).then(Dihedral::from_transform(t));
            let got = dihedral_apply(&stored, composed).to_rgb8();
            assert_eq!(
                got.dimensions(),
                want.dimensions(),
                "orientation {o} then {name}: {composed:?}"
            );
            assert_eq!(
                got.into_raw(),
                want.into_raw(),
                "orientation {o} then {name}: {composed:?}"
            );
        }
    }
    // `to_op` is a bijection onto the seven non-identity operations.
    let identity = Dihedral::from_exif_orientation(1);
    assert_eq!(identity.to_op(), None);
    assert_eq!(
        Dihedral::from_exif_orientation(8)
            .then(Dihedral::from_transform(Transform::Right90))
            .to_op(),
        None,
        "270 then 90 is the identity"
    );
}

/// A273 / P8: the lossless jpegtran path keeps the source EXIF segment verbatim while it
/// rotates the DCT grid. The tag must come back reset to 1 AND the pixels must be what
/// "rotate what I see" means: for a stored-sideways `Orientation=6` photo, "rotate
/// right" is a 180° turn of the stored grid, not another 90°. Checked against the pixel
/// path's own definition (decode with the orientation applied, then transform).
#[test]
fn transform_file_lossless_jpeg_path_composes_orientation_and_resets_the_tag() {
    let dir = scratch_dir("lossless-orient");

    // 32x16 is MCU-aligned for both 4:2:0 and 4:4:4 chroma subsampling, so the lossless
    // jpegtran path takes every case here rather than falling through to the pixel path.
    let mut base = Vec::new();
    asymmetric(32, 16)
        .write_to(&mut std::io::Cursor::new(&mut base), ImageFormat::Jpeg)
        .unwrap();
    let stored = image::load_from_memory(&base).unwrap();

    // (orientation, request): a rotate that composes to 180°, one that composes to the
    // identity (bytes kept, tag reset), a flip that composes to a transpose, and a plain
    // orientation-1 request that must keep behaving exactly as before.
    for (i, (o, t)) in [
        (6, Transform::Right90),
        (8, Transform::Right90),
        (6, Transform::FlipH),
        (1, Transform::Left90),
    ]
    .into_iter()
    .enumerate()
    {
        let name = transform_name(t);
        let input = dir.join(format!("source{i}.jpg"));
        std::fs::write(&input, jpeg_with_exif(&base, &tiff_with_orientation(o))).unwrap();

        let edited = transform_file(input.to_str().unwrap(), t).unwrap();
        assert_eq!(
            edited.extension().and_then(|e| e.to_str()),
            Some("jpg"),
            "the lossless path keeps the source extension"
        );

        let out_bytes = std::fs::read(&edited).unwrap();
        let jpeg =
            img_parts::jpeg::Jpeg::from_bytes(img_parts::Bytes::from(out_bytes.clone())).unwrap();
        let exif_seg = jpeg
            .segments()
            .iter()
            .find(|s| {
                s.marker() == img_parts::jpeg::markers::APP1
                    && s.contents().starts_with(b"Exif\0\0")
            })
            .expect("lossless transform must not drop the EXIF segment entirely");
        assert_eq!(
            orientation_of(&exif_seg.contents()[6..]),
            1,
            "orientation {o} then {name}: the tag must be reset, or viewers double-rotate"
        );

        let want = apply_transform(&viewer_upright(&stored, u32::from(o)), t);
        let got = image::load_from_memory(&out_bytes).unwrap();
        assert_eq!(
            got.dimensions(),
            want.dimensions(),
            "orientation {o} then {name}: wrong shape"
        );
        let (g, w) = (got.to_luma8().into_raw(), want.to_luma8().into_raw());
        let maxd = g
            .iter()
            .zip(&w)
            .map(|(a, b)| (*a as i32 - *b as i32).abs())
            .max()
            .unwrap();
        // Transposing ops may differ from a pixel rotate by 1 (integer IDCT); a wrong
        // rotation differs by whole pixel values everywhere.
        assert!(
            maxd <= 1,
            "orientation {o} then {name}: not the composed rotation (max diff {maxd})"
        );
    }

    let _ = std::fs::remove_dir_all(dir);
}

/// The IFD1 thumbnail shows the pre-rotation framing; after the lossless rotate it
/// must be gone, while IFD0's own values survive.
#[test]
fn transform_file_lossless_jpeg_path_drops_the_stale_ifd1_thumbnail() {
    let dir = scratch_dir("lossless-ifd1");
    let input = dir.join("source.jpg");

    let mut base = Vec::new();
    asymmetric(32, 16)
        .write_to(&mut std::io::Cursor::new(&mut base), ImageFormat::Jpeg)
        .unwrap();
    // IFD0 (Orientation) -> IFD1 (JPEGInterchangeFormat) -> thumbnail bytes.
    let mut tiff = tiff_with_orientation(6);
    let ifd1 = tiff.len() as u32;
    let next_ptr_at = 8 + 2 + 12;
    tiff[next_ptr_at..next_ptr_at + 4].copy_from_slice(&ifd1.to_le_bytes());
    tiff.extend_from_slice(&1u16.to_le_bytes());
    tiff.extend_from_slice(&0x0201u16.to_le_bytes());
    tiff.extend_from_slice(&4u16.to_le_bytes());
    tiff.extend_from_slice(&1u32.to_le_bytes());
    tiff.extend_from_slice(&(ifd1 + 2 + 12 + 4).to_le_bytes());
    tiff.extend_from_slice(&0u32.to_le_bytes());
    tiff.extend_from_slice(b"\xFF\xD8stale-thumbnail\xFF\xD9");
    std::fs::write(&input, jpeg_with_exif(&base, &tiff)).unwrap();

    let edited = transform_file(input.to_str().unwrap(), Transform::Right90).unwrap();
    let out = std::fs::read(&edited).unwrap();
    assert!(
        !out.windows(15).any(|w| w == b"stale-thumbnail"),
        "the un-rotated IFD1 thumbnail survived the lossless rotate"
    );
    assert!(
        out.windows(6).any(|w| w == b"Exif\0\0"),
        "IFD0 itself must survive"
    );

    let _ = std::fs::remove_dir_all(dir);
}

/// A104: `transform_file`'s PIXEL fallback (progressive JPEG / PNG / TIFF / …) decodes
/// and re-encodes, which drops every metadata block on its own unless carried through —
/// exactly what `resize_file` already does and this branch didn't. A plain `.png` source
/// never takes the lossless jpegtran path at all, so this exercises the pixel fallback
/// directly.
#[test]
fn transform_file_pixel_fallback_carries_exif_through_rotation() {
    let dir = scratch_dir("pixel-carry");
    let input = dir.join("source.png");
    std::fs::write(&input, png_with_exif(12, 8, &tiff_with_make())).unwrap();

    let edited = transform_file(input.to_str().unwrap(), Transform::Right90).unwrap();
    let info = crate::strip::read_info(edited.to_str().unwrap());
    assert_eq!(
        info.make.as_deref(),
        Some("SageT"),
        "EXIF Make must survive the pixel-fallback rotate, matching resize_file"
    );

    let _ = std::fs::remove_dir_all(dir);
}
