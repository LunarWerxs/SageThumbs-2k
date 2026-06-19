//! Verifies the Options settings actually gate the real thumbnail provider,
//! driven in-process exactly like Explorer (DllGetClassObject → CreateInstance
//! → Initialize(IStream) → GetThumbnail) against the freshly-built cdylib.
//!
//! `#[ignore]`d because it MUTATES the live `HKCU\Software\SageThumbs2K` values
//! (it saves and restores them), so it must not run concurrently with the rest
//! of the suite. Run explicitly:
//!
//!   cargo test --release --test settings_gate -- --ignored --test-threads=1
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
const ROOT: &str = r"Software\SageThumbs2K";

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

/// Set a DWORD, returning its prior value (None if absent) for restoration.
fn swap(name: &str, value: u32) -> Option<u32> {
    let key = CURRENT_USER.create(ROOT).unwrap();
    let prev = key.get_u32(name).ok();
    key.set_u32(name, value).unwrap();
    prev
}
fn restore(name: &str, prev: Option<u32>) {
    let key = CURRENT_USER.create(ROOT).unwrap();
    match prev {
        Some(v) => {
            let _ = key.set_u32(name, v);
        }
        None => {
            let _ = key.remove_value(name);
        }
    }
}

#[test]
#[ignore]
fn settings_gate_the_provider() {
    let small = encode(solid(80, 60, [10, 200, 30, 255]), ImageFormat::Png);
    // Uncompressed BMP that is comfortably over 1 MB but under the 100 MB default.
    let big = encode(solid(700, 700, [120, 60, 200, 255]), ImageFormat::Bmp);
    assert!(big.len() > 1024 * 1024, "BMP fixture should exceed 1 MB, got {}", big.len());

    // --- EnableThumbs gate ---
    let prev_en = swap("EnableThumbs", 1);
    let enabled = unsafe { get_thumbnail(&small, 64) };
    let _ = swap("EnableThumbs", 0);
    let disabled = unsafe { get_thumbnail(&small, 64) };
    restore("EnableThumbs", prev_en);

    // --- MaxSize gate (EnableThumbs back on for this part) ---
    let prev_en2 = swap("EnableThumbs", 1);
    let prev_max = swap("MaxSize", 100);
    let under_limit = unsafe { get_thumbnail(&big, 64) };
    let _ = swap("MaxSize", 1);
    let over_limit = unsafe { get_thumbnail(&big, 64) };
    restore("MaxSize", prev_max);
    restore("EnableThumbs", prev_en2);

    // Assert only after all mutations are restored.
    assert!(enabled.is_ok(), "EnableThumbs=1 should produce a thumbnail");
    assert!(disabled.is_err(), "EnableThumbs=0 should decline (E_FAIL)");
    assert!(under_limit.is_ok(), "a ~1.9 MB file under a 100 MB MaxSize should thumbnail");
    assert!(over_limit.is_err(), "the same file over a 1 MB MaxSize should be skipped");
}
