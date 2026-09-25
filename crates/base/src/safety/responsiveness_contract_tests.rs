#![cfg(test)]

use super::*;

/// The whole point of the ordering: the window must appear before a slow decode could
/// possibly finish, and any single UI-thread stage must be far cheaper than the whole
/// appearance budget, or it alone could blow it. See the doc comment above
/// `PREVIEW_APPEARANCE_BUDGET` for the full contract this pins.
#[test]
fn responsiveness_budgets_are_ordered() {
    assert!(
        PREVIEW_UI_STAGE_BUDGET < PREVIEW_APPEARANCE_BUDGET,
        "a single UI-thread stage must be cheaper than the whole appearance budget"
    );
    assert!(
        PREVIEW_APPEARANCE_BUDGET < PREVIEW_DECODE_BUDGET,
        "the window must be able to appear well before a slow decode could time out"
    );
}

/// A stage that finished within its budget is silent: nothing worth logging happened.
#[test]
fn stage_stall_report_is_silent_within_budget() {
    assert_eq!(
        stage_stall_report(
            "prepare",
            Duration::from_millis(50),
            Duration::from_millis(100),
            7,
            "x.txt"
        ),
        None
    );
}

/// The boundary itself is still within budget (`elapsed <= budget`), not a stall: a stage
/// that finishes exactly on the budget should not flap between logged/silent on jitter.
#[test]
fn stage_stall_report_treats_the_exact_budget_as_not_stalled() {
    assert_eq!(
        stage_stall_report(
            "decode",
            Duration::from_millis(100),
            Duration::from_millis(100),
            1,
            "a.png"
        ),
        None
    );
}

/// Over budget: a line naming the stage, elapsed/budget in ms, the generation, and the
/// path. This is the shape this whole function exists to test (see the doc comment: this
/// shape is asserted here and nowhere else, so localizing/reformatting it later cannot
/// silently break a hidden parser).
#[test]
fn stage_stall_report_names_stage_elapsed_budget_generation_and_path() {
    let line = stage_stall_report(
        "apply",
        Duration::from_millis(250),
        Duration::from_millis(100),
        42,
        r"C:\slow\file.txt",
    )
    .expect("over budget must report");
    assert!(line.contains("apply"), "{line}");
    assert!(line.contains("250"), "{line}");
    assert!(line.contains("100"), "{line}");
    assert!(line.contains("42"), "{line}");
    assert!(line.contains(r"C:\slow\file.txt"), "{line}");
}
