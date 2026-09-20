#![cfg(test)]

use super::*;

/// The modern quick verbs MUST be the same set, in the same order, as the classic
/// `quick_items()` — i.e. [`QUICK_VERBS`] keys == [`verbs::QUICK_KEYS`]. If `QUICK_KEYS`
/// changes (a quick verb added/removed/reordered) without updating the CLSID table + the
/// manifest verbs, the two menus would silently diverge — this turns that into a CI failure.
#[test]
fn quick_verbs_match_quick_keys() {
    let keys: Vec<&str> = QUICK_VERBS.iter().map(|(_, k, _)| *k).collect();
    assert_eq!(
        keys,
        verbs::QUICK_KEYS,
        "QUICK_VERBS keys must equal verbs::QUICK_KEYS"
    );
}

/// Every quick-verb CLSID resolves to a real top-level `MENU` item, and `is_quick_clsid`
/// recognizes it; a non-quick CLSID does not.
#[test]
fn quick_clsids_resolve_to_menu_items() {
    for (clsid, key, _) in QUICK_VERBS {
        assert!(is_quick_clsid(*clsid), "is_quick_clsid missed {key}");
        let item = quick_root_item(*clsid).unwrap_or_else(|| panic!("no MENU item for {key}"));
        assert_eq!(
            item.title(),
            *key,
            "quick_root_item returned the wrong MENU node for {key}"
        );
    }
    assert!(!is_quick_clsid(crate::guids::CLSID_EXPLORER_COMMAND));
    assert!(quick_root_item(crate::guids::CLSID_EXPLORER_COMMAND).is_none());
}

/// A quick verb's GetState must hide it when EITHER gate is off, not just the
/// master "Quick verbs on the main menu" toggle. Before this, the quick_root
/// branch never consulted `menu_visibility()` at all, so hiding e.g. "Resize" in
/// Settings' "Menu items" list left it visible in the Win11 compact flyout.
#[test]
fn quick_root_visibility_requires_both_the_master_toggle_and_the_per_item_setting() {
    assert!(quick_root_visible(true, true));
    assert!(
        !quick_root_visible(false, true),
        "master toggle off must hide a quick verb even if its own setting is shown"
    );
    assert!(
        !quick_root_visible(true, false),
        "the per-item 'Menu items' hide must hide a quick verb, not just the master toggle"
    );
    assert!(!quick_root_visible(false, false));
}

/// The quick-verb CLSIDs are all distinct (a copy-paste dup would make two verbs activate
/// the same coclass and silently collapse to one item).
#[test]
fn quick_clsids_are_distinct() {
    for (i, (a, _, _)) in QUICK_VERBS.iter().enumerate() {
        for (b, _, _) in &QUICK_VERBS[i + 1..] {
            assert_ne!(a, b, "duplicate quick-verb CLSID");
        }
    }
}
