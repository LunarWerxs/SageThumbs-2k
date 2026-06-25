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
pub(crate) const ACTIONS: &[(u32, &str)] = &[
    (1, "Pick a colour (eyedropper)"),
    (2, "Take a screenshot"),
    (3, "Convert image(s)…"),
    (4, "Rotate image(s) right 90\u{00B0}"),
    (5, "Move file(s) into a new folder"),
    (6, "Strip image metadata"),
    (7, "Open SageThumbs 2K Settings"),
];

/// What a bound action does + what input it needs.
enum Kind {
    /// Screen-wide / no file target — runs in-process in this helper.
    Eyedropper,
    Screenshot,
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
        _ => return None,
    })
}

/// Entry point for `--hotkey-action`: run whichever action the user bound to the custom hotkey.
pub(crate) unsafe fn run_hotkey_action(hinst: HINSTANCE) {
    let Some(kind) = kind_for(settings::custom_action()) else { return };
    match kind {
        Kind::Eyedropper => crate::eyedropper::run_eyedropper(hinst),
        Kind::Screenshot => crate::screenshot::run_capture(hinst),
        Kind::OpenSettings => crate::screenshot::spawn_self(&[]),
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
