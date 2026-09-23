//! The SELECTION document: one flat string holding the whole file as it is rendered,
//! plus each block's byte offsets into it.
//!
//! Every selection offset lives in this coordinate space, so Ctrl+A and copy cover the
//! whole document regardless of what is currently painted or culled. It depends only on
//! the parse, never on the pane width, which is why a scroll or a resize can never
//! invalidate an offset.

use super::*;

/// The `doc` byte offsets of one block's selectable pieces (shape follows the block's).
pub(super) enum DocBase {
    /// Heading / paragraph / list item / quote: one offset per run.
    Runs(Vec<usize>),
    /// Code block: the offset of its text.
    Code(usize),
    /// Table: `[row][cell][run]`, header row first when there is one (matches the draw order).
    Table(Vec<Vec<Vec<usize>>>),
    /// Nothing selectable (rules, images).
    None,
}

/// Build the whole selection document (what Ctrl+C copies) plus each block's run offsets in it.
///
/// Blocks are separated by a BLANK line. Each block ends in a single `\n`, so without one every
/// paragraph, heading and code block runs straight into the next on paste — and in Markdown terms
/// they merge into a single paragraph. Two exceptions: items of the SAME list stay tight (a blank
/// line between them makes a "loose" list that re-renders with paragraph gaps, and a bullet list
/// butted straight against an ordered one reads as a single mangled list), and a block that
/// contributes no text must not leave the separator behind as a stray empty line.
pub(super) fn build_doc(blocks: &[Block]) -> (String, Vec<DocBase>) {
    /// `Some(ordered)` for a list item, `None` for anything else.
    fn item_kind(b: &Block) -> Option<bool> {
        match b {
            Block::Item(_, marker, _, _) => Some(marker.ends_with('.')),
            _ => None,
        }
    }
    let mut doc = String::new();
    let mut bases = Vec::with_capacity(blocks.len());
    let mut prev_kind = None;
    for b in blocks {
        let kind = item_kind(b);
        let mark = doc.len();
        if !doc.is_empty() && !(kind.is_some() && kind == prev_kind) {
            doc.push('\n');
        }
        let after_sep = doc.len();
        bases.push(doc_append(&mut doc, b));
        if doc.len() == after_sep {
            doc.truncate(mark);
        }
        prev_kind = kind;
    }
    (doc, bases)
}

/// Append one run's text to `doc`, recording where it landed. Shared by every block kind
/// below whose selectable pieces are plain inline runs.
fn runs(doc: &mut String, runs: &[Run]) -> Vec<usize> {
    let mut v = Vec::with_capacity(runs.len());
    for r in runs {
        v.push(doc.len());
        doc.push_str(&r.text);
    }
    doc.push('\n');
    v
}

/// A list item: indent, the bullet/number marker, an optional GFM task checkbox, then its
/// runs. Two spaces per level: the nesting the pane DRAWS (`sc(22) * (depth + 1)`) has to
/// survive the copy or every sub-bullet pastes flat at the top level.
fn doc_append_item(
    doc: &mut String,
    depth: u8,
    marker: &str,
    rs: &[Run],
    task: Option<bool>,
) -> DocBase {
    for _ in 0..depth {
        doc.push_str("  ");
    }
    // `marker` is the DISPLAY glyph ("•"), which is not a Markdown bullet. An ordered
    // item's marker is already "N." and carries its number, so that one passes through.
    doc.push_str(if marker.ends_with('.') { marker } else { "-" });
    // GFM order is marker THEN checkbox ("- [x] done"). Emitting the box INSTEAD of the
    // bullet gave "[x] done", which is not a task list anywhere it lands.
    match task {
        Some(true) => doc.push_str(" [x]"),
        Some(false) => doc.push_str(" [ ]"),
        None => {}
    }
    doc.push(' ');
    DocBase::Runs(runs(doc, rs))
}

/// A fenced code block: the opening fence (with the language tag, if any), the text
/// (newline-terminated), and the closing fence.
fn doc_append_code(doc: &mut String, text: &str, lang: highlight::Lang) -> DocBase {
    doc.push_str("```");
    if let Some(t) = highlight::lang_tag(lang) {
        doc.push_str(t);
    }
    doc.push('\n');
    let b = doc.len();
    doc.push_str(text);
    if !text.ends_with('\n') {
        doc.push('\n');
    }
    doc.push_str("```\n");
    DocBase::Code(b)
}

/// A table: header row (if any) then body rows, tab-separated cells so the paste lands in
/// a spreadsheet as columns, one row per line.
fn doc_append_table(doc: &mut String, header: &[Vec<Run>], rows: &[Vec<Vec<Run>>]) -> DocBase {
    let mut all: Vec<&[Vec<Run>]> = Vec::with_capacity(rows.len() + 1);
    if !header.is_empty() {
        all.push(header);
    }
    all.extend(rows.iter().map(|r| r.as_slice()));
    let mut out = Vec::with_capacity(all.len());
    for row in all {
        let mut rb = Vec::with_capacity(row.len());
        for (ci, cell) in row.iter().enumerate() {
            if ci > 0 {
                doc.push('\t'); // tab-separated: pastes into a spreadsheet as columns
            }
            let mut cb = Vec::with_capacity(cell.len());
            for r in cell {
                cb.push(doc.len());
                doc.push_str(&r.text);
            }
            rb.push(cb);
        }
        doc.push('\n');
        out.push(rb);
    }
    DocBase::Table(out)
}

/// Append `block`'s text to the selection document (in reading order) and report where its runs
/// landed. Runs are the only hit-testable pieces: the STRUCTURAL prefixes below (a heading's
/// `#`s, a list bullet and its indent, a quote's `>`, code fences) are copied but never
/// individually selectable — browsers don't highlight bullets either.
///
/// This text is exactly what Ctrl+C puts on the clipboard, so it emits **Markdown**, not bare
/// rendered lines. Flattening to bare lines pastes as an unreadable wall: nesting gone, headings
/// indistinguishable from body text, code runs into prose. Markdown keeps the structure when
/// pasted anywhere Markdown-aware AND still reads correctly as plain text.
pub(super) fn doc_append(doc: &mut String, block: &Block) -> DocBase {
    match block {
        Block::Para(rs, _) => DocBase::Runs(runs(doc, rs)),
        Block::Heading(level, rs, _) => {
            for _ in 0..(*level).clamp(1, 6) {
                doc.push('#');
            }
            doc.push(' ');
            DocBase::Runs(runs(doc, rs))
        }
        Block::Quote(rs) => {
            doc.push_str("> ");
            DocBase::Runs(runs(doc, rs))
        }
        Block::Item(depth, marker, rs, task) => doc_append_item(doc, *depth, marker, rs, *task),
        Block::Code(text, lang) => doc_append_code(doc, text, *lang),
        Block::Table { header, rows, .. } => doc_append_table(doc, header, rows),
        Block::Rule => {
            doc.push_str("---\n");
            DocBase::None
        }
        Block::Image(ib) => {
            // Not selectable (there is no text token to hit-test), but it must not paste as a
            // silent hole in the middle of a document.
            doc.push_str("![");
            doc.push_str(&ib.alt);
            doc.push_str("](");
            doc.push_str(&ib.src);
            doc.push_str(")\n");
            DocBase::None
        }
    }
}
