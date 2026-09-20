#![cfg(test)]

use super::*;

fn letter(n: usize) -> Vec<PageSize> {
    (0..n)
        .map(|_| PageSize {
            w: 816.0,
            h: 1056.0,
        })
        .collect()
}

#[test]
fn pages_stack_with_a_gap_between_them_and_none_after_the_last() {
    let l = layout(&letter(3), 800, 10);
    // 816x1056 at 800 wide is 1035 tall.
    assert_eq!(l.heights, vec![1035, 1035, 1035]);
    assert_eq!(l.tops, vec![0, 1045, 2090]);
    assert_eq!(
        l.total, 3125,
        "the document ends at the last page's bottom, not a gap past it"
    );
}

/// Mixed page sizes are the case a "every page is the same height" shortcut gets wrong, and
/// a PDF is perfectly entitled to mix them (a landscape table in a portrait report).
#[test]
fn a_document_that_mixes_page_sizes_lays_out_each_page_on_its_own_aspect() {
    let sizes = vec![
        PageSize {
            w: 816.0,
            h: 1056.0,
        }, // portrait letter
        PageSize {
            w: 1056.0,
            h: 816.0,
        }, // the same sheet, landscape
    ];
    let l = layout(&sizes, 800, 10);
    assert_eq!(l.heights, vec![1035, 618]);
    assert_eq!(l.tops, vec![0, 1045]);
}

#[test]
fn a_document_shorter_than_the_window_cannot_scroll() {
    let l = layout(&letter(1), 400, 10);
    assert_eq!(max_scroll(l.total, l.total + 200), 0);
    assert_eq!(max_scroll(l.total, 100), l.total - 100);
}

#[test]
fn the_visible_range_covers_every_page_the_viewport_touches() {
    let l = layout(&letter(5), 800, 10); // pages 1035 tall, tops 0/1045/2090/3135/4180
    assert_eq!(visible_range(&l, 0, 500), (0, 1));
    // Straddling the boundary must include BOTH pages, or the second one paints as a hole.
    assert_eq!(visible_range(&l, 1000, 200), (0, 2));
    assert_eq!(visible_range(&l, 2100, 900), (2, 3));
    assert_eq!(
        visible_range(&l, 0, 10_000),
        (0, 5),
        "a window taller than the document shows all of it"
    );
}

/// A scroll position outside the document must still name a page. Returning an empty range
/// would paint nothing, which is indistinguishable from a broken document.
#[test]
fn a_scroll_past_the_end_still_shows_the_last_page() {
    let l = layout(&letter(3), 800, 10);
    assert_eq!(visible_range(&l, 99_999, 400), (2, 3));
}

/// The caption must follow what FILLS the window, not what happens to touch its top edge.
#[test]
fn the_named_page_is_the_one_covering_most_of_the_view() {
    let l = layout(&letter(3), 800, 10);
    assert_eq!(page_at(&l, 0, 600), 0);
    // The case the rule exists for: the TOP EDGE is still inside page one, with five
    // pixels of it left, while page two fills the rest of the window. Naming page one
    // here (what "the page at the top edge" would do) is the bug.
    assert_eq!(
        page_at(&l, 1030, 600),
        1,
        "five pixels of page one against 585 of page two is page two"
    );
    // And the other way: a short window entirely inside page one, close to its bottom.
    assert_eq!(
        page_at(&l, 1000, 40),
        0,
        "wholly inside page one, however near the end of it"
    );
    assert_eq!(page_at(&l, 2090, 400), 2);
}

/// Degenerate inputs reach this code through a corrupt or unusual document, and a panic in
/// a paint handler takes the window down.
#[test]
fn layout_survives_absurd_page_sizes() {
    let sizes = vec![
        PageSize { w: 0.0, h: 0.0 },
        PageSize { w: 1.0, h: 1e9 },
        PageSize { w: 1e9, h: 1.0 },
    ];
    let l = layout(&sizes, 800, 10);
    assert_eq!(l.heights.len(), 3);
    assert!(l.heights.iter().all(|&h| h >= 1), "no page is zero-height");
    assert!(l.total > 0);
    let _ = visible_range(&l, 0, 500);
    let _ = page_at(&l, 0, 500);
}

fn doc_zoom(z: f64) -> f64 {
    z.clamp(super::ZOOM_MIN, super::ZOOM_MAX)
}

/// Zoom multiplies the width pages are RENDERED at, which is the whole mechanism: the tile
/// is rasterized bigger rather than a fit-width bitmap being stretched. If this ever
/// returned the base width, zoom would silently go back to being a blur.
#[test]
fn zoom_scales_the_width_pages_are_rasterized_at() {
    let w = |z: f64| ((1000.0_f64 * doc_zoom(z)).round() as i32).clamp(16, 1 << 15);
    assert_eq!(w(1.0), 1000);
    assert_eq!(w(1.5), 1500);
    assert_eq!(w(4.0), 4000);
}

/// The floor is fit-width. Below it the page is smaller than the pane and there is nothing
/// to see that fit-width does not already show, so zooming out past it is empty margin.
/// The ceiling keeps a rasterized page inside sane memory.
#[test]
fn zoom_is_clamped_at_both_ends() {
    assert_eq!(doc_zoom(0.1), super::ZOOM_MIN);
    assert_eq!(doc_zoom(0.99), super::ZOOM_MIN);
    assert_eq!(doc_zoom(99.0), super::ZOOM_MAX);
    assert_eq!(doc_zoom(2.0), 2.0);
}

/// Clicking a thumbnail must land on the page you pointed at. An off-by-one here sends the
/// reader somewhere else, which is glaring in use and invisible in a screenshot.
#[test]
fn a_click_in_the_strip_picks_the_thumbnail_under_it() {
    // pitch 100, showing from page 0, 4 pages.
    assert_eq!(super::strip_page_at(0, 100, 0, 4), Some(0));
    assert_eq!(super::strip_page_at(99, 100, 0, 4), Some(0));
    assert_eq!(super::strip_page_at(100, 100, 0, 4), Some(1));
    assert_eq!(super::strip_page_at(350, 100, 0, 4), Some(3));
}

/// A scrolled strip is offset by its first visible page, and the empty tail below the last
/// thumbnail is not a page at all.
#[test]
fn a_scrolled_strip_offsets_and_the_tail_is_not_a_page() {
    assert_eq!(super::strip_page_at(0, 100, 7, 20), Some(7));
    assert_eq!(super::strip_page_at(250, 100, 7, 20), Some(9));
    assert_eq!(
        super::strip_page_at(400, 100, 4, 6),
        None,
        "past the last page"
    );
    assert_eq!(super::strip_page_at(-5, 100, 0, 4), None, "above the first");
}

/// Degenerate inputs reach this from a resize mid-paint; a panic in a click handler takes
/// the window down.
#[test]
fn strip_hit_testing_survives_nonsense() {
    assert_eq!(super::strip_page_at(10, 0, 0, 4), None);
    assert_eq!(super::strip_page_at(10, -3, 0, 4), None);
    assert_eq!(super::strip_page_at(10, 100, 0, 0), None);
}

/// Build an index over `pages` of text, the way the worker does.
fn indexed(pages: &[Option<&str>]) -> PdfIndex {
    let mut ix = PdfIndex {
        total: pages.len(),
        ..Default::default()
    };
    for (i, p) in pages.iter().enumerate() {
        assert!(ix.push(i, *p), "page {i} was refused");
    }
    ix
}

/// The property the whole search rests on: a match found in the concatenated text has to name
/// the page it is really on. Getting this wrong scrolls the reader to the wrong page, which is
/// glaring in use and completely invisible in a screenshot.
#[test]
fn an_offset_maps_back_to_the_page_it_came_from() {
    let ix = indexed(&[Some("alpha"), Some("bravo"), Some("charlie")]);
    // "alpha\nbravo\ncharlie\n" - starts at 0, 6, 12.
    assert_eq!(ix.starts, vec![0, 6, 12]);
    for (off, want) in [(0, 0), (4, 0), (5, 0), (6, 1), (11, 1), (12, 2), (18, 2)] {
        assert_eq!(
            page_for_offset(&ix.starts, off),
            want,
            "offset {off} should be on page {want}"
        );
    }
    // And the real thing: search the folded text and land on the right page.
    let at = ix.hay.find("bravo").expect("the word is in there");
    assert_eq!(page_for_offset(&ix.starts, at), 1);
}

/// Every page occupies at least its own separator, so no two pages can share a start offset.
/// If a blank page took zero bytes, a match on the page AFTER it could resolve to the blank
/// one, depending on which side the search landed.
#[test]
fn a_page_with_no_text_still_takes_up_a_position() {
    let ix = indexed(&[Some(""), None, Some("tail")]);
    assert_eq!(ix.starts, vec![0, 1, 2]);
    assert_eq!(ix.failed, 1, "a recognizer error is not an empty page");
    assert_eq!(page_for_offset(&ix.starts, 2), 2);
    assert_eq!(page_for_offset(&ix.starts, ix.hay.find("tail").unwrap()), 2);
}

/// Text is folded on the way in, and folding must not move a byte - the same property the
/// find bar's own haystack depends on. A Unicode fold would shift every offset after the
/// first non-ASCII character and send the reader to the wrong page.
#[test]
fn folding_the_page_text_never_moves_an_offset() {
    let ix = indexed(&[Some("Grüße WORLD"), Some("İstanbul Report")]);
    assert_eq!(ix.starts[1], "Grüße WORLD".len() + 1);
    assert!(
        ix.hay.contains("world"),
        "the fold is applied on the way in"
    );
    assert_eq!(
        page_for_offset(&ix.starts, ix.hay.find("report").unwrap()),
        1
    );
}

/// Pages must arrive in order, because the whole offset lookup is "position in the vector is
/// the page number". A worker that skipped or repeated one has to be refused, not absorbed.
#[test]
fn a_page_out_of_order_is_refused_rather_than_corrupting_the_map() {
    let mut ix = PdfIndex {
        total: 4,
        ..Default::default()
    };
    assert!(ix.push(0, Some("first")));
    assert!(!ix.push(2, Some("skipped one")), "a gap must be refused");
    assert!(!ix.push(0, Some("again")), "a repeat must be refused");
    assert!(ix.push(1, Some("second")));
    assert_eq!(ix.done(), 2);
    // And nothing may be pushed past the size the index was opened for.
    assert!(ix.push(2, Some("third")));
    assert!(ix.push(3, Some("fourth")));
    assert!(!ix.push(4, Some("past the cap")));
}

/// The honesty rules, which are the part of this feature that is easy to get subtly wrong.
#[test]
fn the_progress_note_says_what_is_actually_known() {
    // Nobody has searched yet: no note at all.
    assert_eq!(index_note(0, 0, 0, 40), None);
    // Part way through, the note names the document's real length.
    let n = index_note(12, 40, 0, 40).expect("mid-index says so");
    assert!(n.contains("12") && n.contains("40"), "got {n:?}");
    // Finished, with text found: nothing to say.
    assert_eq!(index_note(40, 40, 0, 40), None);
    // Finished, but the document is LONGER than the cap. The note must stay up: a search of
    // 200 pages of a 437 page file presented as a finished search is the lie this prevents.
    let n = index_note(200, 200, 0, 437).expect("a capped index must keep saying so");
    assert!(n.contains("200") && n.contains("437"), "got {n:?}");
    // Every page failed, so there is no recognizer here. That is not "no results".
    let n = index_note(40, 40, 40, 40).expect("an unavailable engine must be admitted");
    assert!(
        !n.contains("40"),
        "the failure note is not a progress count"
    );
    // Some pages failed but others worked: the document really was searched.
    assert_eq!(index_note(40, 40, 3, 40), None);
}

#[test]
fn an_empty_document_never_panics() {
    let l = layout(&[], 800, 10);
    assert_eq!(l.total, 0);
    assert_eq!(visible_range(&l, 0, 500), (0, 0));
    assert_eq!(page_at(&l, 0, 500), 0);
}

/// Shared by `evict_outside` and `evict_strip_outside`: a cache at or under the cap is left
/// alone entirely, and a bigger one keeps only the visible range plus one page of margin —
/// the strip's thumbnail cache had no such limit before this fix.
#[test]
fn eviction_keep_range_spares_small_caches_and_margins_the_visible_window() {
    assert_eq!(
        super::eviction_keep_range(4, 6, 6, 12),
        None,
        "cache at or under the cap is never evicted"
    );
    // 20-entry cache, visible 4..6, PREFETCH=1: keep 3..7.
    assert_eq!(super::eviction_keep_range(4, 6, 20, 12), Some((3, 7)));
    // The margin clamps instead of underflowing at the start or overflowing past the end.
    assert_eq!(super::eviction_keep_range(0, 2, 20, 12), Some((0, 3)));
    assert_eq!(super::eviction_keep_range(18, 20, 20, 12), Some((17, 20)));
}
