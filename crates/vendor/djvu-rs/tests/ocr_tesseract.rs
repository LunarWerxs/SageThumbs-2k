//! Integration tests for the Tesseract OCR backend.
//!
//! These tests require the `ocr-tesseract` feature and a working Tesseract
//! installation with English language data (`tesseract-ocr-eng`).

#[cfg(feature = "ocr-tesseract")]
mod tesseract_tests {
    use std::path::PathBuf;

    #[cfg(feature = "cli")]
    use assert_cmd::Command;
    #[cfg(feature = "cli")]
    use predicates::prelude::*;

    use djvu_rs::{
        DjVuDocument,
        djvu_render::{RenderOptions, render_pixmap},
        ocr::{OcrBackend, OcrOptions},
        ocr_tesseract::TesseractBackend,
    };

    fn assets_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("references/djvujs/library/assets")
    }

    #[cfg(feature = "cli")]
    fn corpus_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus")
    }

    #[test]
    fn tesseract_recognize_returns_ok() {
        // Render the first page of a small test file and run OCR on it.
        let data = std::fs::read(assets_path().join("boy.djvu")).expect("boy.djvu must exist");
        let doc = DjVuDocument::parse(&data).expect("parse must succeed");
        let page = doc.page(0).expect("page 0 must exist");
        let opts = RenderOptions {
            width: page.width() as u32,
            height: page.height() as u32,
            ..RenderOptions::default()
        };
        let pixmap = render_pixmap(page, &opts).expect("render must succeed");

        let backend = TesseractBackend::new();
        let ocr_opts = OcrOptions::default();
        let result = backend.recognize(&pixmap, &ocr_opts);
        assert!(
            result.is_ok(),
            "TesseractBackend::recognize must not return an error: {:?}",
            result.err()
        );
    }

    #[test]
    fn tesseract_result_has_text_layer() {
        let data = std::fs::read(assets_path().join("boy.djvu")).expect("boy.djvu must exist");
        let doc = DjVuDocument::parse(&data).expect("parse must succeed");
        let page = doc.page(0).expect("page 0 must exist");
        let opts = RenderOptions {
            width: page.width() as u32,
            height: page.height() as u32,
            ..RenderOptions::default()
        };
        let pixmap = render_pixmap(page, &opts).expect("render must succeed");

        let backend = TesseractBackend::new();
        let layer = backend
            .recognize(&pixmap, &OcrOptions::default())
            .expect("recognize must succeed");

        // text field is present (may be empty for a photographic image)
        let _ = layer.text;
        // zones field is present and does not panic
        let _ = layer.zones;
    }

    #[test]
    #[cfg(feature = "cli")]
    fn cli_ocr_embeds_text_layer_into_single_page_output() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("boy_ocr.djvu");

        Command::cargo_bin("djvu")
            .unwrap()
            .args([
                "ocr",
                assets_path().join("boy.djvu").to_str().unwrap(),
                "--output",
                out.to_str().unwrap(),
            ])
            .assert()
            .success()
            .stderr(predicate::str::contains("Embedded text layers for 1 page"));

        let data = std::fs::read(&out).expect("OCR output must exist");
        let doc = DjVuDocument::parse(&data).expect("OCR output must parse");
        assert!(
            doc.page(0)
                .expect("page 0")
                .text_layer()
                .expect("text layer parse")
                .is_some(),
            "OCR output should contain a TXTz/TXTa text layer"
        );
    }

    #[test]
    #[cfg(feature = "cli")]
    fn cli_ocr_embeds_text_layer_into_bundled_output_and_text_cli_reads_it() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("cable_ocr.djvu");

        Command::cargo_bin("djvu")
            .unwrap()
            .args([
                "ocr",
                corpus_path()
                    .join("cable_1973_100133.djvu")
                    .to_str()
                    .unwrap(),
                "--output",
                out.to_str().unwrap(),
            ])
            .assert()
            .success()
            .stderr(predicate::str::contains("Embedded text layers for 2 page"));

        let data = std::fs::read(&out).expect("OCR output must exist");
        let doc = DjVuDocument::parse(&data).expect("OCR output must parse");
        assert_eq!(doc.page_count(), 2);
        for i in 0..2 {
            assert!(
                doc.page(i)
                    .expect("page")
                    .text_layer()
                    .expect("text layer parse")
                    .is_some(),
                "page {i} should contain a TXTz/TXTa text layer"
            );
        }

        Command::cargo_bin("djvu")
            .unwrap()
            .args(["text", out.to_str().unwrap(), "--all"])
            .assert()
            .success()
            .stdout(
                predicate::str::contains("--- Page 1 ---")
                    .and(predicate::str::contains("--- Page 2 ---"))
                    .and(predicate::str::contains("No text layer").not()),
            );
    }
}
