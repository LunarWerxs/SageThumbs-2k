#![cfg(test)]

use super::*;

/// A GFM table wider than [`MAX_TABLE_COLS`] must cap the row instead of growing it
/// unbounded, and say so in a trailing note — before this fix a hand-authored (or crafted)
/// table had no limit at all, unlike every CSV-derived table.
#[test]
fn gfm_table_caps_columns_and_notes_it() {
    let n = MAX_TABLE_COLS + 5;
    let cells: String = (0..n).map(|i| format!(" c{i} |")).collect();
    let seps: String = (0..n).map(|_| " --- |").collect();
    let md = format!("|{cells}\n|{seps}\n");
    let blocks = parse_blocks(&md, false);
    let Block::Table { header, .. } = &blocks[0] else {
        panic!("expected the first block to be a table");
    };
    assert_eq!(header.len(), MAX_TABLE_COLS, "the row must be capped");
    assert!(
        matches!(blocks.get(1), Some(Block::Para(..))),
        "a truncation note must follow the table"
    );
}

/// A table within both caps gets no note at all.
#[test]
fn gfm_table_under_caps_notes_nothing() {
    let blocks = parse_blocks("| a | b |\n| --- | --- |\n| 1 | 2 |\n", false);
    assert_eq!(
        blocks.len(),
        1,
        "no trailing note block for an unwounded table"
    );
}

/// The raw-HTML table builder enforces the same column cap as the GFM one, and notes it —
/// a README `<table>` is just as capable of being hand-crafted absurdly wide.
#[test]
fn html_table_caps_columns_and_notes_it() {
    let n = MAX_TABLE_COLS + 3;
    let mut html = String::from("<table><tr>");
    for i in 0..n {
        html.push_str(&format!("<td>c{i}</td>"));
    }
    html.push_str("</tr></table>");
    let md = format!("{html}\n");
    let blocks = parse_blocks(&md, false);
    let Block::Table { rows, .. } = &blocks[0] else {
        panic!("expected the first block to be a table");
    };
    assert_eq!(rows[0].len(), MAX_TABLE_COLS, "the row must be capped");
    assert!(
        matches!(blocks.get(1), Some(Block::Para(..))),
        "a truncation note must follow the table"
    );
}
