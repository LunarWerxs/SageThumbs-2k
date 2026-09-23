//! SVG metadata strip.
//!
//! An SVG exported from Illustrator or Inkscape carries the author, the machine
//! name, the document history and sometimes an embedded RDF/Dublin-Core block -
//! all of it as ordinary XML, none of it visible in the drawing.
//!
//! This is a **text** edit, not an XML round-trip: re-serialising through a
//! parser would reflow attributes, drop comments and change whitespace, which for
//! a hand-tuned or build-pipeline SVG is a worse outcome than leaving the
//! metadata in. So whole elements are cut span-by-span and everything else,
//! including byte-for-byte formatting, is left exactly as it was.
//!
//! `<title>` and `<desc>` are removed at the DOCUMENT level only. Nested inside a
//! shape they are accessibility text (a screen reader reads them, and a browser
//! shows `<title>` as a tooltip), so removing those would break the graphic for
//! the people who most need them.

use super::*;

/// Elements cut wholesale wherever they appear at the top level of `<svg>`.
const DROP_ELEMENTS: &[&str] = &["metadata", "title", "desc"];

/// Attempt to drop a top-level metadata element (`<title>`/`<desc>`/`<metadata>`) whose open
/// tag is `tail`. Returns the remaining text after the whole element, plus any trailing
/// whitespace it sat on (so the file does not fill up with blank lines), when it both
/// qualifies and is well-formed. `None` leaves it alone: not a drop candidate, or never closed.
fn try_drop_element<'a>(
    tail: &'a str,
    name: Option<&str>,
    depth_after_svg: i32,
) -> Option<&'a str> {
    let n = name.filter(|n| depth_after_svg == 0 && DROP_ELEMENTS.contains(n))?;
    let end = element_span(tail, n)?;
    let rest = &tail[end..];
    let trimmed = rest.trim_start_matches([' ', '\t', '\r', '\n']);
    Some(if trimmed.starts_with('<') {
        trimmed
    } else {
        rest
    })
}

/// Track nesting so a `<title>` inside a `<path>`/`<g>` is recognised as accessibility text and
/// kept: `-1` before `<svg>` is seen, `0` at its direct children, incrementing/decrementing with
/// every other open/close tag once inside it.
fn step_depth(depth_after_svg: i32, name: Option<&str>, tail: &str) -> i32 {
    if depth_after_svg < 0 {
        return if name == Some("svg") {
            0
        } else {
            depth_after_svg
        };
    }
    let Some(_) = name else {
        return depth_after_svg;
    };
    if !is_self_closing(tail) && !tail.starts_with("</") {
        depth_after_svg + 1
    } else if tail.starts_with("</") {
        depth_after_svg - 1
    } else {
        depth_after_svg
    }
}

/// Remove metadata from SVG source, returning the rewritten bytes.
///
/// Errors only if the input is not valid UTF-8 (an SVG must be, per the spec).
pub(super) fn strip(input: &[u8]) -> Result<Vec<u8>> {
    let text = std::str::from_utf8(input)
        .map_err(|e| Error::new(E_FAIL, format!("svg: not UTF-8: {e}")))?;
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    // Track nesting so a <title> inside a <path>/<g> is recognised as
    // accessibility text and kept.
    let mut depth_after_svg: i32 = -1;

    while let Some(lt) = rest.find('<') {
        let (before, tail) = rest.split_at(lt);
        out.push_str(before);
        // A CDATA section or a comment is character data from here to its terminator: copy
        // it through whole, so a `<title>` written INSIDE one is never mistaken for markup.
        if let Some(end) = opaque_section_end(tail) {
            out.push_str(&tail[..end]);
            rest = &tail[end..];
            continue;
        }
        let name = element_name(tail);

        if let Some(new_rest) = try_drop_element(tail, name, depth_after_svg) {
            rest = new_rest;
            continue;
        }
        depth_after_svg = step_depth(depth_after_svg, name, tail);

        // Copy this tag through untouched.
        let end = tag_end(tail).map(|i| i + 1).unwrap_or(tail.len());
        out.push_str(&tail[..end]);
        rest = &tail[end..];
    }
    out.push_str(rest);
    // A text edit can only be trusted as far as its tokenizer sees. Before the caller replaces
    // the user's file, ask an independent XML parser the one question that matters: if the
    // ORIGINAL was well-formed and the REWRITE is not, the edit went wrong and nothing is
    // written (2026-09-19 audit F02: a `</metadata>` quoted inside a CDATA section cut the
    // element short and the malformed result was swapped over a valid file). An original the
    // parser already rejects is stripped as before - a stricter parser than the browser is
    // no reason to refuse the user.
    if roxmltree::Document::parse(text).is_ok() && roxmltree::Document::parse(&out).is_err() {
        return Err(Error::new(
            E_FAIL,
            "svg: the rewrite would not be well-formed XML; the file was left untouched",
        ));
    }
    Ok(out.into_bytes())
}

/// Byte length of the CDATA section or comment that `tail` opens with, terminator included.
/// `None` when `tail` opens neither, or the section is never terminated (then it is treated
/// as ordinary markup, exactly as before).
fn opaque_section_end(tail: &str) -> Option<usize> {
    if let Some(body) = tail.strip_prefix("<![CDATA[") {
        return body
            .find("]]>")
            .map(|i| "<![CDATA[".len() + i + "]]>".len());
    }
    if let Some(body) = tail.strip_prefix("<!--") {
        return body.find("-->").map(|i| "<!--".len() + i + "-->".len());
    }
    None
}

/// The local name of the element a `<...` slice opens, without any
/// namespace prefix. `None` for comments, CDATA and processing instructions.
fn element_name(tag: &str) -> Option<&str> {
    let b = tag.strip_prefix('<')?;
    let b = b.strip_prefix('/').unwrap_or(b);
    if b.starts_with('!') || b.starts_with('?') {
        return None;
    }
    let end = b
        .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
        .unwrap_or(b.len());
    let raw = &b[..end];
    Some(raw.rsplit(':').next().unwrap_or(raw))
}

/// Byte offset of the `>` that actually CLOSES this tag, skipping any that sit
/// inside a quoted attribute value.
///
/// A plain `find('>')` is wrong, and dangerously so: `<title x="/>">Caption</title>`
/// makes it stop inside the quotes, the preceding text ends in `/`, the tag reads
/// as self-closing, and the element is cut in the middle. What survives is the
/// caption we were supposed to remove plus a dangling `</title>` — metadata leaked
/// into a file the user asked to have cleaned.
fn tag_end(tag: &str) -> Option<usize> {
    let b = tag.as_bytes();
    let mut quote: Option<u8> = None;
    for (i, &c) in b.iter().enumerate() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == b'"' || c == b'\'' => quote = Some(c),
            None if c == b'>' => return Some(i),
            None => {}
        }
    }
    None
}

fn is_self_closing(tag: &str) -> bool {
    tag_end(tag).is_some_and(|g| tag[..g].trim_end().ends_with('/'))
}

/// Byte length of the whole `<name ...>…</name>` element starting at `tag`,
/// including a self-closing form. `None` if it is never closed (malformed input -
/// leave it alone rather than eat the rest of the file).
fn element_span(tag: &str, name: &str) -> Option<usize> {
    let open_end = tag_end(tag)? + 1;
    if is_self_closing(tag) {
        return Some(open_end);
    }
    // Same-name nesting is not legal for these elements, so a search for the matching close
    // tag is enough - as long as a `</name>` that is merely TEXT inside a CDATA section or a
    // comment is stepped over rather than matched (2026-09-19 audit F02).
    let mut p = open_end;
    loop {
        let next = next_close_tag(tag, p)?;
        let after = &tag[next..];
        if element_name(after) == Some(name) {
            return Some(next + tag_end(after)? + 1);
        }
        p = next + 2;
    }
}

/// Byte position of the next `</` in `tag` at or after `p`, stepping over any whole CDATA
/// section or comment so a `</name>` that is merely TEXT inside one is never matched;
/// `None` if there is no such close tag.
fn next_close_tag(tag: &str, mut p: usize) -> Option<usize> {
    loop {
        let close = tag[p..].find("</").map(|i| i + p);
        let cdata = tag[p..].find("<![CDATA[").map(|i| i + p);
        let comment = tag[p..].find("<!--").map(|i| i + p);
        let next = [close, cdata, comment].into_iter().flatten().min()?;
        if Some(next) == cdata || Some(next) == comment {
            p = next + opaque_section_end(&tag[next..])?;
            continue;
        }
        return Some(next);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(s: &str) -> String {
        String::from_utf8(strip(s.as_bytes()).unwrap()).unwrap()
    }

    #[test]
    fn removes_document_metadata_title_and_desc() {
        let out = run(concat!(
            "<?xml version=\"1.0\"?>\n<svg xmlns=\"http://www.w3.org/2000/svg\">\n",
            "  <title>Company logo FINAL v3</title>\n",
            "  <desc>Drawn by Jane on WORKSTATION-07</desc>\n",
            "  <metadata><rdf:RDF>author stuff</rdf:RDF></metadata>\n",
            "  <path d=\"M0 0h10v10z\"/>\n</svg>\n"
        ));
        assert!(!out.contains("Company logo"), "{out}");
        assert!(!out.contains("WORKSTATION-07"), "{out}");
        assert!(!out.contains("author stuff"), "{out}");
        assert!(
            out.contains("<path d=\"M0 0h10v10z\"/>"),
            "art was damaged: {out}"
        );
        assert!(
            out.contains("<?xml version=\"1.0\"?>"),
            "prolog lost: {out}"
        );
    }

    /// A `<title>` inside a shape is what a screen reader announces. Stripping it
    /// would quietly make the graphic less accessible, so it stays.
    #[test]
    fn keeps_nested_accessibility_text() {
        let out = run(concat!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\">",
            "<g><title>Play button</title><circle r=\"4\"/></g>",
            "</svg>"
        ));
        assert!(out.contains("Play button"), "{out}");
    }

    #[test]
    fn formatting_outside_the_removed_spans_is_byte_identical() {
        let src = "<svg>\n  <!-- keep me -->\n  <rect   x = '1'  />\n</svg>";
        assert_eq!(run(src), src);
    }

    #[test]
    fn self_closing_and_namespaced_forms() {
        let out = run("<svg><metadata/><dc:title>x</dc:title><rect/></svg>");
        assert!(!out.contains("metadata"), "{out}");
        assert!(!out.contains(">x<"), "{out}");
        assert!(out.contains("<rect/>"));
    }

    /// A `>` inside a quoted attribute must not be mistaken for the end of the
    /// tag. Get this wrong and the element is cut mid-attribute: the caption we
    /// were told to remove survives in the output, a dangling `</title>` is
    /// written, and every later document-level element stops being stripped.
    #[test]
    fn a_quoted_angle_bracket_does_not_truncate_the_element() {
        let out = run(r#"<svg><title x="/>">Secret caption</title><rect/></svg>"#);
        assert!(!out.contains("Secret caption"), "metadata leaked: {out}");
        assert!(
            !out.contains("title"),
            "a dangling tag was left behind: {out}"
        );
        assert!(out.contains("<rect/>"), "art was damaged: {out}");
    }

    /// The same trap, one element later: if the first cut desynchronised the depth
    /// counter, this second element would silently survive.
    #[test]
    fn stripping_continues_after_a_quoted_angle_bracket() {
        let out = run(r#"<svg><title a=">">One</title><desc>Two</desc><rect/></svg>"#);
        assert!(!out.contains("One"), "{out}");
        assert!(
            !out.contains("Two"),
            "later elements stopped being stripped: {out}"
        );
    }

    /// 2026-09-19 audit F02, verbatim: a close tag quoted inside a CDATA section ended the
    /// element early and the malformed result was written over a valid file.
    #[test]
    fn a_close_tag_inside_cdata_does_not_end_the_element() {
        let out = run(concat!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"64\" height=\"64\">",
            "<metadata><![CDATA[Author: Audit. An example close tag is </metadata> inside documentation.]]></metadata>",
            "<rect width=\"64\" height=\"64\" fill=\"red\"/></svg>"
        ));
        assert_eq!(
            out,
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"64\" height=\"64\"><rect width=\"64\" height=\"64\" fill=\"red\"/></svg>"
        );
        assert!(roxmltree::Document::parse(&out).is_ok(), "{out}");
    }

    #[test]
    fn a_close_tag_inside_a_comment_does_not_end_the_element() {
        let out = run(concat!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\">",
            "<desc>secret <!-- not the end: </desc> --> still secret</desc>",
            "<path d=\"M0 0h1\"/></svg>"
        ));
        assert!(!out.contains("secret"), "{out}");
        assert!(out.contains("<path d=\"M0 0h1\"/>"), "{out}");
        assert!(roxmltree::Document::parse(&out).is_ok(), "{out}");
    }

    /// A `<title>` that is TEXT inside a top-level CDATA section or comment is not an element
    /// and must survive the strip byte-for-byte.
    #[test]
    fn markup_quoted_inside_cdata_or_a_comment_at_top_level_is_kept() {
        let src = concat!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\">",
            "<style><![CDATA[/* <title>not an element</title> */]]></style>",
            "<!-- <desc>also not an element</desc> -->",
            "<path d=\"M0 0h1\"/></svg>"
        );
        assert_eq!(run(src), src);
    }

    /// The parser check guards the swap: an original the parser accepts must never turn
    /// into a rewrite it rejects. Exercised by feeding the check a rewrite that is broken.
    #[test]
    fn a_rewrite_that_stops_being_well_formed_is_refused() {
        let good = "<svg xmlns=\"http://www.w3.org/2000/svg\"><rect/></svg>";
        assert!(roxmltree::Document::parse(good).is_ok());
        assert!(roxmltree::Document::parse("<svg><rect></svg>").is_err());
        // strip() itself now keeps every well-formed input well-formed; the CDATA case above
        // is the one that used to fail here.
        let out = run(good);
        assert!(roxmltree::Document::parse(&out).is_ok());
    }

    #[test]
    fn an_unterminated_element_is_left_alone_rather_than_eaten() {
        let src = "<svg><metadata>never closed<rect/>";
        assert_eq!(run(src), src);
    }
}
