//! SageThumbs 2K — a modern, crash-isolated Rust shell extension.
//!
//! In-proc COM surface: an `IThumbnailProvider` (+ `IInitializeWithStream`), the
//! classic owner-drawn `IContextMenu`, and the modern `IExplorerCommand`. Decode
//! is tiered — `image` crate → WIC → ImageMagick subprocess → headerless TGA
//! (see `decode.rs`). This crate also builds the Options/CLI EXEs.

#![allow(non_snake_case)]
// This crate compiles INTO the shell-extension DLL (via the `sagethumbs2k-dll`
// cdylib) and runs in-process inside explorer.exe / dllhost / prevhost under
// `panic = "abort"`, so a stray `.unwrap()`/`.expect()` on hostile input aborts
// the user's whole shell. Forbid them on this surface: use `?`, `ok_or`,
// `unwrap_or`, or a `match` instead. Tests are exempt (clippy.toml
// `allow-unwrap-in-tests`), where an unwrap is just an assertion. The binary
// crates (`src/bin/*` — the Options/CLI EXEs) are their OWN crate roots and do
// NOT inherit this, by design: a panic there crashes only that process, and the
// gate is reserved for the code that shares the shell's address space.
#![warn(clippy::unwrap_used, clippy::expect_used)]

pub mod app_image;
// `pub` only because `settings::ThumbSettings` carries a `badge::BadgeStyle` field; the
// module is an internal drawing detail, not a stable API.
#[doc(hidden)]
pub mod badge;
pub mod clipboard;
mod command;
mod container;
/// Archive entry listing for the Quick preview viewer (no extraction).
pub use container::list_archive;
pub mod cli;
mod contextmenu;
pub mod decode;
mod dib;
pub mod doctor;
mod factory;
pub mod formats;
mod fsutil;
// Structure-aware mutation fuzzing of the pure-Rust parsers, compiled only for tests.
#[cfg(test)]
mod fuzz;
mod guids;
mod jpegtran;
pub mod mcp;
mod mkv;
mod mp4;
// In-box WinRT OCR (`Windows.Media.Ocr`). `pub` so the companion `SageThumbs2K` app bin
// can read text out of a screen capture it already holds in memory, `doc(hidden)` because
// it isn't a stable public API — same arrangement as `parallel` below.
#[doc(hidden)]
pub mod ocr;
// The transparency checkerboard, re-exported from the classic menu tile's painter so the app bin's
// Quick preview draws the SAME backdrop, from the SAME `settings::preview_checker()` toggle. One
// implementation, so the two surfaces cannot drift apart. `doc(hidden)` for the same reason as the
// rest of these: an internal surface, not a stable public API.
#[doc(hidden)]
pub mod checker {
    /// The pixel-space twin, for the one surface that has no device context to draw on:
    /// the Explorer thumbnail bitmap. See [`crate::checkerpx`].
    pub use crate::checkerpx::compose_under;
    pub use crate::contextmenu::paint::{checker_shades, fill_checker};
}
mod checkerpx;
// Internal batch thread pool (Convert dialog / Combine / multi-file context-menu
// verbs). `pub` so the companion `SageThumbs2K` app bin can drive it, `doc(hidden)`
// because it isn't a stable public API — just a shared helper across our own crates.
pub mod i18n;
#[doc(hidden)]
pub mod parallel;
pub mod pdf;
mod previewhandler;
mod propstore;
pub mod register;
pub mod safety;
pub mod settings;
pub mod shellcmd;
mod streamsrc;
mod strip;
mod thumbprovider;
// Explorer's own file-type icon overlay, and how to make it stop covering our badge.
mod topdf;
#[doc(hidden)]
pub mod typeoverlay;
pub mod upload_config;
mod verbs;
// `pub` only so the app EXE's preview player can ask `media_foundation_available()`
// before touching the delay-loaded MF imports; the decode entry points stay internal.
pub mod vcodec;
pub mod video;
mod vstream;

pub use strip::read_info_verbose;
/// Conversion API surfaced for the companion app's Convert… dialog.
pub use topdf::{combine_to_pdf, combine_to_pdf_paged, PdfPage};
pub use verbs::{
    convert_file_opts, convert_file_opts_named, convert_image_to_pdf_in, convert_to_magick_in,
    convert_to_magick_in_named, copy_rgba_to_clipboard, copy_to_clipboard, default_menu_tokens,
    files_to_folder, resize_file, run_action, tags_to_folders, ConvertOpts, Resize, Target,
    Transform, VerbAction, MENU_SEP_TOKEN,
};

/// Is ImageMagick available? Gates the magick-backed Convert targets (PSD/DDS/…),
/// which are hidden on a compact install.
pub fn magick_available() -> bool {
    decode::magick_available()
}

use core::ffi::c_void;
use std::sync::atomic::{AtomicI64, AtomicIsize, Ordering};

use windows::core::{Error, Interface, GUID, HRESULT};
use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, E_FAIL, E_POINTER, HMODULE, S_FALSE, S_OK,
};
use windows::Win32::System::Com::IClassFactory;
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;

/// Live-object + lock count. `DllCanUnloadNow` returns S_OK only at zero.
static MODULE_REFS: AtomicI64 = AtomicI64::new(0);
/// This DLL's HMODULE, captured in DllMain, used to resolve our own path.
static HMODULE_PTR: AtomicIsize = AtomicIsize::new(0);

const DLL_PROCESS_ATTACH: u32 = 1;

pub fn dll_add_ref() {
    MODULE_REFS.fetch_add(1, Ordering::SeqCst);
}

pub fn dll_release() {
    // Clamp at zero: a stray/unbalanced release must NOT push the count negative,
    // or it could cancel a live object's reference and let the DLL unload while in
    // use. `fetch_update` leaves a zero count untouched and only ever decrements.
    let prev = MODULE_REFS.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
        if n > 0 {
            Some(n - 1)
        } else {
            None // already zero — refuse to underflow
        }
    });
    debug_assert!(
        prev.is_ok(),
        "MODULE_REFS underflow: unbalanced LockServer(FALSE)/release"
    );
}

/// Test/diagnostics hook: decode a file's bytes the same way the thumbnail
/// provider does (incl. ebook/comic cover extraction) and report the size.
#[doc(hidden)]
pub fn probe_cover(bytes: &[u8]) -> Option<(u32, u32)> {
    // decode_preview, not decode_full: this probes the THUMBNAIL path (container
    // covers included) — full fidelity would bypass the container tier for PSD.
    decode::decode_preview(bytes)
        .ok()
        .map(|img| (img.width(), img.height()))
}

/// Diagnostics: render the right-click menu preview for `path` to a PNG exactly
/// as the owner-draw paints it. `bg` = `None` for the live menu color, or
/// `Some(0x00RRGGBB)` to preview a chosen menu background.
#[doc(hidden)]
pub fn render_preview_png(path: &str, out_png: &str, bg: Option<u32>) -> bool {
    contextmenu::render_preview_png(path, out_png, bg)
}

/// Test/diagnostics hook: OCR an image file to text (the same path the "Copy
/// text" verb uses, minus the clipboard write). None if no OCR pack / no text.
#[doc(hidden)]
pub fn ocr_probe(path: &str) -> Option<String> {
    ocr::recognize_bytes(std::fs::read(path).ok()?)
        .ok()
        .filter(|t| !t.trim().is_empty())
}

/// RAII module-reference guard. Constructing one (via `Default`) bumps the
/// live-object count; dropping it releases. Each COM coclass carries a
/// `_ref: ModuleRef` field instead of hand-writing an add-ref in its
/// constructor and a matching `impl Drop` — six identical pairs collapse to
/// this one type. (The factory's `LockServer` count is a separate add/release
/// path and intentionally does NOT use this.)
pub struct ModuleRef;

impl Default for ModuleRef {
    fn default() -> Self {
        dll_add_ref();
        ModuleRef
    }
}

impl Drop for ModuleRef {
    fn drop(&mut self) {
        dll_release();
    }
}

// COM entry-point IMPLEMENTATIONS. These used to be the `#[no_mangle] extern "system"`
// `Dll*` exports directly; they now live as plain `pub fn`s here (the rlib `core`),
// and the thin `sagethumbs2k` cdylib crate (`dll/`) wraps each in a `#[no_mangle]`
// shim. Splitting the cdylib into its own crate means NO crate is both `cdylib` AND
// `rlib`, which eliminates the intermittent cargo#6313 link collision that broke CI.

/// DllMain: capture our `HMODULE` (as a raw `isize`, so the cdylib shim needs no
/// `windows` types) on process-attach to resolve our own path later.
pub fn dll_main(hmodule: isize, reason: u32) {
    if reason == DLL_PROCESS_ATTACH {
        HMODULE_PTR.store(hmodule, Ordering::SeqCst);
    }
}

pub fn dll_can_unload_now() -> HRESULT {
    // == 0 (not <= 0): the count is now clamped at zero in `dll_release`, so it can
    // never go negative; testing `<= 0` would fail dangerous on a hypothetical
    // underflow by reporting "safe to unload" while an object is still live.
    if MODULE_REFS.load(Ordering::SeqCst) == 0 {
        S_OK
    } else {
        S_FALSE
    }
}

// The Windows loader calls the cdylib's `DllGetClassObject` by name; this is its body.
// We null-check every pointer before use under the panic guard, so the clippy
// raw-pointer-deref lint doesn't apply.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn dll_get_class_object(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    safety::guard_hr(|| unsafe {
        if rclsid.is_null() || riid.is_null() || ppv.is_null() {
            return E_POINTER;
        }
        *ppv = core::ptr::null_mut();
        let clsid = *rclsid;
        if clsid != guids::CLSID_THUMBNAIL_PROVIDER
            && clsid != guids::CLSID_EXPLORER_COMMAND
            && clsid != guids::CLSID_CONTEXT_MENU
            && clsid != guids::CLSID_PREVIEW_HANDLER
            && clsid != guids::CLSID_PROPERTY_STORE
            // The modern-menu quick verbs (Convert into / Convert… / Resize / Rotate) are their
            // own coclasses, activated via the package surrogate; the factory builds them from
            // the CLSID→MENU mapping in command::QUICK_VERBS.
            && !command::is_quick_clsid(clsid)
        {
            return CLASS_E_CLASSNOTAVAILABLE;
        }
        let factory: IClassFactory = factory::ClassFactory::new(clsid).into();
        factory.query(riid, ppv)
    })
}

pub fn dll_register_server() -> HRESULT {
    safety::guard_hr(
        || match module_path().and_then(|p| register::register(&p)) {
            Ok(()) => S_OK,
            Err(e) => e.code(),
        },
    )
}

pub fn dll_unregister_server() -> HRESULT {
    safety::guard_hr(|| match register::unregister() {
        Ok(()) => S_OK,
        Err(e) => e.code(),
    })
}

/// `CREATE_NO_WINDOW` process-creation flag. Every helper we spawn from a GUI/shell
/// host (magick, st2k, self) passes it so no console window flashes. Defined here
/// once — a mistyped copy (`0x0080_0000`) would pop a console inside Explorer. (The
/// `windows` crate's `Threading` feature IS enabled now — for `CreateMutexW` — but
/// `std::process::CommandExt::creation_flags` wants a bare `u32` anyway.)
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// NUL-terminated UTF-16 for Win32 `*W` APIs. Was independently re-typed as
/// `s.encode_utf16().chain(once(0)).collect()` across half a dozen files (command /
/// contextmenu / propstore / actions / cli / container::select) — one shared helper
/// means the pattern can't drift and reads as intent at the call site. (The app bin
/// has its own `win::wide` twin; bins keep using that one.)
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(core::iter::once(0)).collect()
}

/// File names of the artifacts this crate builds, all installed side-by-side. The
/// co-located layout is an install contract; keeping the names here means a rename
/// is one edit and a typo can't silently break a spawn/icon lookup with no error.
pub(crate) const APP_EXE: &str = "SageThumbs2K.exe";
pub(crate) const CLI_EXE: &str = "st2k.exe";

/// Resolve a sibling file next to OUR DLL (whatever directory the install used).
/// Uses [`module_path`] — NEVER `current_exe()`, which inside the shell host is
/// `explorer.exe`/`dllhost.exe`. Returns the path only if it actually exists, so a
/// DLL-only install (no companion EXE) cleanly yields `None`.
pub(crate) fn sibling_of_dll(name: &str) -> Option<std::path::PathBuf> {
    let dll = module_path().ok()?;
    let p = std::path::Path::new(&dll).parent()?.join(name);
    p.exists().then_some(p)
}

/// This DLL's `HMODULE` (captured in `DllMain`), for use as the `hInstance` of
/// windows/classes we create — e.g. the preview handler's child window.
pub(crate) fn dll_hmodule() -> HMODULE {
    HMODULE(HMODULE_PTR.load(Ordering::SeqCst) as *mut c_void)
}

pub(crate) fn module_path() -> windows::core::Result<String> {
    unsafe {
        let h = HMODULE(HMODULE_PTR.load(Ordering::SeqCst) as *mut c_void);
        let mut buf = vec![0u16; 260];
        loop {
            let n = GetModuleFileNameW(Some(h), &mut buf) as usize;
            if n == 0 {
                return Err(Error::from_thread()); // GetLastError; includes HMODULE-missing
            }
            // n < len is the documented "it fit" signal; n == len means truncated.
            if n < buf.len() {
                return Ok(String::from_utf16_lossy(&buf[..n]));
            }
            if buf.len() >= 32_768 {
                return Err(Error::from(E_FAIL));
            }
            buf.resize((buf.len() * 2).min(32_768), 0);
        }
    }
}

/// Best-effort backing file name/path of a shell-supplied `IStream` (via `IStream::Stat`,
/// which fills `pwcsName` under `STATFLAG_DEFAULT`).
///
/// Shared by the preview handler (which logs it, so a "white preview" report names the exact
/// file) and the thumbnail provider (which needs the extension for the optional format
/// badge). `pwcsName` is a CoTaskMem allocation we own and must free.
pub(crate) unsafe fn stream_name(stream: &windows::Win32::System::Com::IStream) -> Option<String> {
    use windows::Win32::System::Com::{STATFLAG_DEFAULT, STATSTG};
    let mut stat = STATSTG::default();
    stream.Stat(&mut stat, STATFLAG_DEFAULT).ok()?;
    if stat.pwcsName.is_null() {
        return None;
    }
    let s = stat.pwcsName.to_string().ok();
    windows::Win32::System::Com::CoTaskMemFree(Some(stat.pwcsName.0 as *const core::ffi::c_void));
    s
}

#[cfg(test)]
mod unload_guard_tests {
    /// The invariant every DETACHED worker relies on: while a worker holds a [`ModuleRef`],
    /// `DllCanUnloadNow` must report S_FALSE, so the shell can't unload the DLL while that
    /// thread is still executing our code. The budgeted decode workers (preview / property /
    /// video / svg) leak past their wall-clock budget, so without this they let the host
    /// unload the DLL on dialog CLOSE → access-violation crash-on-close. Guards that regression.
    #[test]
    fn detached_worker_ref_blocks_unload() {
        use std::sync::mpsc;
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (ended_tx, ended_rx) = mpsc::channel();
        std::thread::spawn(move || {
            #[allow(clippy::default_constructed_unit_structs)]
            let module = super::ModuleRef::default(); // exactly what the budgeted workers now do
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap(); // hold the ref open until the test releases us
            drop(module);
            ended_tx.send(()).unwrap();
        });
        started_rx.recv().unwrap();
        // Adding a live ref can only ever force S_FALSE — robust under the parallel test
        // harness (other refs only reinforce it), so this assertion is deterministic.
        assert_eq!(
            super::dll_can_unload_now(),
            windows::Win32::Foundation::S_FALSE,
            "a live detached-worker ModuleRef must block DllCanUnloadNow"
        );
        release_tx.send(()).unwrap();
        ended_rx.recv().unwrap();
    }
}
