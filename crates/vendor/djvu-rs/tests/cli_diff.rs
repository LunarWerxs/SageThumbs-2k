//! CLI tests for `djvu diff` (#696).

use assert_cmd::Command;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn corpus(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("corpus")
        .join(name)
}

#[test]
fn diff_identical_file_matches_and_exits_zero() {
    let file = fixture("DjVu3Spec_bundled.djvu");
    let assert = Command::cargo_bin("djvu")
        .unwrap()
        .args(["diff", file.to_str().unwrap(), file.to_str().unwrap()])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(stdout.contains("pages: match"), "stdout: {stdout}");
    assert!(
        stdout.contains("component_graph: match"),
        "stdout: {stdout}"
    );
}

#[test]
fn diff_different_documents_diverges_and_exits_one() {
    let a = fixture("DjVu3Spec_bundled.djvu");
    let b = corpus("cable_1973_100133.djvu");
    let assert = Command::cargo_bin("djvu")
        .unwrap()
        .args(["diff", a.to_str().unwrap(), b.to_str().unwrap(), "--json"])
        .assert()
        .code(1);
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(json["identical"], false);
    let planes = json["planes"].as_array().expect("planes array");
    assert!(!planes.is_empty());
    let pages = planes
        .iter()
        .find(|plane| plane["plane"] == "pages")
        .expect("pages plane");
    assert_eq!(pages["status"], "diverge");
}

#[test]
fn diff_plane_filter_limits_output() {
    let file = fixture("DjVu3Spec_bundled.djvu");
    let assert = Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "diff",
            file.to_str().unwrap(),
            file.to_str().unwrap(),
            "--plane",
            "text",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert_eq!(stdout.trim(), "text: match");
}

#[test]
fn diff_unreadable_input_exits_two() {
    Command::cargo_bin("djvu")
        .unwrap()
        .args(["diff", "/nonexistent/a.djvu", "/nonexistent/b.djvu"])
        .assert()
        .code(2);
}
