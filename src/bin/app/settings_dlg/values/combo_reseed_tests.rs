#![cfg(test)]

use super::*;

/// `seed_combo_selections` re-derives every index from these pure functions — the actual
/// `HWND`/`SendMessageW` plumbing around them isn't unit-testable in-process (this module's
/// own end-to-end test is `#[ignore]`d for the same reason — CLAUDE.md §4), but the index
/// MATH is, and it's the part that would silently select the wrong entry if it drifted from
/// `build_controls`'s seeding (the bug this whole fix exists for: Import writing a value
/// nothing then re-selects, so a stale on-screen index gets read back and saved over it).
#[test]
fn shot_delay_index_maps_the_steps_and_degrades_to_off() {
    // Every offered step maps to its own slot...
    for (i, &secs) in settings::SHOT_DELAY_STEPS.iter().enumerate() {
        assert_eq!(shot_delay_combo_index(secs), i as u32);
    }
    // ...anything else (hand-edited registry) shows as Off rather than blanking the combo.
    assert_eq!(shot_delay_combo_index(4), 0);
    assert_eq!(shot_delay_combo_index(u32::MAX), 0);
    // And "Off" IS the getter's default: a fresh install and pressing Defaults agree.
    assert_eq!(settings::SHOT_DELAY_STEPS[0], 0);
}

#[test]
fn shot_tool_index_falls_back_out_of_range() {
    assert_eq!(shot_tool_combo_index(0), 0);
    assert_eq!(
        shot_tool_combo_index(settings::SHOT_TOOL_COUNT - 1),
        settings::SHOT_TOOL_COUNT - 1
    );
    // A hand-edited or stale registry value past the option count must degrade to the
    // SAME default `Tool::from_default_index` falls back to — not select nothing.
    assert_eq!(
        shot_tool_combo_index(settings::SHOT_TOOL_COUNT),
        settings::DEFAULT_SHOT_TOOL
    );
    assert_eq!(shot_tool_combo_index(u32::MAX), settings::DEFAULT_SHOT_TOOL);
}

#[test]
fn preset_index_matches_a_known_chord_and_falls_back_on_an_unknown_one() {
    let (_, known_packed) = SHOT_PRESETS[SHOT_PRESETS.len() - 1];
    assert_eq!(
        preset_combo_index(known_packed),
        SHOT_PRESETS.len() - 1,
        "must find the LAST preset, not just the first"
    );
    assert_eq!(preset_combo_index(0xFFFF_FFFF), 0);
}

#[test]
fn quick_hotkey_index_unbound_falls_back_to_the_noncolliding_default() {
    let expected = SHOT_PRESETS
        .iter()
        .position(|&(l, _)| l == QUICK_DEFAULT_LABEL)
        .expect("QUICK_DEFAULT_LABEL must be one of the presets");
    // Packed 0 means "no chord stored" — must land on the default preset, not "(none)"
    // (this combo has no such entry — every row is a real chord).
    assert_eq!(quick_hotkey_combo_index(0), expected);
    let (_, real_packed) = SHOT_PRESETS[0];
    assert_eq!(quick_hotkey_combo_index(real_packed), 0);
}

#[test]
fn custom_action_hk_index_unbound_is_the_none_entry_bound_is_offset_by_one() {
    // vk == 0 means unbound regardless of what `packed` happens to hold.
    assert_eq!(custom_action_hk_combo_index(0xABCD, 0), 0);
    let (_, real_packed) = SHOT_PRESETS[0];
    let real_vk = real_packed & 0xFF;
    assert_eq!(custom_action_hk_combo_index(real_packed, real_vk), 1);
    // An unrecognized-but-bound chord still falls back to "(none)" rather than panicking
    // or pointing at the wrong row.
    assert_eq!(custom_action_hk_combo_index(0xFFFF, 1), 0);
}

/// `note` must record the FIRST failure and stay recorded across later successes — a
/// settings write that fails silently used to vanish with `let _ = ...;` and no trace
/// anywhere; `apply_settings` now shows one message when ANY tracked write failed, so a
/// later success clearing the flag would hide the earlier failure from the user.
#[test]
fn note_records_a_failure_and_a_later_success_does_not_clear_it() {
    SAVE_FAILED.with(|f| f.set(false));
    let _ = note(Ok::<(), &str>(()));
    assert!(
        !SAVE_FAILED.with(|f| f.get()),
        "a success must not set the flag"
    );
    let _ = note(Err::<(), &str>("boom"));
    assert!(SAVE_FAILED.with(|f| f.get()), "a failure must set the flag");
    let _ = note(Ok::<(), &str>(()));
    assert!(
        SAVE_FAILED.with(|f| f.get()),
        "a later success must not clear an earlier failure"
    );
}
