//! End-to-end COM test for the `IPropertyStore` coclass — drives the built DLL the
//! way Explorer's Details pane / SearchIndexer does:
//!
//!   LoadLibrary(DLL) -> DllGetClassObject -> IClassFactory::CreateInstance
//!   -> QI IInitializeWithFile -> Initialize(path) -> QI IPropertyStore
//!   -> GetCount / GetValue (dimensions) / SetValue (refused) / null-guard.
//!
//! Closes the audit's "PropertyStore has zero COM coverage" gap. Like
//! `com_roundtrip.rs`, run via `scripts/test.ps1` (or `cargo build` first) so the
//! LoadLibrary picks up a fresh cdylib, not a stale one.
#![cfg(windows)]

use std::ffi::{c_void, OsStr};
use std::os::windows::ffi::OsStrExt;

use windows::core::{s, Error, Interface, Result, GUID, HRESULT, PCWSTR};
use windows::Win32::Foundation::{E_FAIL, HMODULE, PROPERTYKEY, STG_E_ACCESSDENIED};
use windows::Win32::Storage::EnhancedStorage::{PKEY_Image_HorizontalSize, PKEY_Image_VerticalSize};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{CoInitializeEx, IClassFactory, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Shell::PropertiesSystem::{IInitializeWithFile, IPropertyStore};

const CLSID_PROPERTY_STORE: GUID = GUID::from_u128(0x5E1A7C92_8F3D_4B6A_A0E4_3C7B9D2F1A68);

type DllGetClassObjectFn =
    unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;

fn dll_path() -> std::path::PathBuf {
    let exe = std::env::current_exe().unwrap();
    exe.parent().unwrap().parent().unwrap().join("sagethumbs2k.dll")
}

fn to_wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// Write a temp PNG of a known size and return its path.
fn write_temp_png(w: u32, h: u32) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("st2k_propstore_{}_{w}x{h}.png", std::process::id()));
    image::DynamicImage::ImageRgba8(image::RgbaImage::new(w, h))
        .save_with_format(&p, image::ImageFormat::Png)
        .unwrap();
    p
}

/// Build a live `IPropertyStore` over `file` straight out of the DLL's class factory.
unsafe fn make_store(file: &std::path::Path) -> Result<IPropertyStore> {
    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

    let path = dll_path();
    assert!(path.exists(), "cdylib not built at {path:?} — run `cargo build` first");
    let module: HMODULE = LoadLibraryW(PCWSTR(to_wide(path.as_os_str()).as_ptr()))?;
    let proc = GetProcAddress(module, s!("DllGetClassObject")).ok_or_else(|| Error::from(E_FAIL))?;
    let dll_get_class_object: DllGetClassObjectFn = std::mem::transmute(proc);

    let mut factory_ptr: *mut c_void = std::ptr::null_mut();
    dll_get_class_object(&CLSID_PROPERTY_STORE, &IClassFactory::IID, &mut factory_ptr).ok()?;
    assert!(!factory_ptr.is_null(), "null class factory");
    let factory = IClassFactory::from_raw(factory_ptr);

    // The shell inits a property handler with the file PATH (not a stream).
    let init: IInitializeWithFile = factory.CreateInstance(None)?;
    init.Initialize(PCWSTR(to_wide(file.as_os_str()).as_ptr()), 0)?;
    init.cast()
}

#[test]
fn property_store_exposes_image_dimensions() {
    let png = write_temp_png(40, 30);
    let store = unsafe { make_store(&png) }.unwrap();
    unsafe {
        let count = store.GetCount().unwrap();
        assert!(count >= 3, "an image should expose >=3 props (dims + h + v), got {count}");
        // Enumerate the keys (GetAt) and confirm the dimension props are present — proves
        // the COM surface end to end without decoding PROPVARIANT internals (the exact
        // values are covered by strip::read_info's own unit test).
        let mut keys = Vec::new();
        for i in 0..count {
            let mut k = PROPERTYKEY::default();
            store.GetAt(i, &mut k).unwrap();
            keys.push(k);
        }
        let has = |want: PROPERTYKEY| keys.iter().any(|k| k.fmtid == want.fmtid && k.pid == want.pid);
        assert!(has(PKEY_Image_HorizontalSize), "must expose Image.HorizontalSize");
        assert!(has(PKEY_Image_VerticalSize), "must expose Image.VerticalSize");
        assert!(store.GetAt(0, std::ptr::null_mut()).is_err(), "GetAt(null) must error, not crash");
    }
    let _ = std::fs::remove_file(&png);
}

#[test]
fn property_store_skips_oversized_file_fast() {
    // A hooked file larger than the 256 MiB input ceiling must be SKIPPED by the dimension
    // probe — never read into the caller's address space, never decoded — so selecting a
    // multi-GB upload in a host app's file-open dialog can't freeze it. This guards the 0.6.1
    // in-process property-handler hang (an unbounded std::fs::read + full decode on the shell's
    // thread). Uses a sparse `set_len` file: 300 MiB logical, ~no real disk/IO, and the probe
    // refuses it on the metadata length before reading a byte.
    let p = std::env::temp_dir().join(format!("st2k_propstore_oversized_{}.dng", std::process::id()));
    {
        let f = std::fs::File::create(&p).unwrap();
        f.set_len(300 * 1024 * 1024).unwrap(); // > decode::limits::MAX_INPUT_BYTES (256 MiB)
    }
    let store = unsafe { make_store(&p) }.unwrap();
    let t = std::time::Instant::now();
    let count = unsafe { store.GetCount() }.unwrap();
    let ms = t.elapsed().as_millis();
    let _ = std::fs::remove_file(&p);
    assert_eq!(count, 0, "oversized file must yield no properties (read skipped before decode)");
    assert!(ms < 2000, "oversized file must return fast, not block on a whole-file read; took {ms} ms");
}

#[test]
fn property_store_is_read_only() {
    let png = write_temp_png(8, 8);
    let store = unsafe { make_store(&png) }.unwrap();
    let err = unsafe { store.SetValue(&PKEY_Image_HorizontalSize, &PROPVARIANT::default()) }
        .expect_err("SetValue must be refused on a read-only store");
    assert_eq!(err.code(), STG_E_ACCESSDENIED, "read-only store must return STG_E_ACCESSDENIED");
    let _ = std::fs::remove_file(&png);
}

#[test]
fn property_store_rejects_null_key() {
    let png = write_temp_png(8, 8);
    let store = unsafe { make_store(&png) }.unwrap();
    // A null PROPERTYKEY must be rejected (E_INVALIDARG), never deref-crash the host.
    let r = unsafe { store.GetValue(std::ptr::null()) };
    assert!(r.is_err(), "GetValue(null) must return an error, not crash");
    let _ = std::fs::remove_file(&png);
}
