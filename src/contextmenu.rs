//! Classic IContextMenu + IShellExtInit handler.
//!
//! The modern IExplorerCommand verb (command.rs) only shows in the stock Win11
//! menu. Many machines replace that with the classic menu (StartAllBack,
//! ExplorerPatcher, or the {86ca1aa0…} registry tweak), where only classic
//! IContextMenu handlers appear. This handler covers those machines, surfacing
//! the same verbs (verbs.rs) as a "SageThumbs 2K" submenu.
//!
//! It also draws the signature SageThumbs/XnShell **menu preview**: an OWNER-DRAWN
//! menu item showing the image's thumbnail + name + dimensions/size, either at the
//! top of our submenu or directly on the main menu (Options).
//!
//! **Which item kind draws that tile depends on the HOST**, because neither kind is
//! correct everywhere. Measured live on both machine classes:
//!
//! | machine | bitmap item | owner-drawn item |
//! | --- | --- | --- |
//! | no menu skin (the common case) | full tile, menu stays dark | full tile, **menu turns light** |
//! | menu skin loaded (StartAllBack, ExplorerPatcher, …) | **~6 px sliver** | full tile, menu turns light |
//!
//! Two independent causes, deliberately kept apart:
//!
//! - **The sliver is the SKIN.** Windows sizes a bitmap menu item from the bitmap;
//!   a skin's own measurement pass sizes it as an icon and clips the rest. No bitmap
//!   format escapes it (32-bpp DIB, screen DDB and 24-bpp DDB clamp identically) —
//!   this is what 1.3.2-1.3.6 shipped and it is the "preview is a thin sliver" report.
//! - **The light menu is WINDOWS.** One owner-drawn item makes USER32 drop the *entire*
//!   popup off the themed drawing path, including every other handler's items.
//!   Reproduced with zero skin DLLs in the process, so uninstalling the skin does not
//!   avoid it. This is the 1.3.1 / 1.3.7 cost.
//!
//! So [`menu_skin_loaded`] probes the host once and the insertion sites branch:
//! **the bitmap item is the DEFAULT, owner-draw is the positive-match exception.**
//! That direction is the whole safety argument — a skin we have never heard of falls
//! through to the bitmap item and its user sees exactly what they see today, so the
//! name list can only ever *add* fixes, never remove one. The preview also stays
//! opt-out entirely via `MenuPreview = 0`. (The stock Win11 modern menu can host
//! neither kind, so the preview only appears in the classic / "Show more options"
//! path.)

use core::cell::{Cell, RefCell};
use core::sync::atomic::{AtomicUsize, Ordering};

use windows::core::{Error, Ref, Result, HRESULT, HSTRING, PCWSTR, PSTR};
use windows::Win32::Foundation::{
    COLORREF, E_FAIL, E_NOTIMPL, HMODULE, LPARAM, LRESULT, RECT, SIZE, S_OK, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    AlphaBlend, CreateCompatibleBitmap, CreateCompatibleDC, CreateDIBSection, CreateFontIndirectW,
    CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, FillRect, GdiFlush, GetDC, GetStockObject,
    GetSysColor, GetTextExtentPoint32W, ReleaseDC, SelectObject, SetBkMode, SetTextColor,
    AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, COLOR_HIGHLIGHT,
    COLOR_HIGHLIGHTTEXT, COLOR_MENU, COLOR_MENUTEXT, DEFAULT_GUI_FONT, DIB_RGB_COLORS, DT_CENTER,
    DT_END_ELLIPSIS, DT_SINGLELINE, HBITMAP, HDC, HFONT, HGDIOBJ, TRANSPARENT,
};
use windows::Win32::System::Com::{IDataObject, DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL};
use windows::Win32::System::Ole::ReleaseStgMedium;
use windows::Win32::System::ProcessStatus::{K32EnumProcessModules, K32GetModuleBaseNameW};
use windows::Win32::System::Registry::HKEY;
use windows::Win32::System::Threading::GetCurrentProcess;
use windows::Win32::UI::Controls::{DRAWITEMSTRUCT, MEASUREITEMSTRUCT, ODS_SELECTED, ODT_MENU};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    DragQueryFileW, IContextMenu2_Impl, IContextMenu3, IContextMenu3_Impl, IContextMenu_Impl,
    IShellExtInit, IShellExtInit_Impl, ShellExecuteW, CMINVOKECOMMANDINFO, HDROP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, GetSystemMetrics, InsertMenuW, SetMenuItemInfoW,
    SystemParametersInfoW, HMENU, MENUITEMINFOW, MF_BITMAP, MF_BYPOSITION, MF_OWNERDRAW, MF_POPUP,
    MF_SEPARATOR, MF_STRING, MIIM_BITMAP, NONCLIENTMETRICSW, SM_CXMENUCHECK, SM_CYMENUCHECK,
    SPI_GETNONCLIENTMETRICS, SW_SHOWNORMAL, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WM_DRAWITEM,
    WM_MEASUREITEM,
};
use windows_implement::implement;

use crate::{safety, settings, verbs};

mod com;
pub(crate) mod paint;
mod thumb;
pub(crate) use paint::*;
pub(crate) use thumb::*;

const CF_HDROP: u16 = 15;
const CMF_DEFAULTONLY: u32 = 0x0000_0001;

/// Max HEIGHT of the in-menu preview thumbnail, px — kept small so the menu item
/// stays XnView-narrow.
const PREVIEW_BOX: u32 = 88;
/// Max WIDTH of the preview thumbnail. Wider than [`PREVIEW_BOX`] so a panorama /
/// wide image isn't squashed into a tiny sliver — it gets up to this much width
/// (the menu item grows to fit). Normal-aspect images stay height-limited at 88.
const PREVIEW_WIDE: u32 = 220;
/// Cap on the caption text width so a long filename can't widen the whole menu.
const CAPTION_MAX: i32 = 156;
/// Don't decode huge files just for a menu preview (keeps the menu snappy).
const PREVIEW_MAX_BYTES: u64 = 32 * 1024 * 1024;

/// The decoded preview for the current selection (single image only).
pub(crate) struct Preview {
    hbm: HBITMAP,
    w: i32,
    h: i32,
    name: Vec<u16>, // file name, UTF-16 (no NUL — DrawTextW takes a slice)
    info: Vec<u16>, // "1500 × 1500 px – 96 KB"
    /// The "checker backdrop under a transparent thumbnail" setting, snapshotted once
    /// when the preview is built rather than re-read by `paint_preview` on every
    /// `WM_DRAWITEM` (mousing up and down a portable install's menu used to mean one
    /// ini read + parse per repaint).
    checker: bool,
}

impl Drop for Preview {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.hbm.into());
        }
    }
}

#[implement(IShellExtInit, IContextMenu3)]
pub struct ContextMenu {
    _ref: crate::ModuleRef,
    paths: RefCell<Vec<String>>,
    preview: RefCell<Option<Preview>>,
    /// Preview decode started from `IShellExtInit::Initialize` for either visible
    /// placement. The shell can continue querying its other handlers while this
    /// worker runs, so menu construction normally arrives with no UI wait.
    preview_job: RefCell<Option<std::sync::mpsc::Receiver<Option<MenuThumb>>>>,
    /// Snapshot of the cheap single-image/size gate taken during initialization,
    /// avoiding a second filesystem metadata query in `QueryContextMenu`.
    preview_eligible: Cell<bool>,
    /// The `std::fs::metadata` result from the same `Initialize`-time gate, handed to
    /// `build_preview` so it doesn't stat the file a second time on top of the one
    /// `preview_eligible` was computed from.
    preview_meta: RefCell<Option<std::fs::Metadata>>,
    /// Set once `ensure_preview` has tried and failed to build a preview (a corrupt
    /// file, an expired budget, a vanished path). Without this, every `WM_DRAWITEM`
    /// on the owner-drawn preview item re-ran the whole bounded stat + decode attempt
    /// and re-spawned a worker, since only success was cached.
    preview_failed: Cell<bool>,
    /// Absolute menu command id of the preview item (set in QueryContextMenu).
    preview_cmd: Cell<Option<u32>>,
    /// The composed tile handed to the menu on the BITMAP branch (unskinned hosts).
    /// A menu never takes ownership of an `MF_BITMAP` handle, so it is owned here and
    /// must outlive the on-screen menu; freed in `Drop` (the shell releases this
    /// object only after the menu is dismissed). Stays invalid on the owner-draw
    /// branch, which composes straight into the DC the shell hands us.
    tile: Cell<HBITMAP>,
}

impl Default for ContextMenu {
    // ModuleRef::default()'s side effect (live-object add-ref) must run; keep the Default call.
    #[allow(clippy::default_constructed_unit_structs)]
    fn default() -> Self {
        Self {
            _ref: crate::ModuleRef::default(),
            paths: RefCell::new(Vec::new()),
            preview: RefCell::new(None),
            preview_job: RefCell::new(None),
            preview_eligible: Cell::new(false),
            preview_meta: RefCell::new(None),
            preview_failed: Cell::new(false),
            preview_cmd: Cell::new(None),
            tile: Cell::new(HBITMAP::default()),
        }
    }
}

impl Drop for ContextMenu {
    fn drop(&mut self) {
        let bmp = self.tile.replace(HBITMAP::default());
        if !bmp.is_invalid() {
            unsafe {
                let _ = DeleteObject(bmp.into());
            }
        }
    }
}

/// Pull the selected file paths out of the shell's IDataObject (CF_HDROP).
unsafe fn hdrop_paths(obj: &IDataObject) -> Result<Vec<String>> {
    let fmt = FORMATETC {
        cfFormat: CF_HDROP,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    let mut medium = obj.GetData(&fmt)?;
    // GetData can hand back a different storage medium than we asked for; using
    // the hGlobal union field on a non-HGLOBAL medium would read a bogus handle.
    if medium.tymed != TYMED_HGLOBAL.0 as u32 {
        ReleaseStgMedium(&mut medium);
        return Err(E_FAIL.into());
    }
    let hdrop = HDROP(medium.u.hGlobal.0);
    let count = DragQueryFileW(hdrop, 0xFFFF_FFFF, None);
    let mut paths = Vec::new();
    for i in 0..count {
        let len = DragQueryFileW(hdrop, i, None) as usize;
        let mut buf = vec![0u16; len + 1];
        let got = DragQueryFileW(hdrop, i, Some(&mut buf)) as usize;
        paths.push(String::from_utf16_lossy(&buf[..got]));
    }
    ReleaseStgMedium(&mut medium);
    Ok(paths)
}

/// Bound a `std::fs::metadata` call the same way [`decode_menu_thumb_budgeted`]
/// bounds the decode. An unresponsive network share or removable device can make
/// `metadata()` block far longer than a local stat, and this pre-gate is called
/// synchronously on Explorer's own thread, from `IShellExtInit::Initialize` and
/// from [`ContextMenu::ensure_preview`], with nothing else in that chain
/// protecting it. `MENU_PREVIEW_BUDGET` is already the "how long may this whole
/// menu wait" contract; the metadata call must not be the one link left
/// unbounded inside it. Detached on timeout, matching every other recv_timeout
/// worker in this codebase: the call is simply abandoned, not cancelled.
fn metadata_budgeted(path: &str) -> Option<std::fs::Metadata> {
    let path = path.to_owned();
    // Route through `safety::spawn_budgeted` so the detached worker holds a `ModuleRef` for its
    // whole life: on a hung share the stat outlives this call, and without the pin
    // `DllCanUnloadNow` would report the DLL free while the worker still executes its code inside
    // explorer.exe — an access violation when it resumes into unmapped memory. Every other
    // detached worker in the crate pins this way; this hand-rolled one (from the A002 fix) did not.
    crate::safety::spawn_budgeted("st2k-menu-metadata", MENU_PREVIEW_BUDGET, move || {
        std::fs::metadata(&path).ok()
    })
    .flatten()
}

/// Whether `len` bytes is small enough for the in-process menu-preview to read or
/// decode. The one definition shared by [`preview_metadata`]'s pre-gate,
/// [`build_preview`]'s own check immediately before decoding, and the off-thread
/// worker's own pre-read check in `thumb.rs` — three independently-drifting copies
/// of the same two-part budget before this.
pub(crate) fn within_preview_budget(len: u64) -> bool {
    len <= PREVIEW_MAX_BYTES && len <= settings::max_file_size_bytes()
}

/// Cheap pre-gate (metadata only, NO read/decode): the file exists and is within
/// the preview size budget. `Initialize` checks this before reserving the preview
/// slot at all, so an oversized file costs one `metadata` call — and hands the
/// `Metadata` back so [`build_preview`] can reuse it instead of statting the file a
/// second time.
fn preview_metadata(path: &str) -> Option<std::fs::Metadata> {
    let m = metadata_budgeted(path)?;
    within_preview_budget(m.len()).then_some(m)
}

/// Decode `path` into the menu-preview payload (thumbnail DIB + caption lines).
/// Called only when a preview is about to be inserted or painted. `meta`, when
/// given, is the `Initialize`-time [`preview_metadata`] result — reusing it here
/// avoids a second `metadata_budgeted` stat (each one its own bounded worker
/// thread) for the same file on the same right-click.
fn build_preview(
    path: &str,
    prefetched: Option<std::sync::mpsc::Receiver<Option<MenuThumb>>>,
    meta: Option<std::fs::Metadata>,
) -> Option<Preview> {
    let meta = match meta {
        Some(m) => m,
        None => metadata_budgeted(path)?,
    };
    if !within_preview_budget(meta.len()) {
        return None;
    }
    let name: Vec<u16> = std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
        .encode_utf16()
        .collect();
    let kb = meta.len() as f64 / 1024.0;
    let size_txt = if kb >= 1024.0 {
        format!("{:.1} MB", kb / 1024.0)
    } else {
        format!("{kb:.0} KB")
    };

    // Preview fidelity ONLY. The classic context menu loads in explorer.exe (it is
    // NOT process-isolated the way the thumbnail provider is), so we keep this
    // bounded: the baked-in container preview is plenty for an ~88px menu
    // thumbnail and avoids spawning ImageMagick / full-fidelity decode inside the
    // shell. Caption dimensions come from a cheap header probe (PSD/PSB real
    // canvas) so a 4700×800 PSD doesn't read "160 × 26 px" from its thumbnail.
    //
    // On decode failure (a corrupt or in-practice-undecodable file) fall back to a
    // CAPTION-ONLY tile (name + size, no thumbnail): the preview command slot was already
    // reserved in QueryContextMenu, so a name+size row degrades more gracefully than
    // a blank gap. `null` hbm + 0×0 are handled by `paint_preview`.
    // Decode OFF explorer's menu paint thread under a wall-clock budget (the in-proc-COM rule):
    // the cheap tiers are fast on normal files, but a large HEIC/RAW or a 16384² in-cap image
    // has no internal TIME bound and this would otherwise run on the menu's own paint thread.
    // The DIB (a GDI object) is created HERE from the worker's plain-RGBA result; only the
    // decode (the slow part) is offloaded. On timeout -> caption-only tile (handled below).
    let decoded = decode_menu_thumb_budgeted(path, prefetched).and_then(|t| {
        let hbm = unsafe { crate::dib::create_premultiplied_dib(t.w, t.h, &t.rgba).ok()? };
        Some((hbm, t.w, t.h, t.ow, t.oh))
    });
    let (hbm, w, h, info) = match decoded {
        Some((hbm, w, h, ow, oh)) => (
            hbm,
            w,
            h,
            format!("{ow} \u{00d7} {oh} px  \u{2013}  {size_txt}"),
        ),
        None => (HBITMAP::default(), 0, 0, size_txt),
    };
    Some(Preview {
        hbm,
        w,
        h,
        name,
        info: info.encode_utf16().collect(),
        checker: settings::preview_checker(),
    })
}

/// The app logo (256×256 PNG), embedded so the classic menu can show a brand
/// icon in front of the "SageThumbs 2K" submenu anchor.
const MENU_LOGO_PNG: &[u8] = include_bytes!("../assets/logo.png");

/// The logo as a 32-bpp premultiplied-alpha bitmap at the system menu-check size
/// (DPI-aware) — Vista+ menus alpha-blend such `hbmpItem` bitmaps natively. Built at
/// most once per DLL *load* (a `Mutex` rather than a lock-free cache so two racing
/// callers can't each build and leak a competing bitmap) and cached here for the
/// life of that load: live menus may reference it for the host's lifetime, and it's
/// a single small bitmap. Freed on [`free_menu_logo`], called from `DLL_PROCESS_DETACH`
/// — a load/unload cycle inside a long-lived `explorer.exe` used to leak one GDI
/// object every time, since a `OnceLock` is "once per load", not "once ever".
fn menu_logo() -> HBITMAP {
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
fn logo_slot() -> &'static std::sync::Mutex<Option<isize>> {
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
const MENU_SKIN_STEMS: [&str; 3] = ["startallback", "darkmagic", "explorerpatcher"];

/// Is a menu-skinning shell loaded into THIS process?
///
/// Cached: the answer cannot change without the host process restarting, and this is
/// consulted on every right-click. **A false answer here is safe by construction** —
/// see the module header: `false` picks the bitmap item, which is exactly what every
/// user gets today, so an unrecognized or unreadable skin degrades to the status quo
/// rather than to something new. That is why the stem list is a positive-match
/// allowlist and never a blocklist.
fn menu_skin_loaded() -> bool {
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
unsafe fn any_loaded_module_stem_matches(stems: &[&str]) -> bool {
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
fn stem_matches(base_name: &str, stems: &[&str]) -> bool {
    let lower = base_name.to_lowercase();
    let stem = lower.rsplit_once('.').map_or(lower.as_str(), |(s, _)| s);
    stems.iter().any(|s| stem.starts_with(s))
}

/// Compose the preview tile into a screen-compatible **DDB** for an `MF_BITMAP` item.
///
/// A DDB rather than the 32-bpp DIB section the owner-draw path paints into: `MF_BITMAP`
/// was verified live against a DDB, and a menu blits a bitmap item rather than
/// alpha-blending it, so the DIB's alpha channel buys nothing here and its behaviour on
/// this path was never measured. The tile is still painted by the same
/// [`paint_preview`] used by the owner-drawn branch and the diagnostic PNG, so the two
/// renderings cannot drift.
///
/// The caller owns the returned handle (see [`ContextMenu::tile`]).
unsafe fn preview_ddb(p: &Preview) -> HBITMAP {
    let (iw, ih) = tile_size(p);
    if iw <= 0 || ih <= 0 {
        return HBITMAP::default();
    }
    let screen = GetDC(None);
    if screen.is_invalid() {
        return HBITMAP::default();
    }
    let bmp = CreateCompatibleBitmap(screen, iw, ih);
    let memdc = CreateCompatibleDC(Some(screen));
    ReleaseDC(None, screen);
    if bmp.is_invalid() || memdc.is_invalid() {
        if !bmp.is_invalid() {
            let _ = DeleteObject(bmp.into());
        }
        if !memdc.is_invalid() {
            let _ = DeleteDC(memdc);
        }
        return HBITMAP::default();
    }
    let old = SelectObject(memdc, bmp.into());
    let (bg, fg) = menu_theme_colors();
    paint_preview(
        memdc,
        RECT {
            left: 0,
            top: 0,
            right: iw,
            bottom: ih,
        },
        p,
        bg,
        fg,
    );
    let _ = GdiFlush();
    SelectObject(memdc, old);
    let _ = DeleteDC(memdc);
    bmp
}

/// Insert the preview as an OWNER-DRAWN item — the only item kind whose height we can
/// claim, via `WM_MEASUREITEM`.
///
/// **This is the SKINNED-host branch only.** A skin's measurement pass sizes any bitmap
/// item as an icon and clips the ~136 px tile to a ~6 px strip (32-bpp DIB, screen DDB
/// and 24-bpp DDB clamp identically), so owner-draw is the only thing that survives
/// there. Its cost is that one owner-drawn item drops the whole popup onto the classic
/// (light) drawing path — which is why unskinned hosts get [`insert_preview_bitmap`]
/// instead, and why the preview stays opt-out via `MenuPreview = 0` either way.
unsafe fn insert_preview_item(hmenu: HMENU, pos: u32, cmd: u32) -> bool {
    InsertMenuW(
        hmenu,
        pos,
        MF_BYPOSITION | MF_OWNERDRAW,
        cmd as usize,
        PCWSTR::null(),
    )
    .is_ok()
}

/// Insert the preview as a real `MF_BITMAP` item — the DEFAULT (unskinned) branch.
///
/// `MF_BITMAP` rather than `hbmpItem` on an empty-string item (the 1.3.2 shape): both
/// render correctly unskinned, but `MF_BITMAP` gives the tile its own row, whereas
/// `hbmpItem` puts it in the icon gutter and shoves every label right (a menu measured
/// 317 px wide versus 254 px for the same tile). `hbmpItem` remains the fallback if
/// `MF_BITMAP` ever misbehaves; it has four shipped versions of field time behind it.
///
/// `bmp` must outlive the menu — the menu does not take ownership.
unsafe fn insert_preview_bitmap(hmenu: HMENU, pos: u32, cmd: u32, bmp: HBITMAP) -> bool {
    if bmp.is_invalid() {
        return false;
    }
    InsertMenuW(
        hmenu,
        pos,
        MF_BYPOSITION | MF_BITMAP,
        cmd as usize,
        PCWSTR(bmp.0 as *const u16),
    )
    .is_ok()
}

/// Recursively append the verb tree into `parent`, assigning command ids in
/// depth-first leaf order from `idcmdfirst`, stopping after `budget` leaves.
unsafe fn build_menu_into(
    parent: HMENU,
    items: &[verbs::MenuItem],
    idcmdfirst: u32,
    next_leaf: &mut u32,
    budget: u32,
    vis: &settings::MenuVisibility,
) {
    // Deferred, not appended immediately: a separator is held here until the next
    // real (non-hidden, successfully-inserted) item actually gets appended. Per-item
    // visibility can hide every item between two separators in the tree (or a
    // leading/trailing one with nothing on one side at all); appending on sight, as
    // this used to, rendered two adjacent divider rows in that case despite the
    // comment on the Separator arm below claiming otherwise. A pending separator
    // that never finds a following item (budget cutoff, or it was trailing) is
    // simply dropped when the loop ends. `has_emitted` is what makes a LEADING
    // separator drop too: the Separator arm only arms `sep_pending` once something
    // real has already been appended, so there is nothing to flush it against yet.
    let mut sep_pending = false;
    let mut has_emitted = false;
    for it in items {
        // Per-item visibility: a hidden top-level item is skipped from the drawn
        // menu but still advances the leaf counter, so command ids stay aligned
        // with the full tree. (Child keys are never in the toggle set, so they
        // always pass — only top-level toggles can hide; separators have an empty
        // title which is never hidden.) `vis` is a single snapshot of the subkey,
        // so this is one read per item, not a key-open.
        if !vis.shown(it.title()) {
            *next_leaf += verbs::count_leaves(it);
            continue;
        }
        match it {
            verbs::MenuItem::Group(title, children) => {
                let Ok(sub) = CreatePopupMenu() else {
                    // Same advance as the hidden-item branch above: without it,
                    // every later sibling leaf's command id shifts down (a GDI
                    // handle exhaustion / OOM here would misdispatch InvokeCommand
                    // for the rest of the menu, not just drop this group).
                    *next_leaf += verbs::count_leaves(it);
                    continue;
                };
                build_menu_into(sub, children, idcmdfirst, next_leaf, budget, vis);
                if sep_pending {
                    let _ = AppendMenuW(parent, MF_SEPARATOR, 0, PCWSTR::null());
                    sep_pending = false;
                }
                if AppendMenuW(
                    parent,
                    MF_POPUP | MF_STRING,
                    sub.0 as usize,
                    &HSTRING::from(crate::i18n::t(title)),
                )
                .is_ok()
                {
                    has_emitted = true;
                } else {
                    // `sub` only becomes `parent`'s responsibility once the attach
                    // succeeds; an unattached popup is a USER object nothing else
                    // frees, and attach failures cluster right at the 10,000-handle
                    // process quota this would otherwise help exhaust.
                    let _ = DestroyMenu(sub);
                }
            }
            verbs::MenuItem::Verb(title, _) => {
                if *next_leaf >= budget {
                    return;
                }
                if sep_pending {
                    let _ = AppendMenuW(parent, MF_SEPARATOR, 0, PCWSTR::null());
                    sep_pending = false;
                }
                // The leaf's command id is its global leaf index, mapped through
                // the central id_for() so the offset convention lives in one place.
                let cmd =
                    verbs::id_for(verbs::CmdSlot::Leaf(verbs::LeafId(*next_leaf)), idcmdfirst);
                let _ = AppendMenuW(
                    parent,
                    MF_STRING,
                    cmd as usize,
                    &HSTRING::from(crate::i18n::t(title)),
                );
                *next_leaf += 1;
                has_emitted = true;
            }
            verbs::MenuItem::Separator => {
                // A divider: consumes no command id. Deferred rather than appended
                // here (see `sep_pending` above), and only armed once something real
                // has already been appended, so a leading separator has nothing to
                // flush against and is dropped along with duplicates and trailers.
                if has_emitted {
                    sep_pending = true;
                }
            }
        }
    }
}

impl ContextMenu {
    /// Build the preview on first demand. Both placements normally have a worker
    /// already running; this waits only for the small fixed budget. Idempotent:
    /// builds (or gives up) at most once, caching into `self.preview` on success or
    /// setting `self.preview_failed` on failure — without the failure cache, every
    /// `WM_DRAWITEM` repaint on an undecodable/timed-out file re-ran the whole
    /// bounded attempt and re-spawned a worker, since only success was cached.
    unsafe fn ensure_preview(&self) -> bool {
        if self.preview.borrow().is_some() {
            return true;
        }
        if self.preview_failed.get() {
            return false;
        }
        let path = self.paths.borrow().first().cloned();
        let built = path.and_then(|path| {
            let prefetched = self.preview_job.borrow_mut().take();
            // Reuse the `Initialize`-time metadata (see `preview_eligible`) instead
            // of statting the file again here.
            let meta = self.preview_meta.borrow_mut().take();
            build_preview(&path, prefetched, meta)
        });
        match built {
            Some(p) => {
                *self.preview.borrow_mut() = Some(p);
                true
            }
            None => {
                self.preview_failed.set(true);
                false
            }
        }
    }

    /// Decode the selection (if not already) and compose the tile bitmap for the
    /// BITMAP branch, remembering it on `self` so it outlives this call and stays
    /// valid while the menu is on screen. Returns an invalid handle when there is
    /// nothing to show, in which case the caller adds no preview item at all.
    unsafe fn build_tile(&self) -> HBITMAP {
        if !self.ensure_preview() {
            return HBITMAP::default();
        }
        let bmp = {
            let preview = self.preview.borrow();
            match preview.as_ref() {
                Some(p) => preview_ddb(p),
                None => HBITMAP::default(),
            }
        };
        if !bmp.is_invalid() {
            // A second QueryContextMenu on the same object would otherwise leak the
            // first tile.
            let old = self.tile.replace(bmp);
            if !old.is_invalid() {
                let _ = DeleteObject(old.into());
            }
        }
        bmp
    }

    /// Insert the preview item at `pos`, picking the rendering technique for THIS host.
    ///
    /// Bitmap item by default, owner-draw only on a positive skin match — see the
    /// module header for the measurement matrix and for why the default points this
    /// way rather than the other.
    ///
    /// The two branches differ in WHEN they decode. Owner-draw stays lazy until
    /// `WM_MEASUREITEM`; the bitmap branch needs real pixels to hand over, so it
    /// decodes here. Both placements prefetch during `Initialize`, so the bounded
    /// wait is normally hidden behind Explorer's own menu construction.
    unsafe fn insert_preview(&self, hmenu: HMENU, pos: u32, cmd: u32) -> bool {
        if menu_skin_loaded() {
            return insert_preview_item(hmenu, pos, cmd);
        }
        // No tile means no pixels to hand a bitmap item. Insert nothing rather than
        // falling back to owner-draw: an owner-drawn item would drop the whole popup
        // onto the light drawing path in exchange for a 1×1 blank, which is a strictly
        // worse trade than simply having no preview.
        insert_preview_bitmap(hmenu, pos, cmd, self.build_tile())
    }

    /// Report the preview item's real pixel size. Nothing else can on a skinned host:
    /// its measurement pass sizes every bitmap item as an icon, so this is the only
    /// channel through which a 136 px-tall tile can claim its row. Reached only on the
    /// owner-draw branch — see [`insert_preview`](Self::insert_preview).
    unsafe fn measure_preview(&self, cmd: u32, lparam: LPARAM) -> bool {
        let mis = &mut *(lparam.0 as *mut MEASUREITEMSTRUCT);
        if mis.CtlType != ODT_MENU || mis.itemID != cmd {
            return false;
        }
        // A skinned-host flyout may finish its prefetched decode here, at first
        // measure. If that fails — the file vanished or changed between
        // QueryContextMenu and the paint — claim a
        // minimal slot so the reserved item has a valid size and just draws blank.
        if !self.ensure_preview() {
            mis.itemWidth = 1;
            mis.itemHeight = 1;
            return true;
        }
        let preview = self.preview.borrow();
        let Some(p) = preview.as_ref() else {
            return false;
        };
        let (iw, ih) = tile_size(p);
        mis.itemWidth = iw as u32;
        mis.itemHeight = ih as u32;
        true
    }

    /// Paint the tile into the rect the menu gave us, following the hover state.
    unsafe fn draw_preview_item(&self, cmd: u32, lparam: LPARAM) -> bool {
        let dis = &*(lparam.0 as *const DRAWITEMSTRUCT);
        if dis.CtlType != ODT_MENU || dis.itemID != cmd {
            return false;
        }
        // Measure runs before draw and builds the preview; ensure it anyway.
        if !self.ensure_preview() {
            return true; // nothing to draw (rare: lazy build failed)
        }
        let preview = self.preview.borrow();
        let Some(p) = preview.as_ref() else {
            return false;
        };
        let (bg, fg) = if (dis.itemState.0 & ODS_SELECTED.0) != 0 {
            (
                GetSysColor(COLOR_HIGHLIGHT),
                GetSysColor(COLOR_HIGHLIGHTTEXT),
            )
        } else {
            menu_theme_colors()
        };
        paint_preview(dis.hDC, dis.rcItem, p, bg, fg);
        true
    }

    /// Own the preview item's measurement and painting. The preview row itself is
    /// inserted synchronously by `QueryContextMenu`; real Explorer does not
    /// reliably forward `WM_INITMENUPOPUP` for an extension-created child popup.
    unsafe fn menu_msg(&self, umsg: u32, _wparam: WPARAM, lparam: LPARAM) -> bool {
        // The shell always passes a valid struct pointer for the owner-draw
        // messages, but guard anyway: a null lparam would make the casts UB.
        if matches!(umsg, WM_MEASUREITEM | WM_DRAWITEM) {
            let Some(cmd) = self.preview_cmd.get() else {
                return false;
            };
            if lparam.0 == 0 {
                return false;
            }
            return if umsg == WM_MEASUREITEM {
                self.measure_preview(cmd, lparam)
            } else {
                self.draw_preview_item(cmd, lparam)
            };
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{
        DestroyMenu, GetMenuItemInfoW, MENU_ITEM_TYPE, MFT_BITMAP, MFT_OWNERDRAW, MIIM_FTYPE,
        MIIM_ID,
    };

    /// The preview slot must have a real, image-sized rect to claim. Uses the
    /// caption-only shape (no decoded thumbnail), which is also the real fallback
    /// when a file passes the size gate but fails to decode.
    #[test]
    fn preview_measures_a_real_tile_rect() {
        let p = Preview {
            hbm: HBITMAP::default(),
            w: 0,
            h: 0,
            name: crate::wide("photo.jpg"),
            info: crate::wide("1500 x 1500 px - 96 KB"),
            checker: true,
        };
        unsafe {
            let (iw, ih) = tile_size(&p);
            assert!(iw > 0 && ih > 0, "tile must have a positive size");
            assert!(
                ih >= 48,
                "even a caption-only tile needs both caption rows ({ih} px)"
            );
        }
    }

    /// Read the item type + id of the menu item at `pos`.
    unsafe fn item_type_and_id(menu: HMENU, pos: u32) -> (MENU_ITEM_TYPE, u32) {
        let mut item = MENUITEMINFOW {
            cbSize: core::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_FTYPE | MIIM_ID,
            ..Default::default()
        };
        GetMenuItemInfoW(menu, pos, true, &mut item).expect("GetMenuItemInfoW");
        (item.fType, item.wID)
    }

    /// The SKINNED branch must stay OWNER-DRAWN. A menu-skinning shell measures every
    /// bitmap item form — `hbmpItem` on a text item, `MF_BITMAP`, 32-bpp DIB, screen
    /// DDB, 24-bpp DDB — as an ICON and clips the ~136 px tile to a ~6 px strip
    /// (verified live; the 1.3.2-1.3.6 "preview is a sliver" regression). Only
    /// `WM_MEASUREITEM` can claim the height, and only an owner-drawn item gets one.
    #[test]
    fn skinned_preview_item_is_owner_drawn() {
        unsafe {
            let menu = CreatePopupMenu().expect("CreatePopupMenu");
            assert!(
                insert_preview_item(menu, 0, 42),
                "preview item insertion must succeed"
            );
            let (ftype, id) = item_type_and_id(menu, 0);
            assert_eq!(id, 42, "preview command id must be retained");
            assert!(
                ftype.contains(MFT_OWNERDRAW),
                "the skinned-host preview must be OWNER-DRAWN ({ftype:?}); any bitmap \
                 item form is measured as an icon there and clipped to a sliver"
            );
            let _ = DestroyMenu(menu);
        }
    }

    /// The DEFAULT (unskinned) branch must be a real `MF_BITMAP` item and must NOT be
    /// owner-drawn: a single owner-drawn item drops the whole popup off Windows' themed
    /// drawing path, turning a dark menu light for every other handler's items too.
    #[test]
    fn unskinned_preview_item_is_a_bitmap() {
        unsafe {
            let menu = CreatePopupMenu().expect("CreatePopupMenu");
            let p = Preview {
                hbm: HBITMAP::default(),
                w: 0,
                h: 0,
                name: crate::wide("photo.jpg"),
                info: crate::wide("1500 x 1500 px - 96 KB"),
                checker: true,
            };
            let bmp = preview_ddb(&p);
            assert!(!bmp.is_invalid(), "the caption-only tile must compose");
            assert!(
                insert_preview_bitmap(menu, 0, 42, bmp),
                "bitmap preview insertion must succeed"
            );
            let (ftype, id) = item_type_and_id(menu, 0);
            assert_eq!(id, 42, "preview command id must be retained");
            assert!(
                !ftype.contains(MFT_OWNERDRAW),
                "the unskinned preview must NOT be owner-drawn ({ftype:?}); one \
                 owner-drawn item un-themes the entire popup"
            );
            assert!(
                ftype.contains(MFT_BITMAP),
                "the unskinned preview must be a bitmap item ({ftype:?})"
            );
            let _ = DestroyMenu(menu);
            let _ = DeleteObject(bmp.into());
        }
    }

    /// A bitmap item with no bitmap would be an invisible, unclickable row, so the
    /// insert must refuse it and let the caller add nothing at all.
    #[test]
    fn bitmap_preview_refuses_a_null_tile() {
        unsafe {
            let menu = CreatePopupMenu().expect("CreatePopupMenu");
            assert!(
                !insert_preview_bitmap(menu, 0, 42, HBITMAP::default()),
                "a null tile must not be inserted"
            );
            let _ = DestroyMenu(menu);
        }
    }

    /// The host probe is answered once and reused. Its VALUE is environment-dependent
    /// (it is true only inside a skinned `explorer.exe`), so this asserts the property
    /// that must hold everywhere: one right-click cannot disagree with the next.
    #[test]
    fn menu_skin_probe_is_cached() {
        assert_eq!(
            menu_skin_loaded(),
            menu_skin_loaded(),
            "the host probe must be stable within a process"
        );
    }

    /// Regression: the allowlist used to hold only exact x64/amd64 file
    /// names, so an ARM64 build of the same skin (a different module name on that
    /// architecture) could never match. The stem match must catch it regardless of
    /// case or which architecture suffix the vendor picked.
    #[test]
    fn skin_stem_match_is_architecture_and_case_independent() {
        assert!(stem_matches("StartAllBackX64.dll", &MENU_SKIN_STEMS));
        assert!(
            stem_matches("StartAllBackA64.dll", &MENU_SKIN_STEMS),
            "an ARM64 StartAllBack module must match too"
        );
        assert!(stem_matches("ExplorerPatcher.ARM64.dll", &MENU_SKIN_STEMS));
        assert!(stem_matches("DARKMAGICARM64.DLL", &MENU_SKIN_STEMS));
        assert!(
            !stem_matches("explorer.exe", &MENU_SKIN_STEMS),
            "an unrelated module must not match (the allowlist's false-is-safe default)"
        );
    }

    /// The two-part size budget must be one definition, not three independently
    /// drifting copies: the file itself must fit `PREVIEW_MAX_BYTES`, and it
    /// must also fit whatever the user configured as the overall max file size.
    #[test]
    fn within_preview_budget_enforces_both_caps() {
        assert!(within_preview_budget(1024));
        assert!(
            !within_preview_budget(PREVIEW_MAX_BYTES + 1),
            "must reject anything over the fixed menu-preview cap"
        );
    }

    /// A199 regression: a leading separator, a trailing separator, and a run of
    /// adjacent separators (which a hidden item in between would also produce, but
    /// two literal `Separator` entries in a row are a simpler and deterministic way
    /// to exercise the exact same `sep_pending` path) must all collapse to nothing
    /// or to a single divider row: never two adjacent MF_SEPARATOR rows, and never
    /// one at either end of the (sub)menu, matching what the comment on the
    /// Separator arm has always claimed.
    #[test]
    fn separators_never_lead_trail_or_double_up() {
        use windows::Win32::UI::WindowsAndMessaging::{GetMenuItemCount, MFT_SEPARATOR};

        const ITEMS: &[verbs::MenuItem] = &[
            verbs::MenuItem::Separator, // leading -> must be dropped
            verbs::MenuItem::Separator, // duplicate -> collapses into one
            verbs::MenuItem::Verb("SepTestA", verbs::VerbAction::Clipboard),
            verbs::MenuItem::Separator,
            verbs::MenuItem::Separator, // duplicate -> collapses into one
            verbs::MenuItem::Verb("SepTestB", verbs::VerbAction::Clipboard),
            verbs::MenuItem::Separator, // trailing -> must be dropped
        ];

        unsafe {
            let menu = CreatePopupMenu().expect("CreatePopupMenu");
            let vis = settings::menu_visibility();
            let mut next_leaf = 0u32;
            build_menu_into(menu, ITEMS, 1, &mut next_leaf, u32::MAX, &vis);

            assert_eq!(
                GetMenuItemCount(Some(menu)),
                3,
                "want [Verb, Separator, Verb] only: no leading/trailing/doubled dividers"
            );
            let (t0, _) = item_type_and_id(menu, 0);
            assert!(!t0.contains(MFT_SEPARATOR), "row 0 must be the first verb");
            let (t1, _) = item_type_and_id(menu, 1);
            assert!(
                t1.contains(MFT_SEPARATOR),
                "row 1 must be the single divider between the two verbs"
            );
            let (t2, _) = item_type_and_id(menu, 2);
            assert!(!t2.contains(MFT_SEPARATOR), "row 2 must be the second verb");

            let _ = DestroyMenu(menu);
        }
    }
}
