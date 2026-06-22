//! SageThumbs 2K — a modern, crash-isolated Rust shell extension.
//!
//! In-proc COM surface: an `IThumbnailProvider` (+ `IInitializeWithStream`), the
//! classic owner-drawn `IContextMenu`, and the modern `IExplorerCommand`. Decode
//! is tiered — `image` crate → WIC → ImageMagick subprocess → headerless TGA
//! (see `decode.rs`). This crate also builds the Options/CLI EXEs.

#![allow(non_snake_case)]

pub mod app_image;
pub mod clipboard;
mod command;
mod container;
mod contextmenu;
mod decode;
mod dib;
pub mod cli;
mod factory;
pub mod formats;
mod fsutil;
mod guids;
mod jpegtran;
pub mod mcp;
mod mkv;
mod mp4;
mod ocr;
// Internal batch thread pool (Convert dialog / Combine / multi-file context-menu
// verbs). `pub` so the companion `sagethumbs2k-app` bin can drive it, `doc(hidden)`
// because it isn't a stable public API — just a shared helper across our own crates.
#[doc(hidden)]
pub mod parallel;
mod pdf;
mod previewhandler;
mod propstore;
mod strip;
mod topdf;
pub mod i18n;
pub mod register;
pub mod safety;
pub mod settings;
mod thumbprovider;
mod verbs;
mod video;
mod vstream;

/// Conversion API surfaced for the companion app's Convert… dialog.
pub use topdf::combine_to_pdf;
pub use strip::read_info_verbose;
pub use verbs::{
    convert_file_opts, convert_image_to_pdf_in, convert_to_magick_in, default_menu_tokens,
    files_to_folder, tags_to_folders, ConvertOpts, Resize, Target, MENU_SEP_TOKEN,
};

/// Is ImageMagick available? Gates the magick-backed Convert targets (PSD/DDS/…),
/// which are hidden on a compact install.
pub fn magick_available() -> bool {
    decode::magick_available()
}

use core::ffi::c_void;
use std::sync::atomic::{AtomicI64, AtomicIsize, Ordering};

use windows::core::{Error, Interface, GUID, HRESULT};
use windows::Win32::Foundation::{CLASS_E_CLASSNOTAVAILABLE, E_FAIL, E_POINTER, HMODULE, S_FALSE, S_OK};
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
    debug_assert!(prev.is_ok(), "MODULE_REFS underflow: unbalanced LockServer(FALSE)/release");
}

/// Test/diagnostics hook: decode a file's bytes the same way the thumbnail
/// provider does (incl. ebook/comic cover extraction) and report the size.
#[doc(hidden)]
pub fn probe_cover(bytes: &[u8]) -> Option<(u32, u32)> {
    // decode_preview, not decode_full: this probes the THUMBNAIL path (container
    // covers included) — full fidelity would bypass the container tier for PSD.
    decode::decode_preview(bytes).ok().map(|img| (img.width(), img.height()))
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
    let bytes = std::fs::read(path).ok()?;
    ocr::recognize_bytes(&bytes).ok().filter(|t| !t.trim().is_empty())
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
        {
            return CLASS_E_CLASSNOTAVAILABLE;
        }
        let factory: IClassFactory = factory::ClassFactory::new(clsid).into();
        factory.query(riid, ppv)
    })
}

pub fn dll_register_server() -> HRESULT {
    safety::guard_hr(|| match module_path().and_then(|p| register::register(&p)) {
        Ok(()) => S_OK,
        Err(e) => e.code(),
    })
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
/// `windows` crate's `Threading` feature that exports this isn't enabled.)
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// File names of the artifacts this crate builds, all installed side-by-side. The
/// co-located layout is an install contract; keeping the names here means a rename
/// is one edit and a typo can't silently break a spawn/icon lookup with no error.
pub(crate) const APP_EXE: &str = "sagethumbs2k-app.exe";
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
