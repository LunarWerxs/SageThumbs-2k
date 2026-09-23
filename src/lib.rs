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

mod badge;
pub mod cli;
mod command;
mod contextmenu;
pub mod doctor;
mod factory;
pub mod foldermenu;
pub mod mcp;
// The transparency checkerboard, re-exported from the classic menu tile's painter so the app bin's
// Quick preview draws the SAME backdrop, from the SAME `settings::preview_checker()` toggle. One
// implementation, so the two surfaces cannot drift apart. `doc(hidden)` for the same reason as the
// rest of these: an internal surface, not a stable public API.
#[doc(hidden)]
pub mod checker {
    pub use crate::contextmenu::paint::{checker_shades, fill_checker};
    /// The pixel-space twin, for the one surface that has no device context to draw on:
    /// the Explorer thumbnail bitmap. See [`st2k_base::checkerpx`].
    pub use st2k_base::checkerpx::compose_under;
}
pub mod prebuild;
mod previewhandler;
mod propstore;
pub mod register;
mod thumbprovider;
mod topdf;
// Explorer's own file-type icon overlay, and how to make it stop covering our badge.
#[doc(hidden)]
pub mod typeoverlay;
mod verbs;

pub use topdf::{combine_to_pdf, combine_to_pdf_paged};
pub use verbs::{
    convert_file_opts, convert_file_opts_named, convert_image_to_pdf_in,
    convert_to_magick_in_named, copy_rgba_to_clipboard, copy_to_clipboard, default_menu_tokens,
    files_to_folder, rename_by_pattern, rename_pattern_preview, resize_file, run_action,
    tags_to_folders, BatchReport, Combined, ConvertOpts, Corner, FileOutcome, FileStatus,
    OmitCause, Omitted, OnOmit, Resize, Target, Transform, VerbAction, Watermark, MENU_SEP_TOKEN,
};

use core::ffi::c_void;
use st2k_base::{guids, host, safety};
use st2k_codecs::{decode, ocr, video};
use std::time::Duration;

use windows::core::{Interface, GUID, HRESULT};
use windows::Win32::Foundation::{CLASS_E_CLASSNOTAVAILABLE, E_POINTER, S_FALSE, S_OK};
use windows::Win32::System::Com::IClassFactory;

const DLL_PROCESS_ATTACH: u32 = 1;
const DLL_PROCESS_DETACH: u32 = 0;

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

// COM entry-point IMPLEMENTATIONS. These used to be the `#[no_mangle] extern "system"`
// `Dll*` exports directly; they now live as plain `pub fn`s here (the rlib `core`),
// and the thin `sagethumbs2k` cdylib crate (`dll/`) wraps each in a `#[no_mangle]`
// shim. Splitting the cdylib into its own crate means NO crate is both `cdylib` AND
// `rlib`, which eliminates the intermittent cargo#6313 link collision that broke CI.

/// DllMain: capture our `HMODULE` (as a raw `isize`, so the cdylib shim needs no
/// `windows` types) on process-attach to resolve our own path later.
///
/// On process-detach with a null `reserved` (the DLL is being unloaded by `FreeLibrary`
/// while the process lives on; a non-null value means the process itself is exiting and
/// the OS reclaims everything) the classic menu's cached logo bitmap is released. That
/// bitmap is created once per LOAD, so without this every unload/reload cycle of the DLL
/// inside a long-lived `explorer.exe` leaked one GDI object.
pub fn dll_main(hmodule: isize, reason: u32, reserved: *mut c_void) {
    if reason == DLL_PROCESS_ATTACH {
        host::set_module(hmodule);
    } else if reason == DLL_PROCESS_DETACH && reserved.is_null() {
        contextmenu::free_menu_logo();
    }
}

pub fn dll_can_unload_now() -> HRESULT {
    // == 0 (not <= 0): the count is now clamped at zero in `dll_release`, so it can
    // never go negative; testing `<= 0` would fail dangerous on a hypothetical
    // underflow by reporting "safe to unload" while an object is still live.
    let refs = host::live_refs();
    if refs == 0 {
        return S_OK;
    }
    // ISSUE #35 self-heal. A decode worker wedged inside Media Foundation holds a
    // `ModuleRef` forever (it must: unloading the DLL under a running thread is a crash),
    // so this host can never report S_OK again and the surrogate never recycles - one
    // spinning core, and a Media Foundation that may be locked for every other file, until
    // the user reboots. When the ONLY pins left are such workers, nobody has asked this host
    // for an object in a while, and the host is one of the shell's disposable surrogates,
    // exit instead: Explorer starts a fresh surrogate on the next request, which is exactly
    // what a reboot bought the reporter. Never in explorer.exe itself or any other host.
    let view = WedgedHostView {
        refs,
        stranded: video::stranded_workers(),
        oldest_strand: video::oldest_strand_age(),
        idle_for: host::idle_for(),
        host: current_host_exe(),
    };
    if should_exit_wedged_host(&view) {
        safety::log(&format!(
            "host: every remaining pin ({}) is a decode worker wedged inside Media Foundation \
             for {:?}, and nothing has asked this host for an object in {:?}; exiting this \
             surrogate so Explorer starts a fresh one (issue #35)",
            view.refs,
            view.oldest_strand.unwrap_or_default(),
            view.idle_for
        ));
        unsafe { windows::Win32::System::Threading::ExitProcess(0) };
    }
    S_FALSE
}

/// How long a host must have gone without an add-ref before the wedged-host exit may
/// fire. Long enough that a user still browsing a folder (requests every few seconds) is
/// never interrupted; the exit is for the host they have walked away from.
const WEDGED_HOST_IDLE: Duration = Duration::from_secs(30);

/// Everything [`should_exit_wedged_host`] looks at, gathered so the decision is a pure
/// function that can be tested without a wedged worker or a real surrogate.
pub(crate) struct WedgedHostView {
    /// Live `MODULE_REFS`.
    pub refs: i64,
    /// Stranded workers still running (`video::stranded_workers`), each holding one ref.
    pub stranded: usize,
    /// Age of the oldest of those, `None` when there are none.
    pub oldest_strand: Option<Duration>,
    /// Time since the last add-ref.
    pub idle_for: Duration,
    /// The host process's executable name.
    pub host: Option<String>,
}

/// The wedged-host exit decision: the host is a disposable shell surrogate, every remaining
/// reference is a stranded worker, the oldest strand is past `video::STRAND_GRACE` (stuck,
/// not slow), and nothing has asked for an object in [`WEDGED_HOST_IDLE`]. Any other
/// combination keeps the host alive: a class-factory lock, a live object, a worker that may
/// yet finish, a host that is still busy, or a host we do not own.
pub(crate) fn should_exit_wedged_host(v: &WedgedHostView) -> bool {
    let disposable = v.host.as_deref().is_some_and(|h| {
        h.eq_ignore_ascii_case("dllhost.exe") || h.eq_ignore_ascii_case("prevhost.exe")
    });
    disposable
        && v.stranded > 0
        && v.refs == v.stranded as i64
        && v.oldest_strand
            .is_some_and(|age| age >= video::STRAND_GRACE)
        && v.idle_for >= WEDGED_HOST_IDLE
}

/// The file name of the process hosting this DLL (`dllhost.exe`, `prevhost.exe`,
/// `explorer.exe`, a test runner, ...).
fn current_host_exe() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    exe.file_name()?.to_str().map(str::to_string)
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
        || match host::module_path().and_then(|p| register::register(&p)) {
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
            let module = st2k_base::host::ModuleRef::default(); // exactly what the budgeted workers now do
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

    /// The issue #35 surrogate exit fires for exactly one shape of host and no other. Each
    /// row below flips ONE condition of the healthy-to-exit case; every one of them must keep
    /// the host alive, because the alternative is killing a process somebody is using.
    #[test]
    fn wedged_host_exit_needs_every_condition() {
        use super::{should_exit_wedged_host, WedgedHostView, WEDGED_HOST_IDLE};
        use std::time::Duration;
        let grace = st2k_codecs::video::STRAND_GRACE;
        let ok = || WedgedHostView {
            refs: 2,
            stranded: 2,
            oldest_strand: Some(grace),
            idle_for: WEDGED_HOST_IDLE,
            host: Some("DllHost.exe".into()),
        };
        assert!(
            should_exit_wedged_host(&ok()),
            "the all-conditions-met case must exit"
        );
        assert!(should_exit_wedged_host(&WedgedHostView {
            host: Some("prevhost.exe".into()),
            ..ok()
        }));

        let alive = [
            (
                "a live object or LockServer pins us too",
                WedgedHostView { refs: 3, ..ok() },
            ),
            (
                "nothing is stranded",
                WedgedHostView {
                    refs: 0,
                    stranded: 0,
                    oldest_strand: None,
                    ..ok()
                },
            ),
            (
                "the strand is slow, not yet stuck",
                WedgedHostView {
                    oldest_strand: Some(grace - Duration::from_secs(1)),
                    ..ok()
                },
            ),
            (
                "the host is still being used",
                WedgedHostView {
                    idle_for: WEDGED_HOST_IDLE - Duration::from_secs(1),
                    ..ok()
                },
            ),
            (
                "this is Explorer itself",
                WedgedHostView {
                    host: Some("explorer.exe".into()),
                    ..ok()
                },
            ),
            (
                "this is the app",
                WedgedHostView {
                    host: Some("SageThumbs2K.exe".into()),
                    ..ok()
                },
            ),
            ("the host is unknown", WedgedHostView { host: None, ..ok() }),
        ];
        for (why, view) in alive {
            assert!(
                !should_exit_wedged_host(&view),
                "must stay alive when {why}"
            );
        }
    }
}
