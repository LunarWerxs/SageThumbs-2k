//! TDD tests for `djvu text <file>`.

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
fn text_outputs_content_when_layer_present() {
    // watchmaker.djvu has a TXTz layer
    Command::cargo_bin("djvu")
        .unwrap()
        .args(["text", corpus("watchmaker.djvu").to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::is_empty().not());
}

#[test]
fn text_cable_has_readable_content() {
    // US State Dept cable — scanned with OCR text layer
    Command::cargo_bin("djvu")
        .unwrap()
        .args(["text", corpus("cable_1973_100133.djvu").to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::is_empty().not());
}

#[test]
fn text_no_layer_exits_success_with_message() {
    // pathogenic_bacteria has no TXTz — should exit 0, not crash
    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "text",
            corpus("pathogenic_bacteria_1896.djvu").to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("No text layer"));
}

#[test]
fn text_specific_page() {
    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "text",
            corpus("conquete_paix.djvu").to_str().unwrap(),
            "-p",
            "1",
        ])
        .assert()
        .success();
}

#[test]
fn text_all_pages_outputs_multiple_sections() {
    let output = Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "text",
            corpus("conquete_paix.djvu").to_str().unwrap(),
            "--all",
        ])
        .assert()
        .success()
        .get_output()
        .clone();

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Expect page markers like "--- Page N ---"
    let page_markers = stdout.lines().filter(|l| l.starts_with("--- Page")).count();
    assert!(
        page_markers > 1,
        "expected multiple page sections, got {page_markers}"
    );
}

// --- error cases ---

#[test]
fn text_missing_file_exits_nonzero() {
    Command::cargo_bin("djvu")
        .unwrap()
        .args(["text", "/tmp/no_such.djvu"])
        .assert()
        .failure()
        .stderr(predicate::str::is_empty().not());
}

#[test]
fn text_page_out_of_range_exits_nonzero() {
    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "text",
            corpus("watchmaker.djvu").to_str().unwrap(),
            "-p",
            "999",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::is_empty().not());
}

#[test]
fn text_no_args_exits_nonzero() {
    Command::cargo_bin("djvu")
        .unwrap()
        .arg("text")
        .assert()
        .failure()
        .stderr(predicate::str::is_empty().not());
}
