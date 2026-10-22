//! CLI OCR backend-selection behavior.
//!
//! These tests intentionally use lightweight experimental OCR features rather
//! than requiring a system Tesseract installation. They verify that unsupported
//! backend choices fail explicitly before users get a late recognition error.

#![cfg(all(
    feature = "cli",
    any(
        feature = "ocr-neural",
        feature = "ocr-onnx",
        feature = "ocr-tesseract"
    )
))]

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[test]
#[cfg(not(feature = "ocr-tesseract"))]
fn ocr_default_backend_errors_when_tesseract_feature_missing() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.djvu");

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "ocr",
            fixture("boy.djvu").to_str().unwrap(),
            "--output",
            out.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "Tesseract OCR backend is not enabled",
        ));
}

#[test]
fn ocr_candle_backend_reports_experimental_status() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.djvu");

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "ocr",
            fixture("boy.djvu").to_str().unwrap(),
            "--backend",
            "candle",
            "--output",
            out.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("Candle OCR backend is experimental").and(
                predicate::str::contains("no supported model-specific implementation"),
            ),
        );
}

/// The ONNX backend loads only manifest-pinned (SHA-256 verified) models, so
/// an ad-hoc `--model` path is rejected; without the feature it says so.
#[test]
fn ocr_onnx_backend_rejects_ad_hoc_model() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.djvu");

    let expected = if cfg!(feature = "ocr-onnx") {
        predicate::str::contains("--backend onnx does not take --model")
            .and(predicate::str::contains("ocr-model-manifest.toml"))
            .boxed()
    } else {
        predicate::str::contains("rebuild with --features ocr-onnx").boxed()
    };

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "ocr",
            fixture("boy.djvu").to_str().unwrap(),
            "--backend",
            "onnx",
            "--model",
            "dummy.onnx",
            "--output",
            out.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(expected);
}
