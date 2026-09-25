//! Two hotkey combos must never hold the same chord: the bindings, the conflict test and the refusal.

use super::*;

/// Which of the three hotkey-bearing controls a binding in [`conflicting_hotkeys`] came
/// from, so the caller can name it in the Save-blocking message (2026-09-05 audit, F27).
/// Kept out of the chord comparison itself, which only ever needs to know two chords are
/// equal, not what owns them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in super::super) enum HotkeyRole {
    /// The main capture hotkey (`ID_SHOT_HOTKEY`).
    Capture,
    /// The instant/quick-save hotkey (`ID_SHOT_QUICK_HOTKEY`). Only live while its checkbox
    /// is on AND the screenshot feature itself is enabled, same gate `register_configured_hotkey`
    /// applies (daemon.rs).
    QuickSave,
    /// The user-assignable custom-action hotkey (`ID_SHOT_ACTION_HK`). Registered whenever it
    /// has a non-zero chord, independent of the screenshot feature.
    CustomAction,
}

/// One hotkey-bearing control's CURRENT on-screen chord, read before Save folds an unchecked
/// "instant screenshot" box down to a stored `0` (see `apply_screenshot_hotkeys`). `enabled`
/// says whether this binding will actually be registered if Save proceeds, independent of
/// whatever chord its combo happens to be showing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in super::super) struct HotkeyBinding {
    pub(in super::super) role: HotkeyRole,
    pub(in super::super) enabled: bool,
    pub(in super::super) packed: u32,
}

/// Every pair of bindings that are both enabled, both non-zero, and share the same packed
/// chord. Before this existed, `apply_screenshot_hotkeys` wrote each of the three bindings
/// independently, so two functions could end up on the identical chord and save that way:
/// the daemon's sequential `RegisterHotKey` calls (`register_configured_hotkey`) would then
/// fail the LATER registration and record it as a bind failure the status line reports as
/// "another app", when the real cause was this dialog handing out one chord twice
/// (2026-09-05 audit, F27). Pure: it knows nothing about the daemon, so genuine external
/// contention (a different process already holding a chord) stays entirely the daemon's
/// concern, unaffected by this check.
pub(in super::super) fn conflicting_hotkeys(
    bindings: &[HotkeyBinding],
) -> Vec<(HotkeyRole, HotkeyRole)> {
    let mut conflicts = Vec::new();
    for i in 0..bindings.len() {
        let a = &bindings[i];
        if !a.enabled || a.packed == 0 {
            continue;
        }
        for b in &bindings[i + 1..] {
            if b.enabled && b.packed == a.packed {
                conflicts.push((a.role, b.role));
            }
        }
    }
    conflicts
}

/// The current selection's `CB_GETITEMDATA` packed chord for a hotkey combo, or `0` if
/// nothing is selected (`CB_ERR`). Every hotkey combo's items store the real packed chord
/// (curated preset, "(none)", or an appended unrecognized chord) as their item data, so this
/// mirrors exactly what `apply_screenshot_hotkeys` reads back on Save.
pub(super) unsafe fn combo_item_data(hwnd: HWND, id: i32) -> u32 {
    let Ok(c) = GetDlgItem(Some(hwnd), id) else {
        return 0;
    };
    let sel = SendMessageW(c, CB_GETCURSEL, None, None).0;
    if sel < 0 {
        return 0;
    }
    SendMessageW(c, CB_GETITEMDATA, Some(WPARAM(sel as usize)), None).0 as u32
}

/// Read the three hotkey-bearing controls' current on-screen state into [`HotkeyBinding`]s
/// for [`conflicting_hotkeys`] to check, ahead of Save actually writing anything.
pub(super) unsafe fn read_hotkey_bindings(hwnd: HWND) -> [HotkeyBinding; 3] {
    let shot_on = checked(hwnd, ID_SHOT_ENABLE);
    let quick_on = checked(hwnd, ID_SHOT_QUICK_ENABLE);
    let custom_packed = combo_item_data(hwnd, ID_SHOT_ACTION_HK);
    [
        HotkeyBinding {
            role: HotkeyRole::Capture,
            enabled: shot_on,
            packed: combo_item_data(hwnd, ID_SHOT_HOTKEY),
        },
        HotkeyBinding {
            role: HotkeyRole::QuickSave,
            enabled: shot_on && quick_on,
            packed: combo_item_data(hwnd, ID_SHOT_QUICK_HOTKEY),
        },
        HotkeyBinding {
            role: HotkeyRole::CustomAction,
            // Mirrors `register_configured_hotkey`: bound iff its own chord is non-zero,
            // with no separate enable flag of its own.
            enabled: custom_packed != 0,
            packed: custom_packed,
        },
    ]
}

/// Human-readable name for a [`HotkeyRole`] in the conflict message. The two fixed hotkeys
/// get a short locale label; the custom action's name depends on which action is currently
/// selected in its dropdown, so it is read live rather than hard-coded.
pub(super) unsafe fn hotkey_role_name(hwnd: HWND, role: HotkeyRole) -> String {
    match role {
        HotkeyRole::Capture => t("hotkey_name_capture").to_string(),
        HotkeyRole::QuickSave => t("hotkey_name_quick").to_string(),
        HotkeyRole::CustomAction => {
            let sel = GetDlgItem(Some(hwnd), ID_SHOT_ACTION)
                .map(|c| SendMessageW(c, CB_GETCURSEL, None, None).0.max(0) as usize)
                .unwrap_or(0);
            st2k_screenshot::hotkey::ACTIONS
                .get(sel)
                .map(|&(_, key)| st2k_screenshot::hotkey::action_label(key).to_string())
                .unwrap_or_else(|| t("lbl_custom_action").to_string())
        }
    }
}

/// The pure IDOK Save-blocking decision: given the bindings currently on screen, which two
/// roles collide (if any) and must refuse the whole Save. `None` means Save may proceed.
///
/// Factored out of [`block_on_hotkey_conflict`] on 2026-09-05 (audit F27 follow-up) so the
/// wiring the IDOK handler relies on - read bindings, DECIDE, show message - is three
/// separately testable steps instead of one opaque HWND-driven function. Before this split,
/// the only tests exercising the conflict math were `conflicting_hotkeys`'s own four; nothing
/// proved the decision built from its result actually reached the Save path, so an inverted
/// or dropped `if !block_on_hotkey_conflict(hwnd)` in `mod.rs` would have compiled clean and
/// passed every test that existed. Thin on purpose: it does no more than
/// `conflicting_hotkeys` already did, but naming the step lets it be called and asserted on
/// directly, with no HWND, from both this file's tests and (indirectly) the source-contract
/// test in `mod.rs` that checks the wiring is really there.
pub(in super::super) fn hotkey_conflict_decision(
    bindings: &[HotkeyBinding],
) -> Option<(HotkeyRole, HotkeyRole)> {
    conflicting_hotkeys(bindings).into_iter().next()
}

/// If the currently-selected hotkeys conflict (the same chord bound to two enabled
/// functions), tell the user which two and refuse to Save. A duplicate used to be written
/// as if both halves worked; the later `RegisterHotKey` would just fail silently, leaving one
/// function unreachable with no indication it was this dialog's own doing (2026-09-05 audit,
/// F27). Returns `true` when Save must be blocked (the caller shows the message and does not
/// call `apply_settings`).
pub(in super::super) unsafe fn block_on_hotkey_conflict(hwnd: HWND) -> bool {
    let bindings = read_hotkey_bindings(hwnd);
    let Some((a, b)) = hotkey_conflict_decision(&bindings) else {
        return false;
    };
    let msg = t("msg_hotkey_conflict")
        .replace("{a}", &hotkey_role_name(hwnd, a))
        .replace("{b}", &hotkey_role_name(hwnd, b));
    message_box(hwnd, &msg, "SageThumbs 2K");
    true
}
