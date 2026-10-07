//! The SageThumbs submenu as Explorer receives it, through the same COM path Explorer drives:
//! the factory groups in the factory order, each fenced by a divider, and only the verbs the
//! selection can use.
//!
//! Why this drives the real `QueryContextMenu` rather than the builder: from 2.0.0 to
//! 2026-10-07 the builder's own test passed (it fed a whole slice in one call) while the top
//! level, built one item per call, dropped every divider. The owner saw one undivided list of
//! 24 items, "Save frame as image" included on a JPG.
#![cfg(windows)]

mod common;

use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, DestroyMenu, GetMenuItemCount, GetMenuItemInfoW, GetMenuStringW, GetSubMenu,
    HMENU, MENUITEMINFOW, MFT_BITMAP, MFT_OWNERDRAW, MFT_SEPARATOR, MF_BYPOSITION, MIIM_FTYPE,
};

use st2k_actions::verbs::{default_menu_tokens, top_level_needs_video, MENU_SEP_TOKEN};

const TEST_SETTINGS_ROOT: &str = r"Software\SageThumbs2K\__test_context_menu_layout_absent";

/// One row of a built menu, as the user sees it.
#[derive(Debug, PartialEq)]
enum Row {
    /// The preview tile: a bitmap row, or an owner-drawn one on a menu-skinned host.
    Preview,
    Divider,
    Label(String),
}

unsafe fn rows(menu: HMENU) -> Vec<Row> {
    let mut out = Vec::new();
    for i in 0..unsafe { GetMenuItemCount(Some(menu)) }.max(0) as u32 {
        let mut info = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_FTYPE,
            ..Default::default()
        };
        unsafe { GetMenuItemInfoW(menu, i, true, &mut info) }.expect("menu item info");
        out.push(if info.fType.contains(MFT_SEPARATOR) {
            Row::Divider
        } else if info.fType.contains(MFT_BITMAP) || info.fType.contains(MFT_OWNERDRAW) {
            Row::Preview
        } else {
            let mut buf = [0u16; 256];
            let n = unsafe { GetMenuStringW(menu, i, Some(&mut buf), MF_BYPOSITION) };
            Row::Label(String::from_utf16_lossy(&buf[..n.max(0) as usize]))
        });
    }
    out
}

#[test]
fn an_images_menu_shows_the_factory_groups_divided_and_no_video_verb() {
    let dir = std::env::temp_dir().join(format!("st2k_context_menu_layout_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("photo.png");
    image::DynamicImage::ImageRgba8(image::RgbaImage::new(64, 48))
        .save_with_format(&png, image::ImageFormat::Png)
        .unwrap();

    // The labels below and the DLL's must read the same (absent) settings root, and the root
    // is resolved once per process: set it before the first `i18n::t`.
    unsafe { common::set_test_env("ST2K_SETTINGS_ROOT", TEST_SETTINGS_ROOT) };

    // The menu as the factory order says it should read: the preview and its divider, each
    // group, a divider between groups, a divider and Settings last. No video in the
    // selection, so no video verb.
    let mut want = vec![Row::Preview, Row::Divider];
    want.extend(
        default_menu_tokens()
            .into_iter()
            .filter(|key| !top_level_needs_video(key))
            .map(|key| match key {
                MENU_SEP_TOKEN => Row::Divider,
                _ => Row::Label(st2k_base::i18n::t(key).to_string()),
            }),
    );
    want.push(Row::Divider);
    want.push(Row::Label(st2k_base::i18n::t("menu_settings").to_string()));
    assert!(
        want.iter().filter(|r| **r == Row::Divider).count() >= 4,
        "the factory order has lost its groups: {want:?}"
    );

    unsafe {
        let menu = common::classic_context_menu_for(&png, TEST_SETTINGS_ROOT)
            .expect("create and initialize classic context menu");
        let popup = CreatePopupMenu().expect("CreatePopupMenu");
        menu.QueryContextMenu(popup, 0, 1, 0x7fff, 0)
            .ok()
            .expect("QueryContextMenu");
        let submenu = GetSubMenu(popup, 0);
        assert!(
            !submenu.is_invalid(),
            "the SageThumbs submenu is the first root item"
        );
        assert_eq!(rows(submenu), want, "the SageThumbs submenu, row by row");
        let _ = DestroyMenu(popup);
    }
    let _ = std::fs::remove_dir_all(&dir);
}
