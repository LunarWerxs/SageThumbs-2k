#![cfg(test)]

use super::*;

/// Every category must be findable by its own key, and no key may be duplicated — a
/// duplicate would make `category_index` return the FIRST page carrying it, silently
/// sending "open Settings on Quick preview" somewhere else.
#[test]
fn every_category_is_findable_by_its_own_key() {
    for ci in 0..NCAT {
        assert_eq!(
            category_index(nav_key(ci)),
            Some(ci),
            "category {ci} ({}) is not findable by its own key",
            nav_key(ci)
        );
    }
}

/// The Quick preview page is what the viewer's caption gear opens. If it ever stops
/// existing under this key, that button would quietly open page 0 instead.
#[test]
fn the_quick_preview_page_exists() {
    assert!(category_index("nav_quickpreview").is_some());
}
