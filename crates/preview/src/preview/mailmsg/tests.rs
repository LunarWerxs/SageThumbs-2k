#![cfg(test)]

use super::*;
mod fuzz;
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

/// A message longer than [`MAX_IO_BYTES`] used to be silently cut with nothing on screen
/// to say so — `to_markdown` now surfaces `content::read_capped`'s truncated flag as a
/// trailing note, and a message that fits entirely gets none.
#[test]
fn to_markdown_notes_a_file_actually_truncated_by_the_read_cap() {
    let dir = std::env::temp_dir();
    let big = dir.join(format!("st2k_mailtest_big_{}.eml", std::process::id()));
    let small = dir.join(format!("st2k_mailtest_small_{}.eml", std::process::id()));

    let mut body = String::from("Subject: long\r\n\r\n");
    body.push_str(&"x".repeat(MAX_IO_BYTES as usize + 1024));
    std::fs::write(&big, body.as_bytes()).unwrap();
    std::fs::write(&small, b"Subject: short\r\n\r\nhi\r\n").unwrap();

    let note = md_cell(st2k_appkit::win::t("mail_truncated_file"));
    let big_md = to_markdown(big.to_str().unwrap()).expect("still parses as mail");
    let small_md = to_markdown(small.to_str().unwrap()).expect("still parses as mail");

    assert!(big_md.contains(&note), "truncated file must carry the note");
    assert!(
        !small_md.contains(&note),
        "a file that fits must not carry the note"
    );

    let _ = std::fs::remove_file(&big);
    let _ = std::fs::remove_file(&small);
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

// -----------------------------------------------------------------------------------
// `.msg` — a real compound file to test against, and the adversarial half
// -----------------------------------------------------------------------------------
//
// Everything above this line tests `.eml`. The `.msg` half shipped in 2.4.0 with none,
// which is the wrong way round: `.eml` is text this module mostly re-splits, while
// `.msg` is a binary container walked with file-supplied sector numbers, and it is the
// half a hostile file would be written in.
//
// The builder below writes a compound file the way Outlook does, rather than the way
// this reader happens to read: every string lives in the MINISTREAM (real MAPI strings
// are tens of bytes, far under the 4 KB cutoff), the body spans TWO mini sectors so the
// miniFAT is a chain and not a single hop, and both attachment entries carry the SAME
// stream name, which is the thing `read_streams` exists for.

const SECTOR: usize = 512;
const MINI: usize = 64;
const ENDOFCHAIN: u32 = 0xFFFF_FFFE;
const FREESECT: u32 = 0xFFFF_FFFF;

/// A minimal but genuine `.msg`: `streams` is `(directory name, UTF-16 contents)`, in
/// directory order. Duplicate names are allowed and are the point.
fn build_msg(streams: &[(&str, &str)]) -> Vec<u8> {
    // --- ministream: each stream starts on a mini-sector boundary, chained in the miniFAT.
    let mut ministream: Vec<u8> = Vec::new();
    let mut minifat: Vec<u32> = Vec::new();
    let mut placed: Vec<(u32, u64)> = Vec::new(); // (first mini sector, byte length)
    for (_, value) in streams {
        let bytes: Vec<u8> = value.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let first = (ministream.len() / MINI) as u32;
        let sectors = bytes.len().div_ceil(MINI).max(1);
        for i in 0..sectors {
            minifat.push(if i + 1 == sectors {
                ENDOFCHAIN
            } else {
                first + i as u32 + 1
            });
        }
        placed.push((first, bytes.len() as u64));
        ministream.extend_from_slice(&bytes);
        ministream.resize(ministream.len().div_ceil(MINI) * MINI, 0);
    }
    let ministream_len = ministream.len().max(SECTOR);
    ministream.resize(ministream_len.div_ceil(SECTOR) * SECTOR, 0);
    let ministream_sectors = ministream.len() / SECTOR;

    // --- directory: the root entry plus one entry per stream, four to a 512-byte sector.
    let dir_entries = streams.len() + 1;
    let dir_sectors = dir_entries.div_ceil(4);

    // --- sector map. 0 = FAT, then directory, then miniFAT, then the ministream.
    let first_dir = 1u32;
    let first_minifat = first_dir + dir_sectors as u32;
    let first_ministream = first_minifat + 1;
    let total_sectors = 1 + dir_sectors + 1 + ministream_sectors;
    assert!(
        total_sectors <= SECTOR / 4,
        "fixture outgrew its single FAT"
    );

    let mut fat = vec![FREESECT; SECTOR / 4];
    fat[0] = ENDOFCHAIN;
    for i in 0..dir_sectors {
        let s = first_dir as usize + i;
        fat[s] = if i + 1 == dir_sectors {
            ENDOFCHAIN
        } else {
            (s + 1) as u32
        };
    }
    fat[first_minifat as usize] = ENDOFCHAIN;
    for i in 0..ministream_sectors {
        let s = first_ministream as usize + i;
        fat[s] = if i + 1 == ministream_sectors {
            ENDOFCHAIN
        } else {
            (s + 1) as u32
        };
    }

    let mut header = vec![0u8; SECTOR];
    header[0..8].copy_from_slice(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]);
    header[0x18..0x1A].copy_from_slice(&3u16.to_le_bytes());
    header[0x1C..0x1E].copy_from_slice(&0xFFFEu16.to_le_bytes());
    header[0x1E..0x20].copy_from_slice(&9u16.to_le_bytes()); // 512-byte sectors
    header[0x20..0x22].copy_from_slice(&6u16.to_le_bytes()); // 64-byte mini sectors
    header[0x2C..0x30].copy_from_slice(&1u32.to_le_bytes());
    header[0x30..0x34].copy_from_slice(&first_dir.to_le_bytes());
    header[0x38..0x3C].copy_from_slice(&4096u32.to_le_bytes());
    header[0x3C..0x40].copy_from_slice(&first_minifat.to_le_bytes());
    header[0x40..0x44].copy_from_slice(&1u32.to_le_bytes());
    header[0x44..0x48].copy_from_slice(&ENDOFCHAIN.to_le_bytes());
    header[0x4C..0x50].copy_from_slice(&0u32.to_le_bytes()); // DIFAT[0] -> the FAT
    for i in 1..109usize {
        let o = 0x4C + i * 4;
        header[o..o + 4].copy_from_slice(&FREESECT.to_le_bytes());
    }

    let mut dir = vec![0u8; dir_sectors * SECTOR];
    {
        let mut entry = |slot: usize, name: &str, kind: u8, start: u32, size: u64| {
            let base = slot * 128;
            let utf16: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            for (i, c) in utf16.iter().enumerate() {
                dir[base + i * 2..base + i * 2 + 2].copy_from_slice(&c.to_le_bytes());
            }
            dir[base + 64..base + 66].copy_from_slice(&((utf16.len() * 2) as u16).to_le_bytes());
            dir[base + 66] = kind;
            dir[base + 67] = 1;
            for off in [68, 72, 76] {
                dir[base + off..base + off + 4].copy_from_slice(&FREESECT.to_le_bytes());
            }
            dir[base + 116..base + 120].copy_from_slice(&start.to_le_bytes());
            dir[base + 120..base + 128].copy_from_slice(&size.to_le_bytes());
        };
        entry(
            0,
            "Root Entry",
            5,
            first_ministream,
            (ministream_sectors * SECTOR) as u64,
        );
        for (i, ((name, _), (start, size))) in streams.iter().zip(&placed).enumerate() {
            entry(i + 1, name, 2, *start, *size);
        }
    }
    dir[76..80].copy_from_slice(&1u32.to_le_bytes()); // root's child -> the first stream

    let mut minifat_sector = vec![0u8; SECTOR];
    for i in 0..SECTOR / 4 {
        let v = minifat.get(i).copied().unwrap_or(FREESECT);
        minifat_sector[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    let mut fat_sector = vec![0u8; SECTOR];
    for (i, v) in fat.iter().enumerate() {
        fat_sector[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }

    [header, fat_sector, dir, minifat_sector, ministream].concat()
}

/// The ordinary `.msg` the module claims to render, end to end.
///
/// Both attachment entries share one stream name, so a reader that used `read_stream`
/// (first match wins) would silently list ONE — a wrong answer that looks right.
#[test]
fn msg_renders_headers_body_and_both_attachments() {
    let msg = build_msg(&[
        ("__substg1.0_0037001F", "Quarterly numbers"),
        (
            "__substg1.0_1000001F",
            "Figures attached. Ask if anything looks off.",
        ),
        ("__substg1.0_0C1A001F", "Ada Lovelace"),
        ("__substg1.0_5D01001F", "ada@example.com"),
        ("__substg1.0_0E04001F", "Alan Turing"),
        ("__substg1.0_3707001F", "report.pdf"),
        ("__substg1.0_3707001F", "photo.jpg"),
    ]);
    let md = msg_to_markdown(&msg).expect("a real .msg parses");
    assert!(md.contains("# Quarterly numbers"), "subject: {md}");
    // `<` is a markdown metacharacter, so `md_cell` escapes it — the rendered From line
    // is `Ada Lovelace \<ada@example.com>`, not the raw address form.
    assert!(
        md.contains("Ada Lovelace \\<ada@example.com>"),
        "from: {md}"
    );
    assert!(md.contains("Alan Turing"), "to: {md}");
    assert!(md.contains("Figures attached."), "body: {md}");
    assert!(md.contains("report.pdf"), "first attachment: {md}");
    assert!(
        md.contains("photo.jpg"),
        "SECOND attachment missing — `read_streams` collected only one: {md}"
    );
}

/// A `.msg` whose strings are markdown must arrive inert, exactly as the `.eml` side does.
///
/// The `.msg` path builds `Mail` itself instead of going through the header parser, so
/// this is a genuinely separate route to `assemble` and could have been missed there.
/// The attachment NAME is the sharpest case: it is attacker-chosen, it is rendered inside
/// backticks, and a name containing a backtick would otherwise escape the code span.
#[test]
fn hostile_msg_strings_cannot_inject_markdown() {
    let msg = build_msg(&[
        ("__substg1.0_0037001F", "[click me](https://evil.example)"),
        ("__substg1.0_1000001F", "<img src=x onerror=alert(1)>"),
        ("__substg1.0_3707001F", "a`b](https://evil.example)!x"),
    ]);
    let md = msg_to_markdown(&msg).expect("hostile .msg still parses");
    // Assert on the ESCAPED forms, not on the absence of the raw ones: `\<img` still
    // *contains* `<img` as a substring, so a naive `!contains` here passes and fails for
    // the wrong reasons in both directions.
    assert!(
        md.contains("\\[click me\\](https://evil.example)"),
        "the subject's link was not escaped: {md}"
    );
    assert!(
        md.contains("\\<img src=x onerror=alert(1)>"),
        "the body's HTML was not escaped: {md}"
    );
    assert!(
        md.contains("a\\`b\\](https://evil.example)\\!x"),
        "the attachment name was not escaped: {md}"
    );
    // And the real assertion: with the escapes removed, nothing may reach the output as
    // a bare markdown metacharacter — which is what actually makes a link or a tag live.
    for (line_no, line) in md.lines().enumerate() {
        let bare = line
            .replace("\\[", "")
            .replace("\\]", "")
            .replace("\\`", "")
            .replace("\\<", "")
            .replace("\\!", "")
            .replace("\\*", "")
            .replace("\\_", "")
            .replace("\\~", "")
            .replace("\\\\", "");
        assert!(
            !bare.contains(']') && !bare.contains('<'),
            "line {line_no} still carries an unescaped metacharacter: {line}"
        );
    }
}

/// Doubled carriage returns must still parse as mail.
///
/// Found by writing a demo `.eml` from a Python script on Windows: text mode rewrites the
/// `\n` of an already-CRLF string, so every ending became `\r\r\n`. Stripping one CR left
/// the header/body separator as a lone `\r`, which is not an empty line — so the walk read
/// the entire message as headers, found no separator, and the preview fell back to raw
/// MIME source. Any tool that copies mail in text mode does this.
#[test]
fn mail_with_doubled_carriage_returns_still_parses() {
    let eml = b"From: Ada <ada@example.com>\r\r\n\
                Subject: Doubled\r\r\n\
                \r\r\n\
                First line.\r\r\n\
                Second line.\r\r\n";
    let md = eml_to_markdown(eml).expect("\\r\\r\\n mail must still be recognised as mail");
    assert!(md.contains("# Doubled"), "subject: {md}");
    assert!(md.contains("ada@example.com"), "from: {md}");
    assert!(md.contains("First line."), "body: {md}");
    assert!(
        !md.contains("Subject: Doubled"),
        "the raw header block leaked into the body, so the split still went wrong: {md}"
    );
}

/// A plain-text body must not write itself into the document structure.
///
/// Found on a real capture: an email whose body was `# 1. Take the box out of rotation`
/// rendered as a giant heading AND put its own lines into the preview's outline sidebar.
/// The subject is the only thing entitled to be a heading here.
#[test]
fn a_hash_in_the_body_does_not_become_a_heading() {
    let eml = b"From: ops@example.com\r\n\
                Subject: Deploy checklist\r\n\
                \r\n\
                # 1. Take the box out of rotation\r\n\
                   ## indented too\r\n\
                > we discussed this on Friday\r\n\
                - a real list\r\n";
    let md = eml_to_markdown(eml).expect("mail parses");
    assert!(md.contains("# Deploy checklist"), "subject heading: {md}");
    assert!(
        md.contains("\\# 1. Take the box out of rotation"),
        "a body line starting with # was left as a heading: {md}"
    );
    assert!(
        md.contains("\\## indented too"),
        "an indented # was left as a heading: {md}"
    );
    // Exactly one real heading in the document: the subject.
    let headings = md.lines().filter(|l| l.starts_with("# ")).count();
    assert_eq!(headings, 1, "the body added headings to the outline: {md}");
    // The constructs that IMPROVE the render stay untouched.
    assert!(
        md.contains("> we discussed this on Friday"),
        "quoted reply text should still render as a blockquote: {md}"
    );
    assert!(
        md.contains("- a real list"),
        "a list should still render as a list: {md}"
    );
}

/// A stray all-CR line inside the header block must NOT be taken as the separator.
///
/// This is the regression the obvious version of the doubled-CR fix caused, caught by an
/// adversarial review before it shipped. Stripping every trailing CR per line turns a line
/// of three bare CRs into an empty one, which ends the header block early and dumps every
/// header after it into the body as literal text. `undouble_line_endings` exists precisely
/// so the per-line rule can stay strict: the doubling has to be uniform across the whole
/// file before anything is rewritten, and this input's endings are mixed.
#[test]
fn a_stray_all_cr_line_does_not_end_the_header_block() {
    let eml = b"From: a@b.com\r\n\r\r\r\nSubject: hi\r\n\r\nBody text.\r\n";
    let md = eml_to_markdown(eml).expect("still mail");
    assert!(
        md.contains("# hi"),
        "Subject was lost: a stray CR line ended the header block early: {md}"
    );
    assert!(md.contains("a@b.com"), "From was lost: {md}");
    assert!(md.contains("Body text."), "body missing: {md}");
    assert!(
        !md.contains("Subject: hi"),
        "the Subject header leaked into the body as literal text: {md}"
    );
}

/// The rewrite must fire ONLY on uniformly doubled endings, and must be a no-op otherwise.
#[test]
fn undoubling_is_scoped_to_uniformly_doubled_files() {
    use std::borrow::Cow;
    let plain = b"From: a\r\nTo: b\r\n\r\nbody\r\n";
    assert!(
        matches!(undouble_line_endings(plain), Cow::Borrowed(_)),
        "a normal CRLF mail must not be copied at all"
    );
    let mixed = b"From: a\r\n\r\r\r\nTo: b\r\n";
    assert!(
        matches!(undouble_line_endings(mixed), Cow::Borrowed(_)),
        "mixed endings must be left exactly as they are"
    );
    let lf_only = b"From: a\nTo: b\n\nbody\n";
    assert!(matches!(undouble_line_endings(lf_only), Cow::Borrowed(_)));
    let doubled = b"From: a\r\r\nTo: b\r\r\n\r\r\nbody\r\r\n";
    assert_eq!(
        &undouble_line_endings(doubled)[..],
        b"From: a\r\nTo: b\r\n\r\nbody\r\n",
        "a uniformly doubled file must collapse to exactly the single-CR form"
    );
}

/// `<script>` and `<style>` bodies must vanish with their tags, not merely lose the tags.
///
/// Stripping only the angle brackets would leave the script SOURCE as body text, which
/// reads as gibberish at best and as attacker-authored prose at worst.
#[test]
fn html_script_and_style_content_never_reaches_the_body() {
    let eml = b"Subject: Newsletter\r\n\
                Content-Type: text/html\r\n\
                \r\n\
                <html><head><title>SECRETTITLE</title></head>\
                <body><script>var SECRETSCRIPT=1;</script>\
                <style>.a{color:SECRETSTYLE}</style>\
                <p>Real body text.</p></body></html>\r\n";
    let md = eml_to_markdown(eml).expect("html mail parses");
    assert!(md.contains("Real body text."), "body lost: {md}");
    for needle in ["SECRETSCRIPT", "SECRETSTYLE", "SECRETTITLE"] {
        assert!(!md.contains(needle), "{needle} leaked into the body: {md}");
    }
}

/// `<header>` is an ordinary element, not `<head>`: matching it by prefix went looking for a
/// `</head>` that never comes and dropped everything after it.
#[test]
fn an_html_header_element_is_not_mistaken_for_head() {
    let eml = b"Subject: Newsletter\r\n\
                Content-Type: text/html\r\n\
                \r\n\
                <html><body><header>Masthead</header>\
                <p>Everything after the header.</p></body></html>\r\n";
    let md = eml_to_markdown(eml).expect("html mail parses");
    assert!(md.contains("Masthead"), "header text lost: {md}");
    assert!(
        md.contains("Everything after the header."),
        "body lost: {md}"
    );
}

/// A multipart message with more than [`MAX_EML_ATTACHMENTS`] filenamed parts must cap the
/// rendered list (not grow `Mail.attachments` unbounded) and note how many were dropped —
/// before this fix the list had no cap at all, unlike every sibling list in this module
/// (notebook attachments, `.msg` attachments, the body's own line cap).
#[test]
fn eml_attachment_list_caps_and_notes_the_overflow() {
    let n = MAX_EML_ATTACHMENTS + 5;
    let mut eml = String::from(
        "Subject: many attachments\r\nContent-Type: multipart/mixed; boundary=b\r\n\r\n",
    );
    for i in 0..n {
        eml.push_str(&format!(
            "--b\r\nContent-Type: application/octet-stream\r\n\
             Content-Disposition: attachment; filename=\"f{i}.bin\"\r\n\r\nx\r\n"
        ));
    }
    eml.push_str("--b--\r\n");
    let md = eml_to_markdown(eml.as_bytes()).expect("multipart mail parses");
    // Exactly the cap's worth of filenames actually rendered...
    for i in 0..MAX_EML_ATTACHMENTS {
        assert!(md.contains(&format!("f{i}.bin")), "f{i}.bin missing: {md}");
    }
    // ...and the ones past the cap are not, but the overflow is noted.
    for i in MAX_EML_ATTACHMENTS..n {
        assert!(
            !md.contains(&format!("f{i}.bin")),
            "f{i}.bin should be dropped: {md}"
        );
    }
    assert!(
        md.contains("+5 more attachments not shown."),
        "expected the overflow note; md tail was: {}",
        &md[md.len().saturating_sub(200)..]
    );
}

/// Pathological multipart must TERMINATE, and quickly.
///
/// Three separate exhaustion shapes in one file: nesting far past the depth limit, a
/// boundary that is never closed, and thousands of parts. The wall-clock bound is the
/// assertion that matters — a preview that takes a minute is a hang to the person who
/// pressed Space, and this runs on the UI's load path.
#[test]
fn pathological_multipart_terminates_quickly() {
    let mut deep =
        String::from("Subject: deep\r\nContent-Type: multipart/mixed; boundary=b0\r\n\r\n");
    for d in 0..40 {
        deep.push_str(&format!(
            "--b{d}\r\nContent-Type: multipart/mixed; boundary=b{}\r\n\r\n",
            d + 1
        ));
    }
    deep.push_str("--b40\r\nContent-Type: text/plain\r\n\r\nbottom\r\n--b40--\r\n");

    let unterminated =
        "Subject: open\r\nContent-Type: multipart/mixed; boundary=zz\r\n\r\n--zz\r\n\
         Content-Type: text/plain\r\n\r\nno closing delimiter ever arrives"
            .to_string();

    let mut many =
        String::from("Subject: many\r\nContent-Type: multipart/mixed; boundary=p\r\n\r\n");
    for i in 0..5_000 {
        many.push_str(&format!(
            "--p\r\nContent-Type: text/plain\r\n\r\npart {i}\r\n"
        ));
    }
    many.push_str("--p--\r\n");

    let started = std::time::Instant::now();
    for (label, body) in [
        ("nested", deep),
        ("unterminated", unterminated),
        ("many parts", many),
    ] {
        let _ = eml_to_markdown(body.as_bytes());
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{label} multipart took too long to reject"
        );
    }
}

/// A header block that is mostly headers must not blow up the preview either.
///
/// Real spam does carry hundreds of `Received:` lines, and folded continuations mean one
/// logical header can be arbitrarily long.
#[test]
fn absurd_header_blocks_are_bounded() {
    let started = std::time::Instant::now();

    // Under the 500-header cap, plus ONE logical header folded across 5,000 physical
    // lines. Folding does not grow the header COUNT, so the cap never sees it — the only
    // thing bounding that string is the 16 MB read cap, and this proves it still renders.
    let mut ok = String::from("Subject: floods\r\n");
    for i in 0..400 {
        ok.push_str(&format!("X-Pad-{i}: {i}\r\n"));
    }
    ok.push_str("X-Folded: start\r\n");
    for _ in 0..5_000 {
        ok.push_str("\tcontinuation\r\n");
    }
    ok.push_str("\r\nbody\r\n");
    let md = eml_to_markdown(ok.as_bytes()).expect("400 headers is still mail");
    assert!(md.contains("# floods"), "{md}");

    // Past the cap, `split_headers` declines — and declining is CORRECT here. The file
    // falls through to the plain-text view rather than being rendered as mail, which is
    // what a 20,000-header file deserves. What matters is that it decides fast.
    let mut flood = String::from("Subject: too many\r\n");
    for i in 0..20_000 {
        flood.push_str(&format!("X-Pad-{i}: {i}\r\n"));
    }
    flood.push_str("\r\nbody\r\n");
    assert!(
        eml_to_markdown(flood.as_bytes()).is_none(),
        "a 20,000-header block should fall through to the text view, not render as mail"
    );

    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "header flood took too long"
    );
}

// -----------------------------------------------------------------------------------
// Mutation fuzzing
// -----------------------------------------------------------------------------------
//
// The lib crate's `src/fuzz.rs` cannot reach these: mail parsing lives in the app binary,
// so its harness has no path to `eml_to_markdown`. Rather than leave the two newest
// untrusted-input parsers in the build unfuzzed, the loop is reproduced here in the small
// — same idea as `fuzz.rs`, deliberately much cheaper, because these are the only two
// targets and both reject in microseconds.
//
// The seeds are STRUCTURALLY VALID on purpose. A mutation of random bytes never gets past
// `looks_like_ole` or the "does this look like mail" gate, so a run against garbage
// measures the gate and nothing behind it.

/// One fuzz target: its name for the failure message, the entry point, and its seed.
type FuzzTarget<'a> = (&'a str, fn(&[u8]), &'a [u8]);
