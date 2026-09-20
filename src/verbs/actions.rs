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
//! the sibling `st2k.exe` only if it exists. When it's absent (a partial install
//! where only the DLL got registered, or a checkout that never built the CLI) we
//! transparently **fall back to the original in-process code path**, unchanged. A
//! missing helper can therefore never break a verb — it only forfeits the crash
//! isolation.
//!
//! **Don't assume a test takes that fallback.** A test binary runs out of cargo's
//! `deps\` directory and cargo drops `st2k.exe` there too, so on any machine that has
//! built the workspace `st2k_exe()` resolves and the ROUTED path is what runs. That's
//! harmless for the routed verbs (both paths write the same file, so
//! `tests/explorer_command.rs::convert_verb_invoke_creates_file` is green either way),
//! but it has two consequences worth knowing. The fallback arm is exercised by NO test
//! unless one passes `None` deliberately, which is what
//! `helper::tests::the_in_process_fallback_still_converts_when_no_helper_is_present`
//! exists to do. And it is precisely why the *other* sibling lookup needed a real
//! seam: the companion-app launchers spawn a GUI process with side effects no test
//! wants. See [`intercept_launch`].
//!
//! Routed verbs (helper-if-present): **Convert**, **Transform** (→ `rotate`),
//! **ResizeImg** (→ `convert --resize`), **ShrinkForEmail** (→ `convert --resize`),
//! **StripMetadata** (→ `strip`), **CompressToSize** (→ `compress`), **Clipboard**
//! (→ the `clip-pixels` stdout-only child), **Wallpaper** (→ `wallpaper-prepare`),
//! and **SetFolderIcon** (→ `folder-icon`). **SaveVideoFrame** (→ `thumbnail --size
//! 0`) is routed too, but is HELPER-ONLY — see its own bullet below, it does not
//! follow the "falls back in-process when the helper is absent" rule the rest of
//! this list does. The first five map cleanly to a `st2k`
//! CLI verb that drives the *same* engine (`decode_full` + the same
//! convert/transform/strip/compress code), so the produced file is byte-identical
//! and lands at the *same* auto-named path the in-process verb would write — we
//! compute that path and pass it to the CLI as `<out>` where the verb takes one
//! (`rotate`/`strip` auto-name in place, exactly like their in-process twins, so
//! they need no `<out>`).
//!
//! The last three touch shell/desktop state a plain "write the same file" verb
//! doesn't have, so each gets its own child verb instead of reusing an existing one:
//! - **`clip-pixels <file>`** decodes and writes `w h` (two little-endian u32) then
//!   top-down RGBA8 straight to stdout — no clipboard API runs in the child at all.
//!   The parent reads that stdout back, validates it against
//!   `decode::limits::MAX_DIM`/`MAX_ALLOC` and the exact `w*h*4` byte count
//!   ([`helper::parse_clip_pixels`]), and hands the bytes to the existing
//!   `copy_rgba_to_clipboard` — the only in-process work on the routed path is that
//!   bounded memcpy; no image parser ever runs in the shell host.
//! - **`wallpaper-prepare <file> <out-dir>`** runs the decode/resize-to-screen half
//!   (`prepare_wallpaper_in`) in the child and prints the produced PNG's path; the
//!   parent supplies its own `%APPDATA%\SageThumbs2K` as `<out-dir>` and applies the
//!   result (`apply_wallpaper` — registry write + `SystemParametersInfoW`) without
//!   decoding anything itself.
//! - **`folder-icon <file>`** runs the *whole* verb (the .ico + desktop.ini writes)
//!   in the child; the parent only collects the exit status, same as `strip`.
//! - **`thumbnail <file> <out> --size 0`** (SaveVideoFrame) grabs the video's frame
//!   via the OS Media Foundation codecs and writes it full-resolution as a standalone
//!   PNG. HELPER-ONLY, no in-process fallback: video decode must never run inside
//!   `explorer.exe`/`dllhost.exe` (the same crash-isolation doctrine
//!   `decode_menu_preview` follows for the thumbnail path — see CLAUDE.md's decode
//!   tier notes). With no helper installed, every video in the selection is reported
//!   as failed rather than silently decoding a hostile file in the shell host.
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
//! - CombineToCbz (no CLI verb) and the info/sort/rename/dialog/settings/eyedropper
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
use windows::Win32::Foundation::{
    GetLastError, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, E_FAIL,
};
use windows::Win32::Graphics::Gdi::BITMAPINFOHEADER;
use windows::Win32::Storage::FileSystem::{
    GetFileAttributesW, SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_READONLY,
    FILE_ATTRIBUTE_SYSTEM, FILE_FLAGS_AND_ATTRIBUTES,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, MessageBoxW, SystemParametersInfoW, MB_ICONWARNING, MB_OK,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SPIF_SENDCHANGE, SPIF_UPDATEINIFILE,
    SPI_SETDESKWALLPAPER,
};

use super::encode::{
    compress_to_size, edit_output_ext, predict_unique_suffix, read_full_fidelity_capped,
    reserve_unique_suffix, resize_file, shrink_for_email, transform_file, Resize, Target,
};
use super::fileops::{
    combine_to_cbz, combined_path, files_to_folder, reserve_dest, sanitize_component,
    sort_by_date_taken, sort_by_dimensions,
};
use super::menu::{CompressSize, EmailSize, RenamePattern, Transform, VerbAction, WallpaperMode};
use crate::decode;

// Don't flash a console window when we spawn `st2k.exe` from the shell host
// (`explorer.exe`/`dllhost.exe` are GUI processes — a child console would pop).
use crate::CREATE_NO_WINDOW;

mod clipboard;
mod foldericon;
mod helper;
mod launch;
mod rename;
mod wallpaper;
use launch::*;
mod compress;
mod report;
use compress::*;
#[cfg(test)]
use report::reveal_is_noise;
mod organize;
use organize::*;
pub use report::ActionReport;

// Parent-hub import model: pull the children's `pub(super)` items in privately so this
// file reads as if nothing moved, then re-export the public names BY NAME.
use helper::{
    clipboard_one, compress_one, convert_one, folder_icon_one, lock_screen_one, resize_one,
    save_video_frame_one, shrink_one, st2k_exe, strip_one, transform_one, wallpaper_one,
};
use rename::rename_by_exif;

pub use clipboard::{copy_data_uri_to_clipboard, copy_rgba_to_clipboard, copy_to_clipboard};
pub(crate) use foldericon::set_folder_icon;
#[cfg(test)]
pub use wallpaper::set_wallpaper;
pub use wallpaper::{prepare_lock_screen_in, prepare_wallpaper, prepare_wallpaper_in};
// Re-exported onward by the `verbs` facade (and consumed from the bin crates through
// it), which this module can't see - so the lint reads them as unused here.
#[allow(unused_imports)]
pub(crate) use rename::{rename_one, tag_base};
// The free-pattern rename engine's crate-external surface: the companion app's
// "Rename with pattern…" dialog (`rename_dlg.rs`) calls `rename_pattern_preview` on
// every keystroke for its live list and `rename_by_pattern` from OK's worker thread —
// both re-exported onward by `verbs.rs` / `lib.rs`, same path `files_to_folder` takes.
pub use rename::{rename_by_pattern, rename_pattern_preview};

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
    has_category(path, crate::formats::Category::Audio)
}

/// Does `path`'s extension belong to `category`? The one lookup behind [`is_audio`] and
/// [`is_video`].
fn has_category(path: &str, category: crate::formats::Category) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|ext| crate::formats::category(&ext) == category)
}

/// Does `path` have a video extension (one we grab a frame from via OS Media
/// Foundation codecs)? Gates the video-only menu view (`contextmenu/com.rs`) and
/// `VerbAction::SaveVideoFrame`'s file filter, the same way [`is_audio`] gates the
/// audio-only surfaces.
pub fn is_video(path: &str) -> bool {
    has_category(path, crate::formats::Category::Video)
}

/// Per-call unique temp-staging name for an atomic write to a FIXED destination
/// (folder-icon's `SageThumbsFolder.ico`/`desktop.ini`, wallpaper's `wallpaper.png`)
/// — unlike `super::encode`'s `write_atomic`/`OutSlot`, which reserves a fresh,
/// collision-free OUTPUT name per call, these two verbs always write to the SAME
/// destination path. A bare `<out>.st2ktmp` staging name is then shared by every
/// call against that destination, so two quick clicks (two Set-as-folder-icon runs
/// in one folder, in quick succession) write through separate handles to the
/// identical temp file. Stamp a per-process atomic counter into the name — the
/// same fix `launch_with_list` already applies to its listfile names.
fn unique_tmp(out: &Path) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut s = out.to_path_buf().into_os_string();
    s.push(format!(".{}_{n}.st2ktmp", std::process::id()));
    PathBuf::from(s)
}

/// The parent window for the error box, rebuilt from the `isize` the caller passed.
fn owner_hwnd(owner: Option<isize>) -> Option<windows::Win32::Foundation::HWND> {
    owner.map(|h| windows::Win32::Foundation::HWND(h as *mut core::ffi::c_void))
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
    let attempted = paths.len();
    run_action_detached_with(action, attempted, move || paths, owner);
}

/// [`run_action_detached`] with the path list produced ON THE WORKER by `resolve`, after
/// its COM apartment is up. The modern menu's `Invoke` uses it to walk the shell's
/// `IShellItemArray` (fetched back out of the Global Interface Table) off the shell
/// thread, so a thousand-file selection costs explorer.exe nothing but the click.
/// `attempted` is only the count a spawn-failure report shows.
pub fn run_action_detached_with<F>(
    action: VerbAction,
    attempted: usize,
    resolve: F,
    owner: Option<isize>,
) where
    F: FnOnce() -> Vec<String> + Send + 'static,
{
    // Pin the DLL BEFORE `spawn`, not as the worker closure's first line: `spawn` only
    // schedules the thread, it doesn't run it, so a `ModuleRef` taken inside the closure
    // leaves a window — between `spawn` returning here and that first line actually
    // executing — where the DLL could unload out from under a thread that's about to
    // touch it. `ModuleRef::default()` is NOT a no-op — its `Default` impl does the
    // `dll_add_ref()`; clippy's "use `ModuleRef`" suggestion would skip it.
    #[allow(clippy::default_constructed_unit_structs)]
    let module = crate::ModuleRef::default();
    let spawned = std::thread::Builder::new()
        .name("st2k-verb".into())
        .spawn(move || {
            let _module = module;
            // `HWND` wraps a raw pointer and is not `Send`; the owner crosses as an `isize`.
            let parent = owner_hwnd(owner);
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
            let paths = resolve();
            let report = run_action(action, &paths);
            report.surface(parent);
            report.reveal(&paths);
            if inited {
                unsafe { windows::Win32::System::Com::CoUninitialize() };
            }
        });
    // A spawn failure used to vanish silently (the menu item just "did nothing"). Log it
    // and show the same error box a failed verb would, instead of leaving the click
    // unexplained — `module` (still held here) drops right after, releasing the pin this
    // aborted action never used.
    if let Err(e) = spawned {
        crate::safety::log(&format!("run_action_detached: spawn failed: {e}"));
        ActionReport::applied(attempted, 0)
            .with_note("couldn't start the action")
            .surface(owner_hwnd(owner));
    }
}

/// Dispatch a verb over the selected paths (best-effort). Returns an
/// [`ActionReport`] the Invoke callers surface to the user on failure.
pub fn run_action(action: VerbAction, paths: &[String]) -> ActionReport {
    match action {
        VerbAction::Convert(target) => handle_convert(paths, target),
        VerbAction::Transform(t) => handle_transform(paths, t),
        VerbAction::Clipboard => handle_clipboard(paths),
        VerbAction::CopyDataUri => handle_copy_data_uri(paths),
        // Upload the selected image(s) to the keyless host in the companion app, which
        // copies the resulting link(s) to the clipboard. The originals are never modified.
        VerbAction::Upload => delegated_via_listfile(launch_upload(paths)),
        VerbAction::Wallpaper(mode) => handle_wallpaper(paths, mode),
        VerbAction::LockScreen => handle_lock_screen(paths),
        VerbAction::CombineToPdf => handle_combine_to_pdf(paths),
        VerbAction::CombineToCbz => handle_combine_to_cbz(paths),
        VerbAction::Ocr => handle_ocr(paths),
        VerbAction::ImageInfo => handle_image_info(paths),
        VerbAction::StripMetadata => handle_strip_metadata(paths),
        VerbAction::ConvertDialog => delegated_via_listfile(launch_convert_dialog(paths)),
        VerbAction::OpenSettings => {
            launch_app(&[]);
            ActionReport::delegated()
        }
        VerbAction::ResizeImg(r) => handle_resize_img(paths, r),
        VerbAction::ShrinkForEmail(size) => handle_shrink_for_email(paths, size),
        VerbAction::CompressToSize(size) => handle_compress_to_size(paths, size),
        VerbAction::RenameByExif(pattern) => rename_by_exif(paths, pattern),
        VerbAction::SetFolderIcon => handle_set_folder_icon(paths),
        VerbAction::Eyedropper => {
            // A system-wide screen color picker (the selected file is irrelevant).
            let _ = paths;
            launch_app(&["--eyedropper"]);
            ActionReport::delegated()
        }
        VerbAction::FilesToFolder => handle_files_to_folder(paths),
        VerbAction::RenameWithPattern => handle_rename_with_pattern(paths),
        VerbAction::SortByDimensions => handle_sort_by_dimensions(paths),
        VerbAction::SortByDateTaken => handle_sort_by_date_taken(paths),
        VerbAction::TagsToFolders => handle_tags_to_folders(paths),
        VerbAction::SaveVideoFrame => handle_save_video_frame(paths),
    }
}

/// A verb the companion app owns end to end (network, result window): delegated, unless
/// the listfile handoff itself could not be written, in which case there is no window
/// coming to explain the silently dead menu item, so the failure is reported here.
fn delegated_via_listfile(launch: ListLaunch) -> ActionReport {
    match launch {
        ListLaunch::Failed => {
            ActionReport::applied(1, 0).with_note("couldn't hand off the file list")
        }
        _ => ActionReport::delegated(),
    }
}

/// `VerbAction::ImageInfo`: opens its own info window (a message box) for the first
/// image in the selection - the app owns the UX.
fn handle_image_info(paths: &[String]) -> ActionReport {
    if let Some(p) = paths.iter().find(|p| is_image(p.as_str())) {
        show_info(p);
    }
    ActionReport::delegated()
}

/// `VerbAction::Convert` - counts over ALL paths (no image filter), so the attempted
/// count matches its denominator. Each file is converted on the batch pool (routed to
/// the st2k helper per file for crash isolation when present, else in-process -
/// `convert_one(None, …)` IS `convert_file`). Results come back IN ORDER, so the first
/// success matches the old first-in-iteration reveal target. The global magick cap
/// bounds memory across the fanned-out st2k children.
fn handle_convert(paths: &[String], target: Target) -> ActionReport {
    let exe = st2k_exe();
    let exe_ref = exe.as_deref();
    per_file_action(
        paths,
        &format!("Convert to {}", target.ext),
        "conversion failed for some files",
        |p| convert_one(exe_ref, p, target),
    )
}

/// The per-file batch verbs share one shape: run `one` over the selection on the batch
/// pool, count the files that produced an output, reveal the first, and note a partial
/// result in the log and the report. Convert and Transform carried this by hand until
/// 2026-09-19.
fn per_file_action(
    paths: &[String],
    what: &str,
    note: &str,
    one: impl Fn(&str) -> Option<PathBuf> + Sync,
) -> ActionReport {
    let outs: Vec<PathBuf> = crate::parallel::map(paths, |_, p| one(p))
        .into_iter()
        .flatten()
        .collect();
    let n = outs.len();
    let first = outs.into_iter().next();
    let mut r = if n < paths.len() {
        crate::safety::log(&format!("{what}: only {n}/{} succeeded", paths.len()));
        ActionReport::applied(paths.len(), n).with_note(note)
    } else {
        ActionReport::applied(paths.len(), n)
    };
    r.output = first;
    r
}

/// `VerbAction::Transform` - routed per file to `st2k rotate` on the batch pool (else
/// in-process `transform_file`); `transform_one` returns the produced path, so the
/// ordered results give the same count + first-reveal as the old loop.
fn handle_transform(paths: &[String], t: Transform) -> ActionReport {
    let exe = st2k_exe();
    let exe_ref = exe.as_deref();
    per_file_action(
        paths,
        "Transform",
        "rotate/flip failed for some files",
        |p| transform_one(exe_ref, p, t),
    )
}

/// `VerbAction::Clipboard` - clipboard holds one image. Use the first *image* in the
/// selection (not `paths.first()`): the menu gate only requires *some* image, so for a
/// mixed selection the first item may be a non-image. Routed per file to `st2k
/// clip-pixels` (helper-if-present) - the child decodes and prints raw pixels, the
/// parent only does a bounded memcpy - else falls back to in-process `copy_to_clipboard`.
fn handle_clipboard(paths: &[String]) -> ActionReport {
    first_image_action(
        paths,
        "Copy to clipboard",
        "couldn't decode or copy the image",
        |p| clipboard_one(st2k_exe().as_deref(), p),
    )
}

/// The "first image in the selection" verbs share one shape: find the first image path, run
/// the verb on it, and report one applied or one failed with the verb's own note and a log
/// line naming the file. Four handlers carried this by hand until 2026-09-19.
fn first_image_action<E: std::fmt::Debug>(
    paths: &[String],
    what: &str,
    note: &str,
    run: impl FnOnce(&str) -> std::result::Result<(), E>,
) -> ActionReport {
    let Some(p) = paths.iter().find(|p| is_image(p.as_str())) else {
        return ActionReport::default();
    };
    crate::safety::log_debugf!("{what}: using {p}");
    match run(p) {
        Ok(()) => ActionReport::applied(1, 1),
        Err(e) => {
            crate::safety::log(&format!("{what} failed for {p}: {e:?}"));
            ActionReport::applied(1, 0).with_note(note)
        }
    }
}

/// `VerbAction::CopyDataUri` - same "first image in the selection" rule as
/// [`handle_clipboard`]. Reads the file's raw bytes (not decoded pixels - the URI
/// carries the original file byte for byte) and places `data:<mime>;base64,…` on the
/// clipboard as text.
fn handle_copy_data_uri(paths: &[String]) -> ActionReport {
    first_image_action(
        paths,
        "Copy as data URI",
        "couldn't read or copy the file",
        copy_data_uri_to_clipboard,
    )
}

/// `VerbAction::Wallpaper` - one wallpaper. Use the first *image* in the selection (see
/// [`handle_clipboard`]). The decode/resize half is routed to `st2k wallpaper-prepare`
/// (helper-if-present, else in-process `prepare_wallpaper`); applying the result
/// (registry + `SystemParametersInfoW`) always runs in-process either way - see
/// [`helper::wallpaper_one`].
fn handle_wallpaper(paths: &[String], mode: WallpaperMode) -> ActionReport {
    first_image_action(paths, "Set wallpaper", "couldn't set the wallpaper", |p| {
        wallpaper_one(st2k_exe().as_deref(), p, mode)
    })
}

/// `VerbAction::LockScreen` - one lock-screen image. Use the first *image* in the selection
/// (see [`handle_clipboard`]). The decode/resize half is routed to `st2k wallpaper-prepare`
/// (helper-if-present, else in-process `prepare_wallpaper`) - the SAME child verb Set-as-
/// wallpaper uses, since both apply the identical prepared PNG; applying it as the lock
/// screen (`LockScreen::SetImageFileAsync`, no decode) always runs in-process - see
/// [`helper::lock_screen_one`].
fn handle_lock_screen(paths: &[String]) -> ActionReport {
    first_image_action(
        paths,
        "Set lock screen",
        "couldn't set the lock screen",
        |p| lock_screen_one(st2k_exe().as_deref(), p),
    )
}

/// `VerbAction::CombineToPdf`.
fn handle_combine_to_pdf(paths: &[String]) -> ActionReport {
    combine_action(
        paths,
        "pdf",
        "Combine to PDF",
        "couldn't build the PDF",
        |imgs, out| {
            crate::topdf::combine_to_pdf(
                imgs,
                out,
                crate::settings::jpeg_quality(),
                super::OnOmit::Report,
            )
            .map(|combined| combined.omitted.len())
        },
    )
}

/// The combine verbs share one shape: every image in the selection, a held output slot
/// beside the first (what keeps a second, concurrent Combine from picking the same name and
/// renaming over this one's finished file), and REAL counts back. `combine` answers how
/// many images it had to leave out (undecodable), so a partial combine surfaces through the
/// normal `surface()` message box instead of claiming full success - which is what a flat
/// `applied(1, 1)` used to do for a 10-image combine with three unreadable pages.
fn combine_action<E: std::fmt::Debug>(
    paths: &[String],
    ext: &str,
    what: &str,
    note: &str,
    combine: impl FnOnce(&[String], &std::path::Path) -> std::result::Result<usize, E>,
) -> ActionReport {
    let imgs: Vec<String> = paths
        .iter()
        .filter(|p| is_image(p.as_str()))
        .cloned()
        .collect();
    if imgs.is_empty() {
        return ActionReport::default();
    }
    // Hold the slot for the whole write.
    let slot = combined_path(&imgs[0], ext);
    let out = slot.path().to_path_buf();
    match combine(&imgs, &out) {
        Ok(dropped) => {
            let attempted = imgs.len();
            let done = attempted.saturating_sub(dropped);
            let report = ActionReport {
                output: Some(out),
                ..ActionReport::applied(attempted, done)
            };
            if dropped > 0 {
                let plural = if dropped == 1 { "" } else { "s" };
                report.with_note(format!("{dropped} image{plural} couldn't be read"))
            } else {
                report
            }
        }
        Err(e) => {
            crate::safety::log(&format!("{what} failed: {e:?}"));
            ActionReport::applied(1, 0).with_note(note)
        }
    }
}

/// `VerbAction::CombineToCbz`.
fn handle_combine_to_cbz(paths: &[String]) -> ActionReport {
    combine_action(
        paths,
        "cbz",
        "Combine to CBZ",
        "couldn't build the CBZ archive",
        |imgs, out| {
            combine_to_cbz(imgs, out, super::OnOmit::Report).map(|combined| combined.omitted.len())
        },
    )
}

/// `VerbAction::Ocr`.
fn handle_ocr(paths: &[String]) -> ActionReport {
    first_image_action(paths, "OCR", "couldn't read text from the image", |p| {
        crate::ocr::ocr_to_clipboard(p)
    })
}

/// `VerbAction::StripMetadata` - per-image, on the batch pool. Routed per file to
/// `st2k strip` (helper-if-present), else in-process `strip::strip_metadata`;
/// `strip_one` returns the same success bool, so attempted/done/note are identical to
/// the old sequential loop.
fn handle_strip_metadata(paths: &[String]) -> ActionReport {
    let exe = st2k_exe();
    let exe_ref = exe.as_deref();
    let imgs = images_in(paths);
    let oks = crate::parallel::map(&imgs, |_, p| strip_one(exe_ref, p));
    let attempted = imgs.len();
    let done = oks.iter().filter(|&&ok| ok).count();
    let mut r = ActionReport::applied(attempted, done);
    if done < attempted {
        r.note = Some("couldn't rewrite the file without metadata".into());
    }
    r
}

/// `VerbAction::ResizeImg` - per-image, on the batch pool. Routed per file to
/// `st2k convert --resize` (helper-if-present), else in-process `resize_file`; ordered
/// results give the same attempted/done/note + first-reveal as the old loop.
fn handle_resize_img(paths: &[String], r: Resize) -> ActionReport {
    let exe = st2k_exe();
    let exe_ref = exe.as_deref();
    per_file_action(
        &images_in(paths),
        "Resize",
        "couldn't resize some images",
        |p| resize_one(exe_ref, p, r),
    )
}

/// The images in a selection, the per-image verbs' input.
fn images_in(paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .filter(|p| is_image(p.as_str()))
        .cloned()
        .collect()
}

/// `VerbAction::ShrinkForEmail` - per-image, on the batch pool. Routed per file to
/// `st2k convert --resize` (helper-if-present), else in-process `shrink_for_email`;
/// ordered results give the same attempted/done/note + first-reveal as the old loop.
fn handle_shrink_for_email(paths: &[String], size: EmailSize) -> ActionReport {
    let exe = st2k_exe();
    let exe_ref = exe.as_deref();
    per_file_action(
        &images_in(paths),
        "Shrink for email",
        "couldn't shrink some images",
        |p| shrink_one(exe_ref, p, size),
    )
}

/// `VerbAction::SaveVideoFrame` - per-video, on the batch pool. Routed ALWAYS to
/// `st2k thumbnail <in> <out> --size 0` (see the module doc's routing list) — video
/// decode must never run in-process inside `explorer.exe`, so unlike every other
/// handler here there is no in-process fallback: with no helper installed, every
/// video is counted attempted-but-failed rather than silently decoding in the shell
/// host.
fn handle_save_video_frame(paths: &[String]) -> ActionReport {
    let exe = st2k_exe();
    let exe_ref = exe.as_deref();
    let vids: Vec<String> = paths
        .iter()
        .filter(|p| is_video(p.as_str()))
        .cloned()
        .collect();
    let note = if exe_ref.is_none() {
        "the st2k helper is required to save a video frame"
    } else {
        "couldn't extract a frame from some videos"
    };
    per_file_action(&vids, "Save video frame", note, |p| {
        save_video_frame_one(exe_ref, p).ok()
    })
}

#[cfg(test)]
mod tests;
