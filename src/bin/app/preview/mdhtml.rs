//! Zero-dep raw-HTML feeder for the markdown renderer.
//!
//! READMEs open with raw-HTML "heroes" (`<div align="center">` + linked `<img>` + `<h1>` +
//! `<p><b>tagline</b></p>` + a badge row) that pulldown-cmark hands through verbatim as
//! `Event::Html` / `Event::InlineHtml`. This module tokenizes those fragments (tags + text +
//! entities — no external HTML crate) and drives the SAME [`Builder`] the markdown events use,
//! so `<b>`/`<h2>`/`<img>`/`<table>` render identically to their markdown twins. It is a
//! renderer-feeder, not a browser: unknown tags are skipped and their text flows through;
//! `<style>`/`<script>`/`<svg>` contents are dropped entirely. State that must survive across
//! fragments (an open `<div align="center">`, an unterminated comment) lives on the builder —
//! CommonMark splits block HTML at blank lines, so the opener and its `</div>` arrive in
//! DIFFERENT events with markdown in between.

use std::ops::ControlFlow;

use super::markdown::{Builder, ImgW};

/// `feed`'s step while inside `<!-- ... -->` (possibly opened by an earlier fragment).
/// `Break(())` means the comment is still open at the end of this fragment; the wait carries
/// over to the next `feed` call via `b.in_comment`.
fn advance_in_comment(b: &mut Builder, s: &str, i: usize) -> ControlFlow<(), usize> {
    match s[i..].find("-->") {
        Some(p) => {
            b.in_comment = false;
            ControlFlow::Continue(i + p + 3)
        }
        None => ControlFlow::Break(()),
    }
}

/// `feed`'s step while inside `<style>`/`<script>`/`<svg>`, dropping everything until the
/// matching close tag. `Break(())` when the close tag doesn't land in this fragment.
///
/// A raw substring match on `</tag` is not enough: `</styled-component>` contains `</style` as
/// a substring, so a naive `find` would end the skip there and let the rest of the script/style
/// source render as prose (and its embedded tags render live). The byte right after the tag
/// name must be `>`, `/`, ASCII whitespace, or end of input — never another name character —
/// and a false match keeps searching forward for a real close tag instead of giving up.
fn advance_in_skip_tag(
    b: &mut Builder,
    s: &str,
    i: usize,
    tag: &'static str,
) -> ControlFlow<(), usize> {
    let low = s[i..].to_ascii_lowercase();
    let needle = format!("</{tag}");
    let mut from = 0usize;
    loop {
        let Some(rel) = low[from..].find(&needle) else {
            return ControlFlow::Break(());
        };
        let p = from + rel;
        let after_name = p + needle.len();
        let is_boundary = low
            .as_bytes()
            .get(after_name)
            .is_none_or(|&c| c == b'>' || c == b'/' || c.is_ascii_whitespace());
        if !is_boundary {
            from = after_name;
            continue;
        }
        let after = i + after_name;
        let Some(q) = s[after..].find('>') else {
            return ControlFlow::Break(());
        };
        b.skip_tag = None;
        return ControlFlow::Continue(after + q + 1);
    }
}

/// `feed`'s step when the next byte is `<`: a comment/doctype opener, a real tag (dispatched),
/// or a stray `<` emitted literally as text.
fn advance_at_lt(b: &mut Builder, s: &str, i: usize, bytes: &[u8]) -> ControlFlow<(), usize> {
    if s[i..].starts_with("<!--") {
        b.in_comment = true;
        return ControlFlow::Continue(i + 4);
    }
    if i + 1 < bytes.len() && bytes[i + 1] == b'!' {
        // <!DOCTYPE ...> and friends: skip to '>'.
        return match s[i..].find('>') {
            Some(p) => ControlFlow::Continue(i + p + 1),
            None => ControlFlow::Break(()),
        };
    }
    match parse_tag(s, i) {
        Some(t) => {
            dispatch(b, &t);
            ControlFlow::Continue(t.end)
        }
        None => {
            // stray '<' that isn't a tag, emit literally
            b.text("<");
            ControlFlow::Continue(i + 1)
        }
    }
}

/// `feed`'s step for a run of plain text up to the next `<` (or fragment end).
fn advance_text(b: &mut Builder, s: &str, i: usize, bytes: &[u8]) -> usize {
    let next = s[i..].find('<').map(|p| i + p).unwrap_or(bytes.len());
    let txt = decode_entities(&s[i..next]);
    let cleaned = collapse_ws(&txt);
    if !cleaned.is_empty() {
        b.text(&cleaned);
    }
    next
}

/// Tokenize one raw-HTML fragment into builder ops.
pub(super) fn feed(b: &mut Builder, html: &str) {
    let s = html;
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let step = if b.in_comment {
            advance_in_comment(b, s, i)
        } else if let Some(tag) = b.skip_tag {
            advance_in_skip_tag(b, s, i, tag)
        } else if bytes[i] == b'<' {
            advance_at_lt(b, s, i, bytes)
        } else {
            ControlFlow::Continue(advance_text(b, s, i, bytes))
        };
        match step {
            ControlFlow::Continue(next) => i = next,
            ControlFlow::Break(()) => return,
        }
    }
}

/// One parsed tag: `end` = byte index just past the closing `>`.
struct HtmlTag {
    end: usize,
    closing: bool,
    name: String,
    attrs: Vec<(String, String)>,
}

/// Parse a `<tag attr="v" ...>` starting at `s[i] == '<'`. `None` if it doesn't scan as a tag.
fn parse_tag(s: &str, i: usize) -> Option<HtmlTag> {
    let bytes = s.as_bytes();
    let mut j = i + 1;
    let closing = bytes.get(j) == Some(&b'/');
    if closing {
        j += 1;
    }
    let name_start = j;
    while j < bytes.len() && (bytes[j].is_ascii_alphanumeric()) {
        j += 1;
    }
    if j == name_start {
        return None; // "<" not followed by a name
    }
    let name = s[name_start..j].to_ascii_lowercase();

    let (attrs, end) = parse_tag_attrs(s, j)?;
    Some(HtmlTag {
        end,
        closing,
        name,
        attrs,
    })
}

/// Parse the attribute list of a tag, starting right after its name, up to and including the
/// closing `>`. Returns the attrs and the byte index just past that `>`. `None` on an
/// unterminated tag (ran off the end of `s` before finding `>`).
fn parse_tag_attrs(s: &str, start: usize) -> Option<(Vec<(String, String)>, usize)> {
    let bytes = s.as_bytes();
    let mut j = start;
    let mut attrs = Vec::new();
    loop {
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        match bytes.get(j) {
            None => return None, // unterminated tag
            Some(b'>') => return Some((attrs, j + 1)),
            Some(b'/') => {
                j += 1; // self-closing slash — the '>' comes next
                continue;
            }
            _ => {}
        }
        let (aname, aval, next) = parse_one_attr(s, j);
        if next == j {
            j += 1; // stray char — skip it
            continue;
        }
        j = next;
        attrs.push((aname, aval));
    }
}

/// Read an attribute name starting at `j` (up to whitespace or `= > /`). Returns the
/// lowercased name and the position just past it; `None` when there is no name here (the
/// caller's stray-char skip applies).
fn read_attr_name(s: &str, j: usize) -> Option<(String, usize)> {
    let bytes = s.as_bytes();
    let start = j;
    let mut j = j;
    while j < bytes.len()
        && !bytes[j].is_ascii_whitespace()
        && !matches!(bytes[j], b'=' | b'>' | b'/')
    {
        j += 1;
    }
    if j == start {
        return None;
    }
    Some((s[start..j].to_ascii_lowercase(), j))
}

/// Read a quoted attribute value's contents, starting right after the opening quote `q`.
/// Returns the entity-decoded value and the position just past the closing quote (or the end
/// of `s`, for an unterminated value).
fn read_quoted_attr_value(s: &str, j: usize, q: u8) -> (String, usize) {
    let bytes = s.as_bytes();
    let v_start = j;
    let mut j = j;
    while j < bytes.len() && bytes[j] != q {
        j += 1;
    }
    let val = decode_entities(&s[v_start..j.min(bytes.len())]);
    if j < bytes.len() {
        j += 1; // past the closing quote
    }
    (val, j)
}

/// Read an unquoted attribute value: up to whitespace or `>`.
fn read_unquoted_attr_value(s: &str, j: usize) -> (String, usize) {
    let bytes = s.as_bytes();
    let v_start = j;
    let mut j = j;
    while j < bytes.len() && !bytes[j].is_ascii_whitespace() && bytes[j] != b'>' {
        j += 1;
    }
    (decode_entities(&s[v_start..j]), j)
}

/// Read the `=value` part of an attribute: `j` points AT the `=` (the caller has already
/// confirmed `bytes[j] == '='`). Skips whitespace after the `=`, then reads a quoted or
/// unquoted value. Returns the entity-decoded value and the position just past it.
fn read_attr_value(s: &str, j: usize) -> (String, usize) {
    let bytes = s.as_bytes();
    let mut j = j + 1; // past '='
    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
        j += 1;
    }
    match bytes.get(j) {
        Some(&q) if q == b'"' || q == b'\'' => read_quoted_attr_value(s, j + 1, q),
        _ => read_unquoted_attr_value(s, j),
    }
}

/// Parse one `name` or `name="value"` (or `name='value'`, or unquoted) attribute starting at
/// `j`. Returns the attribute name, its (entity-decoded) value, and the byte index just past
/// it. When `j` is not the start of a valid attribute name, returns `j` unchanged as `next`
/// so the caller's stray-char skip applies.
fn parse_one_attr(s: &str, j: usize) -> (String, String, usize) {
    let bytes = s.as_bytes();
    let Some((aname, mut j)) = read_attr_name(s, j) else {
        return (String::new(), String::new(), j);
    };
    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
        j += 1;
    }
    let mut aval = String::new();
    if bytes.get(j) == Some(&b'=') {
        let (v, nj) = read_attr_value(s, j);
        aval = v;
        j = nj;
    }
    (aname, aval, j)
}

fn attr<'a>(t: &'a HtmlTag, name: &str) -> Option<&'a str> {
    t.attrs
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

/// Does this tag center its contents (`align="center"` / `style="text-align:center"`)?
fn is_centered(t: &HtmlTag) -> bool {
    if attr(t, "align").is_some_and(|v| v.eq_ignore_ascii_case("center")) {
        return true;
    }
    attr(t, "style").is_some_and(|v| {
        let squashed: String = v.to_ascii_lowercase().split_whitespace().collect();
        squashed.contains("text-align:center")
    })
}

/// Parse a `width` attribute: `"820"` -> Px, `"31%"` -> Pct, junk -> Natural.
fn parse_width(v: Option<&str>) -> ImgW {
    let Some(v) = v else { return ImgW::Natural };
    let v = v.trim();
    if let Some(p) = v.strip_suffix('%') {
        return p
            .trim()
            .parse::<u32>()
            .map_or(ImgW::Natural, |n| ImgW::Pct(n.clamp(1, 100)));
    }
    let digits: String = v.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits
        .parse::<i32>()
        .map_or(ImgW::Natural, |n| ImgW::Px(n.clamp(1, 4000)))
}

/// Leaf formatting/media tags: a single toggle call (or, for `img`/`br`/`hr`, a single
/// opening-tag-only action). Returns whether `t.name` matched one of them.
fn dispatch_inline(b: &mut Builder, t: &HtmlTag) -> bool {
    let closing = t.closing;
    match t.name.as_str() {
        "b" | "strong" => b.bold(!closing),
        "i" | "em" | "cite" | "var" => b.italic(!closing),
        "s" | "strike" | "del" => b.strikethrough(!closing),
        "code" | "tt" | "kbd" | "samp" => b.code(!closing),
        "a" => {
            if closing {
                b.set_link(None);
            } else {
                b.set_link(attr(t, "href").map(str::to_string));
            }
        }
        "img" if !closing => {
            if let Some(src) = attr(t, "src") {
                if !src.is_empty() {
                    b.image(
                        src,
                        attr(t, "alt").unwrap_or(""),
                        parse_width(attr(t, "width")),
                    );
                }
            }
        }
        "br" if !closing => b.newline(),
        "hr" if !closing => b.rule(),
        _ => return false,
    }
    true
}

/// Paired open/close block tags: paragraphs, headings, containers, `<center>`, `<summary>`,
/// `<blockquote>`. Returns whether `t.name` matched one of them.
fn dispatch_block(b: &mut Builder, t: &HtmlTag) -> bool {
    let closing = t.closing;
    match t.name.as_str() {
        "p" | "figcaption" => {
            if closing {
                b.close_para();
            } else {
                b.open_para();
            }
        }
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let level = t.name.as_bytes()[1] - b'0';
            if closing {
                b.end_heading();
            } else {
                b.start_heading(level);
            }
        }
        "div" | "section" | "article" | "main" | "figure" | "header" | "footer" | "details" => {
            if closing {
                b.close_container(&t.name);
            } else {
                b.open_container(&t.name, is_centered(t));
            }
        }
        "center" => {
            if closing {
                b.close_container("center");
            } else {
                b.open_container("center", true);
            }
        }
        "summary" => {
            if closing {
                b.bold(false);
                b.close_para();
            } else {
                b.open_para();
                b.bold(true);
            }
        }
        "blockquote" => {
            if closing {
                b.close_quote();
            } else {
                b.open_quote();
            }
        }
        _ => return false,
    }
    true
}

/// Lists and the HTML-table trio: `<ul>`/`<ol>`/`<li>`, `<table>`/`<tr>`/`<td>`/`<th>`.
/// Returns whether `t.name` matched one of them.
fn dispatch_list_table(b: &mut Builder, t: &HtmlTag) -> bool {
    let closing = t.closing;
    match t.name.as_str() {
        "ul" => {
            if closing {
                b.close_list();
            } else {
                b.open_list(false, 1);
            }
        }
        "ol" => {
            if closing {
                b.close_list();
            } else {
                let start = attr(t, "start")
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(1);
                b.open_list(true, start);
            }
        }
        "li" => {
            if closing {
                b.close_item();
            } else {
                b.open_item();
            }
        }
        "table" => {
            if closing {
                b.html_table_close();
            } else {
                b.html_table_open();
            }
        }
        "tr" => {
            if closing {
                b.html_tr_close();
            } else {
                b.html_tr_open();
            }
        }
        "td" | "th" => {
            if closing {
                b.html_cell_close();
            } else {
                b.html_cell_open(t.name == "th");
            }
        }
        _ => return false,
    }
    true
}

fn dispatch(b: &mut Builder, t: &HtmlTag) {
    if dispatch_inline(b, t) || dispatch_block(b, t) || dispatch_list_table(b, t) {
        return;
    }
    match t.name.as_str() {
        "style" if !t.closing => b.skip_tag = Some("style"),
        "script" if !t.closing => b.skip_tag = Some("script"),
        "svg" if !t.closing => b.skip_tag = Some("svg"),
        "title" if !t.closing => b.skip_tag = Some("title"),
        // thead/tbody/tfoot/picture/source/span/sub/sup/small/u/font/wbr/…: structural or
        // purely-visual tags we don't style — their text flows through untouched.
        _ => {}
    }
}

/// Collapse HTML whitespace runs (space/tab/CR/LF) to a single space. NBSP (`&nbsp;` ->
/// U+00A0) survives — that's its purpose.
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for ch in s.chars() {
        if matches!(ch, ' ' | '\t' | '\r' | '\n') {
            if !in_ws {
                out.push(' ');
            }
            in_ws = true;
        } else {
            out.push(ch);
            in_ws = false;
        }
    }
    out
}

/// Decode the entities READMEs actually use (+ numeric forms). Unknown entities pass through
/// literally. Shared with `mailmsg`'s HTML-mail body decoding, which used to keep its own
/// 7-entity copy that had drifted from this one's ~27 named entities plus numeric forms — an
/// email body containing `&mdash;` rendered the literal text instead of the dash.
pub(super) fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        rest = &rest[p..];
        // Byte-wise ';' scan — a `&str` slice of the first 12 BYTES would panic (=abort) when a
        // multibyte char straddles the cut (e.g. `"&ééééé…"`); ';' is ASCII so this is safe.
        let semi = match rest.as_bytes()[..rest.len().min(12)]
            .iter()
            .position(|&b| b == b';')
        {
            Some(q) => q,
            None => {
                out.push('&');
                rest = &rest[1..];
                continue;
            }
        };
        let ent = &rest[1..semi];
        let decoded: Option<char> = match ent.strip_prefix('#') {
            Some(num) => numeric_entity(num),
            None => named_entity(ent),
        };
        match decoded {
            Some(ch) => {
                out.push(ch);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// `&#123;` / `&#x7B;`: decimal or hex numeric character reference.
fn numeric_entity(num: &str) -> Option<char> {
    let cp = if let Some(hex) = num.strip_prefix(['x', 'X']) {
        u32::from_str_radix(hex, 16).ok()
    } else {
        num.parse::<u32>().ok()
    };
    cp.and_then(char::from_u32)
}

/// Named entity lookup, split across two tables purely to keep each match's arm count under
/// the complexity gate.
fn named_entity(ent: &str) -> Option<char> {
    named_entity_basic(ent).or_else(|| named_entity_typographic(ent))
}

fn named_entity_basic(ent: &str) -> Option<char> {
    match ent {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some('\u{00A0}'),
        "middot" => Some('·'),
        "bull" => Some('•'),
        "copy" => Some('©'),
        "reg" => Some('®'),
        "trade" => Some('™'),
        "hellip" => Some('…'),
        "mdash" => Some('—'),
        "ndash" => Some('–'),
        _ => None,
    }
}

fn named_entity_typographic(ent: &str) -> Option<char> {
    match ent {
        "ldquo" => Some('“'),
        "rdquo" => Some('”'),
        "lsquo" => Some('‘'),
        "rsquo" => Some('’'),
        "laquo" => Some('«'),
        "raquo" => Some('»'),
        "deg" => Some('°'),
        "times" => Some('×'),
        "larr" => Some('←'),
        "rarr" => Some('→'),
        "uarr" => Some('↑'),
        "darr" => Some('↓'),
        _ => None,
    }
}

#[cfg(test)]
mod skip_tag_tests {
    use super::*;

    /// The bug: `</styled-component>` contains `</style` as a bare substring, so a naive
    /// `find` ended the skip there, treating the rest of the style block (and even the real
    /// `</style>` inside it) as ordinary content to render. The skip must run past that false
    /// match to the actual closing tag.
    #[test]
    fn a_prefix_match_inside_a_longer_tag_name_does_not_end_the_skip() {
        let mut b = Builder::new(false);
        b.skip_tag = Some("style");
        let s = "<style>.a{}</styled-component>fake close, still inside</style>SAFE";
        let start = "<style>".len();
        match advance_in_skip_tag(&mut b, s, start, "style") {
            ControlFlow::Continue(next) => {
                assert_eq!(
                    &s[next..],
                    "SAFE",
                    "must skip past the REAL </style>, not the </styled-component> substring"
                );
                assert!(
                    b.skip_tag.is_none(),
                    "the real close tag must clear skip_tag"
                );
            }
            ControlFlow::Break(()) => panic!("a real closing tag exists in this fragment"),
        }
    }

    /// A close tag immediately followed by `/` (a stray self-closing-style slash) or
    /// whitespace before the `>` is still a real boundary, not a false match to reject.
    #[test]
    fn whitespace_or_slash_before_the_closing_angle_bracket_is_still_a_real_close() {
        let mut b = Builder::new(false);
        b.skip_tag = Some("script");
        let s = "<script>var x=1;</script >after";
        let start = "<script>".len();
        let ControlFlow::Continue(next) = advance_in_skip_tag(&mut b, s, start, "script") else {
            panic!("a real closing tag exists in this fragment");
        };
        assert_eq!(&s[next..], "after");
    }

    /// No closing tag anywhere in the fragment: the skip stays open, carried to the next
    /// `feed` call via `b.skip_tag` (unchanged here).
    #[test]
    fn no_close_tag_breaks_and_leaves_skip_tag_set() {
        let mut b = Builder::new(false);
        b.skip_tag = Some("style");
        let s = "<style>still going, no close in this fragment";
        let start = "<style>".len();
        assert_eq!(
            advance_in_skip_tag(&mut b, s, start, "style"),
            ControlFlow::Break(())
        );
        assert_eq!(b.skip_tag, Some("style"));
    }
}
