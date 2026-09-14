//! TDD coverage for the document optimizer's first lossless-cleanup slice.

use std::fs;

use assert_cmd::Command;
use djvu_rs::Bitmap;
use djvu_rs::djvu_encode::PageEncoder;
use djvu_rs::iff::{self, Chunk};
use djvu_rs::optimizer::{OptimizationPreset, OptimizationRequest, Optimizer};
use tempfile::tempdir;

fn page_with_free_and_unknown_chunk() -> Vec<u8> {
    let bitmap = Bitmap::new(8, 8);
    let encoded = PageEncoder::from_bitmap(&bitmap).encode().unwrap();
    let mut file = iff::parse(&encoded).unwrap();
    match &mut file.root {
        Chunk::Form { children, .. } => {
            children.insert(
                1,
                Chunk::Leaf {
                    id: *b"FREE",
                    data: vec![0; 17],
                },
            );
            children.push(Chunk::Leaf {
                id: *b"Xtra",
                data: b"preserve me".to_vec(),
            });
        }
        Chunk::Leaf { .. } => panic!("page encoder must emit a FORM"),
    }
    iff::emit(&file)
}

#[test]
fn lossless_cleanup_removes_free_but_preserves_pixels_and_unknown_chunks() {
    let input = page_with_free_and_unknown_chunk();
    let optimizer = Optimizer::new(OptimizationRequest::lossless_cleanup());

    let plan = optimizer.plan(&input).unwrap();
    assert_eq!(plan.preset, OptimizationPreset::LosslessCleanup);
    assert!(plan.changed);
    assert_eq!(plan.rewritten_components.len(), 1);
    assert_eq!(plan.rewritten_components[0].chunk_id, *b"FREE");
    assert_eq!(plan.rewritten_components[0].input_bytes, 17);
    assert_eq!(plan.rewritten_components[0].output_bytes, 0);

    let result = optimizer.optimize(&input).unwrap();
    assert!(result.report.changed);
    assert_eq!(result.report.input_bytes, input.len());
    assert_eq!(result.report.output_bytes, result.bytes.len());
    assert!(result.bytes.len() < input.len());

    let document = djvu_rs::DjVuDocument::parse(&result.bytes).unwrap();
    let page = document.page(0).unwrap();
    assert!(page.raw_chunk(b"FREE").is_none());
    assert_eq!(page.raw_chunk(b"Xtra"), Some(&b"preserve me"[..]));
    assert_eq!(page.extract_mask().unwrap().unwrap().width, 8);
}

#[test]
fn already_clean_input_is_byte_identical_and_reported_as_pass_through() {
    let bitmap = Bitmap::new(8, 8);
    let input = PageEncoder::from_bitmap(&bitmap).encode().unwrap();
    let optimizer = Optimizer::new(OptimizationRequest::lossless_cleanup());

    let plan = optimizer.plan(&input).unwrap();
    assert!(!plan.changed);
    assert!(plan.rewritten_components.is_empty());
    assert!(plan.warnings.is_empty());

    let result = optimizer.optimize(&input).unwrap();
    assert_eq!(result.bytes, input);
    assert!(!result.report.changed);
    assert!(result.report.rewritten_components.is_empty());
}

#[test]
fn archival_request_is_typed_and_reports_unmet_target_without_lossy_reencode() {
    let input = page_with_free_and_unknown_chunk();
    let request = OptimizationRequest::archival().with_target_size(1);
    let optimizer = Optimizer::new(request);

    let plan = optimizer.plan(&input).unwrap();
    assert_eq!(plan.preset, OptimizationPreset::Archival);
    assert!(!plan.target_met);
    assert!(
        plan.warnings
            .iter()
            .any(|warning| warning.contains("target size"))
    );

    let result = optimizer.optimize(&input).unwrap();
    assert_eq!(result.bytes.len(), plan.output_bytes);
    assert!(
        result
            .report
            .warnings
            .iter()
            .any(|warning| warning.contains("archival"))
    );
}

#[test]
fn max_ssim_loss_warns_and_does_not_pretend_to_gate_lossless_cleanup() {
    let input = page_with_free_and_unknown_chunk();
    let request = OptimizationRequest::lossless_cleanup().with_max_ssim_loss(0.001);
    let optimizer = Optimizer::new(request);

    let plan = optimizer.plan(&input).unwrap();
    assert!(plan.quality_floor_met);
    assert!(
        plan.warnings
            .iter()
            .any(|warning| warning.contains("does not measure SSIM")),
        "expected SSIM-reservation warning, got {:?}",
        plan.warnings
    );
}

#[test]
fn plan_and_report_have_machine_readable_json() {
    let input = page_with_free_and_unknown_chunk();
    let optimizer = Optimizer::new(OptimizationRequest::lossless_cleanup());
    let plan = optimizer.plan(&input).unwrap();
    let result = optimizer.optimize(&input).unwrap();

    let plan_json: serde_json::Value = serde_json::from_str(&plan.to_json()).unwrap();
    let report_json: serde_json::Value = serde_json::from_str(&result.report.to_json()).unwrap();
    assert_eq!(plan_json["preset"], "lossless-cleanup");
    assert_eq!(plan_json["changed"], true);
    assert_eq!(plan_json["rewritten_components"][0]["chunk_id"], "FREE");
    assert_eq!(report_json["output_bytes"], result.bytes.len());
}

#[test]
fn cli_dry_run_does_not_write_and_normal_run_is_atomic() {
    let dir = tempdir().unwrap();
    let input = dir.path().join("input.djvu");
    let output = dir.path().join("output.djvu");
    fs::write(&input, page_with_free_and_unknown_chunk()).unwrap();

    let dry_run = Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "optimize",
            input.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
            "--preset",
            "lossless-cleanup",
            "--dry-run",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let dry_json: serde_json::Value = serde_json::from_slice(&dry_run).unwrap();
    assert_eq!(dry_json["changed"], true);
    assert!(!output.exists());

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "optimize",
            input.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
            "--preset",
            "lossless-cleanup",
        ])
        .assert()
        .success();
    assert!(output.exists());
    assert_eq!(
        fs::read(&input).unwrap(),
        page_with_free_and_unknown_chunk()
    );
    djvu_rs::DjVuDocument::parse(&fs::read(output).unwrap()).unwrap();

    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "optimize",
            input.to_str().unwrap(),
            "--output",
            input.to_str().unwrap(),
            "--preset",
            "lossless-cleanup",
        ])
        .assert()
        .failure();
}
