#![cfg(test)]

use super::*;

/// Regression for `paint_chrome`'s hardcoded id lists silently falling out of sync with
/// `cat_rows`: these six fields all have a `Row::Pair` row (so they were laid out and
/// live on some page) but a Pair row added since the hand-kept lists were last updated
/// meant `paint_chrome` never framed them — no rounded field frame behind the control,
/// unlike every sibling input. `pair_field_ids` must find every one of them without
/// anyone having to remember to extend a list by hand.
#[test]
fn every_pair_field_is_covered_and_bucketed_correctly() {
    let (edits, combos) = pair_field_ids();
    // The edits missed before this fix.
    assert!(edits.contains(&ID_VIDEO_OFFSET), "ID_VIDEO_OFFSET missing");
    // The combos missed before this fix.
    for id in [
        ID_CORNER_MARK,
        ID_SHOT_QUICK_HOTKEY,
        ID_SHOT_DELAY,
        ID_SHOT_ACTION,
        ID_SHOT_ACTION_HK,
    ] {
        assert!(combos.contains(&id), "combo id {id} missing");
    }
    // A field never appears in both buckets (its field_h picks exactly one shape).
    for id in &edits {
        assert!(
            !combos.contains(id),
            "id {id} classified as both edit and combo"
        );
    }
}

/// The same regression one row kind over: the licence-key edit is a `Row::WideBtn`, which
/// no hand-kept list in `paint_chrome` named, so it shipped as a bare strip with no frame.
#[test]
fn every_wide_text_edit_is_framed_and_none_is_framed_twice() {
    let wide = wide_edit_ids();
    assert!(
        wide.contains(&ID_SEARCH),
        "the format filter lost its frame"
    );
    assert!(
        wide.contains(&ID_LICENCE_KEY_EDIT),
        "the licence key lost its frame"
    );
    let (edits, combos) = pair_field_ids();
    for id in &wide {
        assert!(
            !edits.contains(id) && !combos.contains(id),
            "id {id} would be framed twice"
        );
    }
}
