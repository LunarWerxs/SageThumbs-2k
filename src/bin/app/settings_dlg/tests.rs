#![cfg(test)]

use super::*;

/// A093/A264: `sponsor_layout` used to open a wider gap above the footer
/// (`foot_y` 534 instead of 470) whenever `sponsors_enabled()` was true, to
/// reserve room for the banner it was about to create. That reservation is
/// pointless now that `build_controls` never creates ID_BANNER in the first
/// place (`navrail::V3_ALWAYS_HIDDEN` hides it on every page with no page
/// that un-hides it), so the footer position must be the single fixed
/// no-banner value regardless — this guards against a future edit
/// reintroducing a sponsor-state-dependent gap without also reinstating a
/// way to show the banner.
#[test]
fn sponsor_layout_never_reserves_room_for_the_never_shown_banner() {
    let layout = sponsor_layout(true);
    assert_eq!(
        layout.foot_y, 470,
        "footer must use the fixed no-banner spacing; ID_BANNER is never created"
    );
    assert_eq!(layout.credit_y, layout.foot_y + 6);
}

// ---- IDOK source-contract guard (2026-09-05 audit, F27 follow-up) ------------------

/// `commands.rs` (the WM_COMMAND handlers, `on_command_dialog` included) verbatim, embedded at compile time - same reasoning as `sync_client.rs`'s
/// `SETTINGS_SRC`: `include_str!` resolves relative to this file and is checked by the
/// compiler, so the scan below never depends on the working directory a test happens to
/// run from.
const MOD_SRC: &str = include_str!("commands.rs");

/// The `values::hotkey_conflict_decision`/`block_on_hotkey_conflict` tests prove the
/// CONFLICT MATH is right. Nothing proved the IDOK arm in `on_command_dialog` actually
/// calls it, or calls it in the right order - an inverted `if !block_on_hotkey_conflict
/// (hwnd)` (dropping the `!`), or a reordering that ran `apply_settings`/
/// `spawn_sync_push` unconditionally, would compile clean and pass every other test in
/// this repo. This is a dumb textual scan on the IDOK arm's own source, in the same
/// spirit as `sync_client.rs::settings_in_source` and `navrail`'s measurement tests: it
/// does not understand Rust, it just checks the names appear in the order Save requires.
#[test]
fn idok_arm_blocks_on_hotkey_conflict_before_apply_settings() {
    let start = MOD_SRC
        .find("IDOK => {")
        .expect("IDOK arm not found in commands.rs source - did on_command_dialog change shape?");
    let end = MOD_SRC[start..]
        .find("IDCANCEL =>")
        .map(|i| start + i)
        .expect("IDCANCEL arm not found after IDOK - on_command_dialog's match changed shape");
    let arm = &MOD_SRC[start..end];

    let guard_at = arm.find("if !block_on_hotkey_conflict(hwnd)").expect(
        "IDOK arm no longer guards Save on `if !block_on_hotkey_conflict(hwnd)` - the \
         call or its `!` negation may have been dropped, which would silently let Save \
         write a conflicting hotkey chord again",
    );
    let apply_at = arm
        .find("apply_settings(hwnd)")
        .expect("IDOK arm no longer calls apply_settings");
    let spawn_at = arm
        .find("spawn_sync_push(hwnd)")
        .expect("IDOK arm no longer calls spawn_sync_push");

    assert!(
        guard_at < apply_at,
        "block_on_hotkey_conflict must be checked BEFORE apply_settings in the IDOK arm"
    );
    assert!(
        guard_at < spawn_at,
        "block_on_hotkey_conflict must be checked BEFORE spawn_sync_push in the IDOK arm"
    );
    assert!(
        apply_at < spawn_at,
        "apply_settings must run before spawn_sync_push in the IDOK arm"
    );
}
