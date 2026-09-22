//! TDD coverage for the document optimizer's first lossless-cleanup slice.

use std::fs;

use assert_cmd::Command;
use djvu_rs::djvu_encode::{EncodeQuality, PageEncoder};
use djvu_rs::iff::{self, Chunk};
use djvu_rs::optimizer::{
    OptimizationPhase, OptimizationPreset, OptimizationRequest, OptimizeError, Optimizer,
    ProgressEvent, RewriteAction,
};
use djvu_rs::{Bitmap, Document, Pixmap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
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

/// The bundled spec with a `FREE` chunk inserted at the root, so a run has
/// several components to walk and one rewrite to apply.
fn bundle_with_free_chunk() -> Vec<u8> {
    let bytes = fs::read("tests/fixtures/DjVu3Spec_bundled.djvu").unwrap();
    let mut file = iff::parse(&bytes).unwrap();
    match &mut file.root {
        Chunk::Form { children, .. } => children.insert(
            2,
            Chunk::Leaf {
                id: *b"FREE",
                data: vec![0; 33],
            },
        ),
        Chunk::Leaf { .. } => panic!("bundle must be a FORM"),
    }
    iff::emit(&file)
}

/// Collect every event a run reports, in order.
fn recording_optimizer(
    request: OptimizationRequest,
) -> (Optimizer, Arc<Mutex<Vec<ProgressEvent>>>) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    let optimizer = Optimizer::new(request)
        .with_progress(move |event| sink.lock().unwrap().push(event.clone()));
    (optimizer, events)
}

/// Every event of `phase`, checked for one-step indices and monotone bytes.
fn phase_events(events: &[ProgressEvent], phase: OptimizationPhase) -> Vec<ProgressEvent> {
    let phase_events: Vec<ProgressEvent> = events
        .iter()
        .filter(|event| event.phase == phase)
        .cloned()
        .collect();
    for (position, event) in phase_events.iter().enumerate() {
        assert_eq!(event.component_index, position, "{phase:?} index order");
        assert_eq!(event.component_count, phase_events.len(), "{phase:?} count");
        if position > 0 {
            assert!(
                event.bytes_so_far >= phase_events[position - 1].bytes_so_far,
                "{phase:?} bytes must not decrease"
            );
        }
    }
    phase_events
}

#[test]
fn progress_reports_plan_then_rewrite_then_verify_per_component() {
    let input = bundle_with_free_chunk();
    let (optimizer, events) = recording_optimizer(OptimizationRequest::lossless_cleanup());

    let result = optimizer.optimize(&input).unwrap();
    let events = events.lock().unwrap().clone();

    // Phases arrive in order and never interleave.
    let order: Vec<OptimizationPhase> = events.iter().map(|event| event.phase).collect();
    let mut sorted = order.clone();
    sorted.sort_by_key(|phase| match phase {
        OptimizationPhase::Plan => 0,
        OptimizationPhase::Rewrite => 1,
        OptimizationPhase::Verify => 2,
        _ => 3,
    });
    assert_eq!(order, sorted, "phases must not interleave");

    // Plan: one event per root child of the DJVM, the FREE chunk included.
    let plan = phase_events(&events, OptimizationPhase::Plan);
    let root_children = match iff::parse(&input).unwrap().root {
        Chunk::Form { children, .. } => children.len(),
        Chunk::Leaf { .. } => unreachable!(),
    };
    assert_eq!(plan.len(), root_children);
    assert_eq!(plan[0].component_id, *b"DIRM");
    assert_eq!(plan[2].component_id, *b"FREE");
    assert!(plan.iter().any(|event| event.component_id == *b"DJVU"));
    // The header-inclusive sizes add up to the root payload minus its
    // secondary ID, short of one pad byte per odd-length child.
    let walked = plan.last().unwrap().bytes_so_far;
    let payload = input.len() - 16;
    assert!(
        walked <= payload && walked + root_children >= payload,
        "plan walked {walked} B of a {payload} B payload"
    );

    // Rewrite: exactly the one FREE removal, its input payload as bytes.
    let rewrite = phase_events(&events, OptimizationPhase::Rewrite);
    assert_eq!(rewrite.len(), 1);
    assert_eq!(rewrite[0].component_id, *b"FREE");
    assert_eq!(rewrite[0].bytes_so_far, 33);

    // Verify: the output's components, one fewer than the input's.
    let verify = phase_events(&events, OptimizationPhase::Verify);
    assert_eq!(verify.len(), root_children - 1);
    assert!(verify.iter().all(|event| event.component_id != *b"FREE"));
    let walked = verify.last().unwrap().bytes_so_far;
    let payload = result.bytes.len() - 16;
    assert!(
        walked <= payload && walked + root_children >= payload,
        "verify walked {walked} B of a {payload} B payload"
    );
}

#[test]
fn plan_alone_reports_only_the_plan_phase() {
    let input = page_with_free_and_unknown_chunk();
    let (optimizer, events) = recording_optimizer(OptimizationRequest::lossless_cleanup());

    optimizer.plan(&input).unwrap();
    let events = events.lock().unwrap().clone();

    // A single-page DJVU is one component: the root FORM itself.
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].phase, OptimizationPhase::Plan);
    assert_eq!(events[0].component_id, *b"DJVU");
    assert_eq!(events[0].component_count, 1);
    assert_eq!(events[0].bytes_so_far, input.len() - 4);
}

#[test]
fn pass_through_run_reports_no_rewrite_events() {
    let input = PageEncoder::from_bitmap(&Bitmap::new(8, 8))
        .encode()
        .unwrap();
    let (optimizer, events) = recording_optimizer(OptimizationRequest::lossless_cleanup());

    optimizer.optimize(&input).unwrap();
    let events = events.lock().unwrap().clone();

    let phases: Vec<OptimizationPhase> = events.iter().map(|event| event.phase).collect();
    assert_eq!(phases, [OptimizationPhase::Plan, OptimizationPhase::Verify]);
}

#[test]
fn optimizer_without_hook_is_unchanged() {
    let input = bundle_with_free_chunk();
    let silent = Optimizer::new(OptimizationRequest::lossless_cleanup());
    let (observed, _) = recording_optimizer(OptimizationRequest::lossless_cleanup());
    assert_eq!(
        silent.optimize(&input).unwrap(),
        observed.optimize(&input).unwrap()
    );
}

/// An optimizer that records events and cancels once `after` of them have
/// been reported; the hook is polled before the next component.
fn cancelling_after(after: usize) -> (Optimizer, Arc<Mutex<Vec<ProgressEvent>>>) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::new(AtomicUsize::new(0));
    let sink = Arc::clone(&events);
    let counter = Arc::clone(&seen);
    let optimizer = Optimizer::new(OptimizationRequest::lossless_cleanup())
        .with_progress(move |event| {
            sink.lock().unwrap().push(event.clone());
            counter.fetch_add(1, Ordering::SeqCst);
        })
        .with_cancel(move || seen.load(Ordering::SeqCst) >= after);
    (optimizer, events)
}

#[test]
fn cancel_before_start_reports_nothing() {
    let input = bundle_with_free_chunk();
    let (optimizer, events) = cancelling_after(0);
    assert!(matches!(
        optimizer.plan(&input),
        Err(OptimizeError::Cancelled)
    ));
    assert!(matches!(
        optimizer.optimize(&input),
        Err(OptimizeError::Cancelled)
    ));
    assert!(events.lock().unwrap().is_empty());
}

#[test]
fn cancel_stops_at_a_component_boundary_in_plan() {
    let input = bundle_with_free_chunk();
    let (optimizer, events) = cancelling_after(3);
    assert!(matches!(
        optimizer.optimize(&input),
        Err(OptimizeError::Cancelled)
    ));
    let events = events.lock().unwrap().clone();
    // Exactly the three components handled before the poll saw the stop.
    assert_eq!(events.len(), 3);
    assert!(
        events
            .iter()
            .all(|event| event.phase == OptimizationPhase::Plan)
    );
    assert_eq!(events[2].component_index, 2);
}

#[test]
fn cancel_after_rewrite_withholds_output_and_skips_verify() {
    let input = bundle_with_free_chunk();
    let root_children = match iff::parse(&input).unwrap().root {
        Chunk::Form { children, .. } => children.len(),
        Chunk::Leaf { .. } => unreachable!(),
    };
    // Stop once the single rewrite has been reported: the next poll is the
    // one ahead of verification.
    let (optimizer, events) = cancelling_after(root_children + 1);
    assert!(matches!(
        optimizer.optimize(&input),
        Err(OptimizeError::Cancelled)
    ));
    let events = events.lock().unwrap().clone();
    assert_eq!(events.len(), root_children + 1);
    assert_eq!(events.last().unwrap().phase, OptimizationPhase::Rewrite);
    assert!(
        events
            .iter()
            .all(|event| event.phase != OptimizationPhase::Verify)
    );
}

#[test]
fn cancel_hook_that_never_fires_changes_nothing() {
    let input = bundle_with_free_chunk();
    let plain = Optimizer::new(OptimizationRequest::lossless_cleanup());
    let (polite, _) = cancelling_after(usize::MAX);
    assert_eq!(
        plain.optimize(&input).unwrap(),
        polite.optimize(&input).unwrap()
    );
}

#[test]
fn several_free_chunks_in_one_parent_are_all_removed_in_plan_order() {
    // Two FREE leaves ahead of the Xtra chunk and one after it: removing the
    // earlier ones shifts the later paths, which the optimizer must track.
    let bitmap = Bitmap::new(8, 8);
    let encoded = PageEncoder::from_bitmap(&bitmap).encode().unwrap();
    let mut file = iff::parse(&encoded).unwrap();
    let free = |len: usize| Chunk::Leaf {
        id: *b"FREE",
        data: vec![0; len],
    };
    match &mut file.root {
        Chunk::Form { children, .. } => {
            children.insert(1, free(5));
            children.insert(2, free(6));
            children.push(Chunk::Leaf {
                id: *b"Xtra",
                data: b"preserve me".to_vec(),
            });
            children.push(free(7));
        }
        Chunk::Leaf { .. } => unreachable!(),
    }
    let input = iff::emit(&file);
    let (optimizer, events) = recording_optimizer(OptimizationRequest::lossless_cleanup());

    let result = optimizer.optimize(&input).unwrap();

    let rewrites = &result.report.rewritten_components;
    assert_eq!(rewrites.len(), 3);
    assert_eq!(
        rewrites
            .iter()
            .map(|item| item.input_bytes)
            .collect::<Vec<_>>(),
        [5, 6, 7]
    );
    let output = iff::parse(&result.bytes).unwrap();
    let ids: Vec<[u8; 4]> = match output.root {
        Chunk::Form { children, .. } => children
            .iter()
            .map(|chunk| match chunk {
                Chunk::Leaf { id, .. } => *id,
                Chunk::Form { secondary_id, .. } => *secondary_id,
            })
            .collect(),
        Chunk::Leaf { .. } => unreachable!(),
    };
    assert!(!ids.contains(b"FREE"));
    assert!(ids.contains(b"Xtra"));
    // Rewrite events come in plan order with increasing bytes.
    let rewrite = phase_events(&events.lock().unwrap(), OptimizationPhase::Rewrite);
    assert_eq!(
        rewrite
            .iter()
            .map(|event| event.bytes_so_far)
            .collect::<Vec<_>>(),
        [5, 11, 18]
    );
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
        // stderr is a pipe here, so no progress line and no escape codes.
        .stderr(predicates::str::is_empty())
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
        .success()
        .stderr(predicates::str::is_empty());
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

// ── Archival re-encode (#814, slice 3) ──────────────────────────────────────

/// A textured 96x96 page at the `Photo` profile (`INFO + BG44…`, no mask):
/// colour when `color`, otherwise the same texture as a grey ramp.
fn photo_page(color: bool) -> Vec<u8> {
    let (w, h) = (96u32, 96u32);
    let mut px = Pixmap::try_new(w, h, 0, 0, 0, 255).unwrap();
    for y in 0..h as usize {
        for x in 0..w as usize {
            let i = (y * w as usize + x) * 4;
            let ramp = (x * 255 / 95) as u8;
            let texture = (((x as f64 / 5.0).sin() * (y as f64 / 7.0).cos()) * 60.0 + 128.0) as u8;
            let noise = ((x * 7 + y * 13) % 23) as u8;
            if color {
                px.data[i] = ramp;
                px.data[i + 1] = texture;
                px.data[i + 2] = (255 - ramp).wrapping_add(noise);
            } else {
                let grey = texture.wrapping_add(noise / 2);
                px.data[i] = grey;
                px.data[i + 1] = grey;
                px.data[i + 2] = grey;
            }
            px.data[i + 3] = 255;
        }
    }
    PageEncoder::from_pixmap(&px)
        .with_quality(EncodeQuality::Photo)
        .encode()
        .unwrap()
}

/// A text-like 240x120 bilevel page: rows of one glyph shape with small
/// per-copy variations, the population lossy symbol matching feeds on.
fn text_page() -> Vec<u8> {
    let mut bitmap = Bitmap::new(240, 120);
    for row in 0..6u32 {
        for column in 0..20u32 {
            let (x0, y0) = (column * 12 + 2, row * 20 + 4);
            for dy in 0..12u32 {
                for dx in 0..8u32 {
                    let border = dx == 0 || dx == 7 || dy == 0 || dy == 11;
                    let bar = dy == 5 && dx > 1 && dx < 6;
                    let wobble = (row + column) % 3 == 0 && dx == 3 && dy == 2;
                    if border || bar || wobble {
                        bitmap.set(x0 + dx, y0 + dy, true);
                    }
                }
            }
        }
    }
    PageEncoder::from_bitmap(&bitmap).encode().unwrap()
}

fn rendered(bytes: &[u8]) -> Pixmap {
    Document::from_bytes(bytes.to_vec())
        .unwrap()
        .page(0)
        .unwrap()
        .render()
        .unwrap()
}

fn reencodes(
    components: &[djvu_rs::optimizer::RewrittenComponent],
    action: RewriteAction,
) -> Vec<djvu_rs::optimizer::RewrittenComponent> {
    components
        .iter()
        .filter(|component| component.action == action)
        .cloned()
        .collect()
}

fn assert_background_reencoded_within(input: &[u8], floor: f32) {
    let optimizer = Optimizer::new(OptimizationRequest::archival().with_max_ssim_loss(floor));
    let plan = optimizer.plan(input).unwrap();
    let selected = reencodes(
        &plan.rewritten_components,
        RewriteAction::ReencodeBackground,
    );
    assert_eq!(selected.len(), 1, "one background rewrite, got {plan:?}");
    let component = &selected[0];
    assert_eq!(component.chunk_id, *b"BG44");
    assert!(component.path.is_empty(), "a single page is the root");
    let quality = component.quality.expect("a re-encode is measured");
    assert!(quality.ssim_loss <= f64::from(floor), "{quality:?}");
    assert!(
        quality.slices.is_some_and(|slices| slices < 100),
        "fewer slices than the 100 the Photo profile wrote: {quality:?}"
    );
    assert!(component.output_bytes < component.input_bytes);
    assert!(plan.output_bytes < plan.input_bytes);
    assert!(plan.changed);
    assert!(plan.quality_floor_met);
    assert_eq!(plan.min_ssim, Some(quality.ssim));

    let result = optimizer.optimize(input).unwrap();
    assert_eq!(result.bytes.len(), plan.output_bytes);
    assert_eq!(result.report.min_ssim, plan.min_ssim);
    assert_eq!(
        result.report.rewritten_components,
        plan.rewritten_components
    );

    // The output renders, at the input's size, close to the input's render.
    let before = rendered(input);
    let after = rendered(&result.bytes);
    assert_eq!((after.width, after.height), (before.width, before.height));
    let loss = 1.0 - djvu_rs::quality::ssim(&before, &after);
    assert!(
        loss <= f64::from(floor) + 0.02,
        "rendered SSIM loss {loss} exceeds the floor {floor}"
    );
}

#[test]
fn archival_reencodes_a_colour_background_within_the_floor() {
    assert_background_reencoded_within(&photo_page(true), 0.05);
}

#[test]
fn archival_reencodes_a_grey_background_within_the_floor() {
    assert_background_reencoded_within(&photo_page(false), 0.05);
}

#[test]
fn archival_with_a_tiny_floor_never_exceeds_it() {
    let input = photo_page(true);
    let floor = 1e-6f32;
    let optimizer = Optimizer::new(OptimizationRequest::archival().with_max_ssim_loss(floor));
    let result = optimizer.optimize(&input).unwrap();
    let selected = reencodes(
        &result.report.rewritten_components,
        RewriteAction::ReencodeBackground,
    );
    if selected.is_empty() {
        assert_eq!(result.bytes, input);
        assert!(!result.report.changed);
        assert!(
            result
                .report
                .warnings
                .iter()
                .any(|warning| warning.contains("left untouched")),
            "{:?}",
            result.report.warnings
        );
    } else {
        for component in selected {
            let quality = component.quality.unwrap();
            assert!(quality.ssim_loss <= f64::from(floor), "{quality:?}");
            assert!(component.output_bytes < component.input_bytes);
        }
    }
    assert!(result.report.quality_floor_met);
}

#[test]
fn archival_without_a_floor_stays_pixel_exact_and_names_the_knob() {
    let input = photo_page(true);
    let optimizer = Optimizer::new(OptimizationRequest::archival());
    let result = optimizer.optimize(&input).unwrap();
    assert_eq!(result.bytes, input);
    assert!(!result.report.changed);
    assert!(result.report.rewritten_components.is_empty());
    assert_eq!(result.report.min_ssim, None);
    assert!(
        result
            .report
            .warnings
            .iter()
            .any(|warning| warning.contains("max_ssim_loss")),
        "{:?}",
        result.report.warnings
    );
}

#[test]
fn lossless_preset_never_reencodes_even_with_a_floor() {
    let input = photo_page(true);
    let request = OptimizationRequest::lossless_cleanup()
        .with_max_ssim_loss(0.5)
        .with_lossy_text(true);
    let result = Optimizer::new(request).optimize(&input).unwrap();
    assert_eq!(result.bytes, input);
    assert!(result.report.rewritten_components.is_empty());
    let warnings = &result.report.warnings;
    assert!(
        warnings.iter().any(|w| w.contains("does not measure SSIM")),
        "{warnings:?}"
    );
    assert!(
        warnings.iter().any(|w| w.contains("lossy_text")),
        "{warnings:?}"
    );
}

#[test]
fn lossy_text_is_opt_in_and_honours_the_floor() {
    let input = text_page();
    let floor = 0.1f32;
    let strict = Optimizer::new(OptimizationRequest::archival().with_max_ssim_loss(floor));
    let plan = strict.plan(&input).unwrap();
    assert!(
        reencodes(&plan.rewritten_components, RewriteAction::ReencodeMask).is_empty(),
        "the mask is never touched without lossy_text: {plan:?}"
    );
    assert_eq!(strict.optimize(&input).unwrap().bytes, input);

    let lossy = Optimizer::new(
        OptimizationRequest::archival()
            .with_max_ssim_loss(floor)
            .with_lossy_text(true),
    );
    let result = lossy.optimize(&input).unwrap();
    let selected = reencodes(
        &result.report.rewritten_components,
        RewriteAction::ReencodeMask,
    );
    assert!(
        selected.len() <= 1,
        "one mask per page at most: {:?}",
        result.report
    );
    for component in &selected {
        assert_eq!(component.chunk_id, *b"Sjbz");
        let quality = component.quality.unwrap();
        assert!(quality.ssim_loss <= f64::from(floor), "{quality:?}");
        assert_eq!(quality.slices, None);
        assert!(component.output_bytes < component.input_bytes);
    }
    if selected.is_empty() {
        assert_eq!(result.bytes, input);
    } else {
        assert!(result.bytes.len() < input.len());
    }
    let document = Document::from_bytes(result.bytes.clone()).unwrap();
    let mask = document.page(0).unwrap().decode_mask().unwrap().unwrap();
    assert_eq!((mask.width, mask.height), (240, 120));
}

/// A leaf's identity for comparison: `Chunk` has no `PartialEq`.
fn leaf_key(chunk: &Chunk) -> ([u8; 4], Vec<u8>) {
    match chunk {
        Chunk::Form { secondary_id, .. } => (*secondary_id, Vec::new()),
        Chunk::Leaf { id, data } => (*id, data.clone()),
    }
}

fn free_count(chunk: &Chunk) -> usize {
    match chunk {
        Chunk::Form { children, .. } => children.iter().map(free_count).sum(),
        Chunk::Leaf { id, .. } => usize::from(id == b"FREE"),
    }
}

/// A `FREE` at the root ahead of a page and a `FREE` inside that page: the
/// second path has to be adjusted at the root depth once the first is gone.
#[test]
fn free_chunks_at_two_depths_are_all_removed() {
    let bytes = fs::read("tests/fixtures/DjVu3Spec_bundled.djvu").unwrap();
    let mut file = iff::parse(&bytes).unwrap();
    let Chunk::Form { children, .. } = &mut file.root else {
        panic!("bundle must be a FORM")
    };
    children.insert(
        0,
        Chunk::Leaf {
            id: *b"FREE",
            data: vec![0; 5],
        },
    );
    let page_index = children
        .iter()
        .position(
            |child| matches!(child, Chunk::Form { secondary_id, .. } if secondary_id == b"DJVU"),
        )
        .unwrap();
    let Chunk::Form {
        children: page_children,
        ..
    } = &mut children[page_index]
    else {
        unreachable!()
    };
    let expected_page_children = page_children.clone();
    page_children.insert(
        1,
        Chunk::Leaf {
            id: *b"FREE",
            data: vec![0; 9],
        },
    );
    let input = iff::emit(&file);

    let result = Optimizer::new(OptimizationRequest::lossless_cleanup())
        .optimize(&input)
        .unwrap();
    assert_eq!(result.report.rewritten_components.len(), 2);
    let output = iff::parse(&result.bytes).unwrap();
    assert_eq!(free_count(&output.root), 0);
    let Chunk::Form { children, .. } = &output.root else {
        unreachable!()
    };
    let Chunk::Form {
        children: page_children,
        ..
    } = &children[page_index - 1]
    else {
        panic!("the page moved to where the root FREE was")
    };
    assert_eq!(page_children.len(), expected_page_children.len());
    for (got, want) in page_children.iter().zip(&expected_page_children) {
        assert_eq!(leaf_key(got), leaf_key(want));
    }
}

#[test]
fn json_carries_quality_and_min_ssim() {
    let input = photo_page(true);
    let plan = Optimizer::new(OptimizationRequest::archival().with_max_ssim_loss(0.05))
        .plan(&input)
        .unwrap();
    let json: serde_json::Value = serde_json::from_str(&plan.to_json()).unwrap();
    assert!(json["min_ssim"].is_number(), "{json}");
    let component = &json["rewritten_components"][0];
    assert_eq!(component["action"], "reencode-background");
    assert_eq!(component["chunk_id"], "BG44");
    assert!(component["quality"]["ssim"].is_number(), "{component}");
    assert!(component["quality"]["ssim_loss"].is_number(), "{component}");
    assert!(component["quality"]["slices"].is_number(), "{component}");

    let lossless = Optimizer::new(OptimizationRequest::lossless_cleanup())
        .plan(&page_with_free_and_unknown_chunk())
        .unwrap();
    let json: serde_json::Value = serde_json::from_str(&lossless.to_json()).unwrap();
    assert!(json["min_ssim"].is_null());
    assert!(json["rewritten_components"][0]["quality"].is_null());
}

#[test]
fn cli_archival_reencode_takes_a_floor_and_lossy_text() {
    let dir = tempdir().unwrap();
    let input = dir.path().join("photo.djvu");
    let output = dir.path().join("archived.djvu");
    let bytes = photo_page(true);
    fs::write(&input, &bytes).unwrap();

    let assert = Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "optimize",
            input.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
            "--preset",
            "archival",
            "--max-ssim-loss",
            "0.05",
            "--lossy-text",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json["preset"], "archival");
    assert_eq!(json["changed"], true);
    assert!(json["min_ssim"].is_number(), "{json}");
    let written = fs::read(&output).unwrap();
    assert!(written.len() < bytes.len());
    assert_eq!(json["output_bytes"], written.len());
}
