//! Bare URLs in text become links: the scanner and its trailing-punctuation rule.

use super::*;

/// Split `s` into plain-text runs and clickable link runs for any bare URLs it contains — the
/// GFM "extended autolink" behaviour (`https://…`, `http://…`, `www.…` in running prose become
/// links) that pulldown-cmark 0.12 does not do itself. Only called for plain text (never inside
/// code or an existing `[text](url)` link).
pub(in super::super) fn linkify_into(
    runs: &mut Vec<Run>,
    s: &str,
    bold: bool,
    italic: bool,
    strike: bool,
) {
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
pub(in super::super) fn url_at(s: &str, i: usize) -> Option<(usize, String)> {
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
pub(super) fn is_url_left_boundary(c: u8) -> bool {
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
pub(super) fn url_scheme_at(rest: &str) -> Option<(usize, bool)> {
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
pub(super) fn scan_url_bytes(rest: &str, scheme_len: usize) -> Option<usize> {
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
pub(super) fn trim_trailing_punct(raw: &[u8], scheme_len: usize) -> Option<usize> {
    let mut e = raw.len();
    // `opens` never needs to change: trimming only ever removes non-`(` bytes (plain
    // punctuation, or a `)` — never `(`), so the open-paren count over the shrinking `raw[..e]`
    // prefix is the same as over the whole slice for every `e` this loop ever reaches.
    let opens = raw.iter().filter(|&&x| x == b'(').count();
    let mut closes = raw.iter().filter(|&&x| x == b')').count();
    while e > scheme_len {
        match trim_step(raw[e - 1], opens, closes) {
            // plain punctuation: drop it, counts unchanged
            Some(false) => e -= 1,
            // unbalanced `)`: drop it and retire it from the running count
            Some(true) => {
                e -= 1;
                closes -= 1; // this `)` is no longer part of raw[..e]
            }
            // anything else is kept: trimming stops here
            None => break,
        }
    }
    if e <= scheme_len {
        None
    } else {
        Some(e)
    }
}

/// Decide how one trailing byte is consumed: `Some(true)` trims it and retires a `)`,
/// `Some(false)` trims plain punctuation, `None` keeps it (trimming must stop).
fn trim_step(c: u8, opens: usize, closes: usize) -> Option<bool> {
    if matches!(
        c,
        b'.' | b',' | b';' | b':' | b'!' | b'?' | b'\'' | b'"' | b'*' | b'_' | b'~'
    ) {
        Some(false)
    } else if c == b')' && closes > opens {
        Some(true)
    } else {
        None
    }
}
