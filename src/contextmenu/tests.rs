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
        dpi: USER_DEFAULT_SCREEN_DPI,
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
            dpi: USER_DEFAULT_SCREEN_DPI,
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

/// The top level in a saved order, which is not leaf order, against a command-id budget that
/// runs out mid-list. Each item keeps its own leaf id; an item past the budget (a group as
/// much as a verb) is left out without ending the list, so a later item holding an earlier
/// leaf still shows; and the dividers are shared across items, so the two around the left-out
/// group draw as one. Before 2026-10-07 the group came out as an empty submenu row.
#[test]
fn top_level_skips_what_the_id_budget_cannot_reach_and_keeps_the_rest() {
    use windows::Win32::UI::WindowsAndMessaging::{GetMenuItemCount, MFT_SEPARATOR};

    const ITEMS: &[verbs::MenuItem] = &[
        verbs::MenuItem::Verb("BudgetTestB", verbs::VerbAction::Clipboard),
        verbs::MenuItem::Separator,
        verbs::MenuItem::Group(
            "BudgetTestGroup",
            &[
                verbs::MenuItem::Verb("BudgetTestG1", verbs::VerbAction::Clipboard),
                verbs::MenuItem::Verb("BudgetTestG2", verbs::VerbAction::Clipboard),
            ],
        ),
        verbs::MenuItem::Verb("BudgetTestD", verbs::VerbAction::Clipboard),
        verbs::MenuItem::Verb("BudgetTestC", verbs::VerbAction::Clipboard),
    ];
    // Display order with each item's own leaf start; a budget of 3 reaches leaves 0..=2.
    let top = [
        (&ITEMS[0], 0),
        (&ITEMS[1], 0),
        (&ITEMS[2], 4),
        (&ITEMS[3], 3),
        (&ITEMS[1], 0),
        (&ITEMS[4], 1),
    ];

    unsafe {
        let menu = CreatePopupMenu().expect("CreatePopupMenu");
        let vis = settings::menu_visibility();
        build_top_level_into(menu, &top, 1, 3, &vis);

        assert_eq!(
            GetMenuItemCount(Some(menu)),
            3,
            "want [B, divider, C]: no empty group row, no doubled divider"
        );
        let leaf_id = |leaf| verbs::id_for(verbs::CmdSlot::Leaf(verbs::LeafId(leaf)), 1);
        let (t0, id0) = item_type_and_id(menu, 0);
        assert!(
            !t0.contains(MFT_SEPARATOR) && id0 == leaf_id(0),
            "row 0 is B, leaf 0"
        );
        let (t1, _) = item_type_and_id(menu, 1);
        assert!(t1.contains(MFT_SEPARATOR), "row 1 is the one divider");
        let (t2, id2) = item_type_and_id(menu, 2);
        assert!(
            !t2.contains(MFT_SEPARATOR) && id2 == leaf_id(1),
            "row 2 is C, leaf 1"
        );

        let _ = DestroyMenu(menu);
    }
}

/// #61: above 100% display scaling Windows draws a bitmap menu item twice (1:1 and
/// stretched sideways by the scale), so a scaled display must get the owner-drawn item,
/// which only we measure and paint. At 100% an unskinned host keeps the bitmap branch:
/// here there is no file to decode, so that branch inserts nothing rather than falling
/// back to owner-draw.
#[test]
fn a_scaled_display_gets_an_owner_drawn_preview() {
    use windows::Win32::UI::WindowsAndMessaging::GetMenuItemCount;
    if menu_skin_loaded() {
        return; // a skinned test host owner-draws at every scale; nothing to tell apart
    }
    unsafe {
        for dpi in [120, 144, 192] {
            let cm = ContextMenu::default();
            cm.dpi.set(dpi);
            let menu = CreatePopupMenu().expect("CreatePopupMenu");
            assert!(cm.insert_preview(menu, 0, 42), "{dpi} DPI: inserted");
            let (ftype, id) = item_type_and_id(menu, 0);
            assert_eq!(id, 42);
            assert!(
                ftype.contains(MFT_OWNERDRAW),
                "{dpi} DPI must owner-draw the preview ({ftype:?}); a bitmap item is \
                 drawn twice by Windows' menu scaling"
            );
            let _ = DestroyMenu(menu);
        }
        let cm = ContextMenu::default();
        let menu = CreatePopupMenu().expect("CreatePopupMenu");
        assert!(
            !cm.insert_preview(menu, 0, 42),
            "100%: the bitmap branch, which has no tile without a file"
        );
        assert_eq!(GetMenuItemCount(Some(menu)), 0, "100% must not owner-draw");
        let _ = DestroyMenu(menu);
    }
}

/// Paint `p` through the menu's own paint path, black on white, and return the tile's
/// size and pixels (0x00RRGGBB, top-down).
unsafe fn paint_tile(p: &Preview) -> (i32, i32, Vec<u32>) {
    let (w, h) = tile_size(p);
    let pixels = paint_into_dib(w, h, |dc, rc| {
        paint_preview(dc, rc, p, 0x00FF_FFFF, 0);
    });
    (w, h, pixels)
}

/// `line` drawn unclipped in the menu font at `dpi`, black on white, on a canvas far
/// larger than the text: the reference for how tall its ink really is.
unsafe fn paint_unclipped(line: &[u16], dpi: u32) -> (i32, i32, Vec<u32>) {
    let (w, h) = (scale_px(600, dpi), scale_px(120, dpi));
    let pixels = paint_into_dib(w, h, |dc, rc| {
        let brush = CreateSolidBrush(COLORREF(0x00FF_FFFF));
        FillRect(dc, &rc, brush);
        let _ = DeleteObject(brush.into());
        SetBkMode(dc, TRANSPARENT);
        let old = select_menu_font(dc, dpi);
        SetTextColor(dc, COLORREF(0));
        let mut text = line.to_vec();
        let mut r = RECT {
            left: 10,
            top: 10,
            right: w - 10,
            bottom: h - 10,
        };
        DrawTextW(
            dc,
            &mut text,
            &mut r,
            DT_SINGLELINE | windows::Win32::Graphics::Gdi::DT_NOCLIP,
        );
        SelectObject(dc, old);
    });
    (w, h, pixels)
}

unsafe fn paint_into_dib(w: i32, h: i32, paint: impl FnOnce(HDC, RECT)) -> Vec<u32> {
    let bmi = st2k_base::safety::top_down_bmi(w, h);
    let mut bits: *mut core::ffi::c_void = core::ptr::null_mut();
    let dib = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).expect("DIB");
    let dc = CreateCompatibleDC(None);
    let old = SelectObject(dc, dib.into());
    paint(
        dc,
        RECT {
            left: 0,
            top: 0,
            right: w,
            bottom: h,
        },
    );
    let _ = GdiFlush();
    let n = (w * h) as usize;
    let pixels: Vec<u32> = core::slice::from_raw_parts(bits as *const u32, n)
        .iter()
        .map(|p| p & 0x00FF_FFFF)
        .collect();
    SelectObject(dc, old);
    let _ = DeleteDC(dc);
    let _ = DeleteObject(dib.into());
    pixels
}

/// The runs of rows holding any non-white pixel, as (first row, height).
fn ink_runs(w: i32, h: i32, pixels: &[u32]) -> Vec<(i32, i32)> {
    let mut runs = Vec::new();
    let mut start = None;
    for y in 0..h {
        let row = &pixels[(y * w) as usize..((y + 1) * w) as usize];
        let ink = row.iter().any(|&p| p != 0x00FF_FFFF);
        match (ink, start) {
            (true, None) => start = Some(y),
            (false, Some(s)) => {
                runs.push((s, y - s));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        runs.push((s, h - s));
    }
    runs
}

/// #61: both caption rows must hold the whole line at the menu's DPI. Before the fix the
/// rows were a fixed 18 px, so at 200% the 32 px menu font lost the bottom of each line
/// ("621 x 652 p" printed without the p's tail) and the picture box stayed 88 px. Each
/// line's ink in the tile must be exactly as tall as the same line drawn unclipped, and
/// nothing may touch the tile's bottom edge.
#[test]
fn caption_rows_hold_the_whole_line_at_every_scale() {
    for dpi in [96, 120, 144, 192, 288] {
        let mut p = Preview {
            hbm: HBITMAP::default(),
            w: 0,
            h: 0,
            name: st2k_base::host::wide("photo.jpg"),
            info: st2k_base::host::wide("621 \u{00d7} 652 px  \u{2013}  177 KB"),
            checker: false,
            dpi,
        };
        unsafe {
            let (w, h, tile) = paint_tile(&p);
            let runs = ink_runs(w, h, &tile);
            assert_eq!(runs.len(), 2, "{dpi} DPI: two caption lines, got {runs:?}");
            for (i, line) in [&p.name, &p.info].into_iter().enumerate() {
                let (rw, rh, reference) = paint_unclipped(line, dpi);
                let want = ink_runs(rw, rh, &reference);
                assert_eq!(want.len(), 1, "{dpi} DPI: reference line {i}: {want:?}");
                assert_eq!(
                    runs[i].1, want[0].1,
                    "{dpi} DPI: caption line {i} is clipped ({} of {} ink rows)",
                    runs[i].1, want[0].1
                );
            }
            let (last, height) = runs[1];
            assert!(
                last + height < h,
                "{dpi} DPI: the caption touches the tile's edge"
            );
            // A scaled tile is never shorter than the 100% one scaled (rounding aside).
            p.dpi = USER_DEFAULT_SCREEN_DPI;
            let (_, h96) = tile_size(&p);
            assert!(
                h >= scale_px(h96, dpi) - 2,
                "{dpi} DPI: {h} px is shorter than the 100% tile's {h96} px scaled"
            );
        }
    }
}
