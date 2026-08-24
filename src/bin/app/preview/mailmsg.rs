//! Email preview: `.eml` (RFC 822/MIME) and Outlook `.msg` (OLE/CFB) rendered as markdown —
//! headers, the text body, and the attachment list — through the same pipeline the SQLite
//! view uses. Every competitor's space-bar previewer treats mail as a first-class format;
//! until this module we could not even show one as text.
//!
//! Hooked like the DB VIEW, not like `classify`: an early-return in `loader::load` (+ the
//! `load_static` twin), gated on the extension + the Text toggle. `to_markdown` returning
//! `None` IS the fall-through — a `.msg` that isn't OLE, or an `.eml` that doesn't look like
//! mail, lands on exactly the behaviour it had before this module existed.
//!
//! SECURITY: everything that came out of the file goes through [`docconv::md_cell`] before it
//! touches the markdown, for the same reason dbdoc does it — a subject line of
//! `[click me](https://evil)` must render as text, never as a live link. Remote content never
//! loads (the markdown pipeline fetches nothing; mail bodies are reduced to text first), and
//! attachments are LISTED, never extracted, opened, or written anywhere.

use super::docconv::md_cell;
use sagethumbs2k_core::ole;

/// Extensions this module answers for.
pub(super) fn is_mail_ext(ext: &str) -> bool {
    matches!(ext, "eml" | "msg")
}

/// Reading cap: a mail file bigger than this previews its first 16 MB, which is every real
/// message (huge ones are huge because of base64 attachments we neither decode nor show).
const MAX_IO_BYTES: u64 = 16 * 1024 * 1024;

/// Body lines kept in the rendered preview. Enough for any human-written mail; a truncation
/// note says when a generated monster got cut.
const MAX_BODY_LINES: usize = 2_000;

/// The mail at `path` as markdown, or `None` when it isn't mail this module understands
/// (the caller then falls through to the text/info-card path).
pub(super) fn to_markdown(path: &str) -> Option<String> {
    let bytes = read_capped(path)?;
    if ole::looks_like_ole(&bytes) {
        msg_to_markdown(&bytes)
    } else {
        eml_to_markdown(&bytes)
    }
}

fn read_capped(path: &str) -> Option<Vec<u8>> {
    use std::io::Read;
    let f = std::fs::File::open(path).ok()?;
    let mut out = Vec::new();
    f.take(MAX_IO_BYTES).read_to_end(&mut out).ok()?;
    (!out.is_empty()).then_some(out)
}

// ---------------------------------------------------------------------------------------
// Shared assembly
// ---------------------------------------------------------------------------------------

struct Mail {
    subject: String,
    from: String,
    to: String,
    cc: String,
    date: String,
    body: String,
    attachments: Vec<String>,
}

/// Render the parsed mail as the markdown document the preview shows. Header lines are
/// bold-labelled; the body keeps its own line structure via two-space hard breaks (markdown
/// would otherwise reflow an email's careful line breaks into one paragraph soup).
fn assemble(m: &Mail) -> String {
    let t = |k: &str| crate::win::t(k);
    let mut out = String::new();
    let subject = if m.subject.trim().is_empty() {
        t("mail_no_subject").to_string()
    } else {
        m.subject.clone()
    };
    out.push_str(&format!("# {}\n\n", md_cell(&subject)));
    for (key, val) in [
        ("mail_from", &m.from),
        ("mail_to", &m.to),
        ("mail_cc", &m.cc),
        ("mail_date", &m.date),
    ] {
        if !val.trim().is_empty() {
            out.push_str(&format!("**{}:** {}  \n", md_cell(t(key)), md_cell(val)));
        }
    }
    if !m.attachments.is_empty() {
        out.push_str(&format!(
            "**{} ({}):** {}  \n",
            md_cell(t("mail_attachments")),
            m.attachments.len(),
            m.attachments
                .iter()
                .map(|a| format!("`{}`", md_cell(a)))
                .collect::<Vec<_>>()
                .join(" · ")
        ));
    }
    out.push_str("\n---\n\n");
    for (n, line) in m.body.lines().enumerate() {
        if n >= MAX_BODY_LINES {
            out.push_str(&format!(
                "\n*… {}*\n",
                md_cell(crate::win::t("mail_truncated"))
            ));
            break;
        }
        // Escaped body text + a two-space HARD break, so the mail's own line structure
        // survives markdown's paragraph reflow. A blank source line stays a blank line.
        out.push_str(&md_cell(line));
        out.push_str("  \n");
    }
    out
}

// ---------------------------------------------------------------------------------------
// .eml — RFC 822 + the slice of MIME that covers real mail
// ---------------------------------------------------------------------------------------

fn eml_to_markdown(bytes: &[u8]) -> Option<String> {
    let (headers, body_off) = split_headers(bytes)?;
    // "Looks like mail" gate: a From/To/Subject/Date/Received header must exist, or this is
    // some other colon-delimited text file (YAML, HTTP logs) that should keep its text view.
    let has_mail_header = ["from", "to", "subject", "date", "received", "message-id"]
        .iter()
        .any(|k| header(&headers, k).is_some());
    if !has_mail_header {
        return None;
    }
    let ctype = header(&headers, "content-type").unwrap_or_default();
    let cte = header(&headers, "content-transfer-encoding").unwrap_or_default();
    let mut attachments = Vec::new();
    let body = mime_body(&bytes[body_off..], &ctype, &cte, &mut attachments, 0).unwrap_or_default();
    Some(assemble(&Mail {
        subject: decode_words(&header(&headers, "subject").unwrap_or_default()),
        from: decode_words(&header(&headers, "from").unwrap_or_default()),
        to: decode_words(&header(&headers, "to").unwrap_or_default()),
        cc: decode_words(&header(&headers, "cc").unwrap_or_default()),
        date: header(&headers, "date").unwrap_or_default(),
        body,
        attachments,
    }))
}

/// Split raw mail into (unfolded header lines, body offset). Headers end at the first blank
/// line; folded continuations (leading space/tab) are joined onto their header.
fn split_headers(bytes: &[u8]) -> Option<(Vec<String>, usize)> {
    let mut headers: Vec<String> = Vec::new();
    let mut i = 0usize;
    loop {
        let end = bytes[i..].iter().position(|&b| b == b'\n').map(|p| i + p)?;
        let line = &bytes[i..end];
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            return Some((headers, end + 1));
        }
        let text = latin1_or_utf8(line);
        if (text.starts_with(' ') || text.starts_with('\t')) && !headers.is_empty() {
            let last = headers.last_mut().unwrap();
            last.push(' ');
            last.push_str(text.trim_start());
        } else {
            headers.push(text);
        }
        i = end + 1;
        if headers.len() > 500 {
            return None; // not a mail header block anyone wrote
        }
    }
}

/// The value of header `name` (case-insensitive), joined continuations already applied.
fn header(headers: &[String], name: &str) -> Option<String> {
    headers.iter().find_map(|h| {
        let (k, v) = h.split_once(':')?;
        k.trim()
            .eq_ignore_ascii_case(name)
            .then(|| v.trim().to_string())
    })
}

/// Resolve a (possibly multipart) body to displayable TEXT, collecting attachment filenames.
/// Prefers `text/plain`; falls back to tag-stripped `text/html`. Depth-capped: real mail
/// nests `multipart/mixed(multipart/alternative(text, html), attachment)` — two levels; four
/// is hostile.
fn mime_body(
    raw: &[u8],
    ctype: &str,
    cte: &str,
    attachments: &mut Vec<String>,
    depth: usize,
) -> Option<String> {
    if depth > 4 {
        return None;
    }
    let lower = ctype.to_ascii_lowercase();
    if lower.starts_with("multipart/") {
        let boundary = ctype_param(ctype, "boundary")?;
        let mut plain: Option<String> = None;
        let mut html: Option<String> = None;
        for part in split_multipart(raw, &boundary) {
            let (ph, poff) = match split_headers(part) {
                Some(x) => x,
                None => continue,
            };
            let pct = header(&ph, "content-type").unwrap_or_else(|| "text/plain".into());
            let pcte = header(&ph, "content-transfer-encoding").unwrap_or_default();
            // An attachment is anything with a filename, whatever its type claims.
            if let Some(name) = part_filename(&ph) {
                attachments.push(name);
                continue;
            }
            let pl = pct.to_ascii_lowercase();
            if pl.starts_with("multipart/") {
                if let Some(t) = mime_body(&part[poff..], &pct, &pcte, attachments, depth + 1) {
                    plain.get_or_insert(t);
                }
            } else if pl.starts_with("text/plain") && plain.is_none() {
                plain = Some(decode_text_part(&part[poff..], &pct, &pcte));
            } else if pl.starts_with("text/html") && html.is_none() {
                html = Some(strip_html(&decode_text_part(&part[poff..], &pct, &pcte)));
            }
        }
        plain.or(html)
    } else if lower.starts_with("text/html") {
        Some(strip_html(&decode_text_part(raw, ctype, cte)))
    } else {
        // text/plain, or no content-type at all (plain RFC-822): show as text.
        Some(decode_text_part(raw, ctype, cte))
    }
}

/// A `Content-Type`/`Content-Disposition` parameter value, quoted or bare.
fn ctype_param(value: &str, param: &str) -> Option<String> {
    let lower = value.to_ascii_lowercase();
    let needle = format!("{param}=");
    let at = lower.find(&needle)? + needle.len();
    let rest = &value[at..];
    Some(if let Some(stripped) = rest.strip_prefix('"') {
        stripped.split('"').next().unwrap_or_default().to_string()
    } else {
        rest.split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_string()
    })
}

/// The part's attachment filename, from Content-Disposition or Content-Type `name=`.
fn part_filename(headers: &[String]) -> Option<String> {
    let disp = header(headers, "content-disposition").unwrap_or_default();
    if let Some(f) = ctype_param(&disp, "filename") {
        if !f.is_empty() {
            return Some(decode_words(&f));
        }
    }
    if disp.to_ascii_lowercase().starts_with("attachment") {
        return Some(crate::win::t("mail_unnamed_attachment").to_string());
    }
    let ct = header(headers, "content-type").unwrap_or_default();
    ctype_param(&ct, "name")
        .filter(|n| !n.is_empty())
        .map(|n| decode_words(&n))
}

/// Slice a multipart body into its parts (the bytes between boundary delimiters).
fn split_multipart<'a>(raw: &'a [u8], boundary: &str) -> Vec<&'a [u8]> {
    let delim = format!("--{boundary}");
    let db = delim.as_bytes();
    let mut parts = Vec::new();
    let mut starts: Vec<usize> = Vec::new();
    let mut i = 0usize;
    while let Some(p) = find(&raw[i..], db) {
        starts.push(i + p);
        i += p + db.len();
        if parts.len() > 64 {
            break;
        }
        parts.push(()); // count only; slices are built below
    }
    let mut out = Vec::new();
    for w in starts.windows(2) {
        let from = w[0] + db.len();
        // Skip the delimiter's own line ending.
        let from = from
            + raw[from..w[1]]
                .iter()
                .take_while(|&&b| b == b'\r' || b == b'\n' || b == b'-')
                .count()
                .min(2);
        if from < w[1] {
            out.push(&raw[from..w[1]]);
        }
    }
    out
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// A leaf text part: undo the transfer encoding, then the charset.
fn decode_text_part(raw: &[u8], ctype: &str, cte: &str) -> String {
    let decoded: Vec<u8> = match cte.trim().to_ascii_lowercase().as_str() {
        "base64" => b64_decode(raw),
        "quoted-printable" => qp_decode(raw, false),
        _ => raw.to_vec(),
    };
    let charset = ctype_param(ctype, "charset").unwrap_or_default();
    decode_charset(&decoded, &charset)
}

/// UTF-8 when it is, else Windows-1252-ish (the realistic legacy default for mail).
fn decode_charset(bytes: &[u8], charset: &str) -> String {
    let cs = charset.to_ascii_lowercase();
    if cs.contains("utf-8") || cs.contains("utf8") || cs.is_empty() || cs.contains("ascii") {
        match core::str::from_utf8(bytes) {
            Ok(s) => s.to_string(),
            Err(_) => cp1252(bytes),
        }
    } else {
        // iso-8859-1, windows-1252, and the long tail we don't table: 1252 is the superset
        // that renders the west-European legacy mail this path actually sees.
        cp1252(bytes)
    }
}

/// Windows-1252: latin-1 plus the 0x80–0x9F printables (the smart quotes and dashes real
/// legacy mail is full of).
fn cp1252(bytes: &[u8]) -> String {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž',
        '\u{8f}', '\u{90}', '\u{2018}', '\u{2019}', '“', '”', '•', '–', '—', '˜', '™', 'š', '›',
        'œ', '\u{9d}', 'ž', 'Ÿ',
    ];
    bytes
        .iter()
        .map(|&b| match b {
            0x80..=0x9F => HIGH[(b - 0x80) as usize],
            _ => b as char,
        })
        .collect()
}

fn latin1_or_utf8(bytes: &[u8]) -> String {
    core::str::from_utf8(bytes)
        .map(str::to_string)
        .unwrap_or_else(|_| cp1252(bytes))
}

/// RFC 2047 encoded-words in headers: `=?charset?B|Q?data?=`, possibly several.
fn decode_words(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find("=?") {
        let Some(tail) = rest[start + 2..].find("?=") else {
            break;
        };
        let word = &rest[start + 2..start + 2 + tail];
        let mut it = word.splitn(3, '?');
        let (Some(cs), Some(enc), Some(data)) = (it.next(), it.next(), it.next()) else {
            break;
        };
        out.push_str(&rest[..start]);
        let bytes = match enc {
            "B" | "b" => b64_decode(data.as_bytes()),
            "Q" | "q" => qp_decode(data.as_bytes(), true),
            _ => data.as_bytes().to_vec(),
        };
        out.push_str(&decode_charset(&bytes, cs));
        rest = &rest[start + 2 + tail + 2..];
        // RFC 2047: whitespace BETWEEN two encoded words is not displayed.
        if rest.trim_start().starts_with("=?") {
            rest = rest.trim_start();
        }
    }
    out.push_str(rest);
    out
}

/// Quoted-printable. `header` mode additionally maps `_` to space (the Q encoding).
fn qp_decode(raw: &[u8], header: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        match raw[i] {
            b'=' if i + 2 < raw.len() && raw[i + 1] == b'\r' && raw[i + 2] == b'\n' => i += 3,
            b'=' if i + 1 < raw.len() && raw[i + 1] == b'\n' => i += 2, // soft break
            b'=' if i + 2 < raw.len() => {
                let hex = |b: u8| (b as char).to_digit(16);
                match (hex(raw[i + 1]), hex(raw[i + 2])) {
                    (Some(h), Some(l)) => {
                        out.push(((h << 4) | l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(raw[i]);
                        i += 1;
                    }
                }
            }
            b'_' if header => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    out
}

/// Standard base64, whitespace-tolerant (mail wraps it at 76 columns).
fn b64_decode(raw: &[u8]) -> Vec<u8> {
    fn val(b: u8) -> Option<u32> {
        match b {
            b'A'..=b'Z' => Some((b - b'A') as u32),
            b'a'..=b'z' => Some((b - b'a' + 26) as u32),
            b'0'..=b'9' => Some((b - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(raw.len() / 4 * 3);
    let (mut acc, mut n) = (0u32, 0u32);
    for &b in raw {
        if b == b'=' {
            break;
        }
        let Some(v) = val(b) else { continue }; // skips CRLF and stray whitespace
        acc = (acc << 6) | v;
        n += 6;
        if n >= 8 {
            n -= 8;
            out.push((acc >> n) as u8);
        }
    }
    out
}

/// HTML → readable text: drop script/style wholesale, break on the block tags, strip the
/// rest, decode the entities a mail body realistically uses. NOT a renderer and NOT trying
/// to be one — the locked-down WebView2 path exists for local HTML files the user opted
/// into; a mail body is remote-authored and gets flattened to text, always.
fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    // Drop container elements whose CONTENT must vanish with the tags.
    let lower_all = html.to_ascii_lowercase();
    let mut cleaned = String::with_capacity(html.len());
    let mut pos = 0usize;
    // Single pass: copy, but skip <script>/<style>/<head> ... </...> spans.
    while pos < html.len() {
        let rel = lower_all[pos..].find('<');
        let Some(r) = rel else {
            cleaned.push_str(&html[pos..]);
            break;
        };
        cleaned.push_str(&html[pos..pos + r]);
        let tag_start = pos + r;
        let lower_tail = &lower_all[tag_start..];
        let skip = ["script", "style", "head"].iter().find_map(|t| {
            lower_tail
                .starts_with(&format!("<{t}"))
                .then(|| format!("</{t}>"))
        });
        if let Some(close) = skip {
            match lower_all[tag_start..].find(&close) {
                Some(c) => {
                    pos = tag_start + c + close.len();
                    continue;
                }
                None => break, // unterminated: drop the rest
            }
        }
        match lower_all[tag_start..].find('>') {
            Some(e) => {
                let tag = &lower_all[tag_start + 1..tag_start + e];
                let name: String = tag
                    .trim_start_matches('/')
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .collect();
                if matches!(
                    name.as_str(),
                    "p" | "br" | "div" | "tr" | "li" | "h1" | "h2" | "h3" | "h4" | "table"
                ) {
                    cleaned.push('\n');
                }
                pos = tag_start + e + 1;
            }
            None => break, // unterminated tag: done
        }
    }
    let rest: &str = &cleaned;
    // Entities.
    let mut chars = rest.char_indices();
    while let Some((i, c)) = chars.next() {
        if c != '&' {
            out.push(c);
            continue;
        }
        let tail = &rest[i..];
        let known = [
            ("&amp;", '&'),
            ("&lt;", '<'),
            ("&gt;", '>'),
            ("&quot;", '"'),
            ("&#39;", '\''),
            ("&apos;", '\''),
            ("&nbsp;", ' '),
        ]
        .iter()
        .find(|(e, _)| tail.starts_with(e));
        if let Some((e, ch)) = known {
            out.push(*ch);
            for _ in 0..e.chars().count() - 1 {
                let _ = chars.next();
            }
        } else {
            out.push('&');
        }
    }
    // Collapse the blank-line stutter block tags leave behind.
    let mut collapsed = String::with_capacity(out.len());
    let mut blanks = 0;
    for line in out.lines() {
        if line.trim().is_empty() {
            blanks += 1;
            if blanks > 1 {
                continue;
            }
        } else {
            blanks = 0;
        }
        collapsed.push_str(line.trim_end());
        collapsed.push('\n');
    }
    collapsed.trim().to_string()
}

// ---------------------------------------------------------------------------------------
// .msg — Outlook's OLE container (MS-OXMSG)
// ---------------------------------------------------------------------------------------

/// MSG property streams are named `__substg1.0_XXXXTTTT`: XXXX = property id, TTTT = type
/// (001F = UTF-16LE string, 001E = 8-bit string).
fn msg_string(bytes: &[u8], prop: &str) -> Option<String> {
    if let Some(s) = ole::read_stream(bytes, &format!("__substg1.0_{prop}001F")) {
        let utf16: Vec<u16> = s
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return Some(String::from_utf16_lossy(&utf16));
    }
    ole::read_stream(bytes, &format!("__substg1.0_{prop}001E")).map(|s| cp1252(s.as_slice()))
}

fn msg_to_markdown(bytes: &[u8]) -> Option<String> {
    // Subject is the "is this actually a message" gate: a random OLE file (a legacy .doc
    // renamed .msg) has none of the MAPI streams, so every lookup misses and we fall through.
    let subject = msg_string(bytes, "0037");
    let body = msg_string(bytes, "1000");
    subject.as_ref().or(body.as_ref())?;

    // Attachment long filenames: one `__substg1.0_3707001F` per attachment storage (the flat
    // directory scan returns them all); short names (3704) fill in for old writers.
    let mut attachments: Vec<String> = ole::read_streams(bytes, "__substg1.0_3707001F", 64)
        .unwrap_or_default()
        .iter()
        .map(|s| {
            let utf16: Vec<u16> = s
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16_lossy(&utf16)
        })
        .collect();
    if attachments.is_empty() {
        attachments = ole::read_streams(bytes, "__substg1.0_3704001F", 64)
            .unwrap_or_default()
            .iter()
            .map(|s| {
                let utf16: Vec<u16> = s
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                String::from_utf16_lossy(&utf16)
            })
            .collect();
    }

    Some(assemble(&Mail {
        subject: subject.unwrap_or_default(),
        from: match (msg_string(bytes, "0C1A"), msg_string(bytes, "5D01")) {
            (Some(name), Some(addr)) if !addr.trim().is_empty() => format!("{name} <{addr}>"),
            (Some(name), _) => name,
            (None, Some(addr)) => addr,
            (None, None) => String::new(),
        },
        to: msg_string(bytes, "0E04").unwrap_or_default(),
        cc: msg_string(bytes, "0E03").unwrap_or_default(),
        date: msg_submit_time(bytes).unwrap_or_default(),
        body: body.unwrap_or_default(),
        attachments,
    }))
}

/// PR_CLIENT_SUBMIT_TIME (0039, type 0040 = FILETIME) from the fixed-record properties
/// stream: a 32-byte header, then 16-byte records of tag(4) flags(4) value(8).
fn msg_submit_time(bytes: &[u8]) -> Option<String> {
    let props = ole::read_stream(bytes, "__properties_version1.0")?;
    for rec in props.get(32..)?.chunks_exact(16) {
        let tag = u32::from_le_bytes([rec[0], rec[1], rec[2], rec[3]]);
        if tag == 0x0039_0040 {
            let ft = u64::from_le_bytes(rec[8..16].try_into().ok()?);
            return filetime_to_utc(ft);
        }
    }
    None
}

/// FILETIME (100 ns ticks since 1601-01-01 UTC) → "YYYY-MM-DD HH:MM UTC". Hand-rolled
/// civil-from-days (Howard Hinnant's algorithm) because nothing else in the tree needs a
/// date library and this is 15 lines.
fn filetime_to_utc(ft: u64) -> Option<String> {
    let secs = ft / 10_000_000;
    // Seconds between 1601-01-01 and 1970-01-01.
    let unix = i64::try_from(secs).ok()? - 11_644_473_600;
    if !(0..=253_402_300_799).contains(&unix) {
        return None; // outside 1970..9999: a garbage FILETIME, not a date worth printing
    }
    let days = unix.div_euclid(86_400);
    let secs_of_day = unix.rem_euclid(86_400);
    let (h, min) = (secs_of_day / 3600, (secs_of_day % 3600) / 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    Some(format!("{y:04}-{m:02}-{d:02} {h:02}:{min:02} UTC"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The plainest possible mail must produce the headers and the body, all inert.
    #[test]
    fn plain_eml_renders_headers_and_body() {
        let eml = b"From: Ada <ada@example.com>\r\n\
                    To: Alan <alan@example.com>\r\n\
                    Subject: Lunch?\r\n\
                    Date: Mon, 1 Jan 2024 12:00:00 +0000\r\n\
                    \r\n\
                    Are you free at noon?\r\nBring the notes.\r\n";
        let md = eml_to_markdown(eml).expect("plain mail parses");
        assert!(md.contains("# Lunch?"));
        assert!(md.contains("ada@example.com"));
        assert!(md.contains("Are you free at noon?"));
        assert!(md.contains("Bring the notes."));
    }

    /// A hostile subject must arrive ESCAPED — the whole reason everything routes through
    /// `md_cell`. A live link in a preview of an untrusted file is the bug class this
    /// pipeline was built to prevent (same finding as the CSV cells, 2026-07-13).
    #[test]
    fn hostile_subject_cannot_inject_markdown() {
        let eml = b"From: x@example.com\r\n\
                    Subject: [click me](https://evil.example) `code`\r\n\
                    \r\n\
                    body\r\n";
        let md = eml_to_markdown(eml).expect("parses");
        assert!(
            md.contains("\\[click me\\]"),
            "link brackets must be escaped: {md}"
        );
        assert!(md.contains("\\`code\\`"), "backticks must be escaped: {md}");
    }

    /// Multipart/alternative: the text/plain part is preferred, the HTML ignored, and the
    /// attachment is LISTED by name, never inlined.
    #[test]
    fn multipart_prefers_plain_and_lists_attachments() {
        let eml = b"From: x@example.com\r\n\
                    Subject: Report\r\n\
                    Content-Type: multipart/mixed; boundary=\"BB\"\r\n\
                    \r\n\
                    --BB\r\n\
                    Content-Type: text/plain; charset=utf-8\r\n\
                    \r\n\
                    The plain body.\r\n\
                    --BB\r\n\
                    Content-Type: text/html\r\n\
                    \r\n\
                    <p>The <b>html</b> body.</p>\r\n\
                    --BB\r\n\
                    Content-Type: application/pdf; name=\"q3.pdf\"\r\n\
                    Content-Disposition: attachment; filename=\"q3.pdf\"\r\n\
                    Content-Transfer-Encoding: base64\r\n\
                    \r\n\
                    JVBERi0=\r\n\
                    --BB--\r\n";
        let md = eml_to_markdown(eml).expect("parses");
        assert!(md.contains("The plain body."));
        assert!(!md.contains("html</b>"), "raw html must not leak: {md}");
        assert!(
            md.contains("q3.pdf"),
            "attachment name must be listed: {md}"
        );
    }

    /// Base64 + quoted-printable transfer encodings and RFC 2047 headers all decode.
    #[test]
    fn encodings_decode() {
        assert_eq!(b64_decode(b"aGVsbG8="), b"hello");
        assert_eq!(b64_decode(b"aGVs\r\nbG8="), b"hello", "wrapped base64");
        assert_eq!(qp_decode(b"caf=C3=A9", false), "café".as_bytes());
        assert_eq!(qp_decode(b"a=\r\nb", false), b"ab", "soft break");
        assert_eq!(
            decode_words("=?UTF-8?B?Z3LDvG7DqQ==?="),
            "grüné".trim_end_matches('é').to_string() + "é"
        );
        assert_eq!(decode_words("=?utf-8?Q?caf=C3=A9_x?="), "café x");
    }

    /// An HTML-only mail flattens to readable text with tags gone and entities decoded.
    #[test]
    fn html_only_mail_flattens_to_text() {
        let eml = b"From: x@example.com\r\n\
                    Subject: h\r\n\
                    Content-Type: text/html; charset=utf-8\r\n\
                    \r\n\
                    <html><head><style>p{color:red}</style></head>\
                    <body><p>Q3 &amp; Q4 are &lt;strong&gt;.</p>\
                    <script>alert(1)</script></body></html>\r\n";
        let md = eml_to_markdown(eml).expect("parses");
        assert!(md.contains("Q3 & Q4"), "{md}");
        assert!(!md.contains("alert(1)"), "script content must vanish: {md}");
        assert!(!md.contains("color:red"), "style content must vanish: {md}");
    }

    /// A YAML file (colon-delimited text) must NOT be mistaken for mail — `None` here is
    /// what keeps its normal text preview.
    #[test]
    fn yaml_is_not_mail() {
        let not_mail = b"name: build\r\non: push\r\n\r\njobs: {}\r\n";
        assert!(eml_to_markdown(not_mail).is_none());
    }

    /// A non-OLE, non-mail binary must fall through entirely.
    #[test]
    fn garbage_is_refused() {
        assert!(eml_to_markdown(&[0u8; 64]).is_none());
        assert!(msg_to_markdown(&[0u8; 64]).is_none());
    }

    /// FILETIME conversion: a known timestamp (2024-01-01 00:00 UTC) and garbage refusal.
    #[test]
    fn filetime_converts_and_refuses_garbage() {
        // 2024-01-01 00:00:00 UTC = 133_485_408_000_000_000 ticks since 1601.
        assert_eq!(
            filetime_to_utc(133_485_408_000_000_000).as_deref(),
            Some("2024-01-01 00:00 UTC")
        );
        assert_eq!(
            filetime_to_utc(0),
            None,
            "1601 is not a date worth printing"
        );
        assert_eq!(filetime_to_utc(u64::MAX), None);
    }

    /// cp1252's smart-quote range — the reason it exists over plain latin-1.
    #[test]
    fn cp1252_maps_the_smart_quotes() {
        assert_eq!(cp1252(&[0x93, 0x94, 0x96]), "“”–");
    }
}
