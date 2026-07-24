//! Markdown and raw-HTML event parsing into renderer blocks.

use super::*;

// ---- markdown -> blocks ------------------------------------------------------------------

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
}

impl Builder {
    fn new(remote_ok: bool) -> Builder {
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
        let remote = (is_remote_src(src) && !(self.remote_ok && fetchable))
            || src.starts_with("//")
            || src.starts_with("data:");
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
                t.cur_row.push(c); // unclosed previous cell
            }
            t.cur_cell = Some(Vec::new());
            t.row_all_th &= th;
        }
    }
    pub(in crate::preview) fn html_cell_close(&mut self) {
        if let Some(t) = &mut self.h_tbl {
            if let Some(c) = t.cur_cell.take() {
                t.cur_row.push(c);
            }
        }
    }
    pub(in crate::preview) fn html_tr_close(&mut self) {
        if let Some(t) = &mut self.h_tbl {
            if let Some(c) = t.cur_cell.take() {
                t.cur_row.push(c);
            }
            let row = core::mem::take(&mut t.cur_row);
            if row.is_empty() {
                return;
            }
            if t.row_all_th && t.header.is_empty() && t.rows.is_empty() {
                t.header = row;
            } else {
                t.rows.push(row);
            }
        }
    }
    pub(in crate::preview) fn html_table_close(&mut self) {
        self.html_tr_close(); // forgive an unclosed final row
        if let Some(t) = self.h_tbl.take() {
            if !t.header.is_empty() || !t.rows.is_empty() {
                self.out.push(Block::Table {
                    header: t.header,
                    rows: t.rows,
                    aligns: Vec::new(),
                });
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
    // Left boundary: start of run, whitespace, or a common opener — never mid-word (so
    // `foohttp://x` doesn't match).
    if i > 0
        && !matches!(
            b[i - 1],
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
    {
        return None;
    }
    let rest = &s[i..];
    let lower = rest
        .as_bytes()
        .iter()
        .take(8)
        .map(|c| c.to_ascii_lowercase())
        .collect::<Vec<u8>>();
    let (scheme_len, www) = if lower.starts_with(b"https://") {
        (8, false)
    } else if lower.starts_with(b"http://") {
        (7, false)
    } else if lower.starts_with(b"www.") {
        (4, true)
    } else {
        return None;
    };
    // Consume ASCII URL bytes (RFC-3986 unreserved + sub-delims + `:/?#[]@%`). Stopping at the
    // first non-URL byte ends the link at whitespace, quotes, `<`, backtick, AND any multibyte
    // (non-ASCII) char — the latter also guarantees every cut lands on a char boundary.
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
        return None; // nothing after the scheme
    }
    // Trim trailing punctuation; keep a `)` only if the URL has more `(` than `)`.
    let raw = &rest.as_bytes()[..end];
    let mut e = end;
    while e > scheme_len {
        let c = raw[e - 1];
        if matches!(
            c,
            b'.' | b',' | b';' | b':' | b'!' | b'?' | b'\'' | b'"' | b'*' | b'_' | b'~'
        ) {
            e -= 1;
        } else if c == b')' {
            let opens = raw[..e].iter().filter(|&&x| x == b'(').count();
            let closes = raw[..e].iter().filter(|&&x| x == b')').count();
            if closes > opens {
                e -= 1;
            } else {
                break;
            }
        } else {
            break;
        }
    }
    if e <= scheme_len {
        return None;
    }
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

/// Walk the markdown events into a flat block list with inline styled runs. Raw HTML (block
/// AND inline) is routed through [`super::super::mdhtml::feed`] into the same builder.
pub(super) fn parse_blocks(md: &str, remote_ok: bool) -> Vec<Block> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);

    let mut b = Builder::new(remote_ok);
    let mut in_code = false;
    let mut code_buf = String::new();
    let mut code_lang = highlight::Lang::Plain;

    for ev in Parser::new_ext(md, opts) {
        match ev {
            Event::Start(Tag::Heading { level, .. }) => b.start_heading(heading_num(level)),
            Event::End(TagEnd::Heading(_)) => b.end_heading(),
            Event::Start(Tag::Paragraph) => b.open_para(),
            Event::End(TagEnd::Paragraph) => b.close_para(),
            Event::Start(Tag::CodeBlock(kind)) => {
                in_code = true;
                code_buf.clear();
                code_lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        highlight::lang_from_fence(info.split_whitespace().next().unwrap_or(""))
                    }
                    CodeBlockKind::Indented => highlight::Lang::Plain,
                };
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code = false;
                let text = code_buf.trim_end_matches('\n').to_string();
                code_buf.clear();
                if !text.is_empty() {
                    b.flush();
                    b.out.push(Block::Code(text, code_lang));
                }
            }
            Event::Start(Tag::List(start)) => b.open_list(start.is_some(), start.unwrap_or(1)),
            Event::End(TagEnd::List(_)) => b.close_list(),
            Event::Start(Tag::Item) => b.open_item(),
            Event::End(TagEnd::Item) => b.close_item(),
            Event::Start(Tag::BlockQuote(_)) => b.open_quote(),
            Event::End(TagEnd::BlockQuote(_)) => b.close_quote(),
            Event::Start(Tag::Table(aligns)) => {
                b.flush();
                b.tbl_header.clear();
                b.tbl_rows.clear();
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
                b.out.push(Block::Table {
                    header,
                    rows,
                    aligns,
                });
            }
            Event::Start(Tag::TableHead) => b.cur_row.clear(),
            Event::End(TagEnd::TableHead) => b.tbl_header = core::mem::take(&mut b.cur_row),
            Event::Start(Tag::TableRow) => b.cur_row.clear(),
            Event::End(TagEnd::TableRow) => {
                let row = core::mem::take(&mut b.cur_row);
                b.tbl_rows.push(row);
            }
            Event::Start(Tag::TableCell) => {
                b.in_cell = true;
                b.cur_cell.clear();
            }
            Event::End(TagEnd::TableCell) => {
                b.in_cell = false;
                let cell = core::mem::take(&mut b.cur_cell);
                b.cur_row.push(cell);
            }
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
            Event::Start(Tag::HtmlBlock) => b.html_buf.clear(),
            Event::Html(s) => b.html_buf.push_str(&s),
            Event::End(TagEnd::HtmlBlock) => {
                let buf = core::mem::take(&mut b.html_buf);
                super::super::mdhtml::feed(&mut b, &buf);
            }
            Event::InlineHtml(s) => super::super::mdhtml::feed(&mut b, &s),
            Event::Rule => b.rule(),
            Event::Text(t) => {
                if in_code {
                    code_buf.push_str(&t);
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
    // trailing text + any half-open raw-HTML structures
    b.html_table_close();
    b.flush();
    b.out
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
