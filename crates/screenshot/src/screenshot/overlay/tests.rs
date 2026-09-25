#![cfg(test)]

use super::*;

fn automation_state() -> AutomationState {
    AutomationState {
        forced_shift: false,
        commit_gen: 0,
        painted_gen: 0,
        last_drag: None,
        status: "ready",
        published_title: String::new(),
    }
}

/// A135: `capture_instant` used to have no guard at all against a second capture
/// starting while one is already in flight. This exercises the shared mechanism
/// directly (with a private test-only mutex name, so it can't collide with a real
/// overlay or with another test in this same process) rather than driving the full
/// GDI/window-message `run_capture_inner`/`capture_instant` paths, which need a live
/// desktop session this unit test does not assume.
#[test]
fn single_overlay_guard_blocks_a_second_concurrent_claim() {
    unsafe {
        let name = w!("SageThumbs2K.ShotOverlay.Single.UnitTest");
        let first = claim_single_overlay_slot(name);
        assert!(first.is_ok(), "first claim must succeed");
        let second = claim_single_overlay_slot(name);
        assert!(
            second.is_err(),
            "a second concurrent claim while the first is still held must be refused"
        );
        drop(first);
    }
}

#[test]
fn overlay_style_is_visible_to_windows_automation_without_taskbar_chrome() {
    let style = overlay_ex_style().0;
    assert_ne!(style & WS_EX_TOPMOST.0, 0);
    assert_ne!(style & WS_EX_NOACTIVATE.0, 0);
    assert_eq!(style & WS_EX_TOOLWINDOW.0, 0);
}

#[test]
fn automation_shift_latch_ors_with_physical_shift() {
    let mut state = automation_state();
    assert!(!effective_shift(false, None));
    assert!(effective_shift(true, None));
    assert!(!effective_shift(false, Some(&state)));
    assert!(effective_shift(true, Some(&state)));

    state.forced_shift = true;
    assert!(effective_shift(false, Some(&state)));
    assert!(effective_shift(true, Some(&state)));
}

/// Regression for a 150%-scaled display to the left of a 100% primary. Mouse
/// input and the backing bitmap are overlay-client coordinates, while
/// `MonitorFromRect` expects physical desktop coordinates. Feeding it the client
/// rect used to make a selection on the left display look as though it belonged to
/// the primary, so all selection chrome used the wrong DPI.
#[test]
fn mixed_dpi_selection_maps_client_geometry_to_the_virtual_desktop() {
    // Virtual desktop: 2560x1440 @ 150% on the left and 120 px above a
    // 1920x1080 @ 100% primary at (0, 0). The overlay starts at that origin.
    let selection = RECT {
        left: 320,
        top: 240,
        right: 1120,
        bottom: 840,
    };
    let desktop = client_rect_to_screen(selection, -2560, -120);

    assert_eq!(
        desktop,
        RECT {
            left: -2240,
            top: 120,
            right: -1440,
            bottom: 720,
        }
    );
    // Translation is only for the monitor query: the saved crop/layout stays
    // pixel-for-pixel in the overlay's client bitmap.
    assert_eq!(
        selection.right - selection.left,
        desktop.right - desktop.left
    );
    assert_eq!(
        selection.bottom - selection.top,
        desktop.bottom - desktop.top
    );
}

#[test]
fn automation_title_reports_post_paint_committed_geometry() {
    let mut state = automation_state();
    state.forced_shift = true;
    state.commit_gen = 2;
    state.painted_gen = 2;
    state.last_drag = Some(AutomationDrag {
        tool: Tool::Line,
        anchor: POINT { x: 150, y: 290 },
        raw: POINT { x: 350, y: 370 },
        final_point: POINT { x: 302, y: 442 },
        snapped: true,
    });

    assert_eq!(
        automation_title(&state),
        "SageThumbs 2K Screenshot Automation | snap=1 | commit=2 | painted=2 | status=ready | tool=Line | anchor=150,290 | raw=350,370 | final=302,442 | shifted=1"
    );
}
