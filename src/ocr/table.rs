//! Column detection for OCR results, so a captured TABLE pastes into a spreadsheet.
//!
//! `Windows.Media.Ocr` has no notion of a table, and — measured against the real engine,
//! not assumed — it doesn't even keep a table's ROWS together: any gutter wider than
//! roughly a word-height makes it emit each COLUMN as its own run of "lines" (a sweep of
//! synthetic tables at 30/45/60/80/110 px gutters under 22 px text split at every width).
//! So the raw output of OCR-ing a price list reads column-major: all the names, then all
//! the quantities, then all the prices. Useless to paste anywhere.
//!
//! This module therefore rebuilds the VISUAL rows first — every word, engine line
//! structure ignored, re-grouped by vertical position — then looks for a gutter that lines
//! up across those rows, and if the geometry says "table", joins cells with tabs (rows in
//! reading order), which is all a spreadsheet needs.
//!
//! Deliberately conservative, because the failure modes are asymmetric: a missed table
//! pastes exactly as the engine always pasted, while a false positive rearranges prose. So
//! a column must be witnessed by [`MIN_SUPPORT`] rows, and — the guard that protects
//! two-column DOCUMENTS (a magazine page screenshot, where merging the columns would
//! interleave sentences) — the median cell must be SHORT ([`MAX_CELL_WORDS`]): table cells
//! are labels and numbers, prose columns are sentences.
//!
//! Pure geometry over `(text, x, y, w, h)` boxes — no WinRT types — so the whole decision
//! is unit-testable without an OCR engine or a window station.

/// One recognized word and its box.
#[derive(Clone, Debug)]
pub(crate) struct WordBox {
    pub text: String,
    /// Left edge, device px.
    pub x: f32,
    /// Top edge, device px.
    pub y: f32,
    /// Width, device px.
    pub w: f32,
    /// Height, device px — the proxy for the text size, which every threshold scales by.
    pub h: f32,
}

/// A gap must be at least this many median-word-heights wide to count as a candidate column
/// separator. Ordinary inter-word spaces measure ~0.2–0.4 of the height; genuine table
/// gutters start around a full height. The dead band between the two is what makes the
/// threshold stable.
const GAP_MIN_HEIGHTS: f32 = 0.9;

/// ...and at least this multiple of the recognition's own typical SPACE (both must hold),
/// so a loosely-spaced font can't turn every space into a separator.
const GAP_MIN_MEDIAN: f32 = 2.5;

/// Two candidate separators on different rows belong to the same column if their midpoints
/// sit within this many median-heights of each other.
const CLUSTER_TOL_HEIGHTS: f32 = 0.8;

/// Assembly matches a gap to a confirmed column at a LOOSER tolerance than detection built
/// the column with. Detection must stay strict or noise founds columns; but once a column
/// IS confirmed, a header row's gap often sits further off-centre than the data rows'
/// (short labels over long cells shift the gutter's midpoint), and missing it glues two
/// column headers into one cell — measured live: "Item Qty<TAB>price" over perfect data
/// rows. The gap must still be gutter-wide, so this cannot tab an ordinary space.
const ASSEMBLY_TOL_FACTOR: f32 = 2.0;

/// A column must be witnessed by this many rows to be believed. Two rows agreeing is a
/// coincidence (a heading and one line); three is a table.
const MIN_SUPPORT: usize = 3;

/// Words whose vertical CENTERS sit within this many median-heights belong to one visual
/// row. 0.6 tolerates OCR jitter and mixed ascenders while keeping adjacent 1.2-spaced
/// text lines apart.
const ROW_TOL_HEIGHTS: f32 = 0.6;

/// The prose-column guard: if the MEDIAN cell in the would-be table holds more words than
/// this, it isn't a table — it's columns of sentences, and merging those interleaves a
/// document. Real table cells ("Apple pie", "3.50", "2026-08-24") sit well under it.
const MAX_CELL_WORDS: usize = 4;

/// Assemble the recognition as tab-separated table text, or `None` when the geometry does
/// not support calling it a table — the caller then keeps the engine's own text,
/// byte-for-byte what this feature's predecessor produced.
///
/// Input is the engine's own line structure (flattened internally — measured against the
/// real engine, its lines CANNOT be trusted for tables; see the module doc).
pub(crate) fn assemble(lines: &[Vec<WordBox>]) -> Option<String> {
    let words: Vec<&WordBox> = lines.iter().flatten().collect();
    if words.len() < MIN_SUPPORT * 2 {
        return None; // a table needs at least a couple of cells on several rows
    }
    let heights: Vec<f32> = words.iter().map(|w| w.h).collect();
    let h_med = median(&heights)?;
    if h_med <= 0.0 {
        return None;
    }

    let rows = rebuild_visual_rows(&words, h_med)?;
    let g_med = typical_space_width(&rows, h_med);
    let columns = find_columns(&rows, h_med, g_med)?;
    assemble_rows(&rows, &columns, h_med, g_med)
}

/// The horizontal gap between two words that sit next to each other on a row (negative when
/// they overlap). The base measurement every gutter/space decision below is built from.
fn gap_of(a: &WordBox, b: &WordBox) -> f32 {
    b.x - (a.x + a.w)
}

/// Step 1: rebuild the VISUAL rows — sort by vertical center, sweep-cluster, then sort each
/// row by x. This is the step that undoes the engine's column-major line split. `None` if
/// fewer than [`MIN_SUPPORT`] rows result (mirrors `rows.last_mut()`'s `?`, which cannot
/// actually miss since a row is always pushed first, but keeps the original safety net).
fn rebuild_visual_rows<'a>(words: &[&'a WordBox], h_med: f32) -> Option<Vec<Vec<&'a WordBox>>> {
    let mut order: Vec<&WordBox> = words.to_vec();
    order.sort_by(|a, b| {
        (a.y + a.h / 2.0)
            .partial_cmp(&(b.y + b.h / 2.0))
            .unwrap_or(core::cmp::Ordering::Equal)
    });
    let row_tol = ROW_TOL_HEIGHTS * h_med;
    let mut rows: Vec<Vec<&WordBox>> = Vec::new();
    let mut cur_center = f32::NEG_INFINITY;
    for w in order {
        let c = w.y + w.h / 2.0;
        if rows.is_empty() || (c - cur_center).abs() > row_tol {
            rows.push(Vec::new());
            cur_center = c;
        }
        // Track the row's running mean center so a gently sloping scan doesn't split rows.
        let row = rows.last_mut()?;
        row.push(w);
        let n = row.len() as f32;
        cur_center += (c - cur_center) / n;
    }
    for row in &mut rows {
        row.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(core::cmp::Ordering::Equal));
    }
    if rows.len() < MIN_SUPPORT {
        return None;
    }
    Some(rows)
}

/// Step 2: the recognition's typical SPACE width — from sub-gutter gaps only. The median of
/// ALL gaps is wrong for exactly the target input: in a small table the gutters OUTNUMBER
/// the spaces, so the overall median IS a gutter (caught by the unit tests on this module's
/// first version). With no plausible spaces at all, height alone governs (returns 0.0).
fn typical_space_width(rows: &[Vec<&WordBox>], h_med: f32) -> f32 {
    let mut space_gaps: Vec<f32> = Vec::new();
    for row in rows {
        for pair in row.windows(2) {
            let gap = gap_of(pair[0], pair[1]);
            if gap < GAP_MIN_HEIGHTS * h_med {
                space_gaps.push(gap);
            }
        }
    }
    median(&space_gaps).unwrap_or(0.0).max(0.0)
}

/// Step 3: candidate separators are the midpoint of every wide gap (midpoints, not edges, so
/// left-aligned text columns and right-aligned number columns both cluster — their word
/// edges wander, the gutter's centre does not), clustered and kept only when witnessed by at
/// least [`MIN_SUPPORT`] rows. `None` if no column survives.
fn find_columns(rows: &[Vec<&WordBox>], h_med: f32, g_med: f32) -> Option<Vec<f32>> {
    let wide = |gap: f32| gap >= GAP_MIN_HEIGHTS * h_med && gap >= GAP_MIN_MEDIAN * g_med;
    let mut mids: Vec<f32> = Vec::new();
    for row in rows {
        for pair in row.windows(2) {
            let gap = gap_of(pair[0], pair[1]);
            if wide(gap) {
                mids.push(pair[0].x + pair[0].w + gap / 2.0);
            }
        }
    }
    if mids.is_empty() {
        return None;
    }
    mids.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
    let tol = CLUSTER_TOL_HEIGHTS * h_med;
    let mut columns: Vec<f32> = Vec::new();
    let mut run: Vec<f32> = vec![mids[0]];
    let mut run_last = mids[0];
    for &m in &mids[1..] {
        if m - run_last <= tol {
            run.push(m);
        } else {
            columns.push(run.iter().sum::<f32>() / run.len() as f32);
            run = vec![m];
        }
        run_last = m;
    }
    columns.push(run.iter().sum::<f32>() / run.len() as f32);
    columns.retain(|&c| {
        let support = rows
            .iter()
            .filter(|row| {
                row.windows(2).any(|pair| {
                    let gap = gap_of(pair[0], pair[1]);
                    wide(gap) && (pair[0].x + pair[0].w + gap / 2.0 - c).abs() <= tol
                })
            })
            .count();
        support >= MIN_SUPPORT
    });
    if columns.is_empty() {
        return None;
    }
    Some(columns)
}

/// Step 4: assemble rows, splitting cells at confirmed columns (a LOOSER tolerance than
/// detection built them at, see [`ASSEMBLY_TOL_FACTOR`]) — and apply the prose-column guard:
/// a "table" whose typical cell is a sentence is a two-column DOCUMENT, and tabbing it would
/// interleave its reading order, so refuse and let the engine's own per-column text stand.
fn assemble_rows(
    rows: &[Vec<&WordBox>],
    columns: &[f32],
    h_med: f32,
    g_med: f32,
) -> Option<String> {
    let wide = |gap: f32| gap >= GAP_MIN_HEIGHTS * h_med && gap >= GAP_MIN_MEDIAN * g_med;
    let tol = CLUSTER_TOL_HEIGHTS * h_med;
    let on_column = |prev: &WordBox, w: &WordBox| {
        let gap = gap_of(prev, w);
        wide(gap)
            && columns
                .iter()
                .any(|&c| (prev.x + prev.w + gap / 2.0 - c).abs() <= tol * ASSEMBLY_TOL_FACTOR)
    };
    let mut out = String::new();
    let mut cell_lengths: Vec<usize> = Vec::new();
    for (ri, row) in rows.iter().enumerate() {
        if ri > 0 {
            out.push('\n');
        }
        let mut cell_words = 0usize;
        for (wi, w) in row.iter().enumerate() {
            if wi > 0 {
                if on_column(row[wi - 1], w) {
                    out.push('\t');
                    cell_lengths.push(cell_words);
                    cell_words = 0;
                } else {
                    out.push(' ');
                }
            }
            out.push_str(&w.text);
            cell_words += 1;
        }
        cell_lengths.push(cell_words);
    }
    if median_usize(&cell_lengths)? > MAX_CELL_WORDS {
        return None;
    }
    Some(out)
}

fn median(v: &[f32]) -> Option<f32> {
    if v.is_empty() {
        return None;
    }
    let mut s: Vec<f32> = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
    Some(s[s.len() / 2])
}

fn median_usize(v: &[usize]) -> Option<usize> {
    if v.is_empty() {
        return None;
    }
    let mut s: Vec<usize> = v.to_vec();
    s.sort_unstable();
    Some(s[s.len() / 2])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A word `text` at `(x, y)`, sized like 20px-tall UI text (~10px per char).
    fn w(text: &str, x: f32, y: f32) -> WordBox {
        WordBox {
            text: text.into(),
            x,
            y,
            w: text.len() as f32 * 10.0,
            h: 20.0,
        }
    }

    /// THE case, exactly as the real engine returns it (measured 2026-08-24): a table's
    /// columns arrive as separate column-major "lines". The assembler must rebuild the
    /// visual rows from the y coordinates and tab the gutter.
    #[test]
    fn engine_split_columns_reassemble_into_rows() {
        let lines = vec![
            // Column 1, as the engine's own per-column line blocks:
            vec![w("Item", 0.0, 0.0)],
            vec![w("Apple", 0.0, 38.0), w("pie", 60.0, 38.0)],
            vec![w("Coffee", 0.0, 76.0)],
            // Column 2, its own blocks:
            vec![w("Price", 300.0, 0.0)],
            vec![w("3.50", 300.0, 38.0)],
            vec![w("2.00", 300.0, 76.0)],
        ];
        let out = assemble(&lines).expect("this IS a table");
        assert_eq!(out, "Item\tPrice\nApple pie\t3.50\nCoffee\t2.00");
    }

    /// The merged-row shape (an engine that DID keep rows whole) still works.
    #[test]
    fn already_merged_rows_get_tabs_at_the_gutter() {
        let lines = vec![
            vec![w("Name", 0.0, 0.0), w("Price", 300.0, 0.0)],
            vec![
                w("Apple", 0.0, 38.0),
                w("pie", 60.0, 38.0),
                w("3.50", 300.0, 38.0),
            ],
            vec![w("Coffee", 0.0, 76.0), w("2.00", 300.0, 76.0)],
        ];
        let out = assemble(&lines).expect("table");
        assert_eq!(out, "Name\tPrice\nApple pie\t3.50\nCoffee\t2.00");
    }

    /// Prose must come back None — ordinary spaces are ~5px against 20px words, nowhere
    /// near a gutter, so no candidate separator ever forms.
    #[test]
    fn prose_is_not_a_table() {
        let lines = vec![
            vec![
                w("The", 0.0, 0.0),
                w("quick", 40.0, 0.0),
                w("brown", 95.0, 0.0),
                w("fox", 155.0, 0.0),
            ],
            vec![
                w("jumps", 0.0, 24.0),
                w("over", 60.0, 24.0),
                w("the", 110.0, 24.0),
                w("lazy", 150.0, 24.0),
            ],
            vec![
                w("dog", 0.0, 48.0),
                w("and", 45.0, 48.0),
                w("naps", 90.0, 48.0),
            ],
        ];
        assert!(assemble(&lines).is_none());
    }

    /// TWO COLUMNS OF PROSE — a magazine-page screenshot — must be refused even though its
    /// gutter clusters perfectly: the median cell is a sentence, not a label, and tabbing
    /// it would interleave the reading order. This guard is what makes the row-rebuild
    /// safe to ship.
    #[test]
    fn two_prose_columns_are_refused() {
        let mut lines = Vec::new();
        for r in 0..4 {
            let y = r as f32 * 24.0;
            let mut left = Vec::new();
            let mut right = Vec::new();
            for i in 0..6 {
                left.push(w("word", i as f32 * 55.0, y));
                right.push(w("word", 500.0 + i as f32 * 55.0, y));
            }
            lines.push(left);
            lines.push(right);
        }
        assert!(
            assemble(&lines).is_none(),
            "columns of sentences must not be tabbed into fake table rows"
        );
    }

    /// One heading plus one matching row is a coincidence, not a table.
    #[test]
    fn two_rows_agreeing_is_not_enough() {
        let lines = vec![
            vec![w("Item", 0.0, 0.0), w("Cost", 300.0, 0.0)],
            vec![w("Thing", 0.0, 38.0), w("9.99", 300.0, 38.0)],
            vec![w("subtotal", 40.0, 76.0)],
        ];
        assert!(assemble(&lines).is_none());
    }

    /// Right-aligned number columns: left edges wander, the gutter's midpoint holds still.
    #[test]
    fn right_aligned_numbers_still_cluster() {
        let lines = vec![
            vec![w("Rent", 0.0, 0.0), w("1200.00", 300.0, 0.0)],
            vec![w("Food", 0.0, 38.0), w("87.50", 320.0, 38.0)],
            vec![w("Bus", 0.0, 76.0), w("2.75", 330.0, 76.0)],
        ];
        let out = assemble(&lines).expect("right-aligned table");
        assert_eq!(out.matches('\t').count(), 3, "one tab per row: {out:?}");
    }

    /// Only the gutter becomes a tab; ordinary spaces inside a cell stay spaces.
    #[test]
    fn only_the_gutter_becomes_a_tab() {
        let lines = vec![
            vec![
                w("First", 0.0, 0.0),
                w("Last", 60.0, 0.0),
                w("Age", 400.0, 0.0),
            ],
            vec![
                w("Ada", 0.0, 38.0),
                w("Lovelace", 45.0, 38.0),
                w("36", 400.0, 38.0),
            ],
            vec![
                w("Alan", 0.0, 76.0),
                w("Turing", 50.0, 76.0),
                w("41", 400.0, 76.0),
            ],
        ];
        let out = assemble(&lines).expect("table");
        assert_eq!(out, "First Last\tAge\nAda Lovelace\t36\nAlan Turing\t41");
    }

    /// Degenerate input must not panic or invent columns.
    #[test]
    fn degenerate_inputs_are_calmly_refused() {
        assert!(assemble(&[]).is_none());
        assert!(assemble(&[vec![w("one", 0.0, 0.0)]]).is_none());
        let empty_lines: Vec<Vec<WordBox>> = vec![vec![], vec![], vec![]];
        assert!(assemble(&empty_lines).is_none());
    }
}
