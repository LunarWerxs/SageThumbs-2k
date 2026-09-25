#![cfg(test)]

use super::*;

/// Every row must map to its own name, its own automation id, the same role, and a
/// selected state that tracks exactly the active category — never two rows selected, and
/// never a row that reads as selected while some other page is showing. This is the mapping
/// a screen reader actually hears; a test that cannot fail (e.g. only checking row 0) is
/// worse than none, so it is checked for every (row, active) pair.
#[test]
fn every_row_maps_to_its_own_name_role_and_selected_state() {
    for ci in 0..NCAT {
        for active in 0..NCAT {
            let f = nav_item_facts_for(ci, active, false);
            assert_eq!(f.name, nav_label(ci));
            assert_eq!(f.automation_id, nav_key(ci));
            assert_eq!(f.control_type, UIA_ListItemControlTypeId);
            assert_eq!(
                f.selected,
                ci == active,
                "row {ci} selected must track active {active}, not the other way round"
            );
        }
    }
}

/// Focus and selection are independent: Tab can land keyboard focus on a row before
/// Enter/Space switches the page, so a focused-but-not-active row must report focused
/// without also reporting selected — collapsing the two would tell a screen reader the
/// page changed when only the highlight moved.
#[test]
fn focus_is_reported_independently_of_selection() {
    let f = nav_item_facts_for(2, 0, true);
    assert!(f.focused, "a focused row must say so");
    assert!(!f.selected, "focus alone must not imply selection");
}
