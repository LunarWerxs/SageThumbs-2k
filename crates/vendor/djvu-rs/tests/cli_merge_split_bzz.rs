//! CLI coverage for the `merge`, `split`, and `bzz-encode`/`bzz-decode`
//! subcommands of the `djvu` binary — previously untested whole subcommands.

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::PathBuf;

fn corpus(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/corpus")
        .join(name)
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn out(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name)
}

fn page_count(path: &std::path::Path) -> usize {
    let output = Command::cargo_bin("djvu")
        .unwrap()
        .args(["info", "--count", path.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .clone();
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .find_map(|w| w.parse().ok())
        .expect("count not parsed")
}

// ── bzz-encode / bzz-decode ──────────────────────────────────────────────────

#[test]
fn bzz_encode_then_decode_round_trips() {
    // Any byte file works; use the crate manifest.
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let compressed = out("cli_bzz_rt.bzz");
    let restored = out("cli_bzz_rt.out");

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "bzz-encode",
            src.to_str().unwrap(),
            "--output",
            compressed.to_str().unwrap(),
        ])
        .assert()
        .success();
    assert!(compressed.exists(), "compressed output not written");

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "bzz-decode",
            compressed.to_str().unwrap(),
            "--output",
            restored.to_str().unwrap(),
        ])
        .assert()
        .success();

    let original = std::fs::read(&src).unwrap();
    let round = std::fs::read(&restored).unwrap();
    assert_eq!(original, round, "bzz round-trip changed the bytes");
}

#[test]
fn bzz_decode_rejects_non_bzz_input() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "bzz-decode",
            src.to_str().unwrap(),
            "--output",
            out("cli_bzz_bad.out").to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::is_empty().not());
}

// ── merge ────────────────────────────────────────────────────────────────────

#[test]
fn merge_bundles_two_files_into_multipage() {
    // Two distinct documents → one bundle whose page count is the sum.
    let a = fixture("boy_jb2.djvu");
    let b = corpus("watchmaker.djvu");
    let merged = out("cli_merged.djvu");
    let expected = page_count(&a) + page_count(&b);

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "merge",
            a.to_str().unwrap(),
            b.to_str().unwrap(),
            "--output",
            merged.to_str().unwrap(),
        ])
        .assert()
        .success();

    assert!(merged.exists());
    assert_eq!(
        page_count(&merged),
        expected,
        "merged page count should be the sum of inputs"
    );
}

#[test]
fn merge_missing_input_file_fails() {
    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "merge",
            "/tmp/does_not_exist_a.djvu",
            "/tmp/does_not_exist_b.djvu",
            "--output",
            out("cli_merge_bad.djvu").to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::is_empty().not());
}

// ── split ────────────────────────────────────────────────────────────────────

#[test]
fn split_extracts_single_page() {
    let src = corpus("conquete_paix.djvu");
    let dst = out("cli_split_one.djvu");

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "split",
            src.to_str().unwrap(),
            "--page",
            "1",
            "--output",
            dst.to_str().unwrap(),
        ])
        .assert()
        .success();

    assert!(dst.exists());
    assert_eq!(page_count(&dst), 1);
}

#[test]
fn split_extracts_page_range() {
    let src = corpus("conquete_paix.djvu");
    let dst = out("cli_split_range.djvu");

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "split",
            src.to_str().unwrap(),
            "--pages",
            "1-2",
            "--output",
            dst.to_str().unwrap(),
        ])
        .assert()
        .success();

    assert!(dst.exists());
    assert_eq!(page_count(&dst), 2);
}

#[test]
fn split_out_of_range_page_fails() {
    let src = corpus("conquete_paix.djvu");
    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "split",
            src.to_str().unwrap(),
            "--page",
            "99999",
            "--output",
            out("cli_split_bad.djvu").to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::is_empty().not());
}
