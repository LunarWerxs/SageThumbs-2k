#![cfg(test)]

use super::*;

/// Design-px height one string needs in the window's full-width column, pinned to 96
/// DPI. Pinned rather than measured through `block_h`, whose answer follows the
/// process-wide shot-DPI override that a sibling test in `scaling.rs` flips underneath
/// this one; see `win::design_wrapped_text_h`. The floor is applied here, so this asks
/// exactly the question `block_h` asks.
fn head_h(text: &str) -> i32 {
    need(text, unsafe { content_w(HWND::default()) }, INTRO_H_MIN)
}

/// [`head_h`] for an arbitrary column and floor.
fn need(text: &str, col_w: i32, min_h: i32) -> i32 {
    unsafe { st2k_appkit::win::design_wrapped_text_h(text, col_w) }.max(min_h)
}

/// Every shipped locale's `fr_intro`/`fr_intro_portable` stays within a sane band:
/// never below the design floor (a terse translation must not shrink the box) and never
/// past a generous ceiling (which would mean the measurement itself is broken, e.g.
/// wrapping to the wrong column). Iterates the baked locale table rather than eyeballing
/// a screenshot of two or three of them, the acceptance bar this finding sets.
#[test]
fn every_locale_intro_line_stays_within_a_sane_height_band() {
    // Generous: catches a broken measurement, not a long sentence.
    const SANE_MAX: i32 = INTRO_H_MIN * 4;
    for (code, pairs) in st2k_base::i18n::LOCALES {
        for key in ["fr_intro", "fr_intro_portable"] {
            let Some((_, text)) = pairs.iter().find(|(k, _)| *k == key) else {
                continue;
            };
            let h = head_h(text);
            assert!(
                (INTRO_H_MIN..=SANE_MAX).contains(&h),
                "{code}/{key}: measured height {h}px is outside the sane [{INTRO_H_MIN}, \
                 {SANE_MAX}] band for {text:?}"
            );
        }
    }
}

/// Has teeth: a version of `block_h` that ignores its `text` argument and always returns
/// the floor, i.e. the exact pre-fix behavior of a flat height regardless of the active
/// language, fails this immediately. A paragraph nearly three times the length of the
/// longest shipped intro line cannot possibly wrap into the two-line floor.
#[test]
fn a_measured_block_grows_for_a_paragraph_the_old_fixed_height_could_not_hold() {
    let long = "SageThumbs is already adding thumbnails to Explorer, and this sentence \
        keeps going well past the point where two ordinary lines could possibly hold it, \
        because the whole point of measuring is to stop assuming a length in advance.";
    let h = head_h(long);
    assert!(
        h > INTRO_H_MIN,
        "a paragraph this long must measure taller than the old fixed {INTRO_H_MIN}px \
         box; got {h}px, the row heights have stopped measuring and gone back to guessing"
    );
}

/// A short synthetic string must sit exactly at the floor: the measurement is not
/// supposed to pad a one-line sentence, only to grow the box for a genuinely longer one.
#[test]
fn a_measured_block_floors_a_short_string_at_the_design_minimum() {
    assert_eq!(head_h("Short."), INTRO_H_MIN);
}

/// `dlg_h()` must grow by exactly the same amount the intro measures for the ACTIVE
/// language, not a second, independently-tuned number: this is the arithmetic that
/// reserves the window space `build()`'s control then actually uses.
#[test]
fn dlg_h_grows_by_exactly_the_measured_intro_extra() {
    let extra = intro_extra_h();
    assert_eq!(
        dlg_h(),
        (if offers_thumbnails() {
            DLG_H + THUMBS_ROW_H
        } else {
            DLG_H
        }) + extra,
        "dlg_h() must reserve exactly intro_extra_h() beyond the base layout height"
    );
}

/// The heart of F36 in this window, over all 36 shipped locales rather than the two a
/// screenshot samples: walk the SAME row tables `build`/`build_page2` walk, add up what
/// each row's copy really measures to, and check the page against two bounds.
///
/// The ceiling is the assertion that can fail on real copy. Rows are measured now, so
/// "does the text fit its box" is true by construction; what a measured layout CAN still
/// get wrong is needing a window taller than a modest screen, which `fit_window` would
/// deliver silently. The second half is the teeth: it records, per locale, every row
/// whose copy exceeds the flat box that row used to be given, and fails when that list
/// is empty, since a list of none would mean this test no longer proves the measured
/// rows do anything.
#[test]
fn every_locale_first_run_page_fits_a_reasonable_window() {
    // 340 shipped for years; twice that still opens on a 768px-tall laptop screen. A
    // page past it means a translation, or the measurement, has gone wrong.
    const SANE_MAX_CLIENT_H: i32 = 680;
    let w = unsafe { content_w(HWND::default()) };
    let mut grew_past_the_old_box: Vec<String> = Vec::new();

    // Page 1 in its PORTABLE shape, the taller of the two and the one the finding cites,
    // then page 2 with its two opt-ins. Each entry is (heading key, rows, closing key).
    let pages: [(&str, &[&SwitchRow], &str); 2] = [
        (
            "fr_intro_portable",
            &[&PAGE1_THUMBS_ROW, &PAGE1_PREVIEW_ROW, &PAGE1_SHOT_ROW],
            "fr_prtscn",
        ),
        ("fr2_head", &[&PAGE2_ROWS[0], &PAGE2_ROWS[1]], "fr2_sub"),
    ];

    for (code, pairs) in st2k_base::i18n::LOCALES {
        let value = |key: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| *v)
                .unwrap_or("")
        };
        for (head_key, rows, tail_key) in pages {
            let mut y = 16 + need(value(head_key), w, SWITCH_H_MIN) + 12;
            for row in rows {
                let label_h = need(value(row.key), w - CHK_GLYPH_W, SWITCH_H_MIN);
                let sub_h = need(value(row.sub_key), w - INDENT, row.sub_min_h);
                if label_h > SWITCH_H_MIN {
                    grew_past_the_old_box.push(format!("{code}/{}", row.key));
                }
                if sub_h > row.sub_min_h {
                    grew_past_the_old_box.push(format!("{code}/{}", row.sub_key));
                }
                y += label_h + 2 + sub_h + row.gap;
            }
            // Page 1 closes with the indented PrtScn switch, page 2 with its footer
            // line; both are one measured block, so one term covers either.
            y += need(value(tail_key), w - INDENT - CHK_GLYPH_W, SWITCH_H_MIN);
            let client_h = y + BOTTOM_BLOCK;
            assert!(
                client_h <= SANE_MAX_CLIENT_H,
                "{code}/{head_key}: the measured rows come to {client_h}px of client \
                 height, past the {SANE_MAX_CLIENT_H}px this window should ever need"
            );
        }
    }

    assert!(
        !grew_past_the_old_box.is_empty(),
        "expected some shipped locale to need more than the pre-fix flat boxes; if none \
         do, this test can no longer prove the measured rows do anything"
    );
}

/// Every row's caption renders muted. A row names its caption in one place (its
/// [`SwitchRow`]) and is given its colour in another ([`is_dim_caption`]), with nothing
/// linking the two, so a row added to a page still builds, still lays out and still
/// renders. It just draws its caption in the full-strength foreground beside its muted
/// neighbours, which reads as emphasis nobody chose. `fr2_badge_sub` shipped that way,
/// and no size-based capture could see it; this is the guard.
#[test]
fn every_switch_row_caption_is_a_dim_caption() {
    let rows: [&SwitchRow; 5] = [
        &PAGE1_THUMBS_ROW,
        &PAGE1_PREVIEW_ROW,
        &PAGE1_SHOT_ROW,
        &PAGE2_ROWS[0],
        &PAGE2_ROWS[1],
    ];
    for row in rows {
        assert!(
            is_dim_caption(row.sub_id),
            "the caption under `{}` (id {}) is missing from `is_dim_caption`, so it \
             renders in the normal foreground while the captions around it stay muted",
            row.key,
            row.sub_id
        );
    }
}
