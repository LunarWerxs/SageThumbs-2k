//! The menu's logo bitmap, and the check for third-party menu skinners that would mis-draw it.

use super::*;

/// The app logo (256×256 PNG), embedded so the classic menu can show a brand
/// icon in front of the "SageThumbs 2K" submenu anchor.
pub(super) const MENU_LOGO_PNG: &[u8] = include_bytes!("../../assets/logo.png");

/// The logo as a 32-bpp premultiplied-alpha bitmap at the system menu-check size
/// (DPI-aware) — Vista+ menus alpha-blend such `hbmpItem` bitmaps natively. Built at
/// most once per DLL *load* (a `Mutex` rather than a lock-free cache so two racing
/// callers can't each build and leak a competing bitmap) and cached here for the
/// life of that load: live menus may reference it for the host's lifetime, and it's
/// a single small bitmap. Freed on [`free_menu_logo`], called from `DLL_PROCESS_DETACH`
/// — a load/unload cycle inside a long-lived `explorer.exe` used to leak one GDI
/// object every time, since a `OnceLock` is "once per load", not "once ever".
pub(super) fn menu_logo() -> HBITMAP {
    let mut cached = logo_slot().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(h) = *cached {
        return HBITMAP(h as *mut core::ffi::c_void);
    }
    let cx = unsafe { GetSystemMetrics(SM_CXMENUCHECK) }.max(16);
    let cy = unsafe { GetSystemMetrics(SM_CYMENUCHECK) }.max(16);
    let h = image::load_from_memory(MENU_LOGO_PNG)
        .ok()
        .map(|img| {
            img.resize_exact(cx as u32, cy as u32, image::imageops::FilterType::Lanczos3)
                .to_rgba8()
        })
        .and_then(|rgba| {
            unsafe { crate::dib::create_premultiplied_dib(cx, cy, rgba.as_raw()) }.ok()
        })
        .map(|b| b.0 as isize)
        .unwrap_or(0);
    *cached = Some(h);
    HBITMAP(h as *mut core::ffi::c_void)
}

/// Backing store for [`menu_logo`]'s cache: `Some(0)` means "tried and there is no
/// logo" (a decode failure), `None` means "not built yet".
pub(super) fn logo_slot() -> &'static std::sync::Mutex<Option<isize>> {
    static LOGO: std::sync::Mutex<Option<isize>> = std::sync::Mutex::new(None);
    &LOGO
}

/// Release the cached logo bitmap and clear the cache, so a later `menu_logo()` call
/// (the next DLL load) rebuilds it rather than returning a freed handle. Called from
/// `lib.rs`'s `dll_main` on `DLL_PROCESS_DETACH`.
pub(crate) fn free_menu_logo() {
    let mut cached = logo_slot().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(h) = cached.take() {
        if h != 0 {
            unsafe {
                let _ = DeleteObject(HBITMAP(h as *mut core::ffi::c_void).into());
            }
        }
    }
}

/// Case-insensitive STEMS (base file name, no extension, lower-cased) of the
/// menu-skinning shells whose own measurement pass clips a bitmap menu item to an
/// icon-sized sliver. A skin is injected into `explorer.exe`, and the classic handler
/// runs *inside* `explorer.exe`, so an in-process module check is the direct signal —
/// no registry sniffing, no process enumeration of OTHER processes, nothing that can
/// go stale. Matched by stem rather than one fixed file name per architecture: the
/// earlier x64-only exact names (`StartAllBackX64.dll`, `DarkMagicX64.dll`,
/// `ExplorerPatcher.amd64.dll`) could never match on ARM64 Windows, where
/// explorer.exe and the skins' own modules are ARM64 builds under different names —
/// permanently shipping the sliver regression on a platform this project ships.
pub(super) const MENU_SKIN_STEMS: [&str; 3] = ["startallback", "darkmagic", "explorerpatcher"];

/// Is a menu-skinning shell loaded into THIS process?
///
/// Cached: the answer cannot change without the host process restarting, and this is
/// consulted on every right-click. **A false answer here is safe by construction** —
/// see the module header: `false` picks the bitmap item, which is exactly what every
/// user gets today, so an unrecognized or unreadable skin degrades to the status quo
/// rather than to something new. That is why the stem list is a positive-match
/// allowlist and never a blocklist.
pub(super) fn menu_skin_loaded() -> bool {
    use std::sync::OnceLock;
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| unsafe { any_loaded_module_stem_matches(&MENU_SKIN_STEMS) })
}

/// Enumerate this process's own loaded modules (Psapi) and test each base file
/// name's stem (lower-cased, extension stripped) against `stems` with `starts_with`.
/// Grows the module-handle buffer and retries a bounded number of times so a module
/// loading between the sizing call and the fetch doesn't silently truncate the list;
/// gives up and returns `false` — the safe default, see [`menu_skin_loaded`] — rather
/// than looping forever.
pub(super) unsafe fn any_loaded_module_stem_matches(stems: &[&str]) -> bool {
    let proc = GetCurrentProcess();
    let mut count = 256usize; // generous starting guess; a real process rarely nears this
    for _ in 0..4 {
        let mut modules = vec![HMODULE::default(); count];
        let Ok(bytes) = u32::try_from(
            modules
                .len()
                .saturating_mul(core::mem::size_of::<HMODULE>()),
        ) else {
            return false;
        };
        let mut needed: u32 = 0;
        if !K32EnumProcessModules(proc, modules.as_mut_ptr(), bytes, &mut needed).as_bool() {
            return false;
        }
        let got = (needed as usize) / core::mem::size_of::<HMODULE>();
        if got > modules.len() {
            // The real list outgrew our guess; grow and retry instead of matching
            // against a truncated read.
            count = got;
            continue;
        }
        modules.truncate(got);
        for h in modules {
            let mut name = [0u16; 260]; // MAX_PATH; a base file name never needs more
            let len = K32GetModuleBaseNameW(proc, Some(h), &mut name) as usize;
            let Some(base) = name.get(..len) else {
                continue;
            };
            if stem_matches(&String::from_utf16_lossy(base), stems) {
                return true;
            }
        }
        return false;
    }
    false
}

/// Pure matching logic behind [`any_loaded_module_stem_matches`]: does `base_name`'s
/// stem (lower-cased, extension stripped) start with one of `stems`? Split out so the
/// case/extension handling is unit-testable without enumerating real process modules.
pub(super) fn stem_matches(base_name: &str, stems: &[&str]) -> bool {
    let lower = base_name.to_lowercase();
    let stem = lower.rsplit_once('.').map_or(lower.as_str(), |(s, _)| s);
    stems.iter().any(|s| stem.starts_with(s))
}
