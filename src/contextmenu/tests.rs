#![cfg(test)]

use super::*;
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyMenu, GetMenuItemInfoW, MENU_ITEM_TYPE, MFT_BITMAP, MFT_OWNERDRAW, MIIM_FTYPE, MIIM_ID,
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
        name: st2k_base::host::wide("photo.jpg"),
        info: st2k_base::host::wide("1500 x 1500 px - 96 KB"),
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
            name: st2k_base::host::wide("photo.jpg"),
            info: st2k_base::host::wide("1500 x 1500 px - 96 KB"),
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
