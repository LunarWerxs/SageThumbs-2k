#![cfg(test)]

use super::*;

/// A stored chord that isn't in `SHOT_PRESETS` must NOT resolve to the same
/// index as "nothing saved" — that collapse (both cases returning
/// `unwrap_or(0)`) is exactly what made Save silently replace a legacy/
/// foreign chord with preset 0.
#[test]
fn unknown_chord_does_not_collapse_to_the_unset_default() {
    // A value that was never one of the curated presets.
    let foreign = (0x07 << 8) | 0x99; // Ctrl+Shift+Alt + an odd VK
    assert!(SHOT_PRESETS.iter().all(|&(_, p)| p != foreign));
    assert_eq!(preset_index_for(foreign, 0), None);
}

/// A genuinely unset chord (0 — no hotkey saved yet, e.g. the quick-save
/// combo before the user ever touches it) still gets the caller's default.
#[test]
fn unset_chord_uses_the_caller_default() {
    assert_eq!(preset_index_for(0, 4), Some(4));
}

/// A stored chord that IS one of the curated presets resolves to that
/// preset's own index, never to `default_when_unset`.
#[test]
fn known_chord_resolves_to_its_own_preset_index() {
    for (i, &(_, packed)) in SHOT_PRESETS.iter().enumerate() {
        assert_eq!(preset_index_for(packed, 99), Some(i));
    }
}

#[test]
fn describe_unknown_chord_names_every_modifier() {
    assert_eq!(describe_unknown_chord(0x41), "Custom (VK 0x41)");
    assert_eq!(
        describe_unknown_chord((0x02 << 8) | 0x41),
        "Custom (Ctrl + VK 0x41)"
    );
    assert_eq!(
        describe_unknown_chord((0x07 << 8) | 0x41),
        "Custom (Ctrl + Shift + Alt + VK 0x41)"
    );
}
