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

use windows::core::{w, Error, Ref, Result, HRESULT, HSTRING, PCWSTR, PSTR};
use windows::Win32::Foundation::{
    COLORREF, E_FAIL, E_NOTIMPL, LPARAM, LRESULT, RECT, SIZE, S_OK, WPARAM,
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
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::ReleaseStgMedium;
use windows::Win32::System::Registry::HKEY;
use windows::Win32::UI::Controls::{DRAWITEMSTRUCT, MEASUREITEMSTRUCT, ODS_SELECTED, ODT_MENU};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    DragQueryFileW, IContextMenu2_Impl, IContextMenu3, IContextMenu3_Impl, IContextMenu_Impl,
    IShellExtInit, IShellExtInit_Impl, ShellExecuteW, CMINVOKECOMMANDINFO, HDROP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, GetSystemMetrics, InsertMenuW, SetMenuItemInfoW,
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

/// Cheap pre-gate (metadata only, NO read/decode): the file exists and is within
/// the preview size budget. `QueryContextMenu` checks this before composing the
/// tile at all, so an oversized file costs one `metadata` call. The decode itself
/// still runs on a detached worker under [`MENU_PREVIEW_BUDGET`] (see
/// [`ContextMenu::ensure_preview`]), so a slow file cannot freeze the menu paint.
fn preview_size_ok(path: &str) -> bool {
    std::fs::metadata(path)
        .map(|m| m.len() <= PREVIEW_MAX_BYTES && m.len() <= settings::max_file_size_bytes())
        .unwrap_or(false)
}

/// Decode `path` into the menu-preview payload (thumbnail DIB + caption lines).
/// Called only when a preview is about to be inserted or painted.
fn build_preview(
    path: &str,
    prefetched: Option<std::sync::mpsc::Receiver<Option<MenuThumb>>>,
) -> Option<Preview> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > PREVIEW_MAX_BYTES || meta.len() > settings::max_file_size_bytes() {
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
    })
}

/// The app logo (256×256 PNG), embedded so the classic menu can show a brand
/// icon in front of the "SageThumbs 2K" submenu anchor.
const MENU_LOGO_PNG: &[u8] = include_bytes!("../assets/logo.png");

/// The logo as a 32-bpp premultiplied-alpha bitmap at the system menu-check
/// size (DPI-aware) — Vista+ menus alpha-blend such `hbmpItem` bitmaps natively.
/// Created once per process and never freed: live menus may reference it for
/// the host's lifetime, and it's a single small bitmap.
fn menu_logo() -> HBITMAP {
    use std::sync::OnceLock;
    static LOGO: OnceLock<isize> = OnceLock::new();
    let h = *LOGO.get_or_init(|| {
        let cx = unsafe { GetSystemMetrics(SM_CXMENUCHECK) }.max(16);
        let cy = unsafe { GetSystemMetrics(SM_CYMENUCHECK) }.max(16);
        let Ok(img) = image::load_from_memory(MENU_LOGO_PNG) else {
            return 0;
        };
        let rgba = img
            .resize_exact(cx as u32, cy as u32, image::imageops::FilterType::Lanczos3)
            .to_rgba8();
        unsafe { crate::dib::create_premultiplied_dib(cx, cy, rgba.as_raw()) }
            .map(|b| b.0 as isize)
            .unwrap_or(0)
    });
    HBITMAP(h as *mut core::ffi::c_void)
}

/// Menu-skinning shells whose own measurement pass clips a bitmap menu item to an
/// icon-sized sliver. A skin is injected into `explorer.exe`, and the classic handler
/// runs *inside* `explorer.exe`, so an in-process module check is the direct signal —
/// no registry sniffing, no process enumeration, nothing that can go stale.
const MENU_SKIN_MODULES: [PCWSTR; 3] = [
    w!("StartAllBackX64.dll"),
    w!("DarkMagicX64.dll"),
    w!("ExplorerPatcher.amd64.dll"),
];

/// Is a menu-skinning shell loaded into THIS process?
///
/// Cached: the answer cannot change without the host process restarting, and this is
/// consulted on every right-click. **A false answer here is safe by construction** —
/// see the module header: `false` picks the bitmap item, which is exactly what every
/// user gets today, so an unrecognized skin degrades to the status quo rather than to
/// something new. That is why the list is a positive-match allowlist and never a
/// blocklist.
fn menu_skin_loaded() -> bool {
    use std::sync::OnceLock;
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| {
        MENU_SKIN_MODULES
            .iter()
            .any(|n| unsafe { GetModuleHandleW(*n) }.is_ok())
    })
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
                let Ok(sub) = CreatePopupMenu() else { continue };
                build_menu_into(sub, children, idcmdfirst, next_leaf, budget, vis);
                let _ = AppendMenuW(
                    parent,
                    MF_POPUP | MF_STRING,
                    sub.0 as usize,
                    &HSTRING::from(crate::i18n::t(title)),
                );
            }
            verbs::MenuItem::Verb(title, _) => {
                if *next_leaf >= budget {
                    return;
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
            }
            verbs::MenuItem::Separator => {
                // A divider — consumes no command id. (Skip a leading/trailing one
                // so we never start or end a (sub)menu with a stray separator.)
                let _ = AppendMenuW(parent, MF_SEPARATOR, 0, PCWSTR::null());
            }
        }
    }
}

impl ContextMenu {
    /// Build the preview on first demand. Both placements normally have a worker
    /// already running; this waits only for the small fixed budget. Idempotent:
    /// builds at most once, caching into `self.preview`.
    unsafe fn ensure_preview(&self) -> bool {
        if self.preview.borrow().is_some() {
            return true;
        }
        let path = self.paths.borrow().first().cloned();
        if let Some(path) = path {
            let prefetched = self.preview_job.borrow_mut().take();
            if let Some(p) = build_preview(&path, prefetched) {
                *self.preview.borrow_mut() = Some(p);
                return true;
            }
        }
        false
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
}
