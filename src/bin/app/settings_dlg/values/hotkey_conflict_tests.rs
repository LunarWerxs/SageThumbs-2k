#![cfg(test)]

use super::*;

/// Build one binding tersely for the table below.
fn b(role: HotkeyRole, enabled: bool, packed: u32) -> HotkeyBinding {
    HotkeyBinding {
        role,
        enabled,
        packed,
    }
}

/// Every combination of two (or three) enabled bindings sharing a chord must be flagged,
/// naming the actual pair(s), not just "a conflict exists somewhere" (2026-09-05 audit,
/// F27 acceptance: "all equal pairs among the bindings"). Before this function existed
/// nothing checked this at all, so two enabled bindings on the same chord saved as if
/// both would work.
#[test]
fn every_pair_of_enabled_bindings_sharing_a_chord_is_flagged() {
    use HotkeyRole::*;
    // (capture, quick, custom) packed chords -> expected conflicting pairs, in the
    // (i, j) order `conflicting_hotkeys` walks them.
    type Case = (u32, u32, u32, &'static [(HotkeyRole, HotkeyRole)]);
    let cases: &[Case] = &[
        (0x0203, 0x0203, 0x0450, &[(Capture, QuickSave)]),
        (0x0203, 0x0450, 0x0203, &[(Capture, CustomAction)]),
        (0x0450, 0x0203, 0x0203, &[(QuickSave, CustomAction)]),
        (
            0x0203,
            0x0203,
            0x0203,
            &[
                (Capture, QuickSave),
                (Capture, CustomAction),
                (QuickSave, CustomAction),
            ],
        ),
        (0x0203, 0x0450, 0x0999, &[]),
    ];
    for &(cap, quick, custom, expected) in cases {
        let bindings = [
            b(Capture, true, cap),
            b(QuickSave, true, quick),
            b(CustomAction, true, custom),
        ];
        assert_eq!(
            conflicting_hotkeys(&bindings),
            expected.to_vec(),
            "capture={cap:#06x} quick={quick:#06x} custom={custom:#06x}"
        );
    }
}

/// A binding held disabled (its checkbox off, e.g. "instant screenshot" unticked while its
/// combo still shows a leftover chord) can never actually register, so it must never be
/// reported as conflicting even while it shares a chord with something that IS enabled
/// (F27 acceptance: "a disabled action holding the same chord").
#[test]
fn a_disabled_binding_holding_the_same_chord_never_conflicts() {
    let bindings = [
        HotkeyBinding {
            role: HotkeyRole::Capture,
            enabled: true,
            packed: 0x0203,
        },
        HotkeyBinding {
            role: HotkeyRole::QuickSave,
            enabled: false, // "instant screenshot" unticked
            packed: 0x0203,
        },
        HotkeyBinding {
            role: HotkeyRole::CustomAction,
            enabled: true,
            packed: 0x0450,
        },
    ];
    assert!(conflicting_hotkeys(&bindings).is_empty());
}

/// A packed value of `0` means "unbound" for every one of these controls (the custom
/// action's "(none)" item, or a quick-save chord folded to 0 when its box is off) and must
/// never conflict with another `0`, even if both bindings are otherwise "enabled" (F27
/// acceptance: "zero/unbound values").
#[test]
fn a_zero_unbound_chord_never_conflicts_even_when_enabled() {
    let bindings = [
        HotkeyBinding {
            role: HotkeyRole::Capture,
            enabled: true,
            packed: 0,
        },
        HotkeyBinding {
            role: HotkeyRole::QuickSave,
            enabled: true,
            packed: 0,
        },
        HotkeyBinding {
            role: HotkeyRole::CustomAction,
            enabled: true,
            packed: 0,
        },
    ];
    assert!(conflicting_hotkeys(&bindings).is_empty());
}

/// A hand-edited or legacy chord outside the curated [`SHOT_PRESETS`] list (the same shape
/// `append_unknown_chord_item` gives its own combo row) still conflicts like any other
/// chord: the comparison is on the raw packed value, never on preset membership (F27
/// acceptance: "custom stored chords").
#[test]
fn a_stored_chord_outside_the_curated_presets_still_conflicts_if_shared() {
    let foreign = 0x0777;
    assert!(
        SHOT_PRESETS.iter().all(|&(_, p)| p != foreign),
        "test fixture must actually be outside the curated presets"
    );
    let bindings = [
        HotkeyBinding {
            role: HotkeyRole::Capture,
            enabled: true,
            packed: foreign,
        },
        HotkeyBinding {
            role: HotkeyRole::QuickSave,
            enabled: true,
            packed: 0x0450,
        },
        HotkeyBinding {
            role: HotkeyRole::CustomAction,
            enabled: true,
            packed: foreign,
        },
    ];
    assert_eq!(
        conflicting_hotkeys(&bindings),
        vec![(HotkeyRole::Capture, HotkeyRole::CustomAction)]
    );
}

/// The IDOK decision itself, not just the underlying chord math: with a conflicting pair
/// present, `hotkey_conflict_decision` must refuse (return `Some`) and identify BOTH
/// colliding roles, and each must actually be nameable for the message
/// `block_on_hotkey_conflict` builds from them (2026-09-05 audit, F27 follow-up - the gap
/// was that nothing proved the decision reached the message, only that the chord math was
/// right). `HWND::default()` is enough here because neither role in this case is
/// `CustomAction`, the only branch of `hotkey_role_name` that touches a real control.
#[test]
fn decision_refuses_and_both_roles_are_nameable_when_bindings_conflict() {
    let bindings = [
        b(HotkeyRole::Capture, true, 0x0203),
        b(HotkeyRole::QuickSave, true, 0x0203),
        b(HotkeyRole::CustomAction, true, 0x0450),
    ];
    let decision = hotkey_conflict_decision(&bindings);
    assert_eq!(decision, Some((HotkeyRole::Capture, HotkeyRole::QuickSave)));
    let (a, other) = decision.expect("checked above");
    let (name_a, name_b) = unsafe {
        (
            hotkey_role_name(HWND::default(), a),
            hotkey_role_name(HWND::default(), other),
        )
    };
    assert_eq!(name_a, t("hotkey_name_capture"));
    assert_eq!(name_b, t("hotkey_name_quick"));
    assert_ne!(name_a, name_b, "the message must name two DIFFERENT roles");
}

/// The mirror case: no enabled pair shares a chord, so the decision must let Save
/// proceed. Paired with the test above, this is the pair the F27 acceptance criteria
/// asked for directly: "(a) with a conflicting pair the decision refuses ... (b) with no
/// conflict it proceeds."
#[test]
fn decision_allows_save_to_proceed_when_bindings_do_not_conflict() {
    let bindings = [
        b(HotkeyRole::Capture, true, 0x0203),
        b(HotkeyRole::QuickSave, true, 0x0450),
        b(HotkeyRole::CustomAction, false, 0x0203), // disabled, so shares Capture's chord for free
    ];
    assert_eq!(hotkey_conflict_decision(&bindings), None);
}
