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

use super::markdown::{Builder, ImgW};

/// Tokenize one raw-HTML fragment into builder ops.
pub(super) fn feed(b: &mut Builder, html: &str) {
    let s = html;
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        // Inside <!-- ... --> (possibly started in an earlier fragment).
        if b.in_comment {
            match s[i..].find("-->") {
                Some(p) => {
                    i += p + 3;
                    b.in_comment = false;
                }
                None => return,
            }
            continue;
        }
        // Inside <style>/<script>/<svg>: drop everything until the matching close tag.
        if let Some(tag) = b.skip_tag {
            let low = s[i..].to_ascii_lowercase();
            let needle = format!("</{tag}");
            match low.find(&needle) {
                Some(p) => {
                    let after = i + p;
                    match s[after..].find('>') {
                        Some(q) => {
                            i = after + q + 1;
                            b.skip_tag = None;
                        }
                        None => return,
                    }
                }
                None => return,
            }
            continue;
        }
        if bytes[i] == b'<' {
            if s[i..].starts_with("<!--") {
                b.in_comment = true;
                i += 4;
                continue;
            }
            if i + 1 < bytes.len() && bytes[i + 1] == b'!' {
                // <!DOCTYPE ...> and friends: skip to '>'.
                match s[i..].find('>') {
                    Some(p) => i += p + 1,
                    None => return,
                }
                continue;
            }
            match parse_tag(s, i) {
                Some(t) => {
                    dispatch(b, &t);
                    i = t.end;
                }
                None => {
                    // stray '<' that isn't a tag — emit literally
                    b.text("<");
                    i += 1;
                }
            }
        } else {
            let next = s[i..].find('<').map(|p| i + p).unwrap_or(bytes.len());
            let txt = decode_entities(&s[i..next]);
            let cleaned = collapse_ws(&txt);
            if !cleaned.is_empty() {
                b.text(&cleaned);
            }
            i = next;
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

    // attributes until the closing '>', honoring quotes
    let mut attrs = Vec::new();
    loop {
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        match bytes.get(j) {
            None => return None, // unterminated tag
            Some(b'>') => return Some(HtmlTag { end: j + 1, closing, name, attrs }),
            Some(b'/') => {
                j += 1; // self-closing slash — the '>' comes next
                continue;
            }
            _ => {}
        }
        // attribute name
        let an_start = j;
        while j < bytes.len() && !bytes[j].is_ascii_whitespace() && !matches!(bytes[j], b'=' | b'>' | b'/') {
            j += 1;
        }
        if j == an_start {
            j += 1; // stray char — skip it
            continue;
        }
        let aname = s[an_start..j].to_ascii_lowercase();
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        let mut aval = String::new();
        if bytes.get(j) == Some(&b'=') {
            j += 1;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            match bytes.get(j) {
                Some(&q) if q == b'"' || q == b'\'' => {
                    j += 1;
                    let v_start = j;
                    while j < bytes.len() && bytes[j] != q {
                        j += 1;
                    }
                    aval = decode_entities(&s[v_start..j.min(bytes.len())]);
                    if j < bytes.len() {
                        j += 1; // past the closing quote
                    }
                }
                _ => {
                    let v_start = j;
                    while j < bytes.len() && !bytes[j].is_ascii_whitespace() && bytes[j] != b'>' {
                        j += 1;
                    }
                    aval = decode_entities(&s[v_start..j]);
                }
            }
        }
        attrs.push((aname, aval));
    }
}

fn attr<'a>(t: &'a HtmlTag, name: &str) -> Option<&'a str> {
    t.attrs.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
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
        return p.trim().parse::<u32>().map_or(ImgW::Natural, |n| ImgW::Pct(n.clamp(1, 100)));
    }
    let digits: String = v.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<i32>().map_or(ImgW::Natural, |n| ImgW::Px(n.clamp(1, 4000)))
}

fn dispatch(b: &mut Builder, t: &HtmlTag) {
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
                    b.image(src, attr(t, "alt").unwrap_or(""), parse_width(attr(t, "width")));
                }
            }
        }
        "br" if !closing => b.newline(),
        "hr" if !closing => b.rule(),
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
                let start = attr(t, "start").and_then(|v| v.trim().parse().ok()).unwrap_or(1);
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
        "style" if !closing => b.skip_tag = Some("style"),
        "script" if !closing => b.skip_tag = Some("script"),
        "svg" if !closing => b.skip_tag = Some("svg"),
        "title" if !closing => b.skip_tag = Some("title"),
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
/// literally.
fn decode_entities(s: &str) -> String {
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
        let semi = match rest.as_bytes()[..rest.len().min(12)].iter().position(|&b| b == b';') {
            Some(q) => q,
            None => {
                out.push('&');
                rest = &rest[1..];
                continue;
            }
        };
        let ent = &rest[1..semi];
        let decoded: Option<char> = if let Some(num) = ent.strip_prefix('#') {
            let cp = if let Some(hex) = num.strip_prefix(['x', 'X']) {
                u32::from_str_radix(hex, 16).ok()
            } else {
                num.parse::<u32>().ok()
            };
            cp.and_then(char::from_u32)
        } else {
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
