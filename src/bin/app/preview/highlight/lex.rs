//! The per-line lexer: language specs and the single-pass tokenizer.
//!
//! Split out of `highlight.rs` 2026-07-31 (pure move). The keyword tables it reads live
//! next door in `keywords.rs`; the colour resolution and painting stayed in the parent.

use super::keywords::*;

/// Languages we specially lex. `Plain` = no colouring (falls back to today's uncoloured draw).
#[derive(Clone, Copy, PartialEq)]
pub(in crate::preview) enum Lang {
    Rust,
    Py,
    Js,
    Json,
    Yaml,
    Toml,
    C,
    Cs,
    Java,
    Go,
    Ruby,
    Php,
    Lua,
    Kotlin,
    Swift,
    Sh,
    Batch,
    PowerShell,
    Perl,
    Html,
    Css,
    Xml,
    Sql,
    Plain,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(in crate::preview) enum Tag {
    Plain,
    Comment,
    Str,
    Num,
    Keyword,
}

/// The canonical fenced-code info string for a language — used when a copied Markdown code
/// block is written back out as ``` fences, so it still highlights wherever it's pasted.
/// `None` for `Plain` (emit a bare fence). Lossy on purpose: the parse maps many tags onto one
/// `Lang` (`powershell`/`bash`/`zsh` all become `Sh`), and `Lang` is all we keep.
pub(in crate::preview) fn lang_tag(l: Lang) -> Option<&'static str> {
    Some(match l {
        Lang::Rust => "rust",
        Lang::Py => "python",
        Lang::Js => "js",
        Lang::Json => "json",
        Lang::Yaml => "yaml",
        Lang::Toml => "toml",
        Lang::C => "c",
        Lang::Cs => "csharp",
        Lang::Java => "java",
        Lang::Go => "go",
        Lang::Ruby => "ruby",
        Lang::Php => "php",
        Lang::Lua => "lua",
        Lang::Kotlin => "kotlin",
        Lang::Swift => "swift",
        Lang::Sh => "sh",
        Lang::Batch => "batch",
        Lang::PowerShell => "powershell",
        Lang::Perl => "perl",
        Lang::Html => "html",
        Lang::Css => "css",
        Lang::Xml => "xml",
        Lang::Sql => "sql",
        Lang::Plain => return None,
    })
}

/// Map a file extension (no dot) to a language.
pub(in crate::preview) fn lang_from_ext(ext: &str) -> Lang {
    match ext.to_ascii_lowercase().as_str() {
        "rs" => Lang::Rust,
        "py" | "pyw" | "pyi" => Lang::Py,
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" => Lang::Js,
        "json" | "jsonc" => Lang::Json,
        "yaml" | "yml" => Lang::Yaml,
        "toml" => Lang::Toml,
        "c" | "h" | "cpp" | "cxx" | "cc" | "hpp" | "hxx" => Lang::C,
        "cs" => Lang::Cs,
        "java" => Lang::Java,
        "go" => Lang::Go,
        "rb" | "rake" | "gemspec" => Lang::Ruby,
        "php" | "phtml" => Lang::Php,
        "lua" => Lang::Lua,
        "kt" | "kts" => Lang::Kotlin,
        "swift" => Lang::Swift,
        "sh" | "bash" | "zsh" => Lang::Sh,
        "bat" | "cmd" => Lang::Batch,
        "ps1" | "psm1" | "psd1" => Lang::PowerShell,
        "pl" | "pm" => Lang::Perl,
        "html" | "htm" | "xhtml" => Lang::Html,
        "css" | "scss" | "less" => Lang::Css,
        // svg is XML — reachable via the caption's "view source" toggle on a rendered SVG.
        "xml" | "svg" => Lang::Xml,
        "sql" => Lang::Sql,
        // ini/cfg files share TOML's shape (# / ; comments, key=value, quoted strings).
        "ini" | "cfg" | "conf" | "properties" | "editorconfig" | "gitconfig" => Lang::Toml,
        _ => Lang::Plain,
    }
}

/// Map a markdown fenced-code info-string tag (e.g. ```` ```rust ````) to a language. Also accepts
/// a bare extension as the tag.
pub(in crate::preview) fn lang_from_fence(tag: &str) -> Lang {
    match tag.to_ascii_lowercase().as_str() {
        "rust" | "rs" => Lang::Rust,
        "python" | "py" => Lang::Py,
        "js" | "javascript" | "ts" | "typescript" | "jsx" | "tsx" | "node" => Lang::Js,
        "json" | "jsonc" => Lang::Json,
        "yaml" | "yml" => Lang::Yaml,
        "toml" => Lang::Toml,
        "c" | "cpp" | "c++" | "h" | "hpp" => Lang::C,
        "cs" | "csharp" | "c#" => Lang::Cs,
        "java" => Lang::Java,
        "go" | "golang" => Lang::Go,
        "ruby" | "rb" => Lang::Ruby,
        "php" => Lang::Php,
        "lua" => Lang::Lua,
        "kotlin" | "kt" => Lang::Kotlin,
        "swift" => Lang::Swift,
        "sh" | "bash" | "shell" | "zsh" | "console" => Lang::Sh,
        "bat" | "cmd" | "batch" => Lang::Batch,
        "ps1" | "powershell" | "pwsh" => Lang::PowerShell,
        "perl" | "pl" => Lang::Perl,
        "html" | "htm" => Lang::Html,
        "css" | "scss" | "less" => Lang::Css,
        "xml" | "svg" => Lang::Xml,
        "sql" => Lang::Sql,
        other => lang_from_ext(other),
    }
}

/// Per-language lexer spec.
pub(in crate::preview) struct Spec {
    line_comment: &'static [&'static str],
    block: Option<(&'static str, &'static str)>,
    strings: &'static [u8], // quote chars
    keywords: &'static [&'static str],
    /// Colour a string immediately followed by `:` as an object KEY (`Tag::Keyword`) rather than
    /// a plain string. Only meaningful for the data-serialization languages where a leading key
    /// really is what precedes `:` — everywhere else (every C-family language included) a string
    /// followed by `:` is just as likely the middle branch of a ternary (`cond ? "yes" : "no"`).
    key_heuristic: bool,
}

/// The per-language tuple `spec()` builds from: (line comments, block comment, quotes, keywords).
pub(in crate::preview) type SpecParts = (
    &'static [&'static str],
    Option<(&'static str, &'static str)>,
    &'static [u8],
    &'static [&'static str],
);

pub(in crate::preview) fn spec(lang: Lang) -> Spec {
    let (lc, bl, st, kw): SpecParts = match lang {
        Lang::Rust => (&["//"], Some(("/*", "*/")), b"\"", RUST_KW),
        Lang::Py => (&["#"], None, b"\"'", PY_KW),
        Lang::Js => (&["//"], Some(("/*", "*/")), b"\"'`", JS_KW),
        Lang::Json => (&[], None, b"\"", &["true", "false", "null"]),
        Lang::Yaml => (&["#"], None, b"\"'", YAML_KW),
        Lang::Toml => (&["#"], None, b"\"'", &["true", "false"]),
        Lang::C => (&["//"], Some(("/*", "*/")), b"\"'", C_KW),
        Lang::Cs => (&["//"], Some(("/*", "*/")), b"\"'", CS_KW),
        Lang::Java => (&["//"], Some(("/*", "*/")), b"\"", JAVA_KW),
        Lang::Go => (&["//"], Some(("/*", "*/")), b"\"`", GO_KW),
        Lang::Ruby => (&["#"], None, b"\"'", RUBY_KW),
        Lang::Php => (&["//", "#"], Some(("/*", "*/")), b"\"'", PHP_KW),
        Lang::Lua => (&["--"], None, b"\"'", LUA_KW),
        Lang::Kotlin => (&["//"], Some(("/*", "*/")), b"\"", KOTLIN_KW),
        Lang::Swift => (&["//"], Some(("/*", "*/")), b"\"", SWIFT_KW),
        Lang::Sh => (&["#"], None, b"\"'", SH_KW),
        // Batch comments are conventionally `REM` (any case) or `::` at the start of a
        // statement; matched here as plain substrings like every other language's markers.
        Lang::Batch => (&["REM", "Rem", "rem", "::"], None, b"\"", BATCH_KW),
        // PowerShell's block comment is `<# ... #>`; its keyword set overlaps `SH_KW` heavily
        // (param/foreach/begin/process/end and the flow-control words) so it reuses that table
        // rather than duplicating it.
        Lang::PowerShell => (&["#"], Some(("<#", "#>")), b"\"'", SH_KW),
        // POD documentation blocks (`=pod` ... `=cut`) are Perl's nearest equivalent to a block
        // comment; real POD also accepts `=head1`/`=item`/etc, which this lexer does not chase.
        Lang::Perl => (&["#"], Some(("=pod", "=cut")), b"\"'", PERL_KW),
        Lang::Html | Lang::Xml => (&[], Some(("<!--", "-->")), b"\"'", &[]),
        Lang::Css => (&[], Some(("/*", "*/")), b"\"'", &[]),
        Lang::Sql => (&["--"], Some(("/*", "*/")), b"'", SQL_KW),
        Lang::Plain => (&[], None, b"", &[]),
    };
    Spec {
        line_comment: lc,
        block: bl,
        strings: st,
        keywords: kw,
        key_heuristic: matches!(lang, Lang::Json | Lang::Yaml | Lang::Toml),
    }
}

/// UTF-8 byte length of the char starting with lead byte `b`.
pub(in crate::preview) fn utf8_len(b: u8) -> usize {
    if b >= 0xF8 {
        // 0xF8-0xFF are not valid UTF-8 lead bytes (the encoding tops out at 0xF4, for
        // U+10FFFF) — step one byte instead of overrunning into whatever follows.
        1
    } else if b >= 0xF0 {
        4
    } else if b >= 0xE0 {
        3
    } else if b >= 0xC0 {
        2
    } else {
        1
    }
}

pub(in crate::preview) fn find_from(hay: &str, from: usize, needle: &str) -> Option<usize> {
    hay.get(from..)
        .and_then(|s| s.find(needle))
        .map(|p| p + from)
}

/// Outcome of [`try_block_comment_open`]: whether `i` opens a block comment, and if so whether
/// its close is on this same line.
enum BlockOpen {
    NoMatch,
    /// Closed on this line; scanning resumes at the returned index.
    Closed(usize),
    /// No close on this line, the comment (and `in_block`) carries into the next line.
    ToEol,
}

/// Continuation of a block comment carried over from a previous line (`*in_block` was already
/// true). Pushes the `Comment` run either up to the close (returns `Some(end)`) or to the end of
/// the line (returns `None`, meaning `in_block` stays set for the caller).
fn continue_block_comment<'a>(
    line: &'a str,
    sp: &Spec,
    i: usize,
    out: &mut Vec<(Tag, &'a str)>,
) -> Option<usize> {
    if let Some((_, close)) = sp.block {
        if let Some(pos) = find_from(line, i, close) {
            let end = pos + close.len();
            out.push((Tag::Comment, &line[i..end]));
            return Some(end);
        }
    }
    out.push((Tag::Comment, &line[i..]));
    None
}

/// A line comment starting at `i` (one of `sp.line_comment`), if any: flushes the pending Plain
/// run up to `i` and pushes a `Comment` run for the rest of the line.
fn try_line_comment<'a>(
    line: &'a str,
    sp: &Spec,
    i: usize,
    seg: usize,
    out: &mut Vec<(Tag, &'a str)>,
) -> bool {
    if !sp.line_comment.iter().any(|c| line[i..].starts_with(*c)) {
        return false;
    }
    if i > seg {
        out.push((Tag::Plain, &line[seg..i]));
    }
    out.push((Tag::Comment, &line[i..]));
    true
}

/// A block comment opening at `i`, if any: flushes the pending Plain run, then either finds the
/// close on this same line or consumes the rest of the line. See [`BlockOpen`].
fn try_block_comment_open<'a>(
    line: &'a str,
    sp: &Spec,
    i: usize,
    seg: usize,
    out: &mut Vec<(Tag, &'a str)>,
) -> BlockOpen {
    let Some((open, close)) = sp.block else {
        return BlockOpen::NoMatch;
    };
    if !line[i..].starts_with(open) {
        return BlockOpen::NoMatch;
    }
    if i > seg {
        out.push((Tag::Plain, &line[seg..i]));
    }
    if let Some(pos) = find_from(line, i + open.len(), close) {
        let end = pos + close.len();
        out.push((Tag::Comment, &line[i..end]));
        BlockOpen::Closed(end)
    } else {
        out.push((Tag::Comment, &line[i..]));
        BlockOpen::ToEol
    }
}

/// A string literal opening at `i` (a quote char in `sp.strings`), if any: consumes to the
/// closing quote (or end of line) and classifies it as `Keyword` when immediately followed by
/// `:` (an object KEY / property, colours like QuickLook's blue property names) or `Str`
/// otherwise. Returns the scan position just past the token.
fn try_string_literal<'a>(
    line: &'a str,
    sp: &Spec,
    i: usize,
    n: usize,
    seg: usize,
    out: &mut Vec<(Tag, &'a str)>,
) -> Option<usize> {
    let b = line.as_bytes();
    let ch = b[i];
    if !sp.strings.contains(&ch) {
        return None;
    }
    if i > seg {
        out.push((Tag::Plain, &line[seg..i]));
    }
    let start = i;
    let mut j = i + 1;
    while j < n {
        if b[j] == b'\\' {
            j += 2;
            continue;
        }
        if b[j] == ch {
            j += 1;
            break;
        }
        j += 1;
    }
    let end = j.min(n);
    // Only Json/Yaml/Toml colour a string-then-colon as an object key — everywhere else
    // (every C-family language included) that shape is just as likely a ternary's middle
    // branch (`cond ? "yes" : "no"`), which used to mis-colour as a key on every such line.
    let is_key = sp.key_heuristic
        && line
            .get(end..)
            .is_some_and(|r| r.trim_start().starts_with(':'));
    out.push((
        if is_key { Tag::Keyword } else { Tag::Str },
        &line[start..end],
    ));
    Some(end)
}

/// A number literal starting at `i`, if any: consumes the run of alphanumeric/`.`/`_` bytes
/// (loose on purpose, good enough to colour `0x1F`, `1_000`, `3.14e10` as one run without a
/// real numeric grammar). Returns the scan position just past the token.
fn try_number_literal<'a>(
    line: &'a str,
    i: usize,
    n: usize,
    seg: usize,
    out: &mut Vec<(Tag, &'a str)>,
) -> Option<usize> {
    let b = line.as_bytes();
    if !b[i].is_ascii_digit() {
        return None;
    }
    if i > seg {
        out.push((Tag::Plain, &line[seg..i]));
    }
    let start = i;
    let mut j = i;
    while j < n && (b[j].is_ascii_alphanumeric() || b[j] == b'.' || b[j] == b'_') {
        j += 1;
    }
    out.push((Tag::Num, &line[start..j]));
    Some(j)
}

/// An identifier starting at `i`, if any: scans the run of ident chars regardless, and only when
/// it's a recognized keyword flushes the pending Plain run and pushes a `Keyword` token (a
/// non-keyword identifier stays folded into the surrounding Plain run, few runs per line).
/// Returns `(new scan position, new pending-Plain-run start)`.
fn try_identifier<'a>(
    line: &'a str,
    sp: &Spec,
    i: usize,
    n: usize,
    seg: usize,
    out: &mut Vec<(Tag, &'a str)>,
) -> Option<(usize, usize)> {
    let b = line.as_bytes();
    let ch = b[i];
    if !(ch.is_ascii_alphabetic() || ch == b'_') {
        return None;
    }
    let start = i;
    let mut j = i;
    while j < n && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
        j += 1;
    }
    let word = &line[start..j];
    if sp.keywords.contains(&word) {
        if start > seg {
            out.push((Tag::Plain, &line[seg..start]));
        }
        out.push((Tag::Keyword, word));
        Some((j, j))
    } else {
        Some((j, seg))
    }
}

/// Tokenize one line into `(tag, slice)` runs. `in_block` carries block-comment state across
/// lines. Non-keyword identifiers + punctuation stay in `Plain` runs (few runs per line).
///
/// A thin dispatch loop over the per-token-kind helpers above, tried in order at each scan
/// position; the first one that matches advances `i`/`seg` and the loop continues.
pub(in crate::preview) fn tokenize<'a>(
    line: &'a str,
    sp: &Spec,
    in_block: &mut bool,
) -> Vec<(Tag, &'a str)> {
    let b = line.as_bytes();
    let n = b.len();
    let mut out: Vec<(Tag, &'a str)> = Vec::new();
    let mut i = 0usize;
    let mut seg = 0usize; // start of the pending Plain segment

    while i < n {
        // carried-over block comment
        if *in_block {
            match continue_block_comment(line, sp, i, &mut out) {
                Some(end) => {
                    i = end;
                    seg = end;
                    *in_block = false;
                }
                None => {
                    i = n;
                    seg = n;
                }
            }
            continue;
        }
        // line comment -> rest of line
        if try_line_comment(line, sp, i, seg, &mut out) {
            i = n;
            seg = n;
            continue;
        }
        // block comment open
        match try_block_comment_open(line, sp, i, seg, &mut out) {
            BlockOpen::Closed(end) => {
                i = end;
                seg = end;
                continue;
            }
            BlockOpen::ToEol => {
                i = n;
                seg = n;
                *in_block = true;
                continue;
            }
            BlockOpen::NoMatch => {}
        }
        // string literal
        if let Some(end) = try_string_literal(line, sp, i, n, seg, &mut out) {
            i = end;
            seg = end;
            continue;
        }
        // number literal
        if let Some(end) = try_number_literal(line, i, n, seg, &mut out) {
            i = end;
            seg = end;
            continue;
        }
        // identifier -> keyword lookup (non-keywords stay in the plain segment)
        if let Some((end, new_seg)) = try_identifier(line, sp, i, n, seg, &mut out) {
            i = end;
            seg = new_seg;
            continue;
        }
        // plain char (advance by full UTF-8 char so slices never split a codepoint)
        let ch = b[i];
        i += if ch < 0x80 { 1 } else { utf8_len(ch) };
    }
    if n > seg {
        out.push((Tag::Plain, &line[seg..n]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_len_steps_one_on_an_invalid_lead_byte() {
        // 0xF8-0xFF cannot start a valid UTF-8 char; the old `>= 0xF0` branch reported 4 for
        // these and could walk the scan past whatever byte actually follows.
        assert_eq!(utf8_len(0xF8), 1);
        assert_eq!(utf8_len(0xFF), 1);
    }

    #[test]
    fn utf8_len_still_reports_four_for_a_real_four_byte_lead() {
        assert_eq!(utf8_len(0xF0), 4);
        assert_eq!(utf8_len(0xF4), 4);
    }

    #[test]
    fn ternary_string_is_not_coloured_as_a_key_in_a_c_family_language() {
        let sp = spec(Lang::Js);
        let mut in_block = false;
        let runs = tokenize(r#"cond ? "yes" : "no";"#, &sp, &mut in_block);
        assert!(
            runs.iter()
                .all(|(t, s)| !(*s == "\"yes\"" && *t == Tag::Keyword)),
            "a ternary branch must not be coloured as an object key: {runs:?}"
        );
    }

    #[test]
    fn json_still_colours_a_string_before_a_colon_as_a_key() {
        let sp = spec(Lang::Json);
        let mut in_block = false;
        let runs = tokenize(r#""name": "value""#, &sp, &mut in_block);
        assert!(
            runs.iter()
                .any(|(t, s)| *s == "\"name\"" && *t == Tag::Keyword),
            "Json must keep colouring the key before `:`: {runs:?}"
        );
    }

    #[test]
    fn batch_rem_and_double_colon_are_comments() {
        let sp = spec(Lang::Batch);
        let mut in_block = false;
        assert!(matches!(
            tokenize("REM this is a note", &sp, &mut in_block)[..],
            [(Tag::Comment, _)]
        ));
        assert!(matches!(
            tokenize(":: also a note", &sp, &mut in_block)[..],
            [(Tag::Comment, _)]
        ));
    }

    #[test]
    fn powershell_block_comment_spans_lines() {
        let sp = spec(Lang::PowerShell);
        let mut in_block = false;
        let runs = tokenize("<# starts here", &sp, &mut in_block);
        assert!(matches!(runs[..], [(Tag::Comment, _)]));
        assert!(in_block); // no closing #> on this line — carries to the next
        let runs2 = tokenize("still inside #> Write-Host 'done'", &sp, &mut in_block);
        assert!(!in_block); // closed partway through this line
        assert!(matches!(runs2[0], (Tag::Comment, _)));
    }

    #[test]
    fn perl_pod_block_and_keywords() {
        let sp = spec(Lang::Perl);
        let mut in_block = false;
        assert!(matches!(
            tokenize("=pod", &sp, &mut in_block)[..],
            [(Tag::Comment, _)]
        ));
        assert!(in_block);
        let runs = tokenize("my $x = shift;", &spec(Lang::Perl), &mut false);
        assert!(runs.iter().any(|(t, s)| *t == Tag::Keyword && *s == "my"));
    }

    #[test]
    fn lang_from_ext_separates_batch_and_powershell_from_sh() {
        assert!(matches!(lang_from_ext("bat"), Lang::Batch));
        assert!(matches!(lang_from_ext("cmd"), Lang::Batch));
        assert!(matches!(lang_from_ext("ps1"), Lang::PowerShell));
        assert!(matches!(lang_from_ext("sh"), Lang::Sh));
        assert!(matches!(lang_from_ext("pl"), Lang::Perl));
    }
}
