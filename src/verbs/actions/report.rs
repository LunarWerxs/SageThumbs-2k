//! What a batch verb tells the user afterwards: counts, failures, and whether revealing the output is worth it.

use super::*;

/// Outcome of a dispatched verb so the Invoke callers can tell the user what
/// happened, instead of the old silent log-and-forget. Counts + one sample reason.
#[derive(Default)]
pub struct ActionReport {
    /// How many items the verb actually tried (images for image verbs, all files
    /// for file verbs, 1 for single-target verbs; 0 = nothing applicable).
    pub attempted: usize,
    /// How many succeeded.
    pub done: usize,
    /// A short human reason for the first failure (for the message box), if any.
    pub note: Option<String>,
    /// True when the verb handed off to the companion app / opened its own window
    /// (Convert dialog, Settings, eyedropper, multi-file Files-to-Folder,
    /// Tags-to-Folders, Image-info) — nothing to report inline; the app owns its UX.
    pub delegated: bool,
    /// The first NEW file a file-producing verb wrote (Convert / Resize / Rotate /
    /// Shrink-for-email). [`reveal`] selects it in Explorer on success so the user
    /// can see where the output landed (the verbs write a suffixed sibling that's
    /// easy to miss). `None` for verbs that write nothing / act in place.
    pub output: Option<PathBuf>,
}

impl ActionReport {
    /// The verb handed off to a window / companion app; nothing to surface inline.
    pub(super) fn delegated() -> Self {
        ActionReport {
            delegated: true,
            ..Default::default()
        }
    }

    /// A plain `attempted`/`done` report with no failure note (the caller adds one
    /// via [`with_note`] when there's a shortfall).
    pub(super) fn applied(attempted: usize, done: usize) -> Self {
        ActionReport {
            attempted,
            done,
            ..Default::default()
        }
    }

    /// Attach the first-failure reason (chained onto [`applied`]).
    pub(super) fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }

    /// How many items failed (attempted minus done, never underflowing).
    pub(super) fn failed(&self) -> usize {
        self.attempted.saturating_sub(self.done)
    }

    // (reveal noise-check lives at module scope as `reveal_is_noise` so it's unit-testable.)

    /// Show a result message to the user — ONLY when something failed. Silent on
    /// full success (don't nag), on delegated verbs, and on nothing-applicable.
    /// `parent` is the shell HWND (classic menu) or None (modern command).
    pub fn surface(&self, parent: Option<windows::Win32::Foundation::HWND>) {
        if self.delegated || self.attempted == 0 || self.failed() == 0 {
            return; // nothing went wrong (or there was nothing / a window owns it)
        }
        let failed = self.failed();
        let mut msg = format!("{} of {} items succeeded.", self.done, self.attempted);
        let plural = if failed == 1 { "" } else { "s" };
        match &self.note {
            Some(n) => msg.push_str(&format!("\n\n{failed} failed: {n}")),
            None => msg.push_str(&format!("\n\n{failed} item{plural} failed.")),
        }
        let t = crate::wide(&msg);
        let c = crate::wide("SageThumbs 2K");
        unsafe {
            MessageBoxW(
                parent,
                PCWSTR(t.as_ptr()),
                PCWSTR(c.as_ptr()),
                MB_OK | MB_ICONWARNING,
            );
        }
    }

    /// Select the produced file in Explorer so the user sees where it went —
    /// useful when a verb creates a NEW location (Files-to-folder /
    /// Sort-into-folders make subfolders the user wants to see).
    ///
    /// Fires ONLY on a clean full success with an [`output`](Self::output), and is
    /// suppressed for: delegated verbs, any failure (the message box leads there),
    /// `ST2K_NO_REVEAL` (tests / a user who finds it noisy), and — crucially — when
    /// the output landed in a folder a `source` is already in. The in-place verbs
    /// (Convert into ▸ WebP, Resize, Rotate…) write a sibling next to the file the
    /// user right-clicked, so they're already viewing that folder; popping a fresh
    /// Explorer window of it is just noise (reported as "Convert opens a folder").
    /// `explorer.exe /select,<path>` is the robust, COM-free reveal.
    pub fn reveal(&self, sources: &[String]) {
        if self.delegated || self.failed() > 0 || std::env::var_os("ST2K_NO_REVEAL").is_some() {
            return;
        }
        let Some(out) = self.output.as_ref() else {
            return;
        };
        if reveal_is_noise(out, sources) {
            return;
        }
        let _ = Command::new("explorer.exe")
            .raw_arg(format!("/select,\"{}\"", out.display()))
            .spawn();
    }
}

/// True when revealing `out` would just pop a redundant Explorer window: it's a
/// FILE sitting in a folder one of `sources` already lives in — i.e. an in-place
/// sibling from Convert/Resize/Rotate/Combine, which the user is already viewing.
/// Returns false for a verb that creates a NEW location (a directory output, or a
/// file inside a fresh subfolder) — those still reveal, since the user wants to
/// see the new folder. (Owner report: "Convert into WebP opens a folder.")
pub(super) fn reveal_is_noise(out: &std::path::Path, sources: &[String]) -> bool {
    if !out.is_file() {
        return false;
    }
    let Some(out_dir) = out.parent() else {
        return false;
    };
    sources
        .iter()
        .any(|s| std::path::Path::new(s).parent() == Some(out_dir))
}
