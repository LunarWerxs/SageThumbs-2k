//! The pulldown-cmark event stream, one handler per event family.

use super::*;

/// Per-event code-fence state threaded through the dispatchers below: a fenced/indented
/// code block spans several events (`CodeBlock` start, one or more `Text`s, `CodeBlock` end).
pub(super) struct CodeBlockState {
    pub(super) in_code: bool,
    pub(super) buf: String,
    pub(super) lang: highlight::Lang,
}

/// Walk the markdown events into a flat block list with inline styled runs. Raw HTML (block
/// AND inline) is routed through [`super::super::mdhtml::feed`] into the same builder.
///
/// Each pulldown-cmark [`Event`] is offered to a chain of category dispatchers in turn (an
/// event belongs to exactly one, so the order between them doesn't change what happens, only
/// which function's `match` claims it): structural blocks, then tables, then inline styling,
/// then raw HTML/text/leaf events, which also holds the final catch-all for anything none of
/// them care about.
pub(in super::super) fn parse_blocks(md: &str, remote_ok: bool) -> Vec<Block> {
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
pub(super) fn handle_structural_event<'e>(
    ev: Event<'e>,
    b: &mut Builder,
    code: &mut CodeBlockState,
) -> Option<Event<'e>> {
    match ev {
        Event::Start(Tag::Heading { level, .. }) => b.start_heading(heading_num(level)),
        Event::End(TagEnd::Heading(_)) => b.end_heading(),
        Event::Start(Tag::Paragraph) => b.open_para(),
        Event::End(TagEnd::Paragraph) => b.close_para(),
        Event::Start(Tag::CodeBlock(kind)) => start_code_block(kind, code),
        Event::End(TagEnd::CodeBlock) => end_code_block(b, code),
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

/// Start a fenced or indented code block, initializing the fence buffer and language.
fn start_code_block(kind: CodeBlockKind, code: &mut CodeBlockState) {
    code.in_code = true;
    code.buf.clear();
    code.lang = match kind {
        CodeBlockKind::Fenced(info) => {
            highlight::lang_from_fence(info.split_whitespace().next().unwrap_or(""))
        }
        CodeBlockKind::Indented => highlight::Lang::Plain,
    };
}

/// Finish an open code block and emit it as a block if non-empty.
fn end_code_block(b: &mut Builder, code: &mut CodeBlockState) {
    code.in_code = false;
    let text = code.buf.trim_end_matches('\n').to_string();
    code.buf.clear();
    if !text.is_empty() {
        b.flush();
        b.out.push(Block::Code(text, code.lang));
    }
}

/// Table start/end and its head/row/cell boundaries.
pub(super) fn handle_table_event<'e>(ev: Event<'e>, b: &mut Builder) -> Option<Event<'e>> {
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
        Event::End(TagEnd::Table) => end_table(b),
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

/// Finalize a table block and emit any truncation notice.
fn end_table(b: &mut Builder) {
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

/// Bold/italic/strikethrough toggles, links, and images.
pub(super) fn handle_inline_style_event<'e>(ev: Event<'e>, b: &mut Builder) -> Option<Event<'e>> {
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
pub(super) fn handle_text_and_html_event(ev: Event, b: &mut Builder, code: &mut CodeBlockState) {
    match ev {
        Event::Start(Tag::HtmlBlock) => b.html_buf.clear(),
        Event::Html(s) => b.html_buf.push_str(&s),
        Event::End(TagEnd::HtmlBlock) => {
            let buf = core::mem::take(&mut b.html_buf);
            super::super::super::mdhtml::feed(b, &buf);
        }
        Event::InlineHtml(s) => super::super::super::mdhtml::feed(b, &s),
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

pub(super) fn heading_num(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}
