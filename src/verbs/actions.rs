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
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SPIF_SENDCHANGE, SPIF_UPDATEINIFILE,
    SPI_SETDESKWALLPAPER,
};

use super::encode::{
    edit_output_ext, predict_unique_suffix, read_capped, reserve_unique_suffix, resize_file,
    shrink_for_email, transform_file, with_tmp_suffix, Resize, Target,
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

mod clipboard;
mod foldericon;
mod helper;
mod rename;
mod wallpaper;

// Parent-hub import model: pull the children's `pub(super)` items in privately so this
// file reads as if nothing moved, then re-export the public names BY NAME.
use helper::{convert_one, resize_one, shrink_one, st2k_exe, strip_one, transform_one};
use rename::rename_by_exif;

pub use clipboard::{copy_rgba_to_clipboard, copy_to_clipboard};
pub(crate) use foldericon::set_folder_icon;
pub use wallpaper::{prepare_wallpaper, prepare_wallpaper_in, set_wallpaper};
// Re-exported onward by the `verbs` facade (and consumed from the bin crates through
// it), which this module can't see - so the lint reads them as unused here.
#[allow(unused_imports)]
pub(crate) use rename::{rename_one, tag_base};

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
        ActionReport {
            delegated: true,
            ..Default::default()
        }
    }

    /// A plain `attempted`/`done` report with no failure note (the caller adds one
    /// via [`with_note`] when there's a shortfall).
    fn applied(attempted: usize, done: usize) -> Self {
        ActionReport {
            attempted,
            done,
            ..Default::default()
        }
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
fn reveal_is_noise(out: &std::path::Path, sources: &[String]) -> bool {
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

/// Run a context-menu action on a DETACHED worker thread, then surface any error and
/// reveal new-folder output — so the shell's `IContextMenu::InvokeCommand` /
/// `IExplorerCommand::Invoke` returns immediately instead of blocking explorer.exe's UI
/// thread for the (possibly many-file, many-second) batch. The worker holds a
/// [`crate::ModuleRef`] (so the DLL can't unload mid-action) and initializes its own STA
/// COM apartment (verbs may touch WIC / the shell); it owns clones of every input, so it
/// keeps NO reference to the COM object that launched it. `owner` is the parent HWND (as
/// `isize`) for the error MessageBox, or `None`.
pub fn run_action_detached(action: VerbAction, paths: Vec<String>, owner: Option<isize>) {
    let _ = std::thread::Builder::new()
        .name("st2k-verb".into())
        .spawn(move || {
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
            let parent =
                owner.map(|h| windows::Win32::Foundation::HWND(h as *mut core::ffi::c_void));
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
            let outs: Vec<PathBuf> =
                crate::parallel::map(paths, |_, p| convert_one(exe_ref, p, target))
                    .into_iter()
                    .flatten()
                    .collect();
            let n = outs.len();
            let first = outs.into_iter().next();
            let mut r = if n < paths.len() {
                crate::safety::log(&format!(
                    "Convert to {}: only {}/{} succeeded",
                    target.ext,
                    n,
                    paths.len()
                ));
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
            let outs: Vec<PathBuf> =
                crate::parallel::map(paths, |_, p| transform_one(exe_ref, p, t))
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
            let imgs: Vec<String> = paths
                .iter()
                .filter(|p| is_image(p.as_str()))
                .cloned()
                .collect();
            if imgs.is_empty() {
                return ActionReport::default();
            }
            // Hold the slot for the whole write: it's what keeps a second, concurrent Combine
            // from picking the same name and renaming over this one's finished file.
            let slot = combined_pdf_path(&imgs[0]);
            let out = slot.path().to_path_buf();
            match crate::topdf::combine_to_pdf(&imgs, &out, crate::settings::jpeg_quality()) {
                Ok(_) => ActionReport {
                    output: Some(out),
                    ..ActionReport::applied(1, 1)
                },
                Err(e) => {
                    crate::safety::log(&format!("Combine to PDF failed: {e:?}"));
                    ActionReport::applied(1, 0).with_note("couldn't build the PDF")
                }
            }
        }
        VerbAction::CombineToCbz => {
            let imgs: Vec<String> = paths
                .iter()
                .filter(|p| is_image(p.as_str()))
                .cloned()
                .collect();
            if imgs.is_empty() {
                return ActionReport::default();
            }
            let slot = combined_path(&imgs[0], "cbz");
            let out = slot.path().to_path_buf();
            match combine_to_cbz(&imgs, &out) {
                Ok(()) => ActionReport {
                    output: Some(out),
                    ..ActionReport::applied(1, 1)
                },
                Err(e) => {
                    crate::safety::log(&format!("Combine to CBZ failed: {e:?}"));
                    ActionReport::applied(1, 0).with_note("couldn't build the CBZ archive")
                }
            }
        }
        VerbAction::Ocr => match paths.iter().find(|p| is_image(p.as_str())) {
            Some(p) => match crate::ocr::ocr_to_clipboard(p) {
                Ok(()) => ActionReport::applied(1, 1),
                Err(e) => {
                    crate::safety::log(&format!("OCR failed for {p}: {e:?}"));
                    ActionReport::applied(1, 0).with_note("couldn't read text from the image")
                }
            },
            None => ActionReport::default(),
        },
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
            let imgs: Vec<String> = paths
                .iter()
                .filter(|p| is_image(p.as_str()))
                .cloned()
                .collect();
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
            let imgs: Vec<String> = paths
                .iter()
                .filter(|p| is_image(p.as_str()))
                .cloned()
                .collect();
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
            let imgs: Vec<String> = paths
                .iter()
                .filter(|p| is_image(p.as_str()))
                .cloned()
                .collect();
            let outs: Vec<PathBuf> =
                crate::parallel::map(&imgs, |_, p| shrink_one(exe_ref, p, size))
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
                            ActionReport::applied(1, 0)
                                .with_note("couldn't create or fill the folder")
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
            let audio: Vec<String> = paths
                .iter()
                .filter(|p| is_audio(p.as_str()))
                .cloned()
                .collect();
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
        crate::safety::log(
            "launch_app: companion EXE not found next to the DLL — menu action dropped",
        );
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
    let imgs: Vec<String> = paths
        .iter()
        .filter(|p| is_image(p.as_str()))
        .cloned()
        .collect();
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
    let imgs: Vec<String> = paths
        .iter()
        .filter(|p| is_image(p.as_str()))
        .cloned()
        .collect();
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

/// `combined.pdf` (deduped) next to the first image, ATOMICALLY reserved — see
/// [`crate::verbs::combined_path`] for why this can't be a `while cand.exists()` loop.
fn combined_pdf_path(first: &str) -> crate::verbs::encode::OutSlot {
    let dir = Path::new(first)
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    crate::verbs::encode::reserve(|n| {
        if n == 0 {
            dir.join("combined.pdf")
        } else {
            dir.join(format!("combined ({}).pdf", n + 1))
        }
    })
}

/// Open the verbose, copyable "Image info" window in the companion app (it gathers the
/// full file/image/EXIF metadata via `read_info_verbose` and shows it in a scrollable
/// dialog — far more than the old one-line message box).
fn show_info(path: &str) {
    launch_app(&["--image-info", path]);
}

#[cfg(test)]
mod tests {
    use super::foldericon::merge_shell_class_info;
    use super::helper::routed_edit_output_ext;
    use super::reveal_is_noise;

    /// Setting a folder icon must not eat the rest of desktop.ini. Explorer keeps localized
    /// folder names and tooltips in the same file, and the old code replaced the whole thing.
    #[test]
    fn desktop_ini_merge_preserves_everything_else() {
        // Empty / missing file → just our section.
        let fresh = merge_shell_class_info("", "SageThumbsFolder.ico");
        assert_eq!(
            fresh,
            "[.ShellClassInfo]\r\nIconResource=SageThumbsFolder.ico,0\r\n\
             IconFile=SageThumbsFolder.ico\r\nIconIndex=0\r\n"
        );

        // Existing unrelated section survives, and our keys get their own section appended.
        let loc = "[LocalizedFileNames]\r\nreport.docx=@shell32.dll,-1\r\n";
        let merged = merge_shell_class_info(loc, "SageThumbsFolder.ico");
        assert!(merged.contains("[LocalizedFileNames]"), "{merged}");
        assert!(merged.contains("report.docx=@shell32.dll,-1"), "{merged}");
        assert!(merged.contains("[.ShellClassInfo]"), "{merged}");

        // An existing [.ShellClassInfo] keeps its NON-icon keys; the icon keys are replaced,
        // not duplicated.
        let prior = "[.ShellClassInfo]\r\nInfoTip=My photos\r\nIconResource=old.ico,3\r\n\
                     IconFile=old.ico\r\nIconIndex=3\r\nConfirmFileOp=0\r\n";
        let merged = merge_shell_class_info(prior, "SageThumbsFolder.ico");
        assert!(merged.contains("InfoTip=My photos"), "{merged}");
        assert!(merged.contains("ConfirmFileOp=0"), "{merged}");
        assert!(!merged.contains("old.ico"), "{merged}");
        assert_eq!(merged.matches("IconResource=").count(), 1, "{merged}");
        assert_eq!(merged.matches("[.ShellClassInfo]").count(), 1, "{merged}");

        // Section names are case-insensitive in INI files.
        let odd = "[.shellclassinfo]\r\nIconFile=old.ico\r\n";
        let merged = merge_shell_class_info(odd, "new.ico");
        assert_eq!(merged.matches("[.").count(), 1, "{merged}");
        assert!(merged.contains("IconFile=new.ico"), "{merged}");
        assert!(!merged.contains("old.ico"), "{merged}");

        // Re-running is idempotent — no key or section pile-up.
        let once = merge_shell_class_info("", "a.ico");
        assert_eq!(merge_shell_class_info(&once, "a.ico"), once);
    }

    #[test]
    fn reveal_skips_in_place_sibling_only() {
        let dir =
            std::env::temp_dir().join(format!("st2k_reveal_noise_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("photo.png");
        std::fs::write(&src, b"src").unwrap();
        let sources = vec![src.to_string_lossy().into_owned()];

        // Convert into ▸ WebP: a file sibling next to the source → noise (no popup).
        let webp = dir.join("photo.webp");
        std::fs::write(&webp, b"out").unwrap();
        assert!(
            reveal_is_noise(&webp, &sources),
            "in-place convert must not reveal"
        );

        // Files-to-folder: a NEW directory → not noise (reveal it).
        let newfolder = dir.join("My Folder");
        std::fs::create_dir_all(&newfolder).unwrap();
        assert!(
            !reveal_is_noise(&newfolder, &sources),
            "new folder should reveal"
        );

        // A file inside a new subfolder (different parent) → reveal it.
        let moved = newfolder.join("photo.png");
        std::fs::write(&moved, b"moved").unwrap();
        assert!(
            !reveal_is_noise(&moved, &sources),
            "output in a new folder should reveal"
        );

        // Convert that wrote to a totally different folder → reveal it.
        let other_dir = dir.join("elsewhere");
        std::fs::create_dir_all(&other_dir).unwrap();
        let other = other_dir.join("photo.webp");
        std::fs::write(&other, b"o").unwrap();
        assert!(
            !reveal_is_noise(&other, &sources),
            "output in a different dir should reveal"
        );

        // A nonexistent output path is not a file → not "noise" (reveal attempt is
        // harmless; the file-exists gate is the caller's success check).
        assert!(!reveal_is_noise(&dir.join("ghost.webp"), &sources));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn routed_edits_use_the_same_honest_extension_as_in_process_edits() {
        for source in ["drawing.svg", "photo.heic", "bitmap.pbm", "mystery.unknown"] {
            assert_eq!(
                routed_edit_output_ext(std::path::Path::new(source)),
                "png",
                "{source}"
            );
        }
        for source in ["layered.psd", "texture.dds", "picture.jp2"] {
            assert_eq!(
                routed_edit_output_ext(std::path::Path::new(source)),
                source.rsplit_once('.').unwrap().1,
                "{source}"
            );
        }
        assert_eq!(
            routed_edit_output_ext(std::path::Path::new("photo.JPEG")),
            "jpeg"
        );
    }
}
