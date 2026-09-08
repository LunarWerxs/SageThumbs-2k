//! TDD tests for `djvu render <file>`.

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::PathBuf;

fn corpus(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/corpus")
        .join(name)
}

// --- happy path ---

#[test]
fn render_default_creates_png() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("page.png");

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "render",
            corpus("watchmaker.djvu").to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
        ])
        .assert()
        .success();

    assert!(out.exists(), "output PNG not created");
    // PNG magic bytes: 0x89 0x50 0x4E 0x47
    let bytes = std::fs::read(&out).unwrap();
    assert_eq!(&bytes[..4], b"\x89PNG", "output is not a valid PNG");
}

#[test]
fn render_specific_page() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("page5.png");

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "render",
            corpus("pathogenic_bacteria_1896.djvu").to_str().unwrap(),
            "-p",
            "5",
            "-o",
            out.to_str().unwrap(),
        ])
        .assert()
        .success();

    assert!(out.exists());
}

#[test]
fn render_higher_dpi_produces_larger_image() {
    let dir = tempfile::tempdir().unwrap();
    let low = dir.path().join("low.png");
    let high = dir.path().join("high.png");
    let file = corpus("watchmaker.djvu");

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "render",
            file.to_str().unwrap(),
            "-d",
            "72",
            "-o",
            low.to_str().unwrap(),
        ])
        .assert()
        .success();

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "render",
            file.to_str().unwrap(),
            "-d",
            "300",
            "-o",
            high.to_str().unwrap(),
        ])
        .assert()
        .success();

    let size_low = std::fs::metadata(&low).unwrap().len();
    let size_high = std::fs::metadata(&high).unwrap().len();
    assert!(
        size_high > size_low,
        "300dpi ({size_high}B) should be larger than 72dpi ({size_low}B)"
    );
}

#[test]
#[ignore = "renders all pages of conquete_paix.djvu — slow (~3 min), run with --ignored"]
fn render_all_pages_creates_multiple_files() {
    let dir = tempfile::tempdir().unwrap();

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "render",
            corpus("conquete_paix.djvu").to_str().unwrap(),
            "--all",
            "-o",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let pngs: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "png"))
        .collect();

    assert!(pngs.len() > 1, "expected multiple PNGs, got {}", pngs.len());
}

#[test]
fn render_output_dir_created_if_missing() {
    let dir = tempfile::tempdir().unwrap();
    let subdir = dir.path().join("new_subdir");
    let out = subdir.join("page.png");

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "render",
            corpus("watchmaker.djvu").to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
        ])
        .assert()
        .success();

    assert!(out.exists());
}

// --- error cases ---

#[test]
fn render_page_out_of_range_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("page.png");

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "render",
            corpus("watchmaker.djvu").to_str().unwrap(),
            "-p",
            "999",
            "-o",
            out.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::is_empty().not());
}

#[test]
fn render_missing_file_exits_nonzero() {
    Command::cargo_bin("djvu")
        .unwrap()
        .args(["render", "/tmp/no_such_file.djvu", "-o", "/tmp/out.png"])
        .assert()
        .failure();
}

#[test]
fn render_no_args_exits_nonzero() {
    Command::cargo_bin("djvu")
        .unwrap()
        .arg("render")
        .assert()
        .failure()
        .stderr(predicate::str::is_empty().not());
}
