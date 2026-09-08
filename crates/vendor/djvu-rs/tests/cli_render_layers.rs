//! CLI coverage for the previously-untested `render` layer/rotation/CBZ paths
//! and the `text` format/output paths of the `djvu` binary.

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::PathBuf;

fn corpus(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/corpus")
        .join(name)
}

fn out(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name)
}

fn djvu() -> Command {
    Command::cargo_bin("djvu").unwrap()
}

// ── render --layer ────────────────────────────────────────────────────────────

#[test]
fn render_layer_mask_writes_png() {
    let dst = out("layer_mask.png");
    djvu()
        .args([
            "render",
            corpus("cable_1973_100133.djvu").to_str().unwrap(),
            "--layer",
            "mask",
            "--output",
            dst.to_str().unwrap(),
        ])
        .assert()
        .success();
    assert!(dst.exists() && dst.metadata().unwrap().len() > 0);
}

#[test]
fn render_all_layer_mask_writes_directory() {
    let dir = out("layer_mask_all");
    djvu()
        .args([
            "render",
            corpus("conquete_paix.djvu").to_str().unwrap(),
            "--all",
            "--layer",
            "mask",
            "--output",
            dir.to_str().unwrap(),
        ])
        .assert()
        .success();
    let pngs = std::fs::read_dir(&dir).unwrap().count();
    assert!(pngs > 1, "expected multiple page PNGs, got {pngs}");
}

#[test]
fn render_layer_background_writes_png() {
    // conquete_paix page 3 carries a BG44 background layer.
    let dst = out("layer_bg.png");
    djvu()
        .args([
            "render",
            corpus("conquete_paix.djvu").to_str().unwrap(),
            "--page",
            "3",
            "--layer",
            "background",
            "--output",
            dst.to_str().unwrap(),
        ])
        .assert()
        .success();
    assert!(dst.exists() && dst.metadata().unwrap().len() > 0);
}

#[test]
fn render_layer_foreground_writes_png() {
    // conquete_paix page 1 carries an FGbz foreground layer.
    let dst = out("layer_fg.png");
    djvu()
        .args([
            "render",
            corpus("conquete_paix.djvu").to_str().unwrap(),
            "--page",
            "1",
            "--layer",
            "foreground",
            "--output",
            dst.to_str().unwrap(),
        ])
        .assert()
        .success();
    assert!(dst.exists() && dst.metadata().unwrap().len() > 0);
}

// ── render --rotate ───────────────────────────────────────────────────────────

#[test]
fn render_rotations_write_png() {
    for rot in ["cw90", "rot180", "ccw90"] {
        let dst = out(&format!("rot_{rot}.png"));
        djvu()
            .args([
                "render",
                corpus("watchmaker.djvu").to_str().unwrap(),
                "--rotate",
                rot,
                "--dpi",
                "72",
                "--output",
                dst.to_str().unwrap(),
            ])
            .assert()
            .success();
        assert!(
            dst.exists() && dst.metadata().unwrap().len() > 0,
            "rotate {rot}"
        );
    }
}

// ── render --format cbz ───────────────────────────────────────────────────────

#[test]
fn render_all_cbz_writes_archive() {
    let dst = out("pages.cbz");
    djvu()
        .args([
            "render",
            corpus("conquete_paix.djvu").to_str().unwrap(),
            "--all",
            "--format",
            "cbz",
            "--dpi",
            "72",
            "--output",
            dst.to_str().unwrap(),
        ])
        .assert()
        .success();
    assert!(dst.exists() && dst.metadata().unwrap().len() > 0);
}

// ── text --format / --output ──────────────────────────────────────────────────

#[test]
fn text_hocr_format_emits_markup() {
    djvu()
        .args([
            "text",
            corpus("watchmaker.djvu").to_str().unwrap(),
            "--format",
            "hocr",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("<"));
}

#[test]
fn text_alto_format_emits_markup() {
    djvu()
        .args([
            "text",
            corpus("watchmaker.djvu").to_str().unwrap(),
            "--format",
            "alto",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("<"));
}

#[test]
fn text_output_to_file_writes_content() {
    let dst = out("text_out.txt");
    djvu()
        .args([
            "text",
            corpus("watchmaker.djvu").to_str().unwrap(),
            "--output",
            dst.to_str().unwrap(),
        ])
        .assert()
        .success();
    assert!(dst.exists());
    assert!(
        !std::fs::read_to_string(&dst).unwrap().trim().is_empty(),
        "text file should have content"
    );
}
