//! Markdown and raw-HTML event parsing into renderer blocks.

use super::*;
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
    in_para: bool,
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
            in_para: false,
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
        let target: &mut Vec<Run> = if let Some(t) = &mut self.h_tbl {
            match &mut t.cur_cell {
                Some(cell) => cell,
                None => return, // whitespace between HTML table cells — drop
            }
        } else if self.in_cell {
            &mut self.cur_cell
        } else {
            &mut self.runs
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
        self.in_para = true;
    }
    pub(in crate::preview) fn close_para(&mut self) {
        self.flush();
        self.in_para = false;
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
            let tgt = if let Some(t) = &mut self.h_tbl {
                match &mut t.cur_cell {
                    Some(cell) => cell,
                    None => return,
                }
            } else if self.in_cell {
                &mut self.cur_cell
            } else {
                &mut self.runs
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

/// Split `s` into plain-text runs and clickable link runs for any bare URLs it contains — the
/// GFM "extended autolink" behaviour (`https://…`, `http://…`, `www.…` in running prose become
/// links) that pulldown-cmark 0.12 does not do itself. Only called for plain text (never inside
/// code or an existing `[text](url)` link).
pub(super) fn linkify_into(runs: &mut Vec<Run>, s: &str, bold: bool, italic: bool, strike: bool) {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut plain_start = 0;
    while i < bytes.len() {
        // Cheap gate: extended autolinks only ever begin with `h` (http) or `w` (www).
        if matches!(bytes[i] | 0x20, b'h' | b'w') {
            if let Some((len, url)) = url_at(s, i) {
                if plain_start < i {
                    push_run(runs, &s[plain_start..i], false, bold, italic, strike, None);
                }
                push_run(runs, &s[i..i + len], false, bold, italic, strike, Some(url));
                i += len;
                plain_start = i;
                continue;
            }
        }
        i += 1;
    }
    if plain_start < s.len() {
        push_run(runs, &s[plain_start..], false, bold, italic, strike, None);
    }
}

/// If a bare URL starts at byte `i` in `s`, return its `(byte length, resolved destination)`.
/// Follows the GFM extended-autolink rules closely enough for prose: valid left boundary, a
/// `http(s)://` or `www.` prefix, a host containing a dot, and trailing-punctuation trimming
/// (with balanced-paren handling so `…/Foo_(bar)` keeps its `)`).
pub(super) fn url_at(s: &str, i: usize) -> Option<(usize, String)> {
    let b = s.as_bytes();
    if i > 0 && !is_url_left_boundary(b[i - 1]) {
        return None;
    }
    let rest = &s[i..];
    let (scheme_len, www) = url_scheme_at(rest)?;
    let end = scan_url_bytes(rest, scheme_len)?;
    let e = trim_trailing_punct(&rest.as_bytes()[..end], scheme_len)?;
    let url = &s[i..i + e];
    // Require a dot in the host portion (rejects `https://localhost`-only noise and bare schemes).
    if !url[scheme_len..].contains('.') {
        return None;
    }
    let dest = if www {
        format!("https://{url}")
    } else {
        url.to_string()
    };
    Some((e, dest))
}

/// Left boundary: start of run, whitespace, or a common opener — never mid-word (so
/// `foohttp://x` doesn't match).
fn is_url_left_boundary(c: u8) -> bool {
    matches!(
        c,
        b' ' | b'\t'
            | b'\n'
            | b'\r'
            | b'('
            | b'['
            | b'{'
            | b'<'
            | b'*'
            | b'_'
            | b'~'
            | b'"'
            | b'\''
    )
}

/// A `http(s)://` or `www.` prefix at the start of `rest`: `(scheme byte length, is-www)`.
fn url_scheme_at(rest: &str) -> Option<(usize, bool)> {
    let lower = rest
        .as_bytes()
        .iter()
        .take(8)
        .map(|c| c.to_ascii_lowercase())
        .collect::<Vec<u8>>();
    if lower.starts_with(b"https://") {
        Some((8, false))
    } else if lower.starts_with(b"http://") {
        Some((7, false))
    } else if lower.starts_with(b"www.") {
        Some((4, true))
    } else {
        None
    }
}

/// Consume ASCII URL bytes (RFC-3986 unreserved + sub-delims + `:/?#[]@%`) from the start
/// of `rest`, stopping at the first non-URL byte — whitespace, quotes, `<`, backtick, and
/// any multibyte (non-ASCII) char, the latter also guaranteeing every cut lands on a char
/// boundary. None if nothing follows the scheme.
fn scan_url_bytes(rest: &str, scheme_len: usize) -> Option<usize> {
    let is_url_byte = |c: u8| {
        c.is_ascii_alphanumeric()
            || matches!(
                c,
                b'-' | b'.'
                    | b'_'
                    | b'~'
                    | b':'
                    | b'/'
                    | b'?'
                    | b'#'
                    | b'['
                    | b']'
                    | b'@'
                    | b'!'
                    | b'$'
                    | b'&'
                    | b'\''
                    | b'('
                    | b')'
                    | b'*'
                    | b'+'
                    | b','
                    | b';'
                    | b'='
                    | b'%'
            )
    };
    let mut end = 0;
    for (k, &c) in rest.as_bytes().iter().enumerate() {
        if !is_url_byte(c) {
            break;
        }
        end = k + 1;
    }
    if end <= scheme_len {
        None // nothing after the scheme
    } else {
        Some(end)
    }
}

/// Trim trailing punctuation off `raw` down to `scheme_len`; keep a trailing `)` only if
/// the URL has more `(` than `)`. None if nothing survives past the scheme.
///
/// `opens`/`closes` are the paren counts over the CURRENT `raw[..e]`, maintained incrementally
/// rather than recounted from scratch every time a `)` is examined — a URL trailed by a run of
/// `k` `)` bytes used to recount both totals over the whole shrinking prefix on every one of
/// those `k` steps (O(k²): `https://a.a/` followed by 500,000 `)` was 2.5e11 byte comparisons
/// on the paint thread). Only `)` bytes ever change the running counts (every other trimmed byte
/// is neither `(` nor `)`), so each step needs at most one decrement, not a rescan.
fn trim_trailing_punct(raw: &[u8], scheme_len: usize) -> Option<usize> {
    let mut e = raw.len();
    // `opens` never needs to change: trimming only ever removes non-`(` bytes (plain
    // punctuation, or a `)` — never `(`), so the open-paren count over the shrinking `raw[..e]`
    // prefix is the same as over the whole slice for every `e` this loop ever reaches.
    let opens = raw.iter().filter(|&&x| x == b'(').count();
    let mut closes = raw.iter().filter(|&&x| x == b')').count();
    while e > scheme_len {
        let c = raw[e - 1];
        if matches!(
            c,
            b'.' | b',' | b';' | b':' | b'!' | b'?' | b'\'' | b'"' | b'*' | b'_' | b'~'
        ) {
            e -= 1;
        } else if c == b')' {
            if closes > opens {
                e -= 1;
                closes -= 1; // this `)` is no longer part of raw[..e]
            } else {
                break;
            }
        } else {
            break;
        }
    }
    if e <= scheme_len {
        None
    } else {
        Some(e)
    }
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

/// Per-event code-fence state threaded through the dispatchers below: a fenced/indented
/// code block spans several events (`CodeBlock` start, one or more `Text`s, `CodeBlock` end).
struct CodeBlockState {
    in_code: bool,
    buf: String,
    lang: highlight::Lang,
}

/// Walk the markdown events into a flat block list with inline styled runs. Raw HTML (block
/// AND inline) is routed through [`super::super::mdhtml::feed`] into the same builder.
///
/// Each pulldown-cmark [`Event`] is offered to a chain of category dispatchers in turn (an
/// event belongs to exactly one, so the order between them doesn't change what happens, only
/// which function's `match` claims it): structural blocks, then tables, then inline styling,
/// then raw HTML/text/leaf events, which also holds the final catch-all for anything none of
/// them care about.
pub(super) fn parse_blocks(md: &str, remote_ok: bool) -> Vec<Block> {
    let md = fence_front_matter(md);
    let opts = md_options();

    let mut b = Builder::new(remote_ok);
    let mut code = CodeBlockState {
        in_code: false,
        buf: String::new(),
        lang: highlight::Lang::Plain,
    };

    for ev in Parser::new_ext(&md, opts) {
        let Some(ev) = handle_structural_event(ev, &mut b, &mut code) else {
            continue;
        };
        let Some(ev) = handle_table_event(ev, &mut b) else {
            continue;
        };
        let Some(ev) = handle_inline_style_event(ev, &mut b) else {
            continue;
        };
        handle_text_and_html_event(ev, &mut b, &mut code);
    }
    // trailing text + any half-open raw-HTML structures
    b.html_table_close();
    b.flush();
    b.out
}

/// Headings, paragraphs, code blocks, lists/items, and block quotes. Returns the event back
/// (for the next dispatcher) when it isn't one of these.
fn handle_structural_event<'e>(
    ev: Event<'e>,
    b: &mut Builder,
    code: &mut CodeBlockState,
) -> Option<Event<'e>> {
    match ev {
        Event::Start(Tag::Heading { level, .. }) => b.start_heading(heading_num(level)),
        Event::End(TagEnd::Heading(_)) => b.end_heading(),
        Event::Start(Tag::Paragraph) => b.open_para(),
        Event::End(TagEnd::Paragraph) => b.close_para(),
        Event::Start(Tag::CodeBlock(kind)) => {
            code.in_code = true;
            code.buf.clear();
            code.lang = match kind {
                CodeBlockKind::Fenced(info) => {
                    highlight::lang_from_fence(info.split_whitespace().next().unwrap_or(""))
                }
                CodeBlockKind::Indented => highlight::Lang::Plain,
            };
        }
        Event::End(TagEnd::CodeBlock) => {
            code.in_code = false;
            let text = code.buf.trim_end_matches('\n').to_string();
            code.buf.clear();
            if !text.is_empty() {
                b.flush();
                b.out.push(Block::Code(text, code.lang));
            }
        }
        Event::Start(Tag::List(start)) => b.open_list(start.is_some(), start.unwrap_or(1)),
        Event::End(TagEnd::List(_)) => b.close_list(),
        Event::Start(Tag::Item) => b.open_item(),
        Event::End(TagEnd::Item) => b.close_item(),
        Event::Start(Tag::BlockQuote(_)) => b.open_quote(),
        Event::End(TagEnd::BlockQuote(_)) => b.close_quote(),
        other => return Some(other),
    }
    None
}

/// Table start/end and its head/row/cell boundaries.
fn handle_table_event<'e>(ev: Event<'e>, b: &mut Builder) -> Option<Event<'e>> {
    match ev {
        Event::Start(Tag::Table(aligns)) => {
            b.flush();
            b.tbl_header.clear();
            b.tbl_rows.clear();
            b.tbl_rows_dropped = false;
            b.tbl_cols_dropped = false;
            b.tbl_aligns = aligns
                .iter()
                .map(|a| match a {
                    Alignment::Center => 1,
                    Alignment::Right => 2,
                    _ => 0,
                })
                .collect();
        }
        Event::End(TagEnd::Table) => {
            let header = core::mem::take(&mut b.tbl_header);
            let rows = core::mem::take(&mut b.tbl_rows);
            let aligns = core::mem::take(&mut b.tbl_aligns);
            let note = table_cap_note(b.tbl_rows_dropped, b.tbl_cols_dropped);
            b.out.push(Block::Table {
                header,
                rows,
                aligns,
            });
            if let Some(note) = note {
                b.out.push(Block::Para(note, false));
            }
        }
        Event::Start(Tag::TableHead) => b.cur_row.clear(),
        Event::End(TagEnd::TableHead) => b.tbl_header = core::mem::take(&mut b.cur_row),
        Event::Start(Tag::TableRow) => b.cur_row.clear(),
        Event::End(TagEnd::TableRow) => {
            let row = core::mem::take(&mut b.cur_row);
            if b.tbl_rows.len() < MAX_TABLE_ROWS {
                b.tbl_rows.push(row);
            } else {
                b.tbl_rows_dropped = true;
            }
        }
        Event::Start(Tag::TableCell) => {
            b.in_cell = true;
            b.cur_cell.clear();
        }
        Event::End(TagEnd::TableCell) => {
            b.in_cell = false;
            let cell = core::mem::take(&mut b.cur_cell);
            if b.cur_row.len() < MAX_TABLE_COLS {
                b.cur_row.push(cell);
            } else {
                b.tbl_cols_dropped = true;
            }
        }
        other => return Some(other),
    }
    None
}

/// Bold/italic/strikethrough toggles, links, and images.
fn handle_inline_style_event<'e>(ev: Event<'e>, b: &mut Builder) -> Option<Event<'e>> {
    match ev {
        Event::Start(Tag::Strong) => b.bold(true),
        Event::End(TagEnd::Strong) => b.bold(false),
        Event::Start(Tag::Emphasis) => b.italic(true),
        Event::End(TagEnd::Emphasis) => b.italic(false),
        Event::Start(Tag::Strikethrough) => b.strikethrough(true),
        Event::End(TagEnd::Strikethrough) => b.strikethrough(false),
        Event::Start(Tag::Link { dest_url, .. }) => b.set_link(Some(dest_url.to_string())),
        Event::End(TagEnd::Link) => b.set_link(None),
        Event::Start(Tag::Image { dest_url, .. }) => {
            b.img = Some((dest_url.to_string(), String::new()));
        }
        Event::End(TagEnd::Image) => {
            if let Some((src, alt)) = b.img.take() {
                b.image(&src, &alt, ImgW::Natural);
            }
        }
        other => return Some(other),
    }
    None
}

/// Raw HTML, thematic breaks, text/code runs (routed to the code-fence buffer while inside
/// one), and the remaining leaf events (soft/hard breaks, GFM task markers), plus the final
/// catch-all for anything the earlier dispatchers didn't claim.
fn handle_text_and_html_event(ev: Event, b: &mut Builder, code: &mut CodeBlockState) {
    match ev {
        Event::Start(Tag::HtmlBlock) => b.html_buf.clear(),
        Event::Html(s) => b.html_buf.push_str(&s),
        Event::End(TagEnd::HtmlBlock) => {
            let buf = core::mem::take(&mut b.html_buf);
            super::super::mdhtml::feed(b, &buf);
        }
        Event::InlineHtml(s) => super::super::mdhtml::feed(b, &s),
        Event::Rule => b.rule(),
        Event::Text(t) => {
            if code.in_code {
                code.buf.push_str(&t);
            } else {
                b.text(&t);
            }
        }
        Event::Code(t) => {
            if b.img.is_some() {
                b.text(&t); // alt-text fragment
            } else {
                b.code_text(&t);
            }
        }
        Event::SoftBreak => b.text(" "),
        Event::HardBreak => b.newline(),
        // A GFM task-list checkbox: remember it for the open item (it replaces the
        // bullet at draw time) instead of dumping literal "[ ]"/"[x]" text.
        Event::TaskListMarker(done) => b.task = Some(done),
        _ => {}
    }
}

fn heading_num(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

#[cfg(test)]
mod table_cap_tests {
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
}

#[cfg(test)]
mod trim_trailing_punct_tests {
    use super::*;

    /// The balanced-paren case the function exists for: a trailing `)` that closes an earlier
    /// `(` inside the URL (a Wikipedia-style `Foo_(bar)` link) must survive trimming.
    #[test]
    fn a_balanced_trailing_paren_is_kept() {
        let (len, url) = url_at("https://en.wikipedia.org/wiki/Foo_(bar)", 0).unwrap();
        assert_eq!(len, "https://en.wikipedia.org/wiki/Foo_(bar)".len());
        assert_eq!(url, "https://en.wikipedia.org/wiki/Foo_(bar)");
    }

    /// An unbalanced trailing `)` (prose punctuation, not part of the URL) is trimmed.
    #[test]
    fn an_unbalanced_trailing_paren_is_trimmed() {
        let (len, url) = url_at("(see https://example.com/x)", 5).unwrap();
        assert_eq!(
            &"(see https://example.com/x)"[5..5 + len],
            "https://example.com/x"
        );
        assert_eq!(url, "https://example.com/x");
    }

    /// The bug this guards: `trim_trailing_punct` used to recount `(`/`)` over the whole
    /// shrinking prefix on every trailing `)` it examined — O(k²) for k trailing close-parens,
    /// so a URL followed by hundreds of thousands of `)` (an accepted URL byte) hung the paint
    /// thread. The counts are now maintained incrementally, so this must return promptly.
    #[test]
    fn a_flood_of_trailing_close_parens_does_not_hang() {
        let mut s = String::from("https://a.a/");
        for _ in 0..500_000 {
            s.push(')');
        }
        let started = std::time::Instant::now();
        let (len, _) = url_at(&s, 0).unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "trim_trailing_punct took too long on a flood of trailing ')'"
        );
        // None of the flood is balanced by an opening '(', so every one of them is trimmed.
        assert_eq!(len, "https://a.a/".len());
    }
}

#[cfg(test)]
mod front_matter_tests {
    use super::*;

    /// Front matter with a closing fence becomes ONE preformatted Yaml code block ahead of the
    /// document body, going through the same `Block::Code` path a fenced ```yaml block would.
    #[test]
    fn front_matter_present_becomes_a_code_block() {
        let blocks = parse_blocks("---\ntitle: Test\ndraft: false\n---\n\n# Body\n", false);
        match &blocks[0] {
            Block::Code(text, lang) => {
                assert_eq!(text, "title: Test\ndraft: false");
                assert!(matches!(lang, highlight::Lang::Yaml));
            }
            _ => panic!("expected a front-matter code block first"),
        }
        assert!(
            matches!(&blocks[1], Block::Heading(1, ..)),
            "body must follow the fenced-off front matter"
        );
    }

    /// No closing `---` means it is NOT front matter (maybe just forgotten, maybe the file was
    /// never meant to have any) — the pre-pass must change nothing, so it renders exactly as it
    /// did before: a rule, then the "fields" as a stray paragraph.
    #[test]
    fn unterminated_front_matter_is_left_alone() {
        let blocks = parse_blocks("---\ntitle: Test\ndraft: false\n\nBody text.\n", false);
        assert!(matches!(&blocks[0], Block::Rule));
        assert!(matches!(&blocks[1], Block::Para(..)));
    }

    /// A document that legitimately opens with a thematic break must keep rendering as one —
    /// there is no closing `---` here either, so the leading rule is untouched.
    #[test]
    fn leading_rule_followed_by_paragraph_stays_a_rule() {
        let blocks = parse_blocks("---\n\nJust a normal paragraph.\n", false);
        assert!(matches!(&blocks[0], Block::Rule));
        assert!(matches!(&blocks[1], Block::Para(..)));
    }
}
