//! The user-assignable "action → hotkey" binding (Settings ▸ Screenshots). ONE global
//! hotkey, owned by the screenshot daemon, that fires one of a curated set of actions.
//!
//! Screen-wide actions (colour picker, screenshot, open Settings) need no input and run
//! right here in the spawned `--hotkey-action` helper process. The file actions (convert /
//! rotate / move-to-folder / strip metadata) act on the foreground Explorer selection,
//! falling back to a multi-select file picker when nothing is selected (see
//! [`crate::explorer_selection`]). The helper runs one action, then exits.

use windows::Win32::Foundation::HINSTANCE;

use sagethumbs2k_core::settings;
use sagethumbs2k_core::{run_action, Transform, VerbAction};

/// The curated actions offered in the Settings "Custom action" dropdown, in display order,
/// each paired with its STABLE persisted id (`settings::custom_action`). Keep ids stable —
/// they're stored in HKCU — so only ever APPEND new actions, never renumber. Id `1` (the
/// colour picker) is the default ([`settings::DEFAULT_CUSTOM_ACTION`]).
/// The second element is a LOCALE KEY, not a label: the dropdown is rebuilt on a live
/// language switch, so the text has to be resolved at display time rather than baked in
/// here. Use [`action_label`] to read one.
pub(crate) const ACTIONS: &[(u32, &str)] = &[
    (1, "act_eyedropper"),
    (2, "act_screenshot"),
    (3, "act_convert"),
    (4, "act_rotate"),
    (5, "act_folder"),
    (6, "act_strip"),
    (7, "act_settings"),
    (8, "act_upload"),
    (9, "act_ocr"),
    (10, "act_ocr_screen"),
];

/// The translated label for an [`ACTIONS`] row's locale key.
pub(crate) fn action_label(key: &str) -> &'static str {
    crate::win::t(key)
}

/// What a bound action does + what input it needs.
enum Kind {
    /// Screen-wide / no file target — runs in-process in this helper.
    Eyedropper,
    Screenshot,
    /// The capture overlay in OCR mode: drag a region, its text goes to the clipboard.
    ScreenOcr,
    OpenSettings,
    /// Operates on the selected IMAGE files (Explorer selection, else an images-only picker).
    ImageVerb(VerbAction),
    /// Operates on the selected files of ANY type (Explorer selection, else an all-files picker).
    AnyFileVerb(VerbAction),
}

/// Map a stored action id to its behaviour. `None` = an unknown/legacy id (do nothing).
fn kind_for(id: u32) -> Option<Kind> {
    Some(match id {
        1 => Kind::Eyedropper,
        2 => Kind::Screenshot,
        3 => Kind::ImageVerb(VerbAction::ConvertDialog),
        4 => Kind::ImageVerb(VerbAction::Transform(Transform::Right90)),
        5 => Kind::AnyFileVerb(VerbAction::FilesToFolder),
        6 => Kind::ImageVerb(VerbAction::StripMetadata),
        7 => Kind::OpenSettings,
        8 => Kind::ImageVerb(VerbAction::Upload),
        // OCR is headless-safe like StripMetadata: recognized text goes straight to the
        // clipboard, no window — a natural hotkey target.
        9 => Kind::ImageVerb(VerbAction::Ocr),
        // Same recognizer as 9, but the target is the SCREEN, not a selected file — so it
        // needs the capture overlay (in its no-editor OCR mode), not a file resolver.
        10 => Kind::ScreenOcr,
        _ => return None,
    })
}

/// Entry point for `--hotkey-action`: run whichever action the user bound to the custom hotkey.
pub(crate) unsafe fn run_hotkey_action(hinst: HINSTANCE) {
    let Some(kind) = kind_for(settings::custom_action()) else {
        return;
    };
    match kind {
        Kind::Eyedropper => crate::eyedropper::run_eyedropper(hinst),
        Kind::Screenshot => crate::screenshot::run_capture(hinst),
        Kind::ScreenOcr => crate::screenshot::run_capture_ocr(hinst),
        // Nothing to clean up if the Settings window fails to launch (no temp file was handed
        // over), so the spawn result is genuinely discardable here.
        Kind::OpenSettings => {
            let _ = crate::screenshot::spawn_self(&[]);
        }
        Kind::ImageVerb(action) => run_on_selection(action, true),
        Kind::AnyFileVerb(action) => run_on_selection(action, false),
    }
}

/// Resolve target files (Explorer selection, else a picker) and run the verb. No-op if the
/// user cancels the picker / there's nothing to act on.
unsafe fn run_on_selection(action: VerbAction, images_only: bool) {
    let paths = crate::explorer_selection::selection_or_pick(images_only);
    if paths.is_empty() {
        return;
    }
    // Surface the result like the DLL's detached path does — a failed rotate/strip via the
    // global hotkey otherwise gave zero feedback. No owner HWND here, so messages are top-level.
    let report = run_action(action, &paths);
    report.surface(None);
    report.reveal(&paths);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every dropdown row must actually DO something. Adding to [`ACTIONS`] without adding the
    /// matching [`kind_for`] arm leaves the user an option that silently no-ops, and nothing
    /// else catches it: the `run_hotkey_action` match stays exhaustive either way, so it builds
    /// clean. (Exactly the gap this test was written after hitting.)
    #[test]
    fn every_listed_action_has_a_behaviour() {
        for (id, label) in ACTIONS {
            assert!(
                kind_for(*id).is_some(),
                "action {id} ({label}) is offered in Settings but kind_for() ignores it"
            );
            assert!(!label.is_empty(), "action {id} has no label");
        }
    }

    /// The ids are persisted in HKCU, so they are APPEND-ONLY: renumbering silently repoints
    /// every existing user's bound hotkey at a different action. This pins both halves — no
    /// duplicates, and the list stays 1..=N in order, so a new entry can only go on the end.
    #[test]
    fn action_ids_are_unique_and_append_only() {
        let ids: Vec<u32> = ACTIONS.iter().map(|(id, _)| *id).collect();
        let expected: Vec<u32> = (1..=ids.len() as u32).collect();
        assert_eq!(
            ids, expected,
            "action ids must stay 1..=N in order (append only)"
        );
        assert!(kind_for(0).is_none());
        assert!(kind_for(ids.len() as u32 + 1).is_none());
    }

    /// The two OCR actions are different features: 9 reads a selected FILE, 10 reads the
    /// SCREEN via the capture overlay. Wiring 10 to a file verb would pop a file picker
    /// instead of the region overlay.
    #[test]
    fn screen_ocr_uses_the_capture_overlay_not_a_file_verb() {
        assert!(matches!(kind_for(10), Some(Kind::ScreenOcr)));
        assert!(matches!(
            kind_for(9),
            Some(Kind::ImageVerb(VerbAction::Ocr))
        ));
    }
}
