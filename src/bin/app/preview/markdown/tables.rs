//! GFM table layout and drawing.
//!
//! Column widths come from a three-step measure: step one takes each column's natural
//! (unwrapped) width, step two picks the cell padding the table can actually afford, step
//! three shares the pane out [max-min fair][fair_widths]. Every cell then wraps at the width
//! it was given and paints inside a clip of its own column, so a wide table shrinks
//! gracefully instead of overflowing.

use super::*;

/// Draw one GFM/HTML table GitHub-style: full 1px grid, bold header, zebra body rows,
/// per-column alignment, auto column widths (natural, proportionally shrunk to fit).
#[allow(clippy::too_many_arguments)] // owner-draw helper: many positional draw params by nature
pub(super) unsafe fn draw_table(
    hwnd: HWND,
    hdc: HDC,
    header: &[Vec<Run>],
    rows: &[Vec<Vec<Run>>],
    aligns: &[u8],
    x0: i32,
    y0: i32,
    avail: i32,
    c: &MdColors,
    links: &mut Vec<LinkHit>,
    sel: &mut TblSel,
    vis: (i32, i32),
) -> i32 {
    let sc = |v: i32| crate::win::dpi_scale(hwnd, v);
    let all_cols = header
        .len()
        .max(rows.iter().map(|r| r.len()).max().unwrap_or(0))
        .max(1);
    // A column narrower than a few characters is not a column, it is a vertical stack of single
    // letters. The old code squeezed every column in however many there were, and a 201-column
    // CSV export came out at about six pixels each: the header literally read "c/o/l/0" going
    // DOWNWARDS, and every cell in the file wrapped to one character per line. That is
    // unreadable, and it is also why it was slow, because the row-height pass then has to lay
    // out every character of every cell. Both symptoms are the same defect.
    //
    // So there is a readable floor, and columns past what fits at that floor are reported rather
    // than drawn. Same philosophy as the MAX_ROWS / MAX_COLS caps this file already has: a
    // preview shows you what a file is, and a promise it cannot keep is worth less than an
    // honest limit.
    let ncols = columns_that_fit(all_cols, avail, 2 * sc(6) + sc(46));
    let dropped_cols = all_cols - ncols;
    let vpad = sc(6);
    let fonts = Fonts::new(hwnd, BODY_PX, false, false);
    let hfonts = Fonts::new(hwnd, BODY_PX, true, false);
    let ctx = ctx_for(hwnd, c, c.fg);
    let col_align = |ci: usize| aligns.get(ci).copied().unwrap_or(0);

    // Every row to draw, in order, with its "is the header row" flag (HTML tables may have none).
    let all: Vec<(&[Vec<Run>], bool)> = {
        let mut v: Vec<(&[Vec<Run>], bool)> = Vec::with_capacity(rows.len() + 1);
        if !header.is_empty() {
            v.push((header, true));
        }
        v.extend(rows.iter().map(|r| (r.as_slice(), false)));
        v
    };

    // Step 1: each column's natural (unwrapped) TEXT width. Padding is deliberately NOT folded
    // in yet — how much padding the table can afford depends on this total.
    //
    // MEASURED FROM THE WIDEST FEW CELLS PER COLUMN, not from every cell. `run_block` is a full
    // inline layout pass (font select, tokenise, measure each word, allocate), around 37 us a
    // call, and this loop used to run it on EVERY cell on EVERY paint. A CSV export with 6,666
    // rows and 80 columns is 427,000 cells, so this step alone was ~16 seconds, and steps 4 and
    // 5 below each did it again: a real 5 MB CSV took 49 SECONDS to appear. Arrowing through a
    // folder of them was the symptom.
    //
    // Character count is a free proxy for width, so the candidates are picked by that and only
    // the top few per column are actually measured. It is a proxy, not a guarantee (`WWWW` is
    // wider than `iiiiii`), which is exactly why it takes the top SEVERAL rather than the single
    // longest: for a column to come out narrow, every one of the widest seven by character count
    // would have to be unusually narrow characters AND some shorter cell unusually wide ones.
    // The cost of being wrong is a column that wraps a cell it need not have, never clipped or
    // overlapping text (step 5 clips every cell to its own column regardless).
    const WIDTH_SAMPLES: usize = 7;
    let mut nat_text = vec![0i32; ncols];
    let mut scratch: Vec<LinkHit> = Vec::new();
    let cell_chars = |cell: &[Run]| -> usize { cell.iter().map(|r| r.text.chars().count()).sum() };
    for (ci, nat) in nat_text.iter_mut().enumerate() {
        // The header always counts: it is a different (bold) font and it is the row a narrow
        // column is most visibly wrong for.
        let mut cands: Vec<(usize, usize)> = Vec::new(); // (chars, index into `all`)
        for (ri, (row, _)) in all.iter().enumerate() {
            if let Some(cell) = row.get(ci) {
                cands.push((cell_chars(cell), ri));
            }
        }
        cands.sort_unstable_by_key(|&(chars, _)| std::cmp::Reverse(chars));
        let hdr_first = if all.first().map(|(_, h)| *h).unwrap_or(false) {
            Some(0usize)
        } else {
            None
        };
        for ri in hdr_first
            .into_iter()
            .chain(cands.iter().take(WIDTH_SAMPLES).map(|&(_, ri)| ri))
        {
            let (row, is_hdr) = all[ri];
            let Some(cell) = row.get(ci) else { continue };
            let f = if is_hdr { &hfonts } else { &fonts };
            let (_, w) = run_block(
                hdc,
                cell,
                f,
                0,
                0,
                i32::MAX / 4,
                0,
                true,
                &ctx,
                &mut scratch,
                None,
            );
            *nat = (*nat).max(w);
        }
    }

    // Step 2: GitHub's roomy 13px side padding is most of a column once a CSV export brings
    // nine of them (26px of every ~107px slice). When the table cannot fit at natural width the
    // padding tightens instead of spending the text's share on whitespace.
    let sum_text: i64 = nat_text.iter().map(|w| *w as i64).sum();
    let hpad = if sum_text + ncols as i64 * 2 * sc(13) as i64 <= avail as i64 {
        sc(13)
    } else {
        sc(6)
    };
    // A floor keeps a squeezed column readable — but only as far as the pane can pay for it, so
    // a table with more columns than fit degrades to even slivers rather than overrunning.
    let min_col = (2 * hpad + sc(16)).min(avail / ncols as i32).max(1);
    let nat: Vec<i32> = nat_text
        .iter()
        .map(|w| (w + 2 * hpad).max(min_col))
        .collect();
    let sum_nat: i64 = nat.iter().map(|w| *w as i64).sum();

    // Step 3: natural widths when the whole table fits (a short table stays narrow, GitHub-style),
    // otherwise a max-min fair share of the pane.
    let colw: Vec<i32> = if sum_nat <= avail as i64 {
        nat
    } else {
        fair_widths(&nat, avail)
    };
    let table_w: i32 = colw.iter().sum();
    let cell_x = |ci: usize| x0 + colw[..ci].iter().sum::<i32>();
    // The text a cell may use. `colw` is floored at `min_col` above, so the guard only bites in
    // the more-columns-than-fit case — where the per-cell clip is what keeps things tidy.
    let cell_w = |ci: usize| (colw[ci] - 2 * hpad).max(sc(8));

    // Step 4: row heights (wrap each cell at its column width).
    //
    // Every row's height is needed, even for rows far off screen, because the rows below cannot
    // be placed without it. What is NOT needed is a full layout pass per cell: the overwhelming
    // majority of CSV cells are short and obviously fit on one line, and for those the answer is
    // just the line height.
    //
    // `max_char_w` is the widest glyph in the font, so `chars * max_char_w` is an upper bound on
    // any string's width in it. When that bound already fits the column, the cell CANNOT wrap and
    // needs no measuring at all — a conservative test, so it is never wrong in the direction that
    // matters. Only cells that might wrap pay for `run_block`. `line_lead` and the `sc(3)` in
    // `line_h_probe` are the same value, so a single-line cell's measured height is exactly
    // `line_h_probe` and the fast path is not an approximation.
    let (line_h_probe, max_char_w) = {
        let old = SelectObject(hdc, fonts.reg.into());
        let mut tm = TEXTMETRICW::default();
        let _ = GetTextMetricsW(hdc, &mut tm);
        SelectObject(hdc, hfonts.reg.into());
        let mut htm = TEXTMETRICW::default();
        let _ = GetTextMetricsW(hdc, &mut htm);
        SelectObject(hdc, old);
        (
            (tm.tmHeight + tm.tmExternalLeading + sc(3)).max(1),
            // Bold header glyphs are wider, and a cell may hold bold or code runs whichever row
            // it is in, so the bound uses the widest of the two fonts.
            tm.tmMaxCharWidth.max(htm.tmMaxCharWidth).max(1),
        )
    };
    let mut row_h: Vec<i32> = Vec::new();
    for (row, is_hdr) in &all {
        let f = if *is_hdr { &hfonts } else { &fonts };
        let mut h = line_h_probe;
        for (ci, cell) in row.iter().enumerate().take(ncols) {
            // Upper bound on this cell's unwrapped width, with no GDI call and no allocation.
            // `code_pad` is added per run because an inline-code run draws a padded pill.
            let bound = cell
                .iter()
                .map(|r| {
                    r.text.chars().count() as i64 * i64::from(max_char_w)
                        + if r.code {
                            2 * i64::from(ctx.code_pad)
                        } else {
                            0
                        }
                })
                .sum::<i64>();
            if bound <= i64::from(cell_w(ci)) {
                continue; // cannot wrap, so it is exactly one line and `h` already covers it
            }
            let (ny, _) = run_block(
                hdc,
                cell,
                f,
                0,
                0,
                cell_w(ci),
                0,
                true,
                &ctx,
                &mut scratch,
                None,
            );
            h = h.max(ny);
        }
        row_h.push(h + 2 * vpad);
    }

    // Step 5: draw. Zebra fill first, then text, then the grid on top.
    let mut y = y0;
    let mut body_i = 0usize;
    for (ri, (row, is_hdr)) in all.iter().enumerate() {
        let f = if *is_hdr { &hfonts } else { &fonts };
        let h = row_h[ri];
        // Rows entirely above or below the pane are not drawn. GDI would clip them anyway, but
        // clipping happens AFTER the layout work, and the layout work is the whole cost: a
        // 6,666-row table has about forty rows on screen and was laying out all of them, every
        // paint. Links and selection rects are collected only for what is drawn, which is the
        // same rule the text-block skip in the caller already follows (only visible links can be
        // clicked, only visible selection can be seen).
        if y + h <= vis.0 || y >= vis.1 {
            y += h;
            hline(hdc, x0, x0 + table_w, y, c.border);
            if !is_hdr {
                body_i += 1; // the zebra stripe must not shift when a row is skipped
            }
            continue;
        }
        if !is_hdr {
            // GitHub zebra: every 2nd body row gets the subtle fill.
            if body_i % 2 == 1 {
                let zr = RECT {
                    left: x0,
                    top: y,
                    right: x0 + table_w,
                    bottom: y + h,
                };
                let zb = CreateSolidBrush(COLORREF(c.code_bg));
                FillRect(hdc, &zr, zb);
                let _ = DeleteObject(zb.into());
            }
            body_i += 1;
        }
        for (ci, cell) in row.iter().enumerate().take(ncols) {
            let mut rsel = RunSel {
                range: sel.range,
                doc: sel.doc,
                bases: sel
                    .bases
                    .get(ri)
                    .and_then(|r| r.get(ci))
                    .map(|v| v.as_slice())
                    .unwrap_or(&[]),
                hits: &mut *sel.hits,
                bg: sel.bg,
            };
            // Hard containment, independent of what the measure decided: a cell may not paint
            // outside its own column. Without it an over-long value bled into its neighbour's
            // text (the CSV header row read "usernamepassword") and on past the pane edge.
            let saved = SaveDC(hdc);
            let _ = IntersectClipRect(hdc, cell_x(ci), y, cell_x(ci) + colw[ci], y + h);
            let _ = run_block(
                hdc,
                cell,
                f,
                cell_x(ci) + hpad,
                y + vpad,
                cell_w(ci),
                col_align(ci),
                false,
                &ctx,
                links,
                Some(&mut rsel),
            );
            let _ = RestoreDC(hdc, saved);
        }
        y += h;
        hline(hdc, x0, x0 + table_w, y, c.border); // row separator
    }
    // top edge + verticals
    hline(hdc, x0, x0 + table_w, y0, c.border);
    for ci in 0..=ncols {
        let x = if ci == ncols {
            x0 + table_w
        } else {
            cell_x(ci)
        };
        let pen = CreatePen(PS_SOLID, 1, COLORREF(c.border));
        let op = SelectObject(hdc, HGDIOBJ(pen.0));
        let _ = MoveToEx(hdc, x, y0, None);
        let _ = LineTo(hdc, x, y);
        SelectObject(hdc, op);
        let _ = DeleteObject(HGDIOBJ(pen.0));
    }
    // Say what was left out. Silently dropping columns would be the same sin as silently
    // truncating rows, and the number is the only way a reader can tell this is a preview
    // limit rather than the file's real shape.
    if dropped_cols > 0 {
        let note = vec![Run {
            text: format!(
                "+{dropped_cols} more column{} not shown (too narrow to read at this window width)",
                if dropped_cols == 1 { "" } else { "s" }
            ),
            bold: false,
            italic: true,
            code: false,
            strike: false,
            link: None,
        }];
        let nctx = ctx_for(hwnd, c, c.muted);
        let (ny, _) = run_block(
            hdc,
            &note,
            &fonts,
            x0,
            y + sc(4),
            avail,
            0,
            false,
            &nctx,
            &mut scratch,
            None,
        );
        y = ny + sc(2);
    }
    fonts.free();
    hfonts.free();
    y
}

/// Share `avail` px among columns wanting `nat` px each, max-min fair.
///
/// Hand every column an equal slice; any column whose natural width fits inside that slice
/// takes only what it needs, and its surplus is re-split among the columns still asking for
/// more — repeating until a round satisfies nobody, at which point the rest split what's left
/// evenly. This is what stops one 90-character API-key column from crushing its neighbours:
/// narrow columns keep their FULL natural width (so a `username` header can never collide with
/// the next one), and only the genuinely wide columns share the remainder.
///
/// The result always sums to exactly `avail`, which is load-bearing rather than tidy: the grid
/// lines, the zebra fill and the cell origins are all derived from these widths, so when they
/// summed past the pane the verticals stopped at its edge while the text marched on past it.
fn fair_widths(nat: &[i32], avail: i32) -> Vec<i32> {
    let n = nat.len();
    if n == 0 {
        return Vec::new();
    }
    let mut out = vec![-1i32; n];
    let mut remaining = avail;
    let mut hungry = n;
    while hungry > 0 {
        let share = remaining / hungry as i32;
        let mut settled = 0usize;
        for ci in 0..n {
            if out[ci] < 0 && nat[ci] <= share {
                out[ci] = nat[ci];
                remaining -= nat[ci];
                settled += 1;
            }
        }
        if settled == 0 {
            // Nobody left fits an equal slice — split the remainder evenly and stop.
            for w in out.iter_mut().filter(|w| **w < 0) {
                *w = share;
            }
            break;
        }
        hungry -= settled;
    }
    // Integer division leaves crumbs either way; settle them on the widest column so the row
    // spans exactly `avail`.
    let mut sum: i32 = out.iter().sum();
    while sum > avail {
        let Some(i) = (0..n).max_by_key(|i| out[*i]) else {
            break;
        };
        if out[i] <= 1 {
            break;
        }
        let cut = (sum - avail).min(out[i] - 1);
        out[i] -= cut;
        sum -= cut;
    }
    if sum < avail {
        if let Some(i) = (0..n).max_by_key(|i| out[*i]) {
            out[i] += avail - sum;
        }
    }
    out
}

/// Selection wiring for one [`draw_table`] call (per-cell [`RunSel`]s are built from it).
pub(super) struct TblSel<'a> {
    pub(super) range: Option<(usize, usize)>,
    pub(super) doc: &'a str,
    pub(super) bases: &'a [Vec<Vec<usize>>],
    pub(super) hits: &'a mut Vec<SelHit>,
    pub(super) bg: u32,
}

/// How many of a table's `all_cols` columns can be shown at `avail` px without any of them
/// falling below `readable` px.
///
/// Always at least one: a single unreadably narrow column still beats an empty pane, and the
/// per-cell clip in `draw_table` keeps it tidy either way.
fn columns_that_fit(all_cols: usize, avail: i32, readable: i32) -> usize {
    let readable = readable.max(1);
    let fits = (avail / readable).max(1) as usize;
    all_cols.min(fits).max(1)
}

#[cfg(test)]
mod table_tests {
    use super::{columns_that_fit, fair_widths};

    /// The invariant the whole CSV overflow bug came down to: the widths the grid is drawn from
    /// must span exactly the space the table was given — never a pixel more.
    #[test]
    fn always_sums_to_exactly_avail() {
        for nat in [
            vec![100, 700, 90, 90, 75, 85, 200, 110, 140],
            vec![5000],
            vec![10, 10, 10],
            vec![333; 7],
            vec![1, 999999, 1, 1],
        ] {
            for avail in [61, 300, 964, 1000, 1237] {
                let out = fair_widths(&nat, avail);
                assert_eq!(
                    out.iter().sum::<i32>(),
                    avail,
                    "nat={nat:?} avail={avail} -> {out:?}"
                );
                assert_eq!(out.len(), nat.len());
            }
        }
    }

    /// The collision fix: a column that fits inside its equal slice keeps its FULL natural
    /// width, so a narrow header is never crushed by a giant neighbour. This is the exact
    /// shape of the reported CSV — one 90-character API key beside eight ordinary columns.
    #[test]
    fn narrow_columns_keep_their_natural_width() {
        // name, api_key, username, password, service, entry_type, base_url, source_ip, notes
        let nat = vec![100, 700, 90, 90, 75, 85, 200, 110, 140];
        let out = fair_widths(&nat, 964);
        for (i, (got, want)) in out.iter().zip(&nat).enumerate() {
            if *want <= 964 / 9 {
                assert_eq!(
                    got, want,
                    "column {i} should be untouched at its natural width"
                );
            }
        }
        // ...and the one greedy column is capped rather than allowed to eat the pane.
        assert!(out[1] < 700, "api_key should have been trimmed: {out:?}");
        assert!(
            out[1] > out[0],
            "api_key should still be the widest: {out:?}"
        );
    }

    /// More columns than the pane can pay for still produces a usable, exactly-fitting row
    /// rather than an overrun — the per-cell clip handles legibility from there.
    #[test]
    fn degrades_evenly_when_nothing_fits() {
        let out = fair_widths(&[400; 64], 900);
        assert_eq!(out.iter().sum::<i32>(), 900);
        assert!(out.iter().all(|w| *w >= 0), "no negative widths: {out:?}");
        assert!(
            out.iter().max().unwrap() - out.iter().min().unwrap() <= 900,
            "spread should stay bounded: {out:?}"
        );
    }

    /// A table that already fits is handed straight through by the caller, but calling the
    /// allocator with slack must still be stable (it only ever grows the widest column).
    #[test]
    fn slack_lands_on_the_widest_column() {
        let out = fair_widths(&[50, 80, 30], 400);
        assert_eq!(out.iter().sum::<i32>(), 400);
        assert_eq!(out[0], 50);
        assert_eq!(out[2], 30);
        assert_eq!(out[1], 320);
    }
    /// A table that fits is untouched. This is every ordinary Markdown table and most CSVs, and
    /// it is the case a column cap must not change.
    #[test]
    fn a_table_that_already_fits_keeps_every_column() {
        assert_eq!(columns_that_fit(3, 1200, 58), 3);
        assert_eq!(columns_that_fit(20, 1200, 58), 20);
        // Exactly filling the pane is fitting, not overflowing.
        assert_eq!(columns_that_fit(20, 20 * 58, 58), 20);
    }

    /// The case this exists for. A 201-column CSV used to be squeezed into about six pixels a
    /// column, which rendered every cell as a vertical stack of single characters.
    #[test]
    fn a_table_too_wide_to_read_is_cut_to_what_fits() {
        assert_eq!(columns_that_fit(201, 1200, 58), 20);
        assert_eq!(columns_that_fit(80, 1200, 58), 20);
        assert_eq!(columns_that_fit(52, 1200, 58), 20);
    }

    /// Never zero columns, whatever the window is doing. A pane can legitimately be narrower
    /// than one readable column while a resize is in flight, and returning 0 would divide by
    /// zero in the layout and paint nothing at all.
    #[test]
    fn there_is_always_at_least_one_column() {
        assert_eq!(columns_that_fit(50, 10, 58), 1);
        assert_eq!(columns_that_fit(50, 0, 58), 1);
        assert_eq!(columns_that_fit(50, -400, 58), 1);
        assert_eq!(columns_that_fit(1, 1200, 58), 1);
        assert_eq!(columns_that_fit(0, 1200, 58), 1);
    }

    /// A zero or negative readable floor would divide by zero. It comes from DPI scaling, which
    /// is not something this function gets to assume about.
    #[test]
    fn a_nonsense_readable_floor_cannot_divide_by_zero() {
        assert_eq!(columns_that_fit(10, 1200, 0), 10);
        assert_eq!(columns_that_fit(10, 1200, -5), 10);
    }
}
