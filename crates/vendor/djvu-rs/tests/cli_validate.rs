//! CLI coverage for `djvu validate` human output, strict exits, and JSON schema.

use assert_cmd::Command;
use serde_json::Value;
use std::path::PathBuf;
use tempfile::NamedTempFile;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn warning_only_input() -> NamedTempFile {
    // This real legacy fixture omits final alignment padding after its
    // odd-length BG44 stream. The validator recovers it as a warning without
    // treating the document as invalid.
    let data = std::fs::read(fixture("boy.djvu")).expect("fixture exists");
    let file = NamedTempFile::new().expect("temporary file");
    std::fs::write(file.path(), data).expect("write temporary DjVu");
    file
}

#[test]
fn strict_turns_a_warning_only_file_into_exit_one() {
    let file = warning_only_input();
    Command::cargo_bin("djvu")
        .expect("binary builds")
        .args(["validate", file.path().to_str().expect("utf8 path")])
        .assert()
        .success()
        .stdout(predicates::str::contains("1 warnings"));
    Command::cargo_bin("djvu")
        .expect("binary builds")
        .args([
            "validate",
            file.path().to_str().expect("utf8 path"),
            "--strict",
        ])
        .assert()
        .code(1);
}

#[test]
fn json_has_stable_schema_and_consistent_summary() {
    let output = Command::cargo_bin("djvu")
        .expect("binary builds")
        .args([
            "validate",
            fixture("boy.djvu").to_str().expect("utf8 fixture path"),
            "--json",
        ])
        .output()
        .expect("run validate");
    assert!(output.status.success(), "{output:?}");
    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON output");
    assert!(json["file"].is_string());
    assert!(json["valid"].is_boolean());
    assert!(json["summary"].is_object());
    assert!(json["findings"].is_array());
    let findings = json["findings"].as_array().expect("findings array");
    for finding in findings {
        for field in [
            "severity",
            "layer",
            "code",
            "component",
            "chunk",
            "offset",
            "message",
        ] {
            assert!(finding.get(field).is_some(), "missing {field}: {finding}");
        }
    }
    let summary = json["summary"].as_object().expect("summary object");
    let total: usize = ["errors", "warnings", "tolerated", "recovery"]
        .into_iter()
        .map(|field| summary[field].as_u64().expect("count") as usize)
        .sum();
    assert_eq!(total, findings.len());
}

#[test]
fn unreadable_file_exits_two() {
    Command::cargo_bin("djvu")
        .expect("binary builds")
        .args(["validate", "/tmp/djvu-rs-validate-does-not-exist.djvu"])
        .assert()
        .code(2);
}

#[test]
fn json_reports_resource_estimate() {
    let output = Command::cargo_bin("djvu")
        .expect("binary builds")
        .args([
            "validate",
            fixture("boy.djvu").to_str().expect("utf8 fixture path"),
            "--json",
        ])
        .output()
        .expect("run validate");
    assert!(output.status.success(), "{output:?}");
    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON output");
    let resources = json["resources"].as_object().expect("resources object");
    for field in [
        "file_bytes",
        "pages",
        "components",
        "max_page_pixels",
        "total_pixels",
        "peak_decoded_bytes",
    ] {
        assert!(
            resources.get(field).and_then(Value::as_u64).is_some(),
            "missing numeric {field}: {resources:?}"
        );
    }
    assert_eq!(resources["pages"].as_u64(), Some(1));
    assert_eq!(resources["components"].as_u64(), Some(1));
}

#[test]
fn exceeded_limits_fail_as_resource_errors() {
    let limits = NamedTempFile::new().expect("temp limits file");
    std::fs::write(limits.path(), br#"{"max_pages": 0, "max_page_pixels": 1}"#)
        .expect("write limits");

    let output = Command::cargo_bin("djvu")
        .expect("binary builds")
        .args([
            "validate",
            fixture("boy.djvu").to_str().expect("utf8 fixture path"),
            "--limits",
            limits.path().to_str().expect("utf8 limits path"),
            "--json",
        ])
        .output()
        .expect("run validate");
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON output");
    assert_eq!(json["valid"].as_bool(), Some(false));
    let codes: Vec<&str> = json["findings"]
        .as_array()
        .expect("findings array")
        .iter()
        .filter(|finding| finding["layer"] == "resource")
        .filter_map(|finding| finding["code"].as_str())
        .collect();
    assert!(codes.contains(&"resource.too-many-pages"), "{codes:?}");
    assert!(codes.contains(&"resource.page-too-large"), "{codes:?}");
}

#[test]
fn generous_limits_keep_a_clean_file_valid() {
    let limits = NamedTempFile::new().expect("temp limits file");
    std::fs::write(
        limits.path(),
        br#"{"max_file_bytes": 1000000000, "max_pages": 100000}"#,
    )
    .expect("write limits");

    Command::cargo_bin("djvu")
        .expect("binary builds")
        .args([
            "validate",
            fixture("boy.djvu").to_str().expect("utf8 fixture path"),
            "--limits",
            limits.path().to_str().expect("utf8 limits path"),
        ])
        .assert()
        .success();
}

#[test]
fn malformed_limits_file_exits_two() {
    let limits = NamedTempFile::new().expect("temp limits file");
    std::fs::write(limits.path(), br#"{"max_pages": "lots"}"#).expect("write limits");

    Command::cargo_bin("djvu")
        .expect("binary builds")
        .args([
            "validate",
            fixture("boy.djvu").to_str().expect("utf8 fixture path"),
            "--limits",
            limits.path().to_str().expect("utf8 limits path"),
        ])
        .assert()
        .code(2);
}

#[test]
fn unknown_limits_key_exits_two() {
    let limits = NamedTempFile::new().expect("temp limits file");
    std::fs::write(limits.path(), br#"{"max_frobs": 5}"#).expect("write limits");

    Command::cargo_bin("djvu")
        .expect("binary builds")
        .args([
            "validate",
            fixture("boy.djvu").to_str().expect("utf8 fixture path"),
            "--limits",
            limits.path().to_str().expect("utf8 limits path"),
        ])
        .assert()
        .code(2);
}
