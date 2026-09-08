//! TDD tests for `djvu encode <input>`.

use assert_cmd::Command;
use std::path::Path;

/// Write a minimal grayscale PNG with a checkerboard pattern, half black half white.
fn write_test_png(path: &Path, w: u32, h: u32) {
    let file = std::fs::File::create(path).unwrap();
    let writer = std::io::BufWriter::new(file);
    let mut encoder = png::Encoder::new(writer, w, h);
    encoder.set_color(png::ColorType::Grayscale);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().unwrap();
    let mut data = Vec::with_capacity((w * h) as usize);
    for _y in 0..h {
        for x in 0..w {
            // Half black, half white split by x
            data.push(if x < w / 2 { 0 } else { 255 });
        }
    }
    writer.write_image_data(&data).unwrap();
}

fn write_colored_ink_png(path: &Path, w: u32, h: u32) {
    let file = std::fs::File::create(path).unwrap();
    let writer = std::io::BufWriter::new(file);
    let mut encoder = png::Encoder::new(writer, w, h);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().unwrap();
    let mut data = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            if x >= w / 4 && x < w / 2 && y >= h / 4 && y < h / 2 {
                data.extend_from_slice(&[160, 20, 20]);
            } else {
                data.extend_from_slice(&[255, 255, 255]);
            }
        }
    }
    writer.write_image_data(&data).unwrap();
}

fn write_two_color_ink_png(path: &Path, w: u32, h: u32) {
    let file = std::fs::File::create(path).unwrap();
    let writer = std::io::BufWriter::new(file);
    let mut encoder = png::Encoder::new(writer, w, h);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().unwrap();
    let mut data = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            if x >= w / 4 && x < w / 2 && y >= h / 4 && y < h / 2 {
                data.extend_from_slice(&[160, 20, 20]);
            } else if x >= (w * 5) / 8 && x < (w * 7) / 8 && y >= h / 4 && y < h / 2 {
                data.extend_from_slice(&[20, 40, 180]);
            } else {
                data.extend_from_slice(&[255, 255, 255]);
            }
        }
    }
    writer.write_image_data(&data).unwrap();
}

fn write_mixed_lighting_png(path: &Path, w: u32, h: u32) {
    let file = std::fs::File::create(path).unwrap();
    let writer = std::io::BufWriter::new(file);
    let mut encoder = png::Encoder::new(writer, w, h);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().unwrap();
    let mut data = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            let on_dark_half = x < w / 2;
            let mut v = if on_dark_half { 105 } else { 235 };
            if (8..24).contains(&x) && (8..24).contains(&y) {
                v = 72;
            } else if (40..56).contains(&x) && (8..24).contains(&y) {
                v = 185;
            }
            data.extend_from_slice(&[v, v, v]);
        }
    }
    writer.write_image_data(&data).unwrap();
}

fn mask_ink_pixels(path: &Path) -> u32 {
    let bytes = std::fs::read(path).unwrap();
    let doc = djvu_rs::djvu_document::DjVuDocument::parse(&bytes).unwrap();
    let page = doc.page(0).unwrap();
    let mask = page.extract_mask().unwrap().expect("mask");
    let mut ink = 0;
    for y in 0..mask.height {
        for x in 0..mask.width {
            ink += u32::from(mask.get(x, y));
        }
    }
    ink
}

fn first_background_pixel(path: &Path) -> (u8, u8, u8) {
    let bytes = std::fs::read(path).unwrap();
    let doc = djvu_rs::djvu_document::DjVuDocument::parse(&bytes).unwrap();
    let page = doc.page(0).unwrap();
    page.extract_background()
        .unwrap()
        .expect("background")
        .get_rgb(0, 0)
}

#[test]
fn encode_creates_djvu_file() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.png");
    let output = dir.path().join("out.djvu");
    write_test_png(&input, 64, 48);

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "encode",
            input.to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
            "--dpi",
            "150",
        ])
        .assert()
        .success();

    assert!(output.exists(), "djvu output not created");
    let bytes = std::fs::read(&output).unwrap();
    assert_eq!(&bytes[..4], b"AT&T", "output is not a valid DjVu IFF file");
}

#[test]
fn encode_default_dpi_is_300() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.png");
    let output = dir.path().join("out.djvu");
    write_test_png(&input, 32, 32);

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "encode",
            input.to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();

    let bytes = std::fs::read(&output).unwrap();
    let doc = djvu_rs::Document::from_bytes(bytes).unwrap();
    let page = doc.page(0).unwrap();
    assert_eq!(page.dpi(), 300);
    assert_eq!(page.width(), 32);
    assert_eq!(page.height(), 32);
}

#[test]
fn encode_quality_profile_emits_layered_djvu() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.png");
    let output = dir.path().join("out.djvu");
    write_test_png(&input, 16, 16);

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "encode",
            input.to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
            "--quality",
            "quality",
        ])
        .assert()
        .success();

    // Layered output must contain at least one BG44 chunk in addition to Sjbz.
    let bytes = std::fs::read(&output).unwrap();
    let doc = djvu_rs::djvu_document::DjVuDocument::parse(&bytes).unwrap();
    let page = doc.page(0).unwrap();
    assert!(page.raw_chunk(b"Sjbz").is_some());
    assert!(!page.all_chunks(b"BG44").is_empty());
}

#[test]
fn encode_quality_profile_emits_fgbz_for_colored_ink() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.png");
    let output = dir.path().join("out.djvu");
    write_two_color_ink_png(&input, 32, 32);

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "encode",
            input.to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
            "--quality",
            "quality",
        ])
        .assert()
        .success();

    let bytes = std::fs::read(&output).unwrap();
    let doc = djvu_rs::djvu_document::DjVuDocument::parse(&bytes).unwrap();
    let page = doc.page(0).unwrap();
    let fgbz = page.raw_chunk(b"FGbz").expect("FGbz");
    let (palette, indices) = djvu_rs::fgbz_encode::decode_fgbz(fgbz).unwrap();
    assert!(palette.len() >= 2);
    assert!(indices.len() >= 2);
}

#[test]
fn encode_archival_profile_emits_layered_djvu() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.png");
    let output = dir.path().join("out.djvu");
    write_colored_ink_png(&input, 32, 32);

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "encode",
            input.to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
            "--quality",
            "archival",
        ])
        .assert()
        .success();

    let bytes = std::fs::read(&output).unwrap();
    let doc = djvu_rs::djvu_document::DjVuDocument::parse(&bytes).unwrap();
    let page = doc.page(0).unwrap();
    assert!(page.raw_chunk(b"Sjbz").is_some());
    assert!(!page.all_chunks(b"BG44").is_empty());
    assert!(page.raw_chunk(b"FGbz").is_some());
}

#[test]
fn encode_quality_binarization_flags_affect_layered_mask() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("mixed.png");
    let fixed_output = dir.path().join("fixed.djvu");
    let sauvola_output = dir.path().join("sauvola.djvu");
    write_mixed_lighting_png(&input, 64, 32);

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "encode",
            input.to_str().unwrap(),
            "-o",
            fixed_output.to_str().unwrap(),
            "--quality",
            "quality",
        ])
        .assert()
        .success();

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "encode",
            input.to_str().unwrap(),
            "-o",
            sauvola_output.to_str().unwrap(),
            "--quality",
            "quality",
            "--binarization",
            "sauvola",
            "--sauvola-window",
            "9",
            "--sauvola-k",
            "0.34",
        ])
        .assert()
        .success();

    assert_ne!(
        mask_ink_pixels(&fixed_output),
        mask_ink_pixels(&sauvola_output)
    );
}

#[test]
fn encode_quality_fixed_binarization_keeps_default_output() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("mixed.png");
    let default_output = dir.path().join("default.djvu");
    let fixed_output = dir.path().join("fixed.djvu");
    write_mixed_lighting_png(&input, 64, 32);

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "encode",
            input.to_str().unwrap(),
            "-o",
            default_output.to_str().unwrap(),
            "--quality",
            "quality",
        ])
        .assert()
        .success();

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "encode",
            input.to_str().unwrap(),
            "-o",
            fixed_output.to_str().unwrap(),
            "--quality",
            "quality",
            "--binarization",
            "fixed",
        ])
        .assert()
        .success();

    assert_eq!(
        std::fs::read(default_output).unwrap(),
        std::fs::read(fixed_output).unwrap()
    );
}

#[test]
fn encode_quality_inpaints_masked_background_by_default() {
    // The 24×24 fixture is black on the left half (fully masked) / white on the
    // right. Its top-left BG cell is fully covered by foreground ink. The colour
    // `Quality` profile now diffuses such invisible cells (round 17), so the
    // stored background must NOT be the ink colour (black) — it inpaints to the
    // neighbouring white background instead. This is the size+quality win, and
    // it holds without passing any segmentation flag.
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("ink.png");
    let default_output = dir.path().join("default.djvu");
    write_test_png(&input, 24, 24);

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "encode",
            input.to_str().unwrap(),
            "-o",
            default_output.to_str().unwrap(),
            "--quality",
            "quality",
        ])
        .assert()
        .success();

    let (r, g, b) = first_background_pixel(&default_output);
    assert_ne!(
        (r, g, b),
        (0, 0, 0),
        "fully-masked BG cell should be inpainted, not left as ink black"
    );
}

#[test]
fn encode_lossless_rejects_segmentation_flags() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.png");
    let output = dir.path().join("out.djvu");
    write_test_png(&input, 32, 32);

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "encode",
            input.to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
            "--binarization",
            "sauvola",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "--binarization and --bg-inpaint require --quality quality or --quality archival",
        ));
}

#[test]
fn encode_directory_produces_multipage_bundle() {
    let dir = tempfile::tempdir().unwrap();
    let input_dir = dir.path().join("scans");
    std::fs::create_dir(&input_dir).unwrap();
    write_test_png(&input_dir.join("01.png"), 32, 32);
    write_test_png(&input_dir.join("02.png"), 32, 32);
    write_test_png(&input_dir.join("03.png"), 32, 32);
    let output = dir.path().join("book.djvu");

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "encode",
            input_dir.to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();

    let bytes = std::fs::read(&output).unwrap();
    let doc = djvu_rs::Document::from_bytes(bytes.clone()).unwrap();
    assert_eq!(doc.page_count(), 3);
    let parsed = djvu_rs::djvu_document::DjVuDocument::parse(&bytes).unwrap();
    let page = parsed.page(0).unwrap();
    assert!(page.raw_chunk(b"Sjbz").is_some());
    assert!(page.all_chunks(b"BG44").is_empty());
    assert!(page.raw_chunk(b"FGbz").is_none());
}

#[test]
fn encode_quality_directory_produces_layered_multipage_bundle() {
    let dir = tempfile::tempdir().unwrap();
    let input_dir = dir.path().join("scans");
    std::fs::create_dir(&input_dir).unwrap();
    write_two_color_ink_png(&input_dir.join("01.png"), 32, 32);
    write_colored_ink_png(&input_dir.join("02.png"), 32, 32);
    let output = dir.path().join("book.djvu");

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "encode",
            input_dir.to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
            "--quality",
            "quality",
        ])
        .assert()
        .success();

    let bytes = std::fs::read(&output).unwrap();
    let doc = djvu_rs::djvu_document::DjVuDocument::parse(&bytes).unwrap();
    assert_eq!(doc.page_count(), 2);
    for idx in 0..doc.page_count() {
        let page = doc.page(idx).unwrap();
        assert!(page.raw_chunk(b"Sjbz").is_some(), "page {idx} Sjbz");
        assert!(!page.all_chunks(b"BG44").is_empty(), "page {idx} BG44");
        assert!(page.raw_chunk(b"FGbz").is_some(), "page {idx} FGbz");
        let opts = djvu_rs::djvu_render::RenderOptions {
            width: u32::from(page.width()),
            height: u32::from(page.height()),
            ..Default::default()
        };
        let rendered = djvu_rs::djvu_render::render_pixmap(page, &opts).unwrap();
        assert_eq!((rendered.width, rendered.height), (32, 32));
    }
}

#[test]
fn encode_archival_directory_produces_layered_multipage_bundle() {
    let dir = tempfile::tempdir().unwrap();
    let input_dir = dir.path().join("scans");
    std::fs::create_dir(&input_dir).unwrap();
    write_colored_ink_png(&input_dir.join("01.png"), 32, 32);
    write_two_color_ink_png(&input_dir.join("02.png"), 32, 32);
    let output = dir.path().join("book.djvu");

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "encode",
            input_dir.to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
            "--quality",
            "archival",
        ])
        .assert()
        .success();

    let bytes = std::fs::read(&output).unwrap();
    let doc = djvu_rs::djvu_document::DjVuDocument::parse(&bytes).unwrap();
    assert_eq!(doc.page_count(), 2);
    for idx in 0..doc.page_count() {
        let page = doc.page(idx).unwrap();
        assert!(page.raw_chunk(b"Sjbz").is_some(), "page {idx} Sjbz");
        assert!(!page.all_chunks(b"BG44").is_empty(), "page {idx} BG44");
        assert!(page.raw_chunk(b"FGbz").is_some(), "page {idx} FGbz");
    }
}

#[test]
fn encode_empty_directory_fails() {
    let dir = tempfile::tempdir().unwrap();
    let input_dir = dir.path().join("empty");
    std::fs::create_dir(&input_dir).unwrap();
    let output = dir.path().join("out.djvu");

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "encode",
            input_dir.to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("no image files found"));
}

#[test]
fn encode_missing_input_fails() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("nope.png");
    let output = dir.path().join("out.djvu");

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "encode",
            input.to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
        ])
        .assert()
        .failure();
}
