//! SageThumbs 2K shell-extension DLL — the thin cdylib wrapper.
//!
//! ALL logic lives in the `sagethumbs2k` rlib (lib `sagethumbs2k_core`, aliased
//! `st2k_core` here). This crate exists ONLY to export the COM `Dll*` entry points the
//! Windows loader resolves by name, each a one-line shim over a `st2k_core::dll_*` fn.
//!
//! WHY it's its own crate: keeping the cdylib separate from the rlib means no crate is
//! ever both crate-types at once, which eliminates the intermittent cargo#6313
//! cdylib/rlib output-filename collision that flaked CI (LNK2019 unresolved externals).
#![allow(non_snake_case)]

use core::ffi::c_void;

use windows_core::{GUID, HRESULT};

/// The loader calls `DllMain` on attach/detach. We forward the HMODULE (as a raw
/// pointer-sized `isize`, so this crate needs no `windows` Win32 types) to the core,
/// which stashes it to resolve our own install path later. Always returns TRUE.
#[no_mangle]
pub extern "system" fn DllMain(hmodule: isize, reason: u32, _reserved: *mut c_void) -> i32 {
    st2k_core::dll_main(hmodule, reason);
    1
}

#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    st2k_core::dll_can_unload_now()
}

#[no_mangle]
pub extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    st2k_core::dll_get_class_object(rclsid, riid, ppv)
}

#[no_mangle]
pub extern "system" fn DllRegisterServer() -> HRESULT {
    st2k_core::dll_register_server()
}

#[no_mangle]
pub extern "system" fn DllUnregisterServer() -> HRESULT {
    st2k_core::dll_unregister_server()
}
