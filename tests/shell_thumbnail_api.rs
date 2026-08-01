//! Our thumbnails must be reachable through the GENERIC shell API, not just from
//! Explorer's own view.
//!
//! Icaros has an open report that its thumbnails never appear in Windows 11's
//! Settings > Personalization wallpaper and lock-screen pickers (their #236),
//! suspected to be the sandboxed host rejecting the handler. Those pickers do not
//! use anything private: like the file-open dialog, the taskbar, and most
//! first-party UI, they ask for a thumbnail through `IShellItemImageFactory`.
//!
//! So that is what this exercises. `SIIGBF_THUMBNAILONLY` is the load-bearing
//! flag: without it the shell happily substitutes a generic file-type ICON and
//! every assertion would pass while showing the user nothing. With it, a success
//! means a real thumbnail came from a registered provider — ours.
//!
//! Ignored by default because it needs the DLL actually REGISTERED on this
//! machine, which a plain `cargo test` on a dev box cannot assume:
//!
//! `cargo test --test shell_thumbnail_api -- --ignored --test-threads=1`
#![cfg(windows)]

use windows::core::PCWSTR;
use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{DeleteObject, GetObjectW, BITMAP};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::Shell::{
    IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_THUMBNAILONLY,
};

/// Formats Windows has no thumbnail of its own for, so a returned bitmap can only
/// have come from us. Deliberately spread across decode tiers: a container preview
/// extraction, an ebook cover, a document first page, and album art.
const CASES: &[&str] = &[
    "sample.psd",
    "sample.epub",
    "sample.cbz",
    "sample.heic",
    "sample.flac",
];

fn corpus() -> std::path::PathBuf {
    // `..\test-corpus`, a sibling of the project root.
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("project root has a parent")
        .join("test-corpus")
}

/// Ask the shell for a thumbnail exactly the way a picker does.
unsafe fn shell_thumbnail(path: &std::path::Path, edge: i32) -> Result<(i32, i32), String> {
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let factory: IShellItemImageFactory =
        SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None).map_err(|e| format!("{e:?}"))?;
    let hbmp = factory
        .GetImage(
            SIZE { cx: edge, cy: edge },
            // THUMBNAILONLY: fail rather than hand back a generic file-type icon.
            // Without this the test proves nothing at all.
            SIIGBF_THUMBNAILONLY,
        )
        .map_err(|e| format!("{e:?}"))?;
    let mut bm = BITMAP::default();
    let got = GetObjectW(
        hbmp.into(),
        std::mem::size_of::<BITMAP>() as i32,
        Some(&mut bm as *mut _ as *mut _),
    );
    let _ = DeleteObject(hbmp.into());
    if got == 0 {
        return Err("GetObjectW failed on the returned bitmap".into());
    }
    Ok((bm.bmWidth, bm.bmHeight))
}

use std::os::windows::ffi::OsStrExt;

#[test]
#[ignore = "needs the shell extension registered on this machine"]
fn formats_windows_cannot_read_thumbnail_through_the_generic_shell_api() {
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED)
            .ok()
            .expect("COM init");
    }
    let dir = corpus();
    let mut checked = 0;
    let mut failures: Vec<String> = Vec::new();

    for name in CASES {
        let p = dir.join(name);
        if !p.exists() {
            continue; // corpus is generated; skip what this checkout does not have
        }
        checked += 1;
        match unsafe { shell_thumbnail(&p, 256) } {
            Ok((w, h)) => {
                if w <= 1 || h <= 1 {
                    failures.push(format!("{name}: degenerate bitmap {w}x{h}"));
                }
            }
            Err(e) => failures.push(format!("{name}: {e}")),
        }
    }
    unsafe { CoUninitialize() };

    assert!(
        checked > 0,
        "no corpus samples found under {} — generate it with scripts\\build-corpus.ps1",
        dir.display()
    );
    assert!(
        failures.is_empty(),
        "the shell could not get a thumbnail for {} of {checked} file(s) through \
         IShellItemImageFactory, which is the API the Settings personalization pickers, \
         the file-open dialog and the taskbar all use:\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}
