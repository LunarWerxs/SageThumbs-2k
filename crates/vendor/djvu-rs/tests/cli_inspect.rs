//! CLI coverage for `djvu inspect`'s human and stable JSON output.

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use std::{fs, path::PathBuf};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[test]
fn inspect_human_output_shows_indented_offsets_and_component_ids() {
    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "inspect",
            fixture("DjVu3Spec_bundled.djvu").to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("FORM:DJVM ["))
        .stdout(predicate::str::contains("@ 0x00000004"))
        .stdout(predicate::str::contains("  DIRM ["))
        .stdout(predicate::str::contains("  FORM:DJVU ["))
        .stdout(predicate::str::contains("{"));
}

#[test]
fn inspect_json_matches_the_documented_schema() {
    let output = Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "inspect",
            fixture("DjVu3Spec_bundled.djvu").to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).expect("inspect emits JSON");

    assert_eq!(json["container"], "DJVM");
    assert!(json["file"].as_str().is_some());
    let chunks = json["chunks"].as_array().expect("chunks array");
    assert!(!chunks.is_empty());
    assert_eq!(chunks[0]["id"], "FORM");
    assert_eq!(chunks[0]["form_type"], "DJVM");
    assert_eq!(chunks[0]["offset"], 4);
    assert_eq!(chunks[0]["depth"], 0);
    assert_eq!(chunks[0]["path"], serde_json::json!([]));

    let component_form = chunks
        .iter()
        .find(|chunk| chunk["depth"] == 1 && chunk["id"] == "FORM")
        .expect("bundled component FORM");
    assert!(component_form["component_id"].as_str().is_some());
    assert!(component_form["kind"].as_str().is_some());

    let components = json["components"].as_array().expect("components array");
    assert!(!components.is_empty());
    for key in ["id", "kind", "dirm_index", "includes", "included_by"] {
        assert!(components[0].get(key).is_some(), "component missing {key}");
    }
}

#[test]
fn inspect_reports_a_clean_error_for_unparseable_input() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("not-djvu.bin");
    fs::write(&input, b"not an IFF document").unwrap();

    Command::cargo_bin("djvu")
        .unwrap()
        .args(["inspect", input.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error:"));
}

#[test]
fn inspect_ignores_garbage_after_a_valid_outer_form() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("trailing-garbage.djvu");
    let mut bytes = Vec::from(*b"AT&T");
    bytes.extend_from_slice(b"FORM");
    bytes.extend_from_slice(&14u32.to_be_bytes());
    bytes.extend_from_slice(b"DJVU");
    bytes.extend_from_slice(b"INFO");
    bytes.extend_from_slice(&2u32.to_be_bytes());
    bytes.extend_from_slice(b"ok");
    bytes.extend_from_slice(b"not part of the root FORM");
    fs::write(&input, bytes).unwrap();

    let output = Command::cargo_bin("djvu")
        .unwrap()
        .args(["inspect", input.to_str().unwrap(), "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).expect("valid JSON despite trailing garbage");
    let chunks = json["chunks"].as_array().expect("chunks array");
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0]["id"], "FORM");
    assert_eq!(chunks[1]["id"], "INFO");
    assert!(json.get("components").is_none());
}
