//! Verb dispatch: [`run_action`] maps a [`VerbAction`] over the selected paths,
//! plus the actions that don't belong to the encode / fileops primitives —
//! clipboard, wallpaper, the EXIF/audio batch-rename, set-as-folder-icon,
//! image-info, and the companion-app launchers (Convert…, Files-to-folder, …).
//!
//! ## Out-of-process dispatch for decode/encode-heavy verbs (crash isolation)
//!
//! The shell loads this code *inside* `explorer.exe` (and `dllhost.exe`). A
//! decode/encode of a hostile image is the one place a panic/UB can realistically
//! take the host down — and `panic=abort` means an abort here would kill Explorer,
//! not just our verb. The decode-heavy file verbs therefore prefer to run in the
//! throwaway **`st2k.exe`** helper that's installed next to our DLL: we spawn it
//! per file, *synchronously*, and only collect its exit status. If a malicious
//! file makes the engine abort, it kills that disposable child — Explorer is
//! untouched — and we simply count that file as failed.
//!
//! This is **strictly opt-in on the helper being present**: [`st2k_exe`] returns
//! the sibling `st2k.exe` only if it exists. When it's absent (unit/integration
//! tests, or a partial install where only the DLL got registered) we transparently
//! **fall back to the original in-process code path**, unchanged. A missing helper
//! can therefore never break a verb — it only forfeits the crash isolation. (This
//! is also what keeps `tests/explorer_command.rs::convert_verb_invoke_creates_file`
//! green: no `st2k.exe` sits next to the test binary, so Convert runs in-process
//! and still writes the file.)
//!
//! Routed verbs (helper-if-present): **Convert**, **Transform** (→ `rotate`),
//! **ResizeImg** (→ `convert --resize`), **ShrinkForEmail** (→ `convert --resize`),
//! **StripMetadata** (→ `strip`). Each maps cleanly to a `st2k` CLI verb that drives
//! the *same* engine (`decode_full` + the same convert/transform/strip code), so the
//! produced file is byte-identical and lands at the *same* auto-named path the
//! in-process verb would write — we compute that path and pass it to the CLI as
//! `<out>` where the verb takes one (`rotate`/`strip` auto-name in place, exactly
//! like their in-process twins, so they need no `<out>`).
//!
//! Deliberately **not** routed (kept in-process) — and *why*, since the task scoped
//! these as routing candidates:
//! - **Ocr**: the in-process verb places the recognized text on the *clipboard*
//!   (`ocr::ocr_to_clipboard`); the `st2k ocr` CLI prints to *stdout* and never
//!   touches the clipboard. The clipboard is shell state we can't reproduce from a
//!   child's stdout without reaching into `ocr.rs` (a file this task doesn't own),
//!   so routing would change the observable result — kept in-process to preserve it.
//! - **CombineToPdf**: the in-process path encodes pages at the user's saved JPEG
//!   quality (`settings::jpeg_quality()`); `st2k pdf` has no quality flag and
//!   hard-codes 85, so the bytes would diverge whenever the setting ≠ 85. The
//!   "identical output" guarantee can't hold, so it stays in-process.
//! - Clipboard / Wallpaper / SetFolderIcon (touch shell/desktop state),
//!   CombineToCbz (no CLI verb), and the info/sort/rename/dialog/settings/eyedropper
//!   verbs (UI or pure file moves, not decode-heavy) — never in scope.
//!
//! Crucially, the [`ActionReport`] returned is **identical** between the routed and
//! the fallback path: a routed per-file success increments `done` exactly as an
//! `Ok(())` from the in-process call would, the `attempted` denominators and the
//! first-failure `note`s are unchanged, and `delegated` is never set by routing.
//! Callers can't tell which path ran.
//!
//! Output identity: `rotate`/`strip` route to the *same functions* the in-process
//! verbs call (`transform_file` / `strip_metadata`), so their files are byte-for-byte
//! identical; `ShrinkForEmail` is always a quality-82 JPEG (no `png_level` involved),
//! also byte-identical. `Convert`/`ResizeImg` **to a PNG** are now byte-identical too:
//! `encode::convert_to` (the CLI/helper path) reads the saved `settings::png_level()`
//! (default 9) for the zlib level — the SAME level the in-process `convert_file` /
//! `resize_file` use — so the routed and in-process outputs match. (It used to pin
//! level 6 here, so a PNG output diverged in byte size whenever the setting ≠ 6.)

use core::ffi::c_void;
use std::iter::once;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use image::{DynamicImage, ImageFormat};
use windows::core::{Error, Result, PCWSTR};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Graphics::Gdi::BITMAPINFOHEADER;
use windows::Win32::Storage::FileSystem::{
    GetFileAttributesW, SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_READONLY,
    FILE_ATTRIBUTE_SYSTEM, FILE_FLAGS_AND_ATTRIBUTES,
};
use windows::Win32::UI::Shell::{SHChangeNotify, SHCNE_UPDATEDIR, SHCNF_PATHW};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, MessageBoxW, SystemParametersInfoW, MB_ICONWARNING, MB_OK,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SPIF_SENDCHANGE, SPIF_UPDATEINIFILE, SPI_SETDESKWALLPAPER,
};

use super::encode::{
    predict_unique_suffix, read_capped, reserve_unique_suffix, resize_file, shrink_for_email,
    transform_file, with_tmp_suffix, Resize, Target,
};
use super::fileops::{
    combine_to_cbz, combined_path, files_to_folder, reserve_dest, sanitize_component,
    sort_by_dimensions,
};
use super::menu::{EmailSize, RenamePattern, Transform, VerbAction, WallpaperMode};
use crate::decode;

// Don't flash a console window when we spawn `st2k.exe` from the shell host
// (`explorer.exe`/`dllhost.exe` are GUI processes — a child console would pop).
use crate::CREATE_NO_WINDOW;

/// The `st2k.exe` CLI helper that ships next to our DLL, if it's actually there.
///
/// The installer drops `st2k.exe` in the same directory as `sagethumbs2k.dll`, so
/// we resolve it from the DLL's OWN path ([`crate::module_path`]) — **never**
/// `current_exe()`, which in the shell host is `explorer.exe`/`dllhost.exe`. Returns
/// `Some` only when the file exists; `None` (helper missing — tests, or a DLL-only
/// install) makes every routed verb fall back to its in-process path. See the
/// module docs for the rationale.
fn st2k_exe() -> Option<PathBuf> {
    crate::sibling_of_dll(crate::CLI_EXE)
}

/// Outcome of a routed `st2k` helper run. The three cases are deliberately
/// distinct: a clean exit, a per-file failure (the child ran but reported an error
/// or crashed/aborted on this one file), and a SPAWN failure (the helper couldn't
/// even start — missing/corrupt/arch-mismatched exe). They must not be conflated:
/// a spawn failure breaks EVERY routed verb identically, so the caller degrades to
/// its in-process path instead of failing all files silently.
enum RunOutcome {
    /// Exited 0 — the file was produced exactly as the in-process call would have.
    Ok,
    /// The child ran but failed (non-zero exit / crash / abort) on this file.
    Failed,
    /// The child could not be spawned at all — the helper itself is broken.
    SpawnFailed,
}

/// Run the `st2k` CLI helper synchronously with the given args, no console window.
/// stdin is unused; stdout/stderr are dropped — the routed verbs communicate only
/// through the file they write + the exit code. A spawn error is logged once here
/// (it's a routing-level problem, not a per-file one) and surfaced as
/// [`RunOutcome::SpawnFailed`] so the caller can fall back to in-process.
fn run_st2k(exe: &Path, args: &[&str]) -> RunOutcome {
    match Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status()
    {
        Ok(s) if s.success() => RunOutcome::Ok,
        Ok(_) => RunOutcome::Failed,
        Err(e) => {
            crate::safety::log(&format!(
                "st2k helper FAILED TO SPAWN ({e}) — routing this verb in-process instead"
            ));
            RunOutcome::SpawnFailed
        }
    }
}

/// Render a [`Resize`] preset into the `--resize WxH|N%` syntax the CLI accepts
/// (the inverse of `cli::parse_resize`), or `None` when there's nothing to pass.
/// `Fit`/`FitUp` both serialize to `WxH`; the CLI's `convert` always fits-without-
/// upscale, which matches the resize/email presets (they never upscale either).
fn resize_arg(r: Resize) -> Option<String> {
    match r {
        Resize::None => None,
        Resize::Fit(w, h) | Resize::FitUp(w, h) => Some(format!("{w}x{h}")),
        Resize::Percent(p) => Some(format!("{p}%")),
    }
}

/// Convert one file. Routes to `st2k convert <in> <out> --quality Q
/// [--webp-quality Q]` when the helper is present (computing the SAME `<out>`
/// `convert_file` would pick, and passing `target.webp_quality` so a lossy-WebP
/// verb stays lossy out-of-process), else falls back to the in-process
/// `convert_file`. Returns whether the file was
/// produced — drop-in for the `convert_file(p, target).is_ok()` predicate. Logs
/// failures (with the error detail on the in-process path) like the originals did.
fn convert_one(exe: Option<&Path>, p: &str, target: Target) -> Option<PathBuf> {
    match exe {
        Some(exe) => {
            // Reserve the SAME collision-free destination `convert_file` would pick
            // (atomic `create_new` placeholder, so parallel workers — across the
            // st2k processes too — never claim one name twice), then have the routed
            // CLI write exactly there. The slot is held across the run: on success
            // it keeps the (now non-empty) file, on failure its drop removes the
            // still-empty placeholder. (On the rare spawn-failure fallback the slot
            // still exists, so the in-process retry picks `(1)` — a cosmetic edge in
            // an almost-never path.)
            let slot = super::encode::unique_output(Path::new(p), target.ext);
            let Some(out_s) = slot.path().to_str() else {
                return convert_one(None, p, target);
            };
            let q = crate::settings::jpeg_quality().to_string();
            let mut args = vec!["convert", p, out_s, "--quality", q.as_str()];
            // Lossy WebP (the quick WebP verb): pass the same quality the in-process
            // `convert_file` would use via `target.webp_quality`, so the routed file
            // matches. `wq` outlives `args` (borrowed as &str below).
            let wq;
            if let Some(w) = target.webp_quality {
                wq = w.to_string();
                args.push("--webp-quality");
                args.push(wq.as_str());
            }
            match run_st2k(exe, &args) {
                RunOutcome::Ok => Some(slot.path().to_path_buf()),
                RunOutcome::Failed => {
                    crate::safety::log(&format!("Convert (st2k) failed for {p}"));
                    None
                }
                RunOutcome::SpawnFailed => convert_one(None, p, target),
            }
        }
        None => match super::encode::convert_file(p, target) {
            Ok(out) => Some(out),
            Err(e) => {
                crate::safety::log(&format!("Convert failed for {p}: {e:?}"));
                None
            }
        },
    }
}

/// Rotate/flip one file. Routes to `st2k rotate <in> --by …` (which auto-names the
/// `<stem> (edited).<ext>` sibling itself, via the same `transform_file`), else
/// falls back to in-process `transform_file`.
fn transform_one(exe: Option<&Path>, p: &str, t: Transform) -> Option<PathBuf> {
    match exe {
        Some(exe) => {
            let by = match t {
                Transform::Right90 => "right",
                Transform::Left90 => "left",
                Transform::Rotate180 => "180",
                Transform::FlipH => "fliph",
                Transform::FlipV => "flipv",
            };
            // `st2k rotate` auto-names the `<stem> (edited).<ext>` sibling itself
            // (same `transform_file`). Predict that name BEFORE the run (while it's
            // still free) so it matches what st2k picks — recomputing afterwards
            // would see the new file and pick `(edited 2)` instead.
            let src = Path::new(p);
            let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("png").to_ascii_lowercase();
            // PREDICT (read-only) the name `st2k rotate` will auto-pick — do NOT
            // reserve it, or st2k's own picker would see our placeholder and bump to
            // `(edited 2)`. Rotate names derive from the distinct source stem, so
            // parallel rotates of a selection don't collide on the prediction.
            let predicted = predict_unique_suffix(src, "edited", &ext);
            match run_st2k(exe, &["rotate", p, "--by", by]) {
                RunOutcome::Ok => Some(predicted),
                RunOutcome::Failed => {
                    crate::safety::log(&format!("Transform (st2k) failed for {p}"));
                    None
                }
                RunOutcome::SpawnFailed => transform_one(None, p, t),
            }
        }
        None => transform_file(p, t).ok(),
    }
}

/// Resize one file. Routes to `st2k convert <in> <out> --resize …`, computing the
/// SAME `<stem> (resized).<ext>` sibling (and source format) that `resize_file`
/// writes, else falls back to in-process `resize_file`.
fn resize_one(exe: Option<&Path>, p: &str, r: Resize) -> Option<PathBuf> {
    match exe {
        Some(exe) => {
            let src = Path::new(p);
            let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("png").to_ascii_lowercase();
            let slot = reserve_unique_suffix(src, "resized", &ext);
            let (Some(out_s), Some(rs)) = (slot.path().to_str(), resize_arg(r)) else {
                return resize_one(None, p, r);
            };
            let q = crate::settings::jpeg_quality().to_string();
            match run_st2k(exe, &["convert", p, out_s, "--quality", &q, "--resize", &rs]) {
                RunOutcome::Ok => Some(slot.path().to_path_buf()),
                RunOutcome::Failed => {
                    crate::safety::log(&format!("Resize (st2k) failed for {p}"));
                    None
                }
                RunOutcome::SpawnFailed => resize_one(None, p, r),
            }
        }
        None => match resize_file(p, r) {
            Ok(out) => Some(out),
            Err(e) => {
                crate::safety::log(&format!("Resize failed for {p}: {e:?}"));
                None
            }
        },
    }
}

/// Shrink one file for email. Routes to `st2k convert <in> <out> --resize ExE`
/// onto the SAME `<stem> (email).jpg` sibling at the email JPEG quality, else falls
/// back to in-process `shrink_for_email`. (The CLI `convert` flattens onto white
/// for JPEG just like the in-process path, so the bytes match.)
fn shrink_one(exe: Option<&Path>, p: &str, size: EmailSize) -> Option<PathBuf> {
    match exe {
        Some(exe) => {
            let src = Path::new(p);
            let slot = reserve_unique_suffix(src, "email", "jpg");
            let Some(out_s) = slot.path().to_str() else {
                return shrink_one(None, p, size);
            };
            let edge = size.max_edge();
            let resize = format!("{edge}x{edge}");
            // EMAIL_JPEG_QUALITY (82) is private to encode.rs; pass the same literal.
            match run_st2k(exe, &["convert", p, out_s, "--quality", "82", "--resize", &resize]) {
                RunOutcome::Ok => Some(slot.path().to_path_buf()),
                RunOutcome::Failed => {
                    crate::safety::log(&format!("Shrink for email (st2k) failed for {p}"));
                    None
                }
                RunOutcome::SpawnFailed => shrink_one(None, p, size),
            }
        }
        None => match shrink_for_email(p, size) {
            Ok(out) => Some(out),
            Err(e) => {
                crate::safety::log(&format!("Shrink for email failed for {p}: {e:?}"));
                None
            }
        },
    }
}

/// Strip metadata from one file in place. Routes to `st2k strip <in>` (same
/// `strip::strip_metadata`), else falls back to the in-process call.
fn strip_one(exe: Option<&Path>, p: &str) -> bool {
    match exe {
        Some(exe) => match run_st2k(exe, &["strip", p]) {
            RunOutcome::Ok => true,
            RunOutcome::Failed => {
                crate::safety::log(&format!("Strip metadata (st2k) failed for {p}"));
                false
            }
            RunOutcome::SpawnFailed => strip_one(None, p),
        },
        None => match crate::strip::strip_metadata(p) {
            Ok(()) => true,
            Err(e) => {
                crate::safety::log(&format!("Strip metadata failed for {p}: {e:?}"));
                false
            }
        },
    }
}

/// Does `path` have an extension we can decode? A cheap extension-only gate
/// shared by both menu surfaces (classic `IContextMenu` + modern
/// `IExplorerCommand`) so the verbs only appear/act on supported images.
/// Generic archives (.zip/.rar/.7z) are EXCLUDED even though they're registered
/// formats: they thumbnail/preview, but the image verbs would act on the
/// extracted cover, not the archive — Convert on a zip yielding a PNG of its
/// first photo reads as broken, so archives get no verb menu.
pub fn is_image(path: &str) -> bool {
    // `is_known` is ASCII-case-insensitive, so no lowercase allocation here (this
    // runs per selected path on every right-click).
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| crate::formats::is_known(e) && !crate::formats::is_archive(e))
}

/// Does `path` have an audio extension (one we read tags from)? Gates the
/// audio-only verbs (rename-by-tag dispatch, Tags→Folders) and the audio-only
/// menu views on both surfaces (`contextmenu.rs` / `command.rs`).
pub fn is_audio(path: &str) -> bool {
    match std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
    {
        Some(ext) => crate::formats::category(&ext) == crate::formats::Category::Audio,
        None => false,
    }
}

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
    fn delegated() -> Self {
        ActionReport { delegated: true, ..Default::default() }
    }

    /// A plain `attempted`/`done` report with no failure note (the caller adds one
    /// via [`with_note`] when there's a shortfall).
    fn applied(attempted: usize, done: usize) -> Self {
        ActionReport { attempted, done, ..Default::default() }
    }

    /// Attach the first-failure reason (chained onto [`applied`]).
    fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }

    /// How many items failed (attempted minus done, never underflowing).
    fn failed(&self) -> usize {
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
            MessageBoxW(parent, PCWSTR(t.as_ptr()), PCWSTR(c.as_ptr()), MB_OK | MB_ICONWARNING);
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
        let Some(out) = self.output.as_ref() else { return };
        if reveal_is_noise(out, sources) {
            return;
        }
        let _ = Command::new("explorer.exe").raw_arg(format!("/select,\"{}\"", out.display())).spawn();
    }
}

/// True when revealing `out` would just pop a redundant Explorer window: it's a
/// FILE sitting in a folder one of `sources` already lives in — i.e. an in-place
/// sibling from Convert/Resize/Rotate/Combine, which the user is already viewing.
/// Returns false for a verb that creates a NEW location (a directory output, or a
/// file inside a fresh subfolder) — those still reveal, since the user wants to
/// see the new folder. (Owner report: "Convert into WebP opens a folder.")
fn reveal_is_noise(out: &std::path::Path, sources: &[String]) -> bool {
    if !out.is_file() {
        return false;
    }
    let Some(out_dir) = out.parent() else { return false };
    sources
        .iter()
        .any(|s| std::path::Path::new(s).parent() == Some(out_dir))
}

/// Run a context-menu action on a DETACHED worker thread, then surface any error and
/// reveal new-folder output — so the shell's `IContextMenu::InvokeCommand` /
/// `IExplorerCommand::Invoke` returns immediately instead of blocking explorer.exe's UI
/// thread for the (possibly many-file, many-second) batch. The worker holds a
/// [`crate::ModuleRef`] (so the DLL can't unload mid-action) and initializes its own STA
/// COM apartment (verbs may touch WIC / the shell); it owns clones of every input, so it
/// keeps NO reference to the COM object that launched it. `owner` is the parent HWND (as
/// `isize`) for the error MessageBox, or `None`.
pub fn run_action_detached(action: VerbAction, paths: Vec<String>, owner: Option<isize>) {
    let _ = std::thread::Builder::new().name("st2k-verb".into()).spawn(move || {
        // Keep the DLL pinned for the action's lifetime (a detached thread outlives the
        // Invoke call that spawned it). `ModuleRef::default()` is NOT a no-op — its `Default`
        // impl does the `dll_add_ref()`; clippy's "use `ModuleRef`" suggestion would skip it.
        #[allow(clippy::default_constructed_unit_structs)]
        let _module = crate::ModuleRef::default();
        // STA matches the shell thread the verb used to run on (ShellExecute / clipboard /
        // WIC all behave there). S_OK / S_FALSE add a ref to balance; RPC_E_CHANGED_MODE
        // (already an MTA thread) does not, so only CoUninitialize when we actually inited.
        let inited = unsafe {
            windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            )
        }
        .is_ok();
        let report = run_action(action, &paths);
        let parent = owner.map(|h| windows::Win32::Foundation::HWND(h as *mut core::ffi::c_void));
        report.surface(parent);
        report.reveal(&paths);
        if inited {
            unsafe { windows::Win32::System::Com::CoUninitialize() };
        }
    });
}

/// Dispatch a verb over the selected paths (best-effort). Returns an
/// [`ActionReport`] the Invoke callers surface to the user on failure.
pub fn run_action(action: VerbAction, paths: &[String]) -> ActionReport {
    match action {
        VerbAction::Convert(target) => {
            // Counts over ALL paths (no image filter), so the attempted count matches
            // its denominator. Each file is converted on the batch pool (routed to the
            // st2k helper per file for crash isolation when present, else in-process —
            // `convert_one(None, …)` IS `convert_file`). Results come back IN ORDER, so
            // the first success matches the old first-in-iteration reveal target. The
            // global magick cap bounds memory across the fanned-out st2k children.
            let exe = st2k_exe();
            let exe_ref = exe.as_deref();
            let outs: Vec<PathBuf> = crate::parallel::map(paths, |_, p| convert_one(exe_ref, p, target))
                .into_iter()
                .flatten()
                .collect();
            let n = outs.len();
            let first = outs.into_iter().next();
            let mut r = if n < paths.len() {
                crate::safety::log(&format!("Convert to {}: only {}/{} succeeded", target.ext, n, paths.len()));
                ActionReport::applied(paths.len(), n).with_note("conversion failed for some files")
            } else {
                ActionReport::applied(paths.len(), n)
            };
            r.output = first;
            r
        }
        VerbAction::Transform(t) => {
            // Routed per file to `st2k rotate` on the batch pool (else in-process
            // `transform_file`); `transform_one` returns the produced path, so the
            // ordered results give the same count + first-reveal as the old loop.
            let exe = st2k_exe();
            let exe_ref = exe.as_deref();
            let outs: Vec<PathBuf> = crate::parallel::map(paths, |_, p| transform_one(exe_ref, p, t))
                .into_iter()
                .flatten()
                .collect();
            let n = outs.len();
            let first = outs.into_iter().next();
            let mut r = if n < paths.len() {
                crate::safety::log(&format!("Transform: only {}/{} succeeded", n, paths.len()));
                ActionReport::applied(paths.len(), n).with_note("rotate/flip failed for some files")
            } else {
                ActionReport::applied(paths.len(), n)
            };
            r.output = first;
            r
        }
        VerbAction::Clipboard => {
            // Clipboard holds one image. Use the first *image* in the selection
            // (not paths.first()): the menu gate only requires *some* image, so
            // for a mixed selection the first item may be a non-image.
            match paths.iter().find(|p| is_image(p.as_str())) {
                Some(p) => match copy_to_clipboard(p) {
                    Ok(()) => ActionReport::applied(1, 1),
                    Err(e) => {
                        crate::safety::log(&format!("Copy to clipboard failed for {p}: {e:?}"));
                        ActionReport::applied(1, 0).with_note("couldn't decode or copy the image")
                    }
                },
                None => ActionReport::default(),
            }
        }
        VerbAction::Upload => {
            // Upload the selected image(s) to the keyless host in the companion app,
            // which copies the resulting link(s) to the clipboard. The originals are
            // never modified; the app owns the network + result UX (delegated).
            launch_upload(paths);
            ActionReport::delegated()
        }
        VerbAction::Wallpaper(mode) => {
            // One wallpaper. Use the first *image* in the selection (see above).
            match paths.iter().find(|p| is_image(p.as_str())) {
                Some(p) => {
                    crate::safety::log_debug(&format!("Set wallpaper: using {p}"));
                    match set_wallpaper(p, mode) {
                        Ok(()) => ActionReport::applied(1, 1),
                        Err(e) => {
                            crate::safety::log(&format!("Set wallpaper failed for {p}: {e:?}"));
                            ActionReport::applied(1, 0).with_note("couldn't set the wallpaper")
                        }
                    }
                }
                None => ActionReport::default(),
            }
        }
        VerbAction::CombineToPdf => {
            let imgs: Vec<String> = paths.iter().filter(|p| is_image(p.as_str())).cloned().collect();
            if imgs.is_empty() {
                return ActionReport::default();
            }
            let out = combined_pdf_path(&imgs[0]);
            match crate::topdf::combine_to_pdf(&imgs, &out, crate::settings::jpeg_quality()) {
                Ok(_) => ActionReport { output: Some(out), ..ActionReport::applied(1, 1) },
                Err(e) => {
                    crate::safety::log(&format!("Combine to PDF failed: {e:?}"));
                    ActionReport::applied(1, 0).with_note("couldn't build the PDF")
                }
            }
        }
        VerbAction::CombineToCbz => {
            let imgs: Vec<String> = paths.iter().filter(|p| is_image(p.as_str())).cloned().collect();
            if imgs.is_empty() {
                return ActionReport::default();
            }
            let out = combined_path(&imgs[0], "cbz");
            match combine_to_cbz(&imgs, &out) {
                Ok(()) => ActionReport { output: Some(out), ..ActionReport::applied(1, 1) },
                Err(e) => {
                    crate::safety::log(&format!("Combine to CBZ failed: {e:?}"));
                    ActionReport::applied(1, 0).with_note("couldn't build the CBZ archive")
                }
            }
        }
        VerbAction::Ocr => {
            match paths.iter().find(|p| is_image(p.as_str())) {
                Some(p) => match crate::ocr::ocr_to_clipboard(p) {
                    Ok(()) => ActionReport::applied(1, 1),
                    Err(e) => {
                        crate::safety::log(&format!("OCR failed for {p}: {e:?}"));
                        ActionReport::applied(1, 0).with_note("couldn't read text from the image")
                    }
                },
                None => ActionReport::default(),
            }
        }
        VerbAction::ImageInfo => {
            // Opens its own info window (a message box) — the app owns the UX.
            if let Some(p) = paths.iter().find(|p| is_image(p.as_str())) {
                show_info(p);
            }
            ActionReport::delegated()
        }
        VerbAction::StripMetadata => {
            // Per-image, on the batch pool. Routed per file to `st2k strip`
            // (helper-if-present), else in-process `strip::strip_metadata`; `strip_one`
            // returns the same success bool, so attempted/done/note are identical to
            // the old sequential loop.
            let exe = st2k_exe();
            let exe_ref = exe.as_deref();
            let imgs: Vec<String> = paths.iter().filter(|p| is_image(p.as_str())).cloned().collect();
            let oks = crate::parallel::map(&imgs, |_, p| strip_one(exe_ref, p));
            let attempted = imgs.len();
            let done = oks.iter().filter(|&&ok| ok).count();
            let mut r = ActionReport::applied(attempted, done);
            if done < attempted {
                r.note = Some("couldn't rewrite the file without metadata".into());
            }
            r
        }
        VerbAction::ConvertDialog => {
            launch_convert_dialog(paths);
            ActionReport::delegated()
        }
        VerbAction::OpenSettings => {
            launch_app(&[]);
            ActionReport::delegated()
        }
        VerbAction::ResizeImg(r) => {
            // Per-image, on the batch pool. Routed per file to `st2k convert --resize`
            // (helper-if-present), else in-process `resize_file`; ordered results give
            // the same attempted/done/note + first-reveal as the old loop.
            let exe = st2k_exe();
            let exe_ref = exe.as_deref();
            let imgs: Vec<String> = paths.iter().filter(|p| is_image(p.as_str())).cloned().collect();
            let outs: Vec<PathBuf> = crate::parallel::map(&imgs, |_, p| resize_one(exe_ref, p, r))
                .into_iter()
                .flatten()
                .collect();
            let attempted = imgs.len();
            let done = outs.len();
            let first = outs.into_iter().next();
            let mut rep = ActionReport::applied(attempted, done);
            if done < attempted {
                rep.note = Some("couldn't resize some images".into());
            }
            rep.output = first;
            rep
        }
        VerbAction::ShrinkForEmail(size) => {
            // Per-image, on the batch pool. Routed per file to `st2k convert --resize`
            // (helper-if-present), else in-process `shrink_for_email`; ordered results
            // give the same attempted/done/note + first-reveal as the old loop.
            let exe = st2k_exe();
            let exe_ref = exe.as_deref();
            let imgs: Vec<String> = paths.iter().filter(|p| is_image(p.as_str())).cloned().collect();
            let outs: Vec<PathBuf> = crate::parallel::map(&imgs, |_, p| shrink_one(exe_ref, p, size))
                .into_iter()
                .flatten()
                .collect();
            let attempted = imgs.len();
            let done = outs.len();
            let first = outs.into_iter().next();
            let mut rep = ActionReport::applied(attempted, done);
            if done < attempted {
                rep.note = Some("couldn't shrink some images".into());
            }
            rep.output = first;
            rep
        }
        VerbAction::RenameByExif(pattern) => rename_by_exif(paths, pattern),
        VerbAction::SetFolderIcon => {
            // One folder icon. Use the first *image* in the selection.
            match paths.iter().find(|p| is_image(p.as_str())) {
                Some(p) => match set_folder_icon(p) {
                    Ok(()) => ActionReport::applied(1, 1),
                    Err(e) => {
                        crate::safety::log(&format!("Set folder icon failed for {p}: {e:?}"));
                        ActionReport::applied(1, 0).with_note("couldn't set the folder icon")
                    }
                },
                None => ActionReport::default(),
            }
        }
        VerbAction::Eyedropper => {
            // A system-wide screen color picker (the selected file is irrelevant).
            let _ = paths;
            launch_app(&["--eyedropper"]);
            ActionReport::delegated()
        }
        VerbAction::FilesToFolder => {
            // Operates on ALL selected files (any type), not just images. One file
            // → a folder named after it (no prompt); many → the name-prompt dialog
            // in the companion app.
            match paths.len() {
                0 => ActionReport::default(),
                1 => {
                    let stem = Path::new(&paths[0])
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("New Folder");
                    match files_to_folder(paths, stem) {
                        Ok(_) => ActionReport::applied(1, 1),
                        Err(e) => {
                            crate::safety::log(&format!("Files to folder failed: {e:?}"));
                            ActionReport::applied(1, 0).with_note("couldn't create or fill the folder")
                        }
                    }
                }
                _ => {
                    launch_files_to_folder(paths);
                    ActionReport::delegated()
                }
            }
        }
        VerbAction::SortByDimensions => {
            let (moved, skipped) = sort_by_dimensions(paths);
            if skipped > 0 {
                crate::safety::log(&format!(
                    "Sort by dimensions: {moved} moved, {skipped} skipped (couldn't read size / move)"
                ));
                ActionReport::applied(moved + skipped, moved)
                    .with_note(format!("{skipped} couldn't be read or moved"))
            } else {
                ActionReport::applied(moved + skipped, moved)
            }
        }
        VerbAction::TagsToFolders => {
            // Audio-only; the dialog (destination/template/copy-move) lives in the
            // companion app. No audio in the selection → nothing to do.
            let audio: Vec<String> = paths.iter().filter(|p| is_audio(p.as_str())).cloned().collect();
            if audio.is_empty() {
                ActionReport::default()
            } else {
                launch_tags_to_folders(&audio);
                ActionReport::delegated()
            }
        }
    }
}

/// Launch the companion EXE with no arguments → the Options/Settings window.
/// Resolves the EXE from the DLL's own directory (host-process-safe).
fn launch_app(args: &[&str]) {
    // A failed launch used to vanish without a trace — the menu item just "did nothing"
    // (missing companion EXE on a broken install, or spawn failure). Log it so the
    // Diagnostics log at least explains a dead menu item.
    let Some(exe) = crate::sibling_of_dll(crate::APP_EXE) else {
        crate::safety::log("launch_app: companion EXE not found next to the DLL — menu action dropped");
        return;
    };
    if let Err(e) = std::process::Command::new(exe).args(args).spawn() {
        crate::safety::log(&format!("launch_app: spawn failed: {e}"));
    }
}

/// Launch the companion EXE's Convert… dialog over the selected images. Writes
/// the (filtered) path list to a temp file and passes its path — robust to many
/// files / odd names where a command line would overflow or mis-quote. Resolves
/// the EXE from the DLL's OWN directory (NOT current_exe(), which in the shell
/// host returns explorer.exe/dllhost.exe).
fn launch_convert_dialog(paths: &[String]) {
    let imgs: Vec<String> = paths.iter().filter(|p| is_image(p.as_str())).cloned().collect();
    if imgs.is_empty() {
        return;
    }
    let mut lf = std::env::temp_dir();
    lf.push(format!("st2k_convert_{}.lst", std::process::id()));
    if std::fs::write(&lf, imgs.join("\r\n")).is_err() {
        return;
    }
    if let Some(s) = lf.to_str() {
        launch_app(&["--convert", s]);
    }
}

/// Launch the companion EXE's keyless uploader over the selected images (path list
/// via a temp file, like [`launch_convert_dialog`]). The app POSTs each file and
/// copies the resulting link(s) to the clipboard; the ORIGINAL files are never
/// modified or deleted (the app's `--upload-keep` path keeps them, unlike the
/// screenshot `--upload` path which deletes its throwaway capture).
fn launch_upload(paths: &[String]) {
    let imgs: Vec<String> = paths.iter().filter(|p| is_image(p.as_str())).cloned().collect();
    if imgs.is_empty() {
        return;
    }
    let mut lf = std::env::temp_dir();
    lf.push(format!("st2k_upload_{}.lst", std::process::id()));
    if std::fs::write(&lf, imgs.join("\r\n")).is_err() {
        return;
    }
    if let Some(s) = lf.to_str() {
        launch_app(&["--upload-keep", s]);
    }
}

/// Launch the companion EXE's "Files to folder" name-prompt dialog over the
/// selected files. Writes the (unfiltered — any file type) path list to a temp
/// file and passes its path, like [`launch_convert_dialog`].
fn launch_files_to_folder(paths: &[String]) {
    if paths.is_empty() {
        return;
    }
    let mut lf = std::env::temp_dir();
    lf.push(format!("st2k_f2f_{}.lst", std::process::id()));
    if std::fs::write(&lf, paths.join("\r\n")).is_err() {
        return;
    }
    if let Some(s) = lf.to_str() {
        launch_app(&["--files-to-folder", s]);
    }
}

/// Launch the companion EXE's "Tags to folders" dialog over the selected audio
/// files (path list via a temp file, like [`launch_files_to_folder`]).
fn launch_tags_to_folders(audio: &[String]) {
    if audio.is_empty() {
        return;
    }
    let mut lf = std::env::temp_dir();
    lf.push(format!("st2k_ttf_{}.lst", std::process::id()));
    if std::fs::write(&lf, audio.join("\r\n")).is_err() {
        return;
    }
    if let Some(s) = lf.to_str() {
        launch_app(&["--tags-to-folders", s]);
    }
}

/// `combined.pdf` (deduped) next to the first image.
fn combined_pdf_path(first: &str) -> PathBuf {
    let dir = Path::new(first).parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
    let mut cand = dir.join("combined.pdf");
    let mut n = 2u32;
    while cand.exists() {
        cand = dir.join(format!("combined ({n}).pdf"));
        n += 1;
    }
    cand
}

/// Open the verbose, copyable "Image info" window in the companion app (it gathers the
/// full file/image/EXIF metadata via `read_info_verbose` and shows it in a scrollable
/// dialog — far more than the old one-line message box).
fn show_info(path: &str) {
    launch_app(&["--image-info", path]);
}

/// Decode `path` and place it on the clipboard as CF_DIB (32bpp, bottom-up
/// BGRA — the conventional packed-DIB layout other apps expect).
pub fn copy_to_clipboard(path: &str) -> Result<()> {
    let bytes = read_capped(path)?;
    let img = decode::decode_full(&bytes)?.to_rgba8();
    let (w, h) = (img.width() as i32, img.height() as i32);
    copy_rgba_to_clipboard(w, h, &img.into_raw())
}

/// Place already-decoded top-down RGBA8 pixels on the clipboard as CF_DIB (32bpp, bottom-up
/// BGRA). The pixel half of [`copy_to_clipboard`]; also used by the Quick preview viewer's
/// Ctrl+C so a navigated-to PDF page / animation frame copies what is actually displayed.
pub fn copy_rgba_to_clipboard(w: i32, h: i32, rgba: &[u8]) -> Result<()> {
    if w <= 0 || h <= 0 {
        return Err(Error::new(E_FAIL, "image has zero or negative dimensions"));
    }
    if rgba.len() != (w as usize) * (h as usize) * 4 {
        return Err(Error::new(E_FAIL, "pixel buffer size mismatch"));
    }
    let row = (w * 4) as usize;
    let header = size_of::<BITMAPINFOHEADER>();
    let total = header + row * h as usize;

    // Assemble the whole packed DIB (BITMAPINFOHEADER + bottom-up BGRA pixels)
    // in a plain Vec first, so the only `unsafe` left is alloc / lock / copy /
    // SetClipboardData. The header is serialized field-by-field to match the
    // exact byte layout of a `#[repr(C)]` BITMAPINFOHEADER (40 bytes, no
    // padding); the pixels are emitted bottom row first with R/B swapped.
    let mut dib = Vec::with_capacity(total);
    // BITMAPINFOHEADER: positive biHeight = bottom-up DIB (CF_DIB convention).
    dib.extend_from_slice(&(header as u32).to_le_bytes()); // biSize
    dib.extend_from_slice(&w.to_le_bytes()); // biWidth
    dib.extend_from_slice(&h.to_le_bytes()); // biHeight (positive = bottom-up)
    dib.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    dib.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    dib.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
    dib.extend_from_slice(&0u32.to_le_bytes()); // biSizeImage
    dib.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter
    dib.extend_from_slice(&0i32.to_le_bytes()); // biYPelsPerMeter
    dib.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    dib.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant
    debug_assert_eq!(dib.len(), header);
    // Pixels: bottom-up, RGBA -> BGRA. Walk source rows in reverse (last to
    // first) and swap R/B per pixel.
    for src in rgba.chunks_exact(row).rev() {
        for px in src.chunks_exact(4) {
            dib.push(px[2]); // B
            dib.push(px[1]); // G
            dib.push(px[0]); // R
            dib.push(px[3]); // A
        }
    }
    debug_assert_eq!(dib.len(), total);

    // The unsafe HGLOBAL ownership dance lives once in `crate::clipboard`.
    if unsafe { crate::clipboard::set_clipboard(crate::clipboard::CF_DIB, &dib) } {
        Ok(())
    } else {
        Err(Error::new(E_FAIL, "copy to clipboard failed"))
    }
}

/// %APPDATA%\SageThumbs2K (created on demand) — where the wallpaper image lives.
fn appdata_dir() -> Result<PathBuf> {
    let base = std::env::var("APPDATA")
        .map_err(|e| Error::new(E_FAIL, format!("%APPDATA% not set: {e}")))?;
    let dir = Path::new(&base).join("SageThumbs2K");
    std::fs::create_dir_all(&dir)
        .map_err(|e| Error::new(E_FAIL, format!("create {}: {e}", dir.display())))?;
    Ok(dir)
}

/// Decode `path` (any supported format, incl. ones Windows can't read directly)
/// and write it as a PNG `dir` can hold. Returns the written image path. Split
/// out from [`prepare_wallpaper`] so tests can target a temp dir instead of the
/// real `%APPDATA%` (writing the production wallpaper.png from a test would
/// pollute the live desktop state).
pub fn prepare_wallpaper_in(dir: &Path, path: &str) -> Result<PathBuf> {
    let bytes = read_capped(path)?;
    // A wallpaper never needs more than screen resolution; downscale large
    // sources so we don't re-encode (and block the shell thread on) a giant PNG.
    let img = cap_to_screen(decode::decode_full(&bytes)?);
    let out = dir.join("wallpaper.png");
    // Atomic write (temp + rename) so a failed/interrupted encode can never
    // leave the live, OS-referenced wallpaper file half-written (the desktop
    // re-reads this exact path at logon). Mirrors `convert_file`.
    let tmp = {
        let mut s = out.clone().into_os_string();
        s.push(".st2ktmp");
        PathBuf::from(s)
    };
    img.save_with_format(&tmp, ImageFormat::Png).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        Error::new(E_FAIL, format!("encode wallpaper PNG: {e}"))
    })?;
    std::fs::rename(&tmp, &out).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        Error::new(E_FAIL, format!("rename wallpaper into place: {e}"))
    })?;
    Ok(out)
}

/// Downscale `img` to fit within the virtual-screen bounds, **never upscaling**.
/// The desktop can't display more than screen resolution, and PNG-re-encoding a
/// full-size camera image on the shell thread is pure waste. Falls back to an 8K
/// cap if the metrics are unavailable (e.g. a headless/service context).
fn cap_to_screen(img: DynamicImage) -> DynamicImage {
    let (mut cap_w, mut cap_h) =
        unsafe { (GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN)) };
    if cap_w <= 0 || cap_h <= 0 {
        cap_w = 7680;
        cap_h = 4320;
    }
    let (cap_w, cap_h) = (cap_w as u32, cap_h as u32);
    if img.width() > cap_w || img.height() > cap_h {
        // resize() preserves aspect and fits within the box.
        img.resize(cap_w, cap_h, image::imageops::FilterType::Lanczos3)
    } else {
        img
    }
}

/// Decode `path` (any supported format, incl. ones Windows can't read directly)
/// and write it as a PNG the desktop can use. Returns the wallpaper image path.
pub fn prepare_wallpaper(path: &str) -> Result<PathBuf> {
    prepare_wallpaper_in(&appdata_dir()?, path)
}

/// Set the selected image as the desktop wallpaper with the given placement.
pub fn set_wallpaper(path: &str, mode: WallpaperMode) -> Result<()> {
    let wp = prepare_wallpaper(path)?;

    // Placement: HKCU\Control Panel\Desktop {WallpaperStyle, TileWallpaper}.
    let (style, tile) = match mode {
        WallpaperMode::Stretch => ("2", "0"),
        WallpaperMode::Tile => ("0", "1"),
        WallpaperMode::Center => ("0", "0"),
    };
    if let Ok(k) = windows_registry::CURRENT_USER.create("Control Panel\\Desktop") {
        let _ = k.set_string("WallpaperStyle", style);
        let _ = k.set_string("TileWallpaper", tile);
    }

    // Apply it (and persist + broadcast the change).
    let wide: Vec<u16> = wp.as_os_str().encode_wide().chain(once(0)).collect();
    unsafe {
        SystemParametersInfoW(
            SPI_SETDESKWALLPAPER,
            0,
            Some(wide.as_ptr() as *mut c_void),
            SPIF_UPDATEINIFILE | SPIF_SENDCHANGE,
        )
        .map_err(|e| Error::new(E_FAIL, format!("SPI_SETDESKWALLPAPER failed: {e}")))?;
    }
    Ok(())
}

/// Batch-rename the selected images from their EXIF capture metadata. Files
/// without the needed EXIF (e.g. screenshots) are left untouched. Best-effort —
/// one failure never aborts the rest. Returns counts so the caller can surface a
/// result: only a real rename ERROR (Err) is a failure; a deliberate skip
/// (Ok(false): missing metadata / name clash) is expected and not counted as
/// failed, so `attempted` is renamed + errored (NOT the skips).
fn rename_by_exif(paths: &[String], pattern: RenamePattern) -> ActionReport {
    let mut renamed = 0usize;
    let mut skipped = 0usize;
    let mut errored = 0usize;
    for p in paths.iter().filter(|p| is_image(p.as_str())) {
        match rename_one(p, pattern) {
            Ok(true) => renamed += 1,
            Ok(false) => skipped += 1,
            Err(_) => errored += 1,
        }
    }
    if skipped > 0 || errored > 0 {
        crate::safety::log(&format!(
            "Rename by EXIF: {renamed} renamed, {skipped} skipped (no capture date / name clash), \
             {errored} errored"
        ));
    }
    // Count only true attempts (rename or error) — a skip means the file
    // intentionally has nothing to do, so it shouldn't read as "failed".
    let mut r = ActionReport::applied(renamed + errored, renamed);
    if errored > 0 {
        r.note = Some(format!("{errored} couldn't be renamed (locked or name clash)"));
    }
    r
}

/// Rename one file per `pattern`. Returns Ok(true) if renamed, Ok(false) if it
/// was skipped (the source metadata is absent — no EXIF date / no audio tag — or
/// it's already correctly named).
pub(crate) fn rename_one(path: &str, pattern: RenamePattern) -> Result<bool> {
    let Some(base) = rename_base(path, pattern) else {
        return Ok(false); // missing the metadata this pattern needs → leave it alone
    };
    let base = sanitize_component(&base);

    let src = Path::new(path);
    let dir = src.parent().unwrap_or_else(|| Path::new("."));

    // Reserve a free target atomically (see `reserve_dest` — the same race-prone
    // `while target.exists()` picker `fileops::move_into`/`copy_into` used to have,
    // where an external writer landing a file in the gap between the check and the
    // rename could collide). `None` = the source is already correctly named.
    let Some(slot) = reserve_dest(src, dir, &base)? else {
        return Ok(false);
    };

    // Retry briefly: a freshly-selected file can hold a transient Explorer lock.
    crate::fsutil::rename_retrying(src, slot.path())
        .map_err(|e| Error::new(E_FAIL, format!("rename to {}: {e}", slot.path().display())))?;
    slot.release();
    Ok(true)
}

/// The new base name (no extension) for `path` under `pattern`, or None when the
/// source lacks the metadata that pattern needs (EXIF date / audio title).
fn rename_base(path: &str, pattern: RenamePattern) -> Option<String> {
    match pattern {
        RenamePattern::DateTaken | RenamePattern::CameraDate => {
            let meta = crate::strip::read_capture(path);
            let time = meta.time?;
            Some(match pattern {
                RenamePattern::CameraDate => match meta.camera {
                    Some(cam) => format!("{} {time}", sanitize_component(&cam)),
                    None => time,
                },
                _ => time,
            })
        }
        RenamePattern::ArtistTitle | RenamePattern::TrackTitle => {
            tag_base(pattern, &crate::strip::read_audio_tags(path))
        }
    }
}

/// Format an audio-tag rename base. A title is required (it's the anchor); the
/// artist / track prefix is added when present. Pure, so it's unit-testable
/// without a real tagged file.
pub(crate) fn tag_base(pattern: RenamePattern, t: &crate::strip::AudioTags) -> Option<String> {
    let title = t.title.clone()?;
    Some(match pattern {
        RenamePattern::ArtistTitle => match &t.artist {
            Some(a) => format!("{a} - {title}"),
            None => title,
        },
        RenamePattern::TrackTitle => match t.track {
            Some(n) => format!("{n:02} - {title}"),
            None => title,
        },
        _ => return None,
    })
}

/// Set the selected image as the icon of the folder that contains it: write a
/// hidden square `.ico`, a `desktop.ini` pointing at it, mark the folder
/// customized, and ask the shell to refresh. Mirrors how Explorer's own
/// "Customize ▸ Change Icon" persists a folder icon.
pub(crate) fn set_folder_icon(image_path: &str) -> Result<()> {
    let src = Path::new(image_path);
    let dir = src.parent().ok_or_else(|| Error::new(E_FAIL, "image has no parent folder"))?;

    let bytes = read_capped(image_path)?;
    let icon = make_icon_square(&decode::decode_full(&bytes)?, 256);

    // Encode the ICO into memory, then write it atomically (a half-written icon
    // would make the folder show a broken glyph).
    let mut ico_bytes = Vec::new();
    icon.write_to(&mut std::io::Cursor::new(&mut ico_bytes), ImageFormat::Ico)
        .map_err(|e| Error::new(E_FAIL, format!("encode folder .ico: {e}")))?;
    let ico_name = "SageThumbsFolder.ico";
    let ico_path = dir.join(ico_name);
    let tmp = with_tmp_suffix(&ico_path);
    std::fs::write(&tmp, &ico_bytes).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        Error::new(E_FAIL, format!("write {}: {e}", tmp.display()))
    })?;
    std::fs::rename(&tmp, &ico_path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        Error::new(E_FAIL, format!("rename .ico into place: {e}"))
    })?;

    // desktop.ini references the icon by a RELATIVE name (so it survives a move).
    // `IconResource` is the modern key; `IconFile`/`IconIndex` keep older Explorer
    // happy. CRLF + a trailing newline, matching what Explorer writes.
    let ini_path = dir.join("desktop.ini");
    let ini = format!(
        "[.ShellClassInfo]\r\nIconResource={ico_name},0\r\nIconFile={ico_name}\r\nIconIndex=0\r\n"
    );
    std::fs::write(&ini_path, ini.as_bytes())
        .map_err(|e| Error::new(E_FAIL, format!("write desktop.ini: {e}")))?;

    // Hide the helper files; mark the folder System+ReadOnly so Explorer actually
    // reads desktop.ini (the documented requirement to honor a custom icon).
    add_attrs(&ico_path, FILE_ATTRIBUTE_HIDDEN);
    add_attrs(&ini_path, FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM);
    add_attrs(dir, FILE_ATTRIBUTE_READONLY | FILE_ATTRIBUTE_SYSTEM);

    // Nudge the shell to repaint the folder with its new icon.
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(once(0)).collect();
    unsafe {
        SHChangeNotify(
            SHCNE_UPDATEDIR,
            SHCNF_PATHW,
            Some(wide.as_ptr() as *const c_void),
            None,
        );
    }
    Ok(())
}

/// Fit `img` inside a transparent `size`×`size` RGBA canvas, centered — so a
/// non-square image becomes a clean square icon (Explorer scales/letterboxes
/// otherwise). Never upscales the source beyond the canvas.
fn make_icon_square(img: &DynamicImage, size: u32) -> DynamicImage {
    let fit = img.resize(size, size, image::imageops::FilterType::Lanczos3).to_rgba8();
    let mut canvas = image::RgbaImage::from_pixel(size, size, image::Rgba([0, 0, 0, 0]));
    let ox = ((size - fit.width()) / 2) as i64;
    let oy = ((size - fit.height()) / 2) as i64;
    image::imageops::overlay(&mut canvas, &fit, ox, oy);
    DynamicImage::ImageRgba8(canvas)
}

/// OR `add` into a path's existing file attributes (best-effort; a permission
/// failure just leaves the file as-is).
fn add_attrs(path: &Path, add: FILE_FLAGS_AND_ATTRIBUTES) {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(once(0)).collect();
    unsafe {
        let cur = GetFileAttributesW(PCWSTR(wide.as_ptr()));
        // GetFileAttributesW returns INVALID_FILE_ATTRIBUTES (u32::MAX) on error;
        // start from zero in that case rather than OR-ing the sentinel in.
        let base = if cur == u32::MAX {
            FILE_FLAGS_AND_ATTRIBUTES(0)
        } else {
            FILE_FLAGS_AND_ATTRIBUTES(cur)
        };
        let _ = SetFileAttributesW(PCWSTR(wide.as_ptr()), base | add);
    }
}

#[cfg(test)]
mod tests {
    use super::reveal_is_noise;

    #[test]
    fn reveal_skips_in_place_sibling_only() {
        let dir = std::env::temp_dir().join(format!("st2k_reveal_noise_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("photo.png");
        std::fs::write(&src, b"src").unwrap();
        let sources = vec![src.to_string_lossy().into_owned()];

        // Convert into ▸ WebP: a file sibling next to the source → noise (no popup).
        let webp = dir.join("photo.webp");
        std::fs::write(&webp, b"out").unwrap();
        assert!(reveal_is_noise(&webp, &sources), "in-place convert must not reveal");

        // Files-to-folder: a NEW directory → not noise (reveal it).
        let newfolder = dir.join("My Folder");
        std::fs::create_dir_all(&newfolder).unwrap();
        assert!(!reveal_is_noise(&newfolder, &sources), "new folder should reveal");

        // A file inside a new subfolder (different parent) → reveal it.
        let moved = newfolder.join("photo.png");
        std::fs::write(&moved, b"moved").unwrap();
        assert!(!reveal_is_noise(&moved, &sources), "output in a new folder should reveal");

        // Convert that wrote to a totally different folder → reveal it.
        let other_dir = dir.join("elsewhere");
        std::fs::create_dir_all(&other_dir).unwrap();
        let other = other_dir.join("photo.webp");
        std::fs::write(&other, b"o").unwrap();
        assert!(!reveal_is_noise(&other, &sources), "output in a different dir should reveal");

        // A nonexistent output path is not a file → not "noise" (reveal attempt is
        // harmless; the file-exists gate is the caller's success check).
        assert!(!reveal_is_noise(&dir.join("ghost.webp"), &sources));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
