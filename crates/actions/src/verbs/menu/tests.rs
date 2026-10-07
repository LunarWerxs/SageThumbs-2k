#![cfg(test)]

use super::*;

/// Depth-first leaf titles under `items` (the same walk `leaves()` does, but
/// scoped to an arbitrary subtree — used to check a quick group's alignment).
fn leaf_titles(items: &'static [MenuItem]) -> Vec<&'static str> {
    let mut out = Vec::new();
    fn walk(items: &'static [MenuItem], out: &mut Vec<&'static str>) {
        for it in items {
            match it {
                MenuItem::Group(_, c) => walk(c, out),
                MenuItem::Verb(t, _) => out.push(t),
                MenuItem::Separator => {}
            }
        }
    }
    walk(items, &mut out);
    out
}

/// `leaf_count()` (the cheap walk `id_for` uses for the preview offset) must
/// equal `leaves().len()` — they are two encodings of the same count.
#[test]
fn leaf_count_matches_leaves() {
    assert_eq!(leaf_count() as usize, leaves().len());
}

/// Every leaf id round-trips: `id_for(Leaf(i))` is offset `i`, and `slot_for`
/// maps it straight back to `Leaf(i)`. This is the contract the classic surface
/// relies on when the shell hands an id back in `InvokeCommand`.
#[test]
fn leaf_ids_round_trip() {
    let n = leaf_count();
    for i in 0..n {
        let id = id_for(CmdSlot::Leaf(LeafId(i)), 0);
        assert_eq!(id, i, "leaf offset must equal its index at idcmdfirst=0");
        match slot_for(id, n) {
            Some(CmdSlot::Leaf(LeafId(j))) => {
                assert_eq!(j, i, "slot_for must invert id_for for leaf {i}")
            }
            _ => panic!("leaf {i} did not round-trip to a Leaf slot"),
        }
    }
}

/// The owner-drawn preview sits exactly one slot past the last leaf, and that
/// slot round-trips to `Preview`; anything past it is not one of ours.
#[test]
fn preview_slot_is_just_past_last_leaf() {
    let n = leaf_count();
    let id = id_for(CmdSlot::Preview, 0);
    assert_eq!(id, n, "preview offset must be leaf_count()");
    assert!(matches!(slot_for(id, n), Some(CmdSlot::Preview)));
    assert!(slot_for(n + 1, n).is_none(), "past the preview is not ours");
}

/// Each `QuickItem`'s stored global index must line up with `leaves()`, so a
/// click on a quick-verb copy fires the SAME action as its in-submenu twin.
/// Footgun — reorder/insert a MENU item and these
/// indices shift — this test turns that silent misdispatch into a CI failure.
#[test]
fn quick_items_align_with_leaves() {
    let all = leaves();
    for qi in quick_items() {
        match qi {
            QuickItem::Leaf(title, idx) => assert_eq!(
                all[idx as usize].0, title,
                "quick leaf `{title}` (index {idx}) is misaligned with leaves()",
            ),
            QuickItem::Group(title, children, start) => {
                for (k, t) in leaf_titles(children).into_iter().enumerate() {
                    assert_eq!(
                        all[start as usize + k].0,
                        t,
                        "quick group `{title}` child {k} misaligned with global leaves",
                    );
                }
            }
        }
    }
}

/// Every `QUICK_KEYS` entry names a real top-level MENU item, and `quick_items`
/// yields exactly one per key (a typo'd key would silently vanish from the
/// quick menu otherwise).
#[test]
fn quick_keys_exist_in_menu() {
    for key in QUICK_KEYS {
        assert!(
            MENU.iter().any(|it| it.title() == *key),
            "QUICK_KEYS names `{key}`, not a top-level MENU item",
        );
    }
    assert_eq!(quick_items().len(), QUICK_KEYS.len());
}

/// Leaf-count tripwire: a MENU edit that adds/removes a verb changes this and
/// forces a conscious review of the index math above. Bump the number ONLY
/// after confirming `quick_items()` / the preview slot still line up.
#[test]
fn leaf_count_snapshot() {
    assert_eq!(
        leaf_count(),
        // Was 53; +1 for menu_save_video_frame (G193, the video-verbs fix).
        // +1 for menu_combine_pdf_searchable.
        55,
        "MENU leaf count changed — re-check quick_items()/preview-slot math, then update this snapshot",
    );
}

/// The keys a top-level item is addressed by (drag-reorder + per-item gating), in
/// default tree order: every reorderable item, never the `menu_settings` tail.
fn default_reorderable_keys() -> Vec<String> {
    MENU.iter()
        .map(|it| it.title())
        .filter(|t| !t.is_empty() && *t != "menu_settings")
        .map(String::from)
        .collect()
}

/// `order_top_level_with` keeps command ids STABLE (each item carries its ORIGINAL
/// leaf-start index, so dispatch through the default `leaves()` never misfires),
/// reproduces the default tree from the factory tokens, and renders user-placed
/// `MENU_SEP_TOKEN` dividers WYSIWYG (with leading/consecutive/trailing normalized).
#[test]
fn ordered_top_level_id_stability_and_separators() {
    let default_titles: Vec<&str> = MENU.iter().map(|it| it.title()).collect();
    let keys = default_reorderable_keys();

    // Empty order → the default tree verbatim (every separator included).
    let empty: Vec<&str> = order_top_level_with(&[])
        .iter()
        .map(|(it, _)| it.title())
        .collect();
    assert_eq!(
        empty, default_titles,
        "empty order must be the default tree"
    );

    // The factory tokens (items + divider markers) reproduce the default tree exactly.
    let factory: Vec<String> = default_menu_tokens()
        .iter()
        .map(|s| s.to_string())
        .collect();
    let factory_titles: Vec<&str> = order_top_level_with(&factory)
        .iter()
        .map(|(it, _)| it.title())
        .collect();
    assert_eq!(
        factory_titles, default_titles,
        "factory tokens must equal the default tree"
    );

    // Canonical leaf-start per top-level item.
    let mut canon = std::collections::HashMap::new();
    let mut idx = 0u32;
    for it in MENU {
        if !it.title().is_empty() {
            canon.insert(it.title(), idx);
        }
        idx += count_leaves(it);
    }

    // Reversed items (no divider tokens) → items reversed, ids still canonical, then
    // exactly one divider + Settings last; every item exactly once.
    let mut rev = keys.clone();
    rev.reverse();
    let out = order_top_level_with(&rev);
    for (it, start) in &out {
        if !it.title().is_empty() {
            assert_eq!(
                *start,
                canon[it.title()],
                "id offset drifted for {}",
                it.title()
            );
        }
    }
    assert_eq!(
        out.first().unwrap().0.title(),
        keys.last().unwrap().as_str(),
        "reversed → the last default-order reorderable item comes first"
    );
    assert_eq!(
        out.last().unwrap().0.title(),
        "menu_settings",
        "Settings stays last"
    );
    assert_eq!(
        out.iter()
            .filter(|(it, _)| matches!(it, MenuItem::Separator))
            .count(),
        1,
        "no divider tokens → only the Settings divider",
    );
    for k in &keys {
        assert_eq!(
            out.iter()
                .filter(|(it, _)| it.title() == k.as_str())
                .count(),
            1,
            "item {k} appears exactly once",
        );
    }

    // A user-placed divider renders exactly between the two items it sits between.
    let mut custom: Vec<String> = vec!["menu_resize".into(), MENU_SEP_TOKEN.into()];
    custom.extend(keys.iter().filter(|k| *k != "menu_resize").cloned());
    let titles: Vec<&str> = order_top_level_with(&custom)
        .iter()
        .map(|(it, _)| it.title())
        .collect();
    let ri = titles.iter().position(|t| *t == "menu_resize").unwrap();
    assert_eq!(titles[ri + 1], "", "divider must follow menu_resize");
    assert_eq!(
        titles[ri + 2],
        "menu_convert_into",
        "convert_into after the divider"
    );

    // Leading / consecutive / trailing divider tokens normalize away.
    let mut messy: Vec<String> = vec![MENU_SEP_TOKEN.into(), MENU_SEP_TOKEN.into()];
    messy.extend(keys.iter().cloned());
    messy.push(MENU_SEP_TOKEN.into());
    let out2 = order_top_level_with(&messy);
    assert!(
        !matches!(out2.first().unwrap().0, MenuItem::Separator),
        "no leading divider"
    );
    assert_eq!(
        out2.iter()
            .filter(|(it, _)| matches!(it, MenuItem::Separator))
            .count(),
        1,
        "messy dividers collapse to just the Settings divider",
    );
}

/// `audio_top_level` surfaces ONLY the audio-relevant verbs (Rename ▸ ·
/// Files to folder · Sort ▸, in MENU order), then exactly one divider + the
/// always-last Settings — each carrying its ORIGINAL leaf-start index so a click
/// dispatches to the SAME action as on the full menu. `top_level_audio_ok` agrees
/// on exactly that set (incl. Settings). This is the audio counterpart of the
/// `condensed`/`ordered` id-stability tests above.
#[test]
fn audio_top_level_is_audio_set_with_stable_ids() {
    // Canonical leaf-start per top-level item (depth-first leaf order) — the same
    // map the ordered test checks ids against.
    let mut canon = std::collections::HashMap::new();
    let mut idx = 0u32;
    for it in MENU {
        if !it.title().is_empty() {
            canon.insert(it.title(), idx);
        }
        idx += count_leaves(it);
    }

    let out = audio_top_level();
    let titles: Vec<&str> = out.iter().map(|(it, _)| it.title()).collect();

    // The audio verbs in MENU order, then a divider ("") + Settings last. Pick color is a
    // system-wide screen picker (works on any selection), so it's in this set too.
    assert_eq!(
        titles,
        vec![
            "menu_rename",
            "menu_files_to_folder",
            "menu_sort",
            "menu_pick_color",
            "",
            "menu_settings"
        ],
        "audio menu = rename / files-to-folder / sort / pick-color + divider + Settings",
    );
    assert_eq!(
        out.iter()
            .filter(|(it, _)| matches!(it, MenuItem::Separator))
            .count(),
        1,
        "exactly one divider, before Settings",
    );
    assert_eq!(
        out.last().unwrap().0.title(),
        "menu_settings",
        "Settings stays last"
    );

    // Every non-divider item keeps its canonical leaf-start index → ids stay stable.
    for (it, start) in &out {
        if !it.title().is_empty() {
            assert_eq!(
                *start,
                canon[it.title()],
                "id offset drifted for {}",
                it.title()
            );
        }
    }

    // `top_level_audio_ok` matches `audio_top_level`'s set plus Settings, and rejects
    // the image-only verbs (which the surfaces hide on an audio-only selection).
    for k in [
        "menu_files_to_folder",
        "menu_rename",
        "menu_sort",
        "menu_pick_color",
        "menu_settings",
    ] {
        assert!(top_level_audio_ok(k), "{k} should be audio-ok");
    }
    for k in [
        "menu_convert_into",
        "menu_convert_dialog",
        "menu_resize",
        "menu_rotate",
        "menu_wallpaper",
        "menu_lock_screen",
        "menu_copy",
        "menu_set_folder_icon",
    ] {
        assert!(!top_level_audio_ok(k), "{k} is image-only");
    }
}

/// `video_top_level` surfaces the frame-grab leaf plus the file-agnostic verbs
/// (Files to folder · Pick color, in MENU order), then exactly one divider + the
/// always-last Settings — each carrying its ORIGINAL leaf-start index so a click
/// dispatches to the SAME action as on the full menu. Video counterpart of the
/// `audio_top_level` test above.
#[test]
fn video_top_level_is_video_set_with_stable_ids() {
    let mut canon = std::collections::HashMap::new();
    let mut idx = 0u32;
    for it in MENU {
        if !it.title().is_empty() {
            canon.insert(it.title(), idx);
        }
        idx += count_leaves(it);
    }

    let out = video_top_level();
    let titles: Vec<&str> = out.iter().map(|(it, _)| it.title()).collect();
    assert_eq!(
        titles,
        vec![
            "menu_save_video_frame",
            "menu_files_to_folder",
            "menu_pick_color",
            "",
            "menu_settings"
        ],
        "video menu = save-frame / files-to-folder / pick-color + divider + Settings",
    );
    assert_eq!(
        out.iter()
            .filter(|(it, _)| matches!(it, MenuItem::Separator))
            .count(),
        1,
        "exactly one divider, before Settings",
    );
    assert_eq!(
        out.last().unwrap().0.title(),
        "menu_settings",
        "Settings stays last"
    );
    for (it, start) in &out {
        if !it.title().is_empty() {
            assert_eq!(
                *start,
                canon[it.title()],
                "id offset drifted for {}",
                it.title()
            );
        }
    }
}

/// The `MenuOrder` an older Settings wrote on this owner's PC (2026-10-07): the 3.0 factory
/// order, saved by an OK nobody moved a row in. Searchable PDF, Save frame and Compress were
/// missing from it, and so sat at the bottom of the menu.
const OWNERS_OLD_VALUE: &str = "menu_convert_into,menu_convert_dialog,menu_combine_pdf,\
menu_combine_cbz,--,menu_resize,menu_email,menu_rotate,--,menu_rename,menu_files_to_folder,\
menu_sort,--,menu_copy_text,menu_image_info,menu_pick_color,menu_strip_meta,menu_copy,\
menu_copy_data_uri,menu_upload,--,menu_set_folder_icon,menu_wallpaper,menu_lock_screen";

fn split(value: &str) -> Vec<String> {
    value.split(',').map(String::from).collect()
}

/// An old factory order an older Settings froze is not a choice anyone made: it shows the
/// current factory order, including when rows that Settings appended later trail it.
#[test]
fn an_untouched_old_order_follows_the_factory_order() {
    let factory = default_menu_tokens();
    assert_eq!(effective_menu_tokens(&split(OWNERS_OLD_VALUE)), factory);

    // 0.7 saved upload in place; a 3.0 OK appended data URI + lock screen, a 3.4 OK the
    // searchable PDF, each after everything else.
    let appended = "menu_convert_into,menu_convert_dialog,menu_combine_pdf,menu_combine_cbz,--,\
menu_resize,menu_email,menu_rotate,--,menu_rename,menu_files_to_folder,menu_sort,--,\
menu_copy_text,menu_image_info,menu_pick_color,menu_strip_meta,menu_copy,menu_upload,--,\
menu_set_folder_icon,menu_wallpaper,menu_copy_data_uri,menu_lock_screen,\
menu_combine_pdf_searchable";
    assert_eq!(effective_menu_tokens(&split(appended)), factory);

    // Nothing left to arrange (a marked order whose every key has since been renamed) is the
    // factory order too, its dividers included.
    let stale = [
        MENU_ORDER_CUSTOM.to_string(),
        "menu_renamed_away".to_string(),
    ];
    assert_eq!(effective_menu_tokens(&stale), factory);
}

/// An order the user arranged is theirs, kept as placed; what it lacks joins its factory
/// group (straight after its factory predecessor) instead of the bottom of the menu.
#[test]
fn a_users_own_order_is_kept_and_new_items_join_their_group() {
    // The old value with Rotate moved to the top: a real choice, though unmarked.
    let moved = OWNERS_OLD_VALUE.replace(",menu_rotate", "");
    let got = effective_menu_tokens(&split(&format!("menu_rotate,{moved}")));
    assert_eq!(
        got[0], "menu_rotate",
        "the user's own first item stays first"
    );
    let after = |key: &str| got[got.iter().position(|t| *t == key).unwrap() + 1];
    assert_eq!(after("menu_email"), "menu_compress");
    assert_eq!(after("menu_combine_pdf"), "menu_combine_pdf_searchable");
    assert_eq!(after("menu_combine_cbz"), "menu_save_video_frame");
    for key in default_menu_tokens()
        .iter()
        .filter(|t| **t != MENU_SEP_TOKEN)
    {
        let n = got.iter().filter(|t| *t == key).count();
        assert_eq!(n, 1, "{key} must appear exactly once");
    }

    // Copy text dragged to the bottom. This value was saved by 3.0 or later (it holds 3.0's
    // rows in place), and no such Settings ever appended Copy text: the user put it there.
    let dragged = format!(
        "{},menu_copy_text",
        OWNERS_OLD_VALUE.replace("menu_copy_text,", "")
    );
    let got = effective_menu_tokens(&split(&dragged));
    assert_eq!(
        got.last(),
        Some(&"menu_copy_text"),
        "a drag to the end is kept"
    );

    // A marked order is a choice even when it IS the old layout (someone who preferred it).
    let mut marked = vec![MENU_ORDER_CUSTOM.to_string()];
    marked.extend(split(OWNERS_OLD_VALUE));
    let got = effective_menu_tokens(&marked);
    assert_eq!(
        got[..4],
        [
            "menu_convert_into",
            "menu_convert_dialog",
            "menu_combine_pdf",
            "menu_combine_pdf_searchable"
        ]
    );
    assert_ne!(got, default_menu_tokens());
}

/// Settings saves nothing for the factory order (so the next default reaches the user) and
/// a marked copy for anything else, which reads back exactly as arranged.
#[test]
fn settings_saves_only_an_order_the_user_arranged() {
    let factory = default_menu_tokens();
    assert!(menu_order_to_save(&factory).is_empty());

    let mut arranged = factory.clone();
    arranged.swap(0, 2);
    let saved = menu_order_to_save(&arranged);
    assert_eq!(saved[0], MENU_ORDER_CUSTOM);
    assert_eq!(effective_menu_tokens(&saved), arranged);
}
