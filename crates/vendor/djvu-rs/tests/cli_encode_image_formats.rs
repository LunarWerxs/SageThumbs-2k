//! Integration tests for `djvu encode` with JPEG and TIFF inputs.
//!
//! JPEG fixtures are generated in-process using `jpeg-encoder` (a
//! dev-dependency). TIFF fixtures are generated using the `tiff` crate
//! (feature-gated). No binary fixtures are committed.

use assert_cmd::Command;
use djvu_rs::djvu_document::DjVuDocument;
use std::path::Path;

// ── JPEG fixture helpers ──────────────────────────────────────────────────────

/// Write a small RGB JPEG file and return the bytes-on-disk size as a sanity
/// check. Colour: alternating red and white rows to give the codec something
/// to work with.
fn write_test_jpeg(path: &Path, width: u32, height: u32) {
    let w = width as usize;
    let h = height as usize;
    let mut rgb = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        for x in 0..w {
            if (x + y) % 4 < 2 {
                rgb.extend_from_slice(&[200u8, 30, 30]); // reddish
            } else {
                rgb.extend_from_slice(&[240u8, 240, 240]); // near-white
            }
        }
    }
    let encoder = jpeg_encoder::Encoder::new_file(path, 85).unwrap();
    encoder
        .encode(
            &rgb,
            width as u16,
            height as u16,
            jpeg_encoder::ColorType::Rgb,
        )
        .unwrap();
}

// ── Helpers shared with cli_encode.rs ────────────────────────────────────────

fn page_info(djvu_path: &Path) -> (u16, u16, u16) {
    let bytes = std::fs::read(djvu_path).unwrap();
    let doc = DjVuDocument::parse(&bytes).unwrap();
    let page = doc.page(0).unwrap();
    (page.width(), page.height(), page.dpi())
}

fn page_count(djvu_path: &Path) -> usize {
    let bytes = std::fs::read(djvu_path).unwrap();
    let doc = djvu_rs::Document::from_bytes(bytes).unwrap();
    doc.page_count()
}

// ── JPEG tests ────────────────────────────────────────────────────────────────

/// Single JPEG → DjVu: file is created, parses, dimensions and DPI are correct.
#[test]
fn encode_jpeg_single_file_creates_valid_djvu() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("photo.jpg");
    let output = dir.path().join("out.djvu");
    write_test_jpeg(&input, 48, 36);

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

    let (w, h, dpi) = page_info(&output);
    assert_eq!(w, 48, "width mismatch");
    assert_eq!(h, 36, "height mismatch");
    assert_eq!(dpi, 150, "DPI mismatch");
}

/// JPEG input with `.jpeg` extension also works.
#[test]
fn encode_jpeg_extension_variant_works() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("photo.jpeg");
    let output = dir.path().join("out.djvu");
    write_test_jpeg(&input, 32, 32);

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
    assert_eq!(&bytes[..4], b"AT&T");
}

/// Directory of JPEGs produces a valid multi-page DjVu bundle.
#[test]
fn encode_jpeg_directory_produces_multipage_bundle() {
    let dir = tempfile::tempdir().unwrap();
    let input_dir = dir.path().join("scans");
    std::fs::create_dir(&input_dir).unwrap();
    write_test_jpeg(&input_dir.join("01.jpg"), 32, 32);
    write_test_jpeg(&input_dir.join("02.jpg"), 32, 32);
    write_test_jpeg(&input_dir.join("03.jpg"), 32, 32);
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

    assert_eq!(page_count(&output), 3, "expected 3 pages");
}

/// Mixed PNG + JPEG directory: all recognised images become pages in sorted order.
#[test]
fn encode_mixed_png_jpeg_directory() {
    let dir = tempfile::tempdir().unwrap();
    let input_dir = dir.path().join("mixed");
    std::fs::create_dir(&input_dir).unwrap();
    // PNG helper (from cli_encode.rs pattern)
    let png_path = input_dir.join("01.png");
    {
        let file = std::fs::File::create(&png_path).unwrap();
        let writer = std::io::BufWriter::new(file);
        let mut enc = png::Encoder::new(writer, 32, 32);
        enc.set_color(png::ColorType::Grayscale);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().unwrap();
        w.write_image_data(&vec![200u8; 32 * 32]).unwrap();
    }
    write_test_jpeg(&input_dir.join("02.jpg"), 32, 32);
    let output = dir.path().join("mixed.djvu");

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

    assert_eq!(page_count(&output), 2, "expected 2 pages (1 PNG + 1 JPEG)");
}

/// Quality encode from JPEG also produces layered output.
#[test]
fn encode_jpeg_quality_produces_layered_djvu() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("photo.jpg");
    let output = dir.path().join("out.djvu");
    write_test_jpeg(&input, 32, 32);

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
    let doc = DjVuDocument::parse(&bytes).unwrap();
    let page = doc.page(0).unwrap();
    assert!(
        page.raw_chunk(b"Sjbz").is_some(),
        "expected Sjbz mask chunk"
    );
    assert!(
        !page.all_chunks(b"BG44").is_empty(),
        "expected BG44 background"
    );
}

// ── TIFF tests ────────────────────────────────────────────────────────────────

#[cfg(feature = "tiff")]
mod tiff_tests {
    use super::*;

    /// Write a tiny RGB TIFF file.
    fn write_test_tiff(path: &Path, width: u32, height: u32) {
        use tiff::encoder::{TiffEncoder, colortype};
        let file = std::fs::File::create(path).unwrap();
        let mut tiff = TiffEncoder::new(file).unwrap();
        let w = width as usize;
        let h = height as usize;
        let mut rgb = Vec::with_capacity(w * h * 3);
        for y in 0..h {
            for x in 0..w {
                if (x + y) % 3 == 0 {
                    rgb.extend_from_slice(&[180u8, 20, 20]);
                } else {
                    rgb.extend_from_slice(&[240u8, 240, 240]);
                }
            }
        }
        tiff.write_image::<colortype::RGB8>(width, height, &rgb)
            .unwrap();
    }

    /// Single .tif file encodes to a valid DjVu with correct dimensions.
    #[test]
    fn encode_tiff_single_file_creates_valid_djvu() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("scan.tif");
        let output = dir.path().join("out.djvu");
        write_test_tiff(&input, 40, 30);

        Command::cargo_bin("djvu")
            .unwrap()
            .args([
                "encode",
                input.to_str().unwrap(),
                "-o",
                output.to_str().unwrap(),
                "--dpi",
                "200",
            ])
            .assert()
            .success();

        assert!(output.exists(), "djvu output not created");
        let bytes = std::fs::read(&output).unwrap();
        assert_eq!(&bytes[..4], b"AT&T", "not a valid DjVu file");

        let (w, h, dpi) = page_info(&output);
        assert_eq!(w, 40);
        assert_eq!(h, 30);
        assert_eq!(dpi, 200);
    }

    /// .tiff extension also works.
    #[test]
    fn encode_tiff_extension_variant_works() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("scan.tiff");
        let output = dir.path().join("out.djvu");
        write_test_tiff(&input, 32, 32);

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
        assert_eq!(&bytes[..4], b"AT&T");
    }

    /// Directory of TIFF files produces a multi-page bundle.
    #[test]
    fn encode_tiff_directory_produces_multipage_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let input_dir = dir.path().join("scans");
        std::fs::create_dir(&input_dir).unwrap();
        write_test_tiff(&input_dir.join("01.tif"), 32, 32);
        write_test_tiff(&input_dir.join("02.tif"), 32, 32);
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

        assert_eq!(page_count(&output), 2);
    }
}

// ── Feature-off guard ─────────────────────────────────────────────────────────

/// When the `tiff` feature is absent, a .tiff input must fail with a clear
/// message rather than panic.
#[cfg(not(feature = "tiff"))]
#[test]
fn encode_tiff_without_feature_reports_clear_error() {
    let dir = tempfile::tempdir().unwrap();
    // Create a fake (empty) file with a .tiff extension — the CLI must reject
    // it before ever calling the TIFF decoder.
    let input = dir.path().join("fake.tiff");
    std::fs::write(&input, b"").unwrap();
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
        .failure()
        .stderr(predicates::str::contains("tiff"));
}
