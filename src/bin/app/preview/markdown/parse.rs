//! Markdown and raw-HTML event parsing into renderer blocks.

use super::*;
mod events;
mod linkify;
pub(super) use events::parse_blocks;
pub(super) use linkify::linkify_into;
#[cfg(test)]
pub(super) use linkify::url_at;
use std::borrow::Cow;

// ---- markdown -> blocks ------------------------------------------------------------------

/// Row/column caps for a hand-authored table (GFM `| a | b |` or raw-HTML `<table>`), matching
/// the philosophy `docconv`'s CSV/TSV/PSV import already applies — a crafted `.md`/README table
/// had NO limit at all before this, unlike every CSV-derived table, which already carried one.
/// Independent of (and enforced well before) `tables.rs::columns_that_fit`'s DISPLAY-width
/// truncation: this bounds how much a hostile document can make the Builder allocate in the
/// first place, regardless of how wide the eventual viewer window is.
const MAX_TABLE_ROWS: usize = 10_000;
const MAX_TABLE_COLS: usize = 64;

/// The truncation-note runs for a table that hit [`MAX_TABLE_ROWS`]/[`MAX_TABLE_COLS`] while
/// being built, or `None` when neither cap was hit. Shared by the GFM (`handle_table_event`)
/// and raw-HTML (`Builder::html_table_close`) table builders — every other cap in this area
/// (docconv's CSV import, this file's own display-width column fit) already says so when it
/// truncates; this one silently dropped rows/cells with no note before this fix.
fn table_cap_note(rows_dropped: bool, cols_dropped: bool) -> Option<Vec<Run>> {
    let text = match (rows_dropped, cols_dropped) {
        (false, false) => return None,
        (true, true) => format!(
            "This table was too large to show in full and was capped at {MAX_TABLE_ROWS} rows \
             and {MAX_TABLE_COLS} columns."
        ),
        (true, false) => format!(
            "This table was too large to show in full and was capped at {MAX_TABLE_ROWS} rows."
        ),
        (false, true) => format!(
            "This table was too large to show in full and was capped at {MAX_TABLE_COLS} \
             columns."
        ),
    };
    Some(vec![Run {
        text,
        bold: false,
        italic: true,
        code: false,
        strike: false,
        link: None,
    }])
}

/// Shared block-builder state driven by BOTH the pulldown-cmark event loop and the raw-HTML
/// feeder in [`super::super::mdhtml`]. Raw HTML toggles the same inline-style counters and emits the
/// same [`Block`]s, so `<b>`/`<h1>`/`<img>`/`<table>` render identically to their markdown twins.
pub(in crate::preview) struct Builder {
    pub(in crate::preview) out: Vec<Block>,
    runs: Vec<Run>,
    heading: Option<u8>,
    in_quote: u32,
    in_item: bool,
    /// A GFM task-list marker (`- [ ]` / `- [x]`) seen for the item currently open: the
    /// checkbox replaces the item's bullet. Set by the `TaskListMarker` event, consumed
    /// when the item flushes.
    task: Option<bool>,
    lists: Vec<(bool, u64)>,
    strong: u32,
    emph: u32,
    strike: u32,
    code_html: u32, // raw-HTML <code>/<kbd> nesting
    link: Option<String>,
    // markdown table state
    in_cell: bool,
    cur_cell: Vec<Run>,
    cur_row: Vec<Vec<Run>>,
    tbl_header: Vec<Vec<Run>>,
    tbl_rows: Vec<Vec<Vec<Run>>>,
    tbl_aligns: Vec<u8>,
    /// Set when the current GFM table dropped a row/cell past [`MAX_TABLE_ROWS`]/
    /// [`MAX_TABLE_COLS`]; consumed (and reset) at `TagEnd::Table` to append a note.
    tbl_rows_dropped: bool,
    tbl_cols_dropped: bool,
    // markdown image capture (alt text arrives as Text events between Start/End)
    img: Option<(String, String)>, // (dest url, alt buffer)
    // raw-HTML state (owned here so it persists across separate HtmlBlock events — a
    // `<div align="center">` opener and its `</div>` arrive in DIFFERENT blocks)
    center: u32,
    html_stack: Vec<(String, bool)>, // (open container tag, contributed-center)
    html_buf: String,
    pub(in crate::preview) skip_tag: Option<&'static str>, // inside <style>/<script>: skip until close
    pub(in crate::preview) in_comment: bool,               // inside <!-- ... -->
    h_tbl: Option<HtmlTbl>,
    /// The remote-images toggle: when true, http(s) image srcs become [`Block::Image`]s (the
    /// draw side fetches them asynchronously); when false they stay alt-text pills.
    remote_ok: bool,
}

/// Raw-HTML table under construction.
struct HtmlTbl {
    header: Vec<Vec<Run>>,
    rows: Vec<Vec<Vec<Run>>>,
    cur_row: Vec<Vec<Run>>,
    cur_cell: Option<Vec<Run>>,
    row_all_th: bool,
    /// Same purpose as `Builder`'s own `tbl_rows_dropped`/`tbl_cols_dropped`, for a raw-HTML
    /// `<table>` instead of a GFM one.
    rows_dropped: bool,
    cols_dropped: bool,
}

impl HtmlTbl {
    /// Push `c` onto the row under construction unless [`MAX_TABLE_COLS`] was already reached,
    /// in which case the cell is dropped and noted rather than growing the row unbounded.
    fn push_cell(&mut self, c: Vec<Run>) {
        if self.cur_row.len() < MAX_TABLE_COLS {
            self.cur_row.push(c);
        } else {
            self.cols_dropped = true;
        }
    }
}

impl Builder {
    pub(in crate::preview) fn new(remote_ok: bool) -> Builder {
        Builder {
            remote_ok,
            out: Vec::new(),
            runs: Vec::new(),
            heading: None,
            in_quote: 0,
            in_item: false,
            task: None,
            lists: Vec::new(),
            strong: 0,
            emph: 0,
            strike: 0,
            code_html: 0,
            link: None,
            in_cell: false,
            cur_cell: Vec::new(),
            cur_row: Vec::new(),
            tbl_header: Vec::new(),
            tbl_rows: Vec::new(),
            tbl_aligns: Vec::new(),
            tbl_rows_dropped: false,
            tbl_cols_dropped: false,
            img: None,
            center: 0,
            html_stack: Vec::new(),
            html_buf: String::new(),
            skip_tag: None,
            in_comment: false,
            h_tbl: None,
        }
    }

    /// The run buffer currently collecting styled text — HTML table cell / GFM table cell /
    /// current block. `None` means the text falls between HTML table cells and is dropped.
    fn run_target(&mut self) -> Option<&mut Vec<Run>> {
        if let Some(t) = &mut self.h_tbl {
            t.cur_cell.as_mut()
        } else if self.in_cell {
            Some(&mut self.cur_cell)
        } else {
            Some(&mut self.runs)
        }
    }

    /// Append styled text to whatever is currently collecting (image alt / HTML table cell /
    /// markdown table cell / the current block's runs).
    pub(in crate::preview) fn text(&mut self, s: &str) {
        if let Some((_, alt)) = &mut self.img {
            alt.push_str(s);
            return;
        }
        let (bold, italic, code, strike, link) = (
            self.strong > 0,
            self.emph > 0,
            self.code_html > 0,
            self.strike > 0,
            self.link.clone(),
        );
        // Pick the destination run buffer (HTML table cell / GFM table cell / current block).
        let Some(target) = self.run_target() else {
            return; // whitespace between HTML table cells — drop
        };
        // Autolink bare URLs in plain (non-code, not-already-linked) text — GFM extended
        // autolinking, which pulldown-cmark 0.12 does NOT do on its own.
        if !code && link.is_none() {
            linkify_into(target, s, bold, italic, strike);
        } else {
            push_run(target, s, code, bold, italic, strike, link);
        }
    }

    /// Explicit-code text (markdown `` ` `` spans) — same routing, forced code style.
    fn code_text(&mut self, s: &str) {
        self.code_html += 1;
        self.text(s);
        self.code_html -= 1;
    }

    /// A hard line break within the current block.
    pub(in crate::preview) fn newline(&mut self) {
        self.text("\n");
    }

    /// Close out the currently-accumulated runs as a block (heading > item > quote > para).
    pub(in crate::preview) fn flush(&mut self) {
        let blank = self.runs.iter().all(|r| r.text.trim().is_empty());
        let taken = core::mem::take(&mut self.runs);
        if blank && self.heading.is_none() {
            return;
        }
        let center = self.center > 0;
        if let Some(lvl) = self.heading.take() {
            self.out.push(Block::Heading(lvl, taken, center));
        } else if self.in_item {
            let depth = (self.lists.len().saturating_sub(1)) as u8;
            let task = self.task.take();
            let marker = match self.lists.last() {
                Some((true, n)) => format!("{n}."),
                _ => "•".to_string(),
            };
            self.out.push(Block::Item(depth, marker, taken, task));
        } else if self.in_quote > 0 {
            self.out.push(Block::Quote(taken));
        } else {
            self.out.push(Block::Para(taken, center));
        }
    }

    // ---- semantic ops shared with the HTML feeder ----------------------------------------

    pub(in crate::preview) fn start_heading(&mut self, level: u8) {
        self.flush();
        self.heading = Some(level);
    }
    pub(in crate::preview) fn end_heading(&mut self) {
        self.flush();
    }
    pub(in crate::preview) fn open_para(&mut self) {
        self.flush();
    }
    pub(in crate::preview) fn close_para(&mut self) {
        self.flush();
    }
    pub(in crate::preview) fn rule(&mut self) {
        self.flush();
        self.out.push(Block::Rule);
    }
    pub(in crate::preview) fn bold(&mut self, on: bool) {
        adj(&mut self.strong, on);
    }
    pub(in crate::preview) fn italic(&mut self, on: bool) {
        adj(&mut self.emph, on);
    }
    pub(in crate::preview) fn strikethrough(&mut self, on: bool) {
        adj(&mut self.strike, on);
    }
    pub(in crate::preview) fn code(&mut self, on: bool) {
        adj(&mut self.code_html, on);
    }
    pub(in crate::preview) fn set_link(&mut self, url: Option<String>) {
        self.link = url;
    }
    pub(in crate::preview) fn open_container(&mut self, tag: &str, centers: bool) {
        self.flush();
        if centers {
            self.center += 1;
        }
        self.html_stack.push((tag.to_string(), centers));
    }
    pub(in crate::preview) fn close_container(&mut self, tag: &str) {
        self.flush();
        // pop the nearest matching open tag (HTML in READMEs is flat; be forgiving)
        if let Some(pos) = self.html_stack.iter().rposition(|(t, _)| t == tag) {
            let (_, centered) = self.html_stack.remove(pos);
            if centered {
                self.center = self.center.saturating_sub(1);
            }
        }
    }
    pub(in crate::preview) fn open_quote(&mut self) {
        self.flush();
        self.in_quote += 1;
    }
    pub(in crate::preview) fn close_quote(&mut self) {
        self.flush();
        self.in_quote = self.in_quote.saturating_sub(1);
    }
    pub(in crate::preview) fn open_list(&mut self, ordered: bool, start: u64) {
        self.flush();
        self.lists.push((ordered, start));
    }
    pub(in crate::preview) fn close_list(&mut self) {
        self.flush();
        self.lists.pop();
    }
    pub(in crate::preview) fn open_item(&mut self) {
        self.flush();
        self.task = None;
        self.in_item = true;
    }
    pub(in crate::preview) fn close_item(&mut self) {
        self.flush();
        self.in_item = false;
        if let Some((true, n)) = self.lists.last_mut() {
            *n += 1;
        }
    }

    /// An image: local (or remote with the opt-in toggle) src -> its own [`Block::Image`];
    /// otherwise -> alt-text pill run.
    pub(in crate::preview) fn image(&mut self, src: &str, alt: &str, width: ImgW) {
        let link = self.link.clone();
        // `//`/`data:` never render. Of the web schemes, only httpS can ever succeed (the fetch
        // layer is HTTPS-only), so plain `http://` pills up front instead of spawning a worker
        // that is guaranteed to fail (review finding, 2026-07-13).
        let fetchable = src
            .trim_start()
            .to_ascii_lowercase()
            .starts_with("https://");
        let remote = is_gated_image_src(src) && !(self.remote_ok && fetchable);
        let in_cell = self.in_cell || self.h_tbl.as_ref().is_some_and(|t| t.cur_cell.is_some());
        // Inside a list item or blockquote a block-level image would SPLIT the block (flush mid-
        // item duplicates the marker; a quote's bar breaks in two) and escape its indent — degrade
        // to the inline pill there, same as cells/headings (review finding, 2026-07-13).
        if remote || in_cell || self.heading.is_some() || self.in_item || self.in_quote > 0 {
            let label = if alt.trim().is_empty() {
                "image"
            } else {
                alt.trim()
            };
            // NBSP-join so the pill lays out as ONE unbroken token (its shaded panel stays whole).
            let label = label.replace(' ', "\u{00A0}");
            let text = format!("\u{00A0}{label}\u{00A0}");
            let (bold, italic) = (self.strong > 0, self.emph > 0);
            let Some(tgt) = self.run_target() else {
                return;
            };
            tgt.push(Run {
                text,
                bold,
                italic,
                code: true,
                strike: false,
                link,
            });
        } else {
            self.flush();
            self.out.push(Block::Image(ImgBlock {
                src: src.to_string(),
                alt: alt.to_string(),
                width,
                center: self.center > 0,
                link,
            }));
        }
    }

    // ---- raw-HTML table ops ---------------------------------------------------------------

    pub(in crate::preview) fn html_table_open(&mut self) {
        self.flush();
        self.h_tbl = Some(HtmlTbl {
            header: Vec::new(),
            rows: Vec::new(),
            cur_row: Vec::new(),
            cur_cell: None,
            row_all_th: true,
            rows_dropped: false,
            cols_dropped: false,
        });
    }
    pub(in crate::preview) fn html_tr_open(&mut self) {
        if let Some(t) = &mut self.h_tbl {
            t.cur_row.clear();
            t.cur_cell = None;
            t.row_all_th = true;
        }
    }
    pub(in crate::preview) fn html_cell_open(&mut self, th: bool) {
        if let Some(t) = &mut self.h_tbl {
            if let Some(c) = t.cur_cell.take() {
                t.push_cell(c); // unclosed previous cell
            }
            t.cur_cell = Some(Vec::new());
            t.row_all_th &= th;
        }
    }
    pub(in crate::preview) fn html_cell_close(&mut self) {
        if let Some(t) = &mut self.h_tbl {
            if let Some(c) = t.cur_cell.take() {
                t.push_cell(c);
            }
        }
    }
    pub(in crate::preview) fn html_tr_close(&mut self) {
        if let Some(t) = &mut self.h_tbl {
            if let Some(c) = t.cur_cell.take() {
                t.push_cell(c);
            }
            let row = core::mem::take(&mut t.cur_row);
            if row.is_empty() {
                return;
            }
            if t.row_all_th && t.header.is_empty() && t.rows.is_empty() {
                t.header = row;
            } else if t.rows.len() < MAX_TABLE_ROWS {
                t.rows.push(row);
            } else {
                t.rows_dropped = true;
            }
        }
    }
    pub(in crate::preview) fn html_table_close(&mut self) {
        self.html_tr_close(); // forgive an unclosed final row
        if let Some(t) = self.h_tbl.take() {
            let dropped_note = table_cap_note(t.rows_dropped, t.cols_dropped);
            if !t.header.is_empty() || !t.rows.is_empty() {
                self.out.push(Block::Table {
                    header: t.header,
                    rows: t.rows,
                    aligns: Vec::new(),
                });
                if let Some(note) = dropped_note {
                    self.out.push(Block::Para(note, false));
                }
            }
        }
    }
}

fn adj(v: &mut u32, on: bool) {
    if on {
        *v += 1;
    } else {
        *v = v.saturating_sub(1);
    }
}

/// Append `text` as a run with the given inline style, merging into the previous run when the
/// style matches (keeps the token stream tight).
fn push_run(
    runs: &mut Vec<Run>,
    text: &str,
    code: bool,
    bold: bool,
    italic: bool,
    strike: bool,
    link: Option<String>,
) {
    if text.is_empty() {
        return;
    }
    if !code {
        if let Some(last) = runs.last_mut() {
            if !last.code
                && last.bold == bold
                && last.italic == italic
                && last.strike == strike
                && last.link == link
            {
                last.text.push_str(text);
                return;
            }
        }
    }
    runs.push(Run {
        text: text.to_string(),
        bold,
        italic,
        code,
        strike,
        link,
    });
}

/// If `md` opens with YAML front matter (offset 0: a line of exactly `---`, then a run of
/// fields, then a closing `---`), re-fence it as a ```yaml code block so it renders as
/// preformatted text, the way QuickLook does, instead of the fields flowing in as a stray
/// paragraph between two thematic breaks (SSG READMEs and Obsidian/Jekyll notes all open this
/// way). Goes through the normal fenced-code-block path on purpose (a bespoke `Block` here
/// would give the fields no entry in the selection document). Leaves `md` completely untouched
/// when no closing fence exists — a document that just starts with a rule must keep rendering
/// as one — and is only ever checked at the very start of the document.
fn fence_front_matter(md: &str) -> Cow<'_, str> {
    let mut lines = md.split_inclusive('\n');
    let Some(open) = lines.next() else {
        return Cow::Borrowed(md);
    };
    if open.trim_end() != "---" {
        return Cow::Borrowed(md);
    }
    let mut body_end = open.len();
    let mut close_end = None;
    for line in lines {
        if line.trim_end() == "---" {
            close_end = Some(body_end + line.len());
            break;
        }
        body_end += line.len();
    }
    let Some(close_end) = close_end else {
        return Cow::Borrowed(md); // unterminated: change nothing, keep it a lone rule
    };
    let body = &md[open.len()..body_end];
    // A fence exactly 3 backticks wide could be closed early by a field value that itself
    // contains a code fence; widen it past the longest backtick run already inside `body`.
    // Shared with `docconv`'s notebook/CSV fence wrapping and `dbdoc`'s DDL fence, so this fix
    // has one home instead of a third copy of the same counting loop.
    let fence = super::super::docconv::fence_for(body);
    let mut out = String::with_capacity(md.len() + fence.len() * 2 + 8);
    out.push_str(&fence);
    out.push_str("yaml\n");
    out.push_str(body);
    if !body.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&fence);
    out.push('\n');
    out.push_str(&md[close_end..]);
    Cow::Owned(out)
}

#[cfg(test)]
mod front_matter_tests;
#[cfg(test)]
mod table_cap_tests;
#[cfg(test)]
mod trim_trailing_punct_tests;
