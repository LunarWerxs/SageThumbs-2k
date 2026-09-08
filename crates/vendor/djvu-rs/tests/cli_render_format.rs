//! TDD tests for `djvu render --format pdf|cbz`.

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::PathBuf;

fn corpus(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/corpus")
        .join(name)
}

// ── PDF ───────────────────────────────────────────────────────────────────────

#[test]
fn render_pdf_single_page_has_pdf_magic() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("page.pdf");

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "render",
            corpus("watchmaker.djvu").to_str().unwrap(),
            "--format",
            "pdf",
            "-o",
            out.to_str().unwrap(),
        ])
        .assert()
        .success();

    assert!(out.exists(), "PDF not created");
    let bytes = std::fs::read(&out).unwrap();
    assert!(bytes.starts_with(b"%PDF-"), "not a valid PDF (bad magic)");
}

#[test]
#[ignore = "renders all pages of conquete_paix.djvu — slow, run with --ignored"]
fn render_pdf_all_pages_has_page_count() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("all.pdf");

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "render",
            corpus("conquete_paix.djvu").to_str().unwrap(),
            "--format",
            "pdf",
            "--all",
            "-o",
            out.to_str().unwrap(),
        ])
        .assert()
        .success();

    assert!(out.exists());
    let bytes = std::fs::read(&out).unwrap();
    // PDF page count is stored as /Count N in the Pages dict (ASCII in the header portion)
    assert!(
        bytes.windows(6).any(|w| w == b"/Count"),
        "PDF missing /Count entry"
    );
}

#[test]
fn render_pdf_output_is_not_empty() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("page.pdf");

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "render",
            corpus("watchmaker.djvu").to_str().unwrap(),
            "--format",
            "pdf",
            "-o",
            out.to_str().unwrap(),
        ])
        .assert()
        .success();

    let len = std::fs::metadata(&out).unwrap().len();
    assert!(len > 1024, "PDF is suspiciously small ({len} bytes)");
}

// ── CBZ ───────────────────────────────────────────────────────────────────────

#[test]
fn render_cbz_single_page_has_zip_magic() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("page.cbz");

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "render",
            corpus("watchmaker.djvu").to_str().unwrap(),
            "--format",
            "cbz",
            "-o",
            out.to_str().unwrap(),
        ])
        .assert()
        .success();

    assert!(out.exists(), "CBZ not created");
    let bytes = std::fs::read(&out).unwrap();
    // ZIP local file header magic: PK\x03\x04
    assert_eq!(&bytes[..4], b"PK\x03\x04", "not a valid ZIP/CBZ");
}

#[test]
#[ignore = "renders all pages of conquete_paix.djvu — slow, run with --ignored"]
fn render_cbz_all_pages_contains_multiple_entries() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("all.cbz");

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "render",
            corpus("conquete_paix.djvu").to_str().unwrap(),
            "--format",
            "cbz",
            "--all",
            "-o",
            out.to_str().unwrap(),
        ])
        .assert()
        .success();

    assert!(out.exists());
    // Count PNG entries in the ZIP using the central directory signature PK\x01\x02
    let bytes = std::fs::read(&out).unwrap();
    let count = bytes.windows(4).filter(|w| *w == b"PK\x01\x02").count();
    assert!(
        count > 1,
        "CBZ should contain multiple entries, got {count}"
    );
}

#[test]
fn render_cbz_entry_is_valid_png() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("page.cbz");

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "render",
            corpus("watchmaker.djvu").to_str().unwrap(),
            "--format",
            "cbz",
            "-o",
            out.to_str().unwrap(),
        ])
        .assert()
        .success();

    // Extract and verify the first PNG inside the ZIP
    let bytes = std::fs::read(&out).unwrap();
    // Find PNG magic \x89PNG after local file header (at offset ~30 + name len)
    let png_magic = b"\x89PNG";
    assert!(
        bytes.windows(4).any(|w| w == png_magic),
        "CBZ entry does not contain a valid PNG"
    );
}

// ── error cases ───────────────────────────────────────────────────────────────

#[test]
fn render_unknown_format_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("page.xyz");

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "render",
            corpus("watchmaker.djvu").to_str().unwrap(),
            "--format",
            "xyz",
            "-o",
            out.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::is_empty().not());
}
