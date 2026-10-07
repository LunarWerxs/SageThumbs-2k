//! Latency guard for the legacy Explorer context-menu surface.
//!
//! This is deliberately an integration test rather than a micro-benchmark: it drives the
//! exact COM path Explorer uses (shell item -> IDataObject -> IShellExtInit ->
//! QueryContextMenu).  The budget is intentionally generous; it catches the old synchronous
//! preview decode (which could wait two seconds) without failing due to ordinary CI noise.
#![cfg(windows)]

mod common;

use std::time::{Duration, Instant};

use windows::core::Interface;
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::Shell::IContextMenu3;
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, DestroyMenu, GetMenuItemCount, GetMenuItemInfoW, GetSubMenu, HMENU,
    MENUITEMINFOW, MFT_BITMAP, MFT_OWNERDRAW, MFT_SEPARATOR, MIIM_FTYPE, WM_INITMENUPOPUP,
};

/// The handler's own host probe, repeated here (it is crate-private, and this test drives the DLL
/// from outside). A menu skin is injected into the host process, so an in-process module check is
/// the whole test.
fn menu_skin_loaded() -> bool {
    use windows::core::w;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    [
        w!("StartAllBackX64.dll"),
        w!("DarkMagicX64.dll"),
        w!("ExplorerPatcher.amd64.dll"),
    ]
    .iter()
    .any(|n| unsafe { GetModuleHandleW(*n) }.is_ok())
}

const TEST_SETTINGS_ROOT: &str = r"Software\SageThumbs2K\__test_context_menu_latency_absent";
const RIGHT_CLICK_BUDGET: Duration = Duration::from_millis(750);
const POPUP_INIT_BUDGET: Duration = Duration::from_millis(500);

#[test]
fn first_query_context_menu_is_fast_with_default_preview_enabled() {
    let dir =
        std::env::temp_dir().join(format!("st2k_context_menu_latency_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("large-enough-to-decode.png");
    // A real but modest image: preview decode starts on a worker during Initialize and menu
    // construction may wait only for the handler's small fixed budget, never an unbounded decode.
    image::DynamicImage::ImageRgba8(image::RgbaImage::new(4096, 4096))
        .save_with_format(&png, image::ImageFormat::Png)
        .unwrap();

    unsafe {
        let started = Instant::now();
        let menu = common::classic_context_menu_for(&png, TEST_SETTINGS_ROOT)
            .expect("create and initialize classic context menu");
        let popup: HMENU = CreatePopupMenu().expect("CreatePopupMenu");
        let result = menu.QueryContextMenu(popup, 0, 1, 0x7fff, 0);
        let elapsed = started.elapsed();
        result.ok().expect("QueryContextMenu");
        assert!(
            elapsed < RIGHT_CLICK_BUDGET,
            "context-menu initialization + first QueryContextMenu took {} ms (budget {} ms): preview work must stay off the shell thread",
            elapsed.as_millis(), RIGHT_CLICK_BUDGET.as_millis()
        );

        // Product defaults place the preview inside the SageThumbs submenu. Real Explorer does
        // not reliably forward WM_INITMENUPOPUP for an extension-created child popup, so the
        // preview + separating divider must already exist when QueryContextMenu returns.
        let submenu = GetSubMenu(popup, 0);
        assert!(
            !submenu.is_invalid(),
            "first root item must be the SageThumbs submenu under default settings"
        );
        let after_query = GetMenuItemCount(Some(submenu));
        assert!(
            after_query >= 3,
            "QueryContextMenu must synchronously insert preview + separator before command items"
        );

        let mut preview = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_FTYPE,
            ..Default::default()
        };
        GetMenuItemInfoW(submenu, 0, true, &mut preview).expect("preview menu item info");
        // The handler picks the item kind from the HOST (see contextmenu.rs): a menu-skinning
        // shell clips any bitmap item to an icon-sized sliver, so those get owner-draw; everyone
        // else gets a real bitmap item, because one owner-drawn item un-themes the whole popup.
        // Re-probe the same way the handler does, so this asserts the branch rather than one
        // machine's answer. A plain test process has no skin injected, so in practice this is the
        // bitmap branch, which is exactly the default worth guarding.
        if menu_skin_loaded() {
            assert!(
                preview.fType.contains(MFT_OWNERDRAW),
                "on a menu-skinned host the flyout preview must be OWNER-DRAWN ({:?}): a skin \
                 measures any bitmap item as an icon and clips the tile to a sliver",
                preview.fType
            );
        } else {
            assert!(
                preview.fType.contains(MFT_BITMAP) && !preview.fType.contains(MFT_OWNERDRAW),
                "on a stock host the flyout preview must be a BITMAP item ({:?}): one \
                 owner-drawn item drops the entire popup off the themed drawing path",
                preview.fType
            );
        }
        let mut divider = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_FTYPE,
            ..Default::default()
        };
        GetMenuItemInfoW(submenu, 1, true, &mut divider).expect("preview divider item info");
        assert_eq!(
            divider.fType, MFT_SEPARATOR,
            "preview must be followed by a separator"
        );

        // Explorer may still forward the lifecycle notification. It must remain a fast no-op
        // and must never duplicate the already-present preview row.
        let menu3: IContextMenu3 = menu.cast().expect("IContextMenu3");
        let popup_started = Instant::now();
        menu3
            .HandleMenuMsg2(
                WM_INITMENUPOPUP,
                WPARAM(submenu.0 as usize),
                LPARAM(0),
                None,
            )
            .expect("WM_INITMENUPOPUP compatibility notification");
        let popup_elapsed = popup_started.elapsed();
        assert!(
            popup_elapsed < POPUP_INIT_BUDGET,
            "WM_INITMENUPOPUP handling took {} ms (budget {} ms)",
            popup_elapsed.as_millis(),
            POPUP_INIT_BUDGET.as_millis()
        );
        assert_eq!(
            GetMenuItemCount(Some(submenu)),
            after_query,
            "submenu initialization must not duplicate the preview"
        );
        let _ = DestroyMenu(popup);
    }
    let _ = std::fs::remove_dir_all(&dir);
}
