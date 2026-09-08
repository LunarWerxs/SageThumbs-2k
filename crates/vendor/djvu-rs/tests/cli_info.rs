//! TDD tests for `djvu info <file>`.
//!
//! All tests are written before the implementation (Red phase).

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
fn info_shows_page_count() {
    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "info",
            corpus("pathogenic_bacteria_1896.djvu").to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("520"));
}

#[test]
fn info_shows_page_dimensions() {
    // watchmaker.djvu is a single-page color file — dimensions must be non-zero numbers
    Command::cargo_bin("djvu")
        .unwrap()
        .args(["info", corpus("watchmaker.djvu").to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::is_match(r"\d+ *[x×] *\d+").unwrap());
}

#[test]
fn info_shows_dpi() {
    Command::cargo_bin("djvu")
        .unwrap()
        .args(["info", corpus("watchmaker.djvu").to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::is_match(r"\d+ *dpi").unwrap());
}

#[test]
fn info_single_page_doc_shows_one_page() {
    Command::cargo_bin("djvu")
        .unwrap()
        .args(["info", corpus("watchmaker.djvu").to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("1"));
}

#[test]
fn info_multipage_doc_lists_pages() {
    // conquete_paix.djvu is multi-page; output must show page count > 1
    let output = Command::cargo_bin("djvu")
        .unwrap()
        .args(["info", corpus("conquete_paix.djvu").to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .clone();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let page_count: usize = stdout
        .lines()
        .find_map(|l| {
            let l = l.to_lowercase();
            if l.contains("page") {
                l.split_whitespace().find_map(|w| w.parse().ok())
            } else {
                None
            }
        })
        .expect("page count not found in output");

    assert!(page_count > 1, "expected >1 pages, got {page_count}");
}

// --- error cases ---

#[test]
fn info_missing_file_exits_nonzero() {
    Command::cargo_bin("djvu")
        .unwrap()
        .args(["info", "/tmp/does_not_exist_xyz.djvu"])
        .assert()
        .failure()
        .stderr(predicate::str::is_empty().not());
}

#[test]
fn info_non_djvu_file_exits_nonzero() {
    // Pass the Cargo.toml — valid file but not DjVu
    let cargo_toml = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    Command::cargo_bin("djvu")
        .unwrap()
        .args(["info", cargo_toml.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::is_empty().not());
}

#[test]
fn info_no_args_prints_usage() {
    Command::cargo_bin("djvu")
        .unwrap()
        .arg("info")
        .assert()
        .failure()
        .stderr(predicate::str::is_empty().not());
}
