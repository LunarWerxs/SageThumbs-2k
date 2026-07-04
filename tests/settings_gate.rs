//! Verifies the Options settings actually gate the real thumbnail provider,
//! driven in-process exactly like Explorer (DllGetClassObject → CreateInstance
//! → Initialize(IStream) → GetThumbnail) against the freshly-built cdylib.
//!
//! HERMETIC: instead of mutating the developer's live `HKCU\Software\SageThumbs2K`
//! (the old approach — it save/restored, but a panic mid-test could leak a changed
//! EnableThumbs/MaxSize onto the box), this redirects the DLL's settings reads to a
//! THROWAWAY subkey via `ST2K_SETTINGS_ROOT` (honored by `settings::hkcu_root`). The
//! test writes EnableThumbs/MaxSize into that scratch key and reads the provider's
//! response; the user's real settings are never touched. The scratch key is wiped at
//! the start (clean slate) and end. Redirection makes it safe to run in the normal
//! suite, so it's no longer `#[ignore]`d.
//!
//! Run via `scripts/test.ps1` (build before test) so LoadLibrary gets a fresh cdylib —
//! plain `cargo test` does not refresh target/<profile>/*.dll.
#![cfg(windows)]

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;

use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use windows::core::{s, Error, Interface, Result, GUID, HRESULT, PCWSTR};
use windows::Win32::Foundation::{E_FAIL, HMODULE};
use windows::Win32::Graphics::Gdi::{DeleteObject, HBITMAP};
use windows::Win32::System::Com::{CoInitializeEx, IClassFactory, IStream, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Shell::PropertiesSystem::IInitializeWithStream;
use windows::Win32::UI::Shell::{SHCreateMemStream, IThumbnailProvider, WTS_ALPHATYPE, WTSAT_UNKNOWN};
use windows_registry::CURRENT_USER;

const CLSID_THUMBNAIL_PROVIDER: GUID = GUID::from_u128(0x7B2E6A14_9C3D_4F8A_B1E7_2A5D9F0C6E31);
/// Throwaway HKCU subkey the DLL's settings reads are redirected to (via ST2K_SETTINGS_ROOT),
/// so this test never touches the developer's real `Software\SageThumbs2K` values. A child of
/// the real root, but isolated: creating/removing it leaves the parent's own values alone.
const TEST_ROOT: &str = r"Software\SageThumbs2K\__test_gate";

type DllGetClassObjectFn =
    unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;

fn dll_path() -> std::path::PathBuf {
    let exe = std::env::current_exe().unwrap();
    exe.parent().unwrap().parent().unwrap().join("sagethumbs2k.dll")
}

/// Run a full GetThumbnail handshake on `bytes`; Ok means a thumbnail was
/// produced, Err means the provider declined (disabled / oversized / undecodable).
unsafe fn get_thumbnail(bytes: &[u8], cx: u32) -> Result<()> {
    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    let path = dll_path();
    // The cdylib is a build artifact that MUST be present for this in-process
    // probe — there is no meaningful "skip" here. If it's missing the harness
    // built the test but not the DLL (e.g. `cargo test` instead of a full
    // `cargo build --release`), so PANIC loudly rather than letting a missing
    // artifact look like a pass.
    assert!(
        path.exists(),
        "cdylib not built at {path:?} — run `cargo build --release` first (this is a \
         build-artifact precondition, not an environment skip)"
    );
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let module: HMODULE = LoadLibraryW(PCWSTR(wide.as_ptr()))?;
    let proc = GetProcAddress(module, s!("DllGetClassObject")).ok_or_else(|| Error::from(E_FAIL))?;
    let dll_get_class_object: DllGetClassObjectFn = std::mem::transmute(proc);

    let mut factory_ptr: *mut c_void = std::ptr::null_mut();
    dll_get_class_object(&CLSID_THUMBNAIL_PROVIDER, &IClassFactory::IID, &mut factory_ptr).ok()?;
    let factory = IClassFactory::from_raw(factory_ptr);
    let init: IInitializeWithStream = factory.CreateInstance(None)?;
    let stream: IStream = SHCreateMemStream(Some(bytes)).ok_or_else(|| Error::from(E_FAIL))?;
    init.Initialize(&stream, 0)?;
    let provider: IThumbnailProvider = init.cast()?;
    let mut hbmp = HBITMAP::default();
    let mut alpha: WTS_ALPHATYPE = WTSAT_UNKNOWN;
    provider.GetThumbnail(cx, &mut hbmp, &mut alpha)?;
    if !hbmp.is_invalid() {
        let _ = DeleteObject(hbmp.into());
    }
    Ok(())
}

fn encode(img: RgbaImage, fmt: ImageFormat) -> Vec<u8> {
    let mut bytes = Vec::new();
    DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut bytes), fmt)
        .unwrap();
    bytes
}

fn solid(w: u32, h: u32, rgba: [u8; 4]) -> RgbaImage {
    let mut img = RgbaImage::new(w, h);
    for p in img.pixels_mut() {
        *p = Rgba(rgba);
    }
    img
}

/// Write a DWORD into the throwaway settings root the DLL is redirected to. No save/restore:
/// the whole key is scratch and gets wiped at the end, so we just set what each step needs.
fn put(name: &str, value: u32) {
    CURRENT_USER.create(TEST_ROOT).unwrap().set_u32(name, value).unwrap();
}

/// Delete the throwaway settings key (idempotent). The user's real `Software\SageThumbs2K`
/// values live directly under the parent and are untouched by removing this child subtree.
fn reset_scratch() {
    let _ = CURRENT_USER.remove_tree(TEST_ROOT);
}

#[test]
fn settings_gate_the_provider() {
    // Redirect the DLL's settings reads to the scratch key BEFORE it's loaded — the first
    // get_thumbnail LoadLibrary's the cdylib, whose `settings::hkcu_root` caches this env var
    // once. Only the ROOT PATH is cached; the provider still re-reads the VALUES per
    // GetThumbnail (see settings::thumb_settings), so flipping them between calls takes effect.
    std::env::set_var("ST2K_SETTINGS_ROOT", TEST_ROOT);
    reset_scratch(); // clean slate — no stale values from a prior aborted run

    let small = encode(solid(80, 60, [10, 200, 30, 255]), ImageFormat::Png);
    // Uncompressed BMP that is comfortably over 1 MB but under the 100 MB default.
    let big = encode(solid(700, 700, [120, 60, 200, 255]), ImageFormat::Bmp);
    assert!(big.len() > 1024 * 1024, "BMP fixture should exceed 1 MB, got {}", big.len());

    // --- EnableThumbs gate ---
    put("EnableThumbs", 1);
    let enabled = unsafe { get_thumbnail(&small, 64) };
    put("EnableThumbs", 0);
    let disabled = unsafe { get_thumbnail(&small, 64) };

    // --- MaxSize gate (EnableThumbs back on for this part) ---
    put("EnableThumbs", 1);
    put("MaxSize", 100);
    let under_limit = unsafe { get_thumbnail(&big, 64) };
    put("MaxSize", 1);
    let over_limit = unsafe { get_thumbnail(&big, 64) };

    reset_scratch(); // drop the throwaway key; the user's real settings were never touched

    assert!(enabled.is_ok(), "EnableThumbs=1 should produce a thumbnail");
    assert!(disabled.is_err(), "EnableThumbs=0 should decline (E_FAIL)");
    assert!(under_limit.is_ok(), "a ~1.9 MB file under a 100 MB MaxSize should thumbnail");
    assert!(over_limit.is_err(), "the same file over a 1 MB MaxSize should be skipped");
}
