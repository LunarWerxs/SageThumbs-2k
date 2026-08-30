//! Formats that are drawn or moving rather than a grid of pixels.
//! SVG (plain and gzip-wrapped), metafiles, and animated GIF, plus the two
//! surfaces that have to decide whether to render them at all.

use super::*;

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
