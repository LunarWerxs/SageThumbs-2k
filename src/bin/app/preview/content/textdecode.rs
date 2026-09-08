/// Read a text/code file for preview: cap at 5 MB, reject binaries, decode (BOM-aware, lossy),
/// truncate absurdly long lines, and mark a capped file. `None` if unreadable or binary.
pub(crate) fn read_text(path: &str) -> Option<String> {
    const CAP: usize = 5 * 1024 * 1024;
    let (bytes, capped) = read_capped(path, CAP)?;
    if is_binary(&bytes) {
        return None;
    }
    let mut text = truncate_long_lines(&decode_text(&bytes, capped), 10_000);
    if capped {
        text.push_str("\n\n… (file truncated at 5 MB)");
    }
    Some(text)
}

/// Like [`read_text`] but WITHOUT the long-line truncation — for structured documents
/// (CSV/TSV/`.ipynb`) that must be parsed whole. A minified single-line notebook JSON or a wide
/// CSV row would otherwise be cut at 10 000 chars, breaking the parse. Same 5 MB cap + binary
/// reject + BOM-aware decode. `None` if unreadable or binary.
pub(crate) fn read_doc(path: &str) -> Option<String> {
    const CAP: usize = 5 * 1024 * 1024;
    let (bytes, capped) = read_capped(path, CAP)?;
    if is_binary(&bytes) {
        return None;
    }
    Some(decode_text(&bytes, capped))
}

/// Quick "is this a text file" sniff for unknown extensions: read the first 16 KB and treat it
/// as text unless it has two consecutive NUL bytes (the standard binary heuristic).
pub(super) fn looks_like_text(path: &str) -> bool {
    match read_capped(path, 16 * 1024) {
        Some((bytes, _)) => !bytes.is_empty() && !is_binary(&bytes),
        None => false,
    }
}

/// Read up to `cap` bytes of `path`; the bool is whether the file was longer (i.e. truncated).
///
/// The shared bottom of both the unknown-extension sniff (`looks_like_text`/`looks_like_image`,
/// via `classify`) and the text/markdown read (`read_text`/`read_doc`), the two file reads the
/// 2026-09-05 audit (F10) named as blocking the UI thread on slow/stalled storage before the
/// viewer could even show. `loader::resolve_load` now runs both off the UI thread; see
/// `slow_read_seam` for the test hook that proves it.
pub(crate) fn read_capped(path: &str, cap: usize) -> Option<(Vec<u8>, bool)> {
    slow_read_seam();
    use std::io::Read;
    let f = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    // Read one byte past the cap so we can tell "exactly cap" from "longer than cap".
    f.take(cap as u64 + 1).read_to_end(&mut buf).ok()?;
    let capped = buf.len() > cap;
    buf.truncate(cap);
    Some((buf, capped))
}

/// Test-only delay hook (2026-09-05 audit, F10 acceptance): set `ST2K_PREVIEW_SLOW_READ_MS` to
/// make every call to [`read_capped`] sleep that long first, so a test can prove the viewer
/// window shows and stays responsive while a "slow disk" read is still in flight, and that
/// switching selections never lets a slow, stale read paint over a newer one, without touching
/// a real network share. Read once (like `ST2K_NO_CANCEL`/`ST2K_THEME`), so the hot path costs
/// one atomic load when unset (the default).
fn slow_read_seam() {
    static MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let ms = *MS.get_or_init(|| {
        std::env::var("ST2K_PREVIEW_SLOW_READ_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    });
    if ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

/// Two consecutive NUL bytes in the first 16 KB = binary (matches the plan's sniff).
fn is_binary(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .take(16 * 1024)
        .zip(bytes.iter().skip(1))
        .any(|(a, b)| *a == 0 && *b == 0)
}

/// Decode bytes to a String: honor a UTF-16 LE/BE or UTF-8 BOM, sniff BOM-less UTF-16, take
/// strict UTF-8 when it validates, and otherwise fall back to the legacy codepage tiers in
/// [`decode_legacy`]. `capped` is whether `bytes` was cut short by [`read_capped`]'s cap — see
/// the back-off below.
///
/// The UTF-8-lossy-everything shortcut this replaced turned every non-Unicode CJK file into a
/// solid wall of U+FFFD: GBK/GB18030 is still a Chinese national standard, and Shift-JIS,
/// Big5 and EUC-KR are all over real-world `.txt`/`.csv`/`.srt` files. Those users saw
/// nothing but replacement characters.
fn decode_text(bytes: &[u8], capped: bool) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    // UTF-32 BOMs must be tested BEFORE UTF-16's, because the UTF-32LE BOM ("FF FE 00 00") STARTS
    // with the UTF-16LE one. Checked in the other order, every UTF-32LE file decoded as UTF-16LE
    // and came out as text interleaved with NULs.
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE, 0x00, 0x00]) {
        return utf32(rest, true);
    }
    if let Some(rest) = bytes.strip_prefix(&[0x00, 0x00, 0xFE, 0xFF]) {
        return utf32(rest, false);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return utf16_le(rest);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return utf16_be(rest);
    }
    // BOM-less UTF-16. Notepad writes a BOM but plenty of tools (and Windows' own older
    // exports) don't; without this such a file decodes as interleaved-NUL garbage. ASCII-range
    // UTF-16 text never trips `is_binary` (it has no two CONSECUTIVE NULs), so it reaches here.
    match sniff_utf16(bytes) {
        Some(true) => return utf16_le(bytes),
        Some(false) => return utf16_be(bytes),
        None => {}
    }
    // Strict, not lossy: valid UTF-8 is the overwhelmingly common case and must win outright,
    // but a FAILED validation is now real evidence that this is a legacy-codepage file rather
    // than something to paper over with U+FFFD.
    match core::str::from_utf8(bytes) {
        Ok(s) => s.to_owned(),
        Err(e) => {
            // The read cap can land mid-character: the cut lands inside the LAST char's
            // multi-byte sequence, `from_utf8` fails on the whole (otherwise perfectly valid)
            // buffer, and every byte in it would get re-guessed as a legacy codepage — for a
            // pure-ASCII file, THAT can validate as a DBCS reading and come out as mojibake
            // A truncation can only ever cost the last 1-3 bytes of one UTF-8 char
            // (4 bytes is the longest encoding), so back off to the last valid boundary and
            // keep the strict reading instead of falling to `decode_legacy` — but only when
            // the read was actually capped AND the invalid tail is that small; a longer
            // invalid run past a capped read is still real evidence of a non-UTF-8 file.
            let valid_up_to = e.valid_up_to();
            if capped && bytes.len() - valid_up_to <= 3 {
                return String::from_utf8_lossy(&bytes[..valid_up_to]).into_owned();
            }
            decode_legacy(bytes)
        }
    }
}

/// Decode `bytes` as UTF-32 (`le` picks the byte order); a trailing partial unit and any invalid
/// scalar (a surrogate, or above U+10FFFF) become U+FFFD rather than failing the whole file.
/// BOM-only, never sniffed: a BOM-less UTF-32 file is vanishingly rare and guessing one would
/// misread ordinary ASCII that happens to contain NULs.
fn utf32(bytes: &[u8], le: bool) -> String {
    let (chunks, _) = bytes.as_chunks::<4>();
    chunks
        .iter()
        .map(|c| {
            let v = if le {
                u32::from_le_bytes([c[0], c[1], c[2], c[3]])
            } else {
                u32::from_be_bytes([c[0], c[1], c[2], c[3]])
            };
            char::from_u32(v).unwrap_or(char::REPLACEMENT_CHARACTER)
        })
        .collect()
}

/// Decode `bytes` as UTF-16LE (odd trailing byte dropped).
fn utf16_le(bytes: &[u8]) -> String {
    let (chunks, _) = bytes.as_chunks::<2>();
    let u: Vec<u16> = chunks
        .iter()
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&u)
}

/// Decode `bytes` as UTF-16BE (odd trailing byte dropped).
fn utf16_be(bytes: &[u8]) -> String {
    let (chunks, _) = bytes.as_chunks::<2>();
    let u: Vec<u16> = chunks
        .iter()
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&u)
}

/// BOM-less UTF-16 sniff over the first 4 KB: `Some(true)` = LE, `Some(false)` = BE, `None` =
/// not UTF-16. Looks for the giveaway pattern of ASCII-range text — a NUL in a consistent
/// parity slot for most 2-byte units — and demands a strong majority so a legacy-codepage file
/// (which has essentially no NULs at all) can never be mistaken for UTF-16.
fn sniff_utf16(bytes: &[u8]) -> Option<bool> {
    let head = &bytes[..bytes.len().min(4096)];
    if head.len() < 16 {
        return None;
    }
    let pairs = head.len() / 2;
    let (mut hi_nul, mut lo_nul) = (0usize, 0usize);
    let (chunks, _) = head.as_chunks::<2>();
    for c in chunks {
        // c[1] is the high byte in LE: NUL there means an ASCII-range char stored little-endian.
        if c[1] == 0 && c[0] != 0 {
            hi_nul += 1;
        }
        if c[0] == 0 && c[1] != 0 {
            lo_nul += 1;
        }
    }
    // 60% of units carrying the same-parity NUL is far above anything 8-bit text produces.
    let thresh = pairs * 3 / 5;
    match (hi_nul > thresh, lo_nul > thresh) {
        (true, false) => Some(true),
        (false, true) => Some(false),
        _ => None,
    }
}

/// Decode non-UTF-8 bytes via Windows' in-box codepage tables — zero bundled bytes, since every
/// one of these ships with the OS.
///
/// The double-byte codepages are tried FIRST, and only they compete on score. That ordering is
/// the whole trick: a single-byte codepage like Windows-1252 maps almost every possible byte, so
/// "1252 validated" is no evidence at all, and letting it into the contest means Latin-1
/// mojibake (`ÖÐÄÄ`) beats the correct `中文` on any per-character scoring you care to invent. A
/// DBCS codepage validating the WHOLE buffer under `MB_ERR_INVALID_CHARS` is real evidence,
/// because it requires every high byte to form a well-formed lead/trail pair — an ordinary
/// Latin-1 file with SEVERAL isolated accented characters fails that immediately, since it is
/// unlikely every one of them happens to pair with its neighbor.
///
/// A SHORT buffer with only one or two accented characters doesn't get that protection for
/// free: one stray byte pairing with the very next ASCII letter is enough to validate the
/// whole (tiny) buffer, and [`cjk_score`]'s majority check has nothing else to weigh it
/// against. Below [`SHORT_BUFFER_RESCUE_BYTES`], a non-dominant win is treated as inconclusive
/// and the system code page is preferred instead - see the check at the end of this function.
///
/// Ties fall to the system ANSI codepage when it is itself DBCS, which is the case that matters
/// most: a Chinese/Japanese/Korean user opening a local file on their own localized Windows,
/// where the ACP already IS 936/932/949.
///
/// Known limit: pure-ideograph text with no kana or hangul is genuinely ambiguous between GBK,
/// Shift-JIS and EUC-KR — the same bytes are valid in all three. Real sentences carry kana or
/// hangul and [`cjk_score`] keys off those, but a short hanzi-only string on a non-CJK machine
/// can still land on the wrong one. Dedicated detectors have the same problem without
/// frequency tables, which are more weight than this is worth.
fn decode_legacy(bytes: &[u8]) -> String {
    use windows::Win32::Globalization::GetACP;

    let acp = unsafe { GetACP() };
    // ACP first when it's double-byte, so it wins ties; then the rest, minus any duplicate.
    let mut candidates: Vec<u32> = Vec::with_capacity(DBCS_CODEPAGES.len() + 1);
    if is_dbcs(acp) {
        candidates.push(acp);
    }
    candidates.extend(DBCS_CODEPAGES.iter().copied().filter(|cp| *cp != acp));

    let mut best: Option<(i64, String)> = None;
    for cp in candidates {
        let Some(s) = decode_codepage(bytes, cp, true) else {
            continue;
        };
        let score = cjk_score(&s);
        if score <= 0 {
            continue; // validated, but the result doesn't look like CJK text
        }
        // Strictly-greater, so a tie goes to whichever was scored FIRST.
        if best.as_ref().is_none_or(|(b, _)| score > *b) {
            best = Some((score, s));
        }
    }
    if let Some((score, s)) = best {
        // A short buffer starves the majority/dominance check of samples: a single accented
        // Latin-1 byte followed by an ordinary ASCII letter (the shape of "caf\xE9e" or
        // "Stra\xDFe") can happen to be a validly-assigned DBCS lead/trail pair, and with only
        // ONE non-ASCII character in the whole buffer, "100% of the non-ASCII content looks
        // like CJK" is trivially true. That is not evidence at this size. The tell is the
        // trail byte: a Latin-1 accent only ever pairs with the plain ASCII letter after it,
        // while the hanzi a short Chinese line is made of pair high byte with high byte. So
        // below the rescue threshold a non-dominant reading (no kana or hangul carrying it)
        // that had to swallow an ASCII byte as a trail is treated as inconclusive and the
        // system code page is preferred; a reading built from high-high pairs stands.
        let short_and_inconclusive = bytes.len() < SHORT_BUFFER_RESCUE_BYTES
            && score < CJK_DOMINANT_BONUS
            && pairs_an_ascii_trail(bytes);
        if !short_and_inconclusive {
            return s;
        }
        if let Some(acp_s) = decode_codepage(bytes, acp, true) {
            return acp_s;
        }
        return s;
    }

    // No double-byte reading held up. Fall back to the system codepage — correct for the
    // single-byte locales (Cyrillic, Greek, Turkish, Vietnamese, Arabic, Thai) where a user's
    // own files match their own ACP — and finally to a lossy 1252, which maps every byte and so
    // always yields something readable instead of U+FFFD soup.
    decode_codepage(bytes, acp, true)
        .or_else(|| decode_codepage(bytes, acp, false))
        .or_else(|| decode_codepage(bytes, 1252, false))
        .unwrap_or_else(|| String::from_utf8_lossy(bytes).into_owned())
}

/// The double-byte codepages worth testing, in preference order. These are the encodings
/// real-world text files in the affected markets are actually saved in.
const DBCS_CODEPAGES: &[u32] = &[
    936, // GBK / GB18030 — Simplified Chinese
    932, // Shift-JIS — Japanese
    949, // EUC-KR / Unified Hangul — Korean
    950, // Big5 — Traditional Chinese
];

/// Below this many bytes, a DBCS win that paired a high byte with an ASCII trail needs to be
/// DOMINANT (see [`CJK_DOMINANT_BONUS`]) to beat the system code page - see the comment in
/// [`decode_legacy`] for why a short buffer's score can't be trusted otherwise.
const SHORT_BUFFER_RESCUE_BYTES: usize = 64;

/// Whether reading `bytes` as double-byte pairs (any high byte starts a pair) ever pairs a
/// high lead with an ASCII trail. Every DBCS code page tested here allows such trails, and it
/// is exactly what a lone Latin-1 accent inside a Latin word produces (`\xDF` + `e`); the
/// GB2312 range that ordinary Chinese text is written in never does (trails are 0xA1 and up).
fn pairs_an_ascii_trail(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] >= 0x80 {
            if let Some(&trail) = bytes.get(i + 1) {
                if trail < 0x80 {
                    return true;
                }
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    false
}

/// The score bonus [`cjk_score`] awards a reading whose letters are genuinely dominated by kana
/// or hangul. Shared with [`decode_legacy`] so the "is this win actually confident" check can't
/// drift from the value that put it there.
const CJK_DOMINANT_BONUS: i64 = 1_000;

/// Is `cp` one of the double-byte codepages we test?
fn is_dbcs(cp: u32) -> bool {
    DBCS_CODEPAGES.contains(&cp)
}

/// Decode `bytes` with Windows codepage `cp`. With `strict`, an invalid byte sequence for that
/// codepage makes this return `None` (that's `MB_ERR_INVALID_CHARS`); without it, undecodable
/// bytes become the codepage's default char.
fn decode_codepage(bytes: &[u8], cp: u32, strict: bool) -> Option<String> {
    use windows::Win32::Globalization::{MultiByteToWideChar, MB_ERR_INVALID_CHARS};

    if bytes.is_empty() {
        return Some(String::new());
    }
    let flags = if strict {
        MB_ERR_INVALID_CHARS
    } else {
        Default::default()
    };
    let n = unsafe { MultiByteToWideChar(cp, flags, bytes, None) };
    if n <= 0 {
        return None;
    }
    let mut buf = vec![0u16; n as usize];
    let written = unsafe { MultiByteToWideChar(cp, flags, bytes, Some(&mut buf)) };
    if written <= 0 {
        return None;
    }
    buf.truncate(written as usize);
    Some(String::from_utf16_lossy(&buf))
}

/// How much does this decode look like genuine CJK text? `0` means "reject this codepage".
///
/// Only non-ASCII characters are judged — the ASCII in a source file or a CSV decodes
/// identically under every candidate, so counting it would just dilute the signal.
///
/// The discrimination comes from SCRIPT DOMINANCE, not from per-character weights. A per-char
/// bonus for kana/hangul reads well and is wrong: decoding Chinese GBK bytes through EUC-KR
/// yields a roughly 50/50 hangul-and-hanja mixture, and enough of those small bonuses beat the
/// correct all-ideograph reading outright (it did, until this was rewritten). What actually
/// separates the languages is the SHAPE of the mixture — real Korean is overwhelmingly hangul,
/// real Japanese always carries a solid fraction of kana, and real Chinese has neither — so the
/// bonus is awarded once, on the whole string, only when a script genuinely dominates.
fn cjk_score(s: &str) -> i64 {
    // Letters are the script evidence; CJK punctuation is shared by all of them and so is
    // counted as plausible but kept out of the fractions.
    let (mut ideo, mut kana, mut hangul, mut punct, mut bad, mut non_ascii) = (0i64, 0, 0, 0, 0, 0);
    for ch in s.chars().take(20_000) {
        let c = ch as u32;
        if c < 0x80 {
            continue;
        }
        non_ascii += 1;
        match c {
            0x3040..=0x30FF => kana += 1,
            0xAC00..=0xD7A3 => hangul += 1,
            0x4E00..=0x9FFF => ideo += 1,
            0x3000..=0x303F | 0xFF01..=0xFF60 | 0xFFE0..=0xFFE6 => punct += 1,
            // Halfwidth katakana is DELIBERATELY not kana: bytes 0xA1–0xDF decode to it under
            // Shift-JIS unconditionally, so any high-byte run validates as a katakana string and
            // would otherwise hijack every Chinese and Korean file on the machine.
            0xFF61..=0xFF9F => bad += 1,
            // Private use, specials, rare extension and compatibility blocks: what a WRONG
            // table produces.
            0xE000..=0xF8FF | 0xFFF0..=0xFFFF => bad += 3,
            0x3400..=0x4DBF | 0xF900..=0xFAFF | 0x2_0000..=0x3_FFFF => bad += 2,
            _ => bad += 1,
        }
    }
    if non_ascii == 0 {
        return 0; // pure ASCII — a double-byte reading adds nothing over plain UTF-8
    }
    let good = ideo + kana + hangul + punct;
    // Demand a strong majority of the non-ASCII content be plausible CJK, so an ordinary
    // Latin-1 file that happens to validate can't be dragged into a CJK reading.
    if good * 4 < non_ascii * 3 {
        return 0;
    }
    let letters = ideo + kana + hangul;
    // Dominance thresholds: Korean prose is almost entirely hangul (a wrong reading of Chinese
    // lands near half), and any real Japanese sentence carries particles and okurigana in kana.
    // Chinese matches neither and wins on the base score alone, plus the ACP/list ordering that
    // breaks its tie with Big5.
    let dominant = letters > 0 && ((hangul * 20 >= letters * 13) || (kana * 20 >= letters * 3));
    let bonus = if dominant { CJK_DOMINANT_BONUS } else { 0 };
    (good - bad + bonus).max(1)
}

/// Cap any single line at `max` chars (so one minified/no-newline line can't blow up layout).
fn truncate_long_lines(text: &str, max: usize) -> String {
    if !text.lines().any(|l| l.chars().count() > max) {
        return text.to_string();
    }
    text.lines()
        .map(|l| {
            if l.chars().count() > max {
                let mut s: String = l.chars().take(max).collect();
                s.push('…');
                s
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod encoding_tests {
    use super::*;

    /// True if `s` contains a common CJK ideograph — the marker of a decode gone wrong when the
    /// input was Latin text.
    fn has_cjk(s: &str) -> bool {
        s.chars().any(|c| matches!(c as u32, 0x4E00..=0x9FFF))
    }

    #[test]
    fn utf8_wins_outright() {
        assert_eq!(
            decode_text("hello — 世界 🌏".as_bytes(), false),
            "hello — 世界 🌏"
        );
        assert_eq!(decode_text(b"plain ascii\n", false), "plain ascii\n");
        assert_eq!(decode_text(b"", false), "");
    }

    #[test]
    fn strips_boms() {
        let mut b = vec![0xEF, 0xBB, 0xBF];
        b.extend_from_slice("héllo".as_bytes());
        assert_eq!(decode_text(&b, false), "héllo");

        let mut le = vec![0xFF, 0xFE];
        le.extend("hi".encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(decode_text(&le, false), "hi");

        let mut be = vec![0xFE, 0xFF];
        be.extend("hi".encode_utf16().flat_map(u16::to_be_bytes));
        assert_eq!(decode_text(&be, false), "hi");
    }

    #[test]
    fn bomless_utf16_is_sniffed() {
        let text = "the quick brown fox jumps over the lazy dog";
        let le: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(decode_text(&le, false), text);
        let be: Vec<u8> = text.encode_utf16().flat_map(u16::to_be_bytes).collect();
        assert_eq!(decode_text(&be, false), text);
    }

    /// GBK is still a Chinese national standard; before this these bytes were a wall of U+FFFD.
    #[test]
    fn gbk_chinese_decodes() {
        const GBK: &[u8] = &[
            0xC4, 0xE3, 0xBA, 0xC3, 0xA3, 0xAC, 0xCA, 0xC0, 0xBD, 0xE7, 0xA3, 0xA1, 0xD5, 0xE2,
            0xCA, 0xC7, 0xD2, 0xBB, 0xB8, 0xF6, 0xB2, 0xE2, 0xCA, 0xD4, 0xCE, 0xC4, 0xBC, 0xFE,
            0xA1, 0xA3,
        ];
        assert_eq!(decode_text(GBK, false), "你好，世界！这是一个测试文件。");
    }

    /// Kana is the signal that keeps Shift-JIS from being read as GBK.
    #[test]
    fn shift_jis_japanese_decodes() {
        const SJIS: &[u8] = &[
            0x82, 0xB1, 0x82, 0xEA, 0x82, 0xCD, 0x93, 0xFA, 0x96, 0x7B, 0x8C, 0xEA, 0x82, 0xCC,
            0x83, 0x65, 0x83, 0x4C, 0x83, 0x58, 0x83, 0x67, 0x82, 0xC5, 0x82, 0xB7, 0x81, 0x42,
        ];
        assert_eq!(decode_text(SJIS, false), "これは日本語のテキストです。");
    }

    /// Hangul is the equivalent signal for EUC-KR.
    #[test]
    fn euc_kr_korean_decodes() {
        const EUCKR: &[u8] = &[
            0xBE, 0xC8, 0xB3, 0xE7, 0xC7, 0xCF, 0xBC, 0xBC, 0xBF, 0xE4, 0x20, 0xC7, 0xD1, 0xB1,
            0xB9, 0xBE, 0xEE, 0x20, 0xC5, 0xD8, 0xBD, 0xBA, 0xC6, 0xAE, 0xC0, 0xD4, 0xB4, 0xCF,
            0xB4, 0xD9, 0x2E,
        ];
        assert_eq!(decode_text(EUCKR, false), "안녕하세요 한국어 텍스트입니다.");
    }

    /// The regression that matters in the other direction: ordinary accented Latin text must
    /// never be dragged into a CJK reading just because some table accepted the bytes.
    #[test]
    fn latin1_is_not_mangled_into_cjk() {
        const L1: &[u8] = &[
            0x43, 0x61, 0x66, 0xE9, 0x20, 0x72, 0xE9, 0x73, 0x75, 0x6D, 0xE9, 0x20, 0x6E, 0x61,
            0xEF, 0x76, 0x65, 0x20, 0x73, 0x65, 0xF1, 0x6F, 0x72,
        ];
        let out = decode_text(L1, false);
        assert!(!has_cjk(&out), "Latin-1 text decoded as CJK: {out:?}");
        assert!(out.starts_with("Caf"), "unexpected decode: {out:?}");
    }

    /// A pure-ASCII byte stream must come back byte-identical whatever the machine's ACP is.
    #[test]
    fn ascii_is_never_reinterpreted() {
        let src = "fn main() { println!(\"hi\"); }\n";
        assert_eq!(decode_text(src.as_bytes(), false), src);
    }

    /// A capped read that lands mid-character must back off to the last valid UTF-8
    /// boundary instead of misreading the whole (otherwise valid) buffer as a legacy codepage.
    #[test]
    fn a_capped_cut_mid_character_backs_off_instead_of_going_legacy() {
        let mut bytes = "hello 世".as_bytes().to_vec(); // "世" = E4 B8 96 (3 bytes)
        bytes.truncate(bytes.len() - 1); // cut the last byte of "世" — an incomplete lead+cont pair
        assert_eq!(decode_text(&bytes, true), "hello ");
    }

    /// The SAME cut bytes, but NOT reported as capped (e.g. the file is genuinely that length) —
    /// must still fall through to the legacy tiers, not silently drop trailing garbage.
    #[test]
    fn an_uncapped_truncated_sequence_still_falls_to_legacy() {
        let mut bytes = "hello 世".as_bytes().to_vec();
        bytes.truncate(bytes.len() - 1);
        // Not capped: the ASCII prefix decodes fine under any codepage, so the legacy path
        // returns something containing the ASCII text rather than silently truncating it.
        assert!(decode_text(&bytes, false).starts_with("hello "));
    }

    #[test]
    fn cjk_score_rejects_halfwidth_katakana_soup() {
        // What Shift-JIS makes of arbitrary high bytes — must not qualify as CJK text.
        assert_eq!(cjk_score("ﾖﾐﾄﾄﾊﾟ"), 0);
        // Real Japanese does.
        assert!(cjk_score("これは日本語です") > 0);
    }

    /// A trailing accented byte has no following byte to pair with, so no DBCS codepage can
    /// even validate the buffer - this one was never broken, but it's the smallest case in the
    /// short-file family below, so it gets its own locked-down test.
    #[test]
    fn short_trailing_accent_with_no_pairing_partner_falls_to_acp() {
        // "caf" + U+00E9 (e-acute, cp1252 0xE9) with nothing after it to form a DBCS trail byte.
        let out = decode_text(b"caf\xe9", false);
        assert!(
            !has_cjk(&out),
            "a lone trailing accent decoded as CJK: {out:?}"
        );
        assert!(out.starts_with("caf"), "unexpected decode: {out:?}");
    }

    /// The bug this fix closes. Verified against the real Windows codepage tables: 0xDF
    /// (cp1252 sharp-s/eszett) followed by 0x65 ('e') is a validly-assigned GBK lead/trail
    /// pair, decoding to a real Chinese character. With only ONE non-ASCII byte in the whole
    /// buffer, `cjk_score`'s majority check ("is most of the non-ASCII content plausible CJK")
    /// is trivially satisfied by a sample of one, so before this fix `decode_legacy` returned
    /// that Chinese character in place of "e" and threw the accent away entirely.
    #[test]
    fn short_latin_words_with_two_accidental_pairs_still_read_as_the_system_codepage() {
        // "père Noël\r\n" in Windows-1252: both accents pair with the ASCII letter after them,
        // which several DBCS code pages accept, so the count of pairs alone cannot tell it
        // from a two-character Chinese line; the ASCII trails can.
        const PERE_NOEL: &[u8] = &[
            0x70, 0xE8, 0x72, 0x65, 0x20, 0x4E, 0x6F, 0xEB, 0x6C, 0x0D, 0x0A,
        ];
        let out = decode_text(PERE_NOEL, false);
        assert!(
            !has_cjk(&out),
            "accents inside Latin words decoded as CJK: {out:?}"
        );
        assert!(
            out.starts_with('p') && out.contains("re No") && out.ends_with("l\r\n"),
            "the surrounding ASCII text must survive verbatim: {out:?}"
        );
    }

    #[test]
    fn short_high_high_pairs_are_not_rescued_into_the_system_codepage() {
        // Two GBK hanzi ("你好") plus a newline: 6 bytes, every pair high byte + high byte,
        // which no Latin-1 accent produces. Which CJK reading wins between GBK and EUC-KR for
        // a hanzi-only line is the documented ambiguity above and not asserted here; what is
        // asserted is that the short-buffer rescue leaves it a CJK reading instead of
        // handing it to the system code page as Latin-1 mojibake.
        const NIHAO: &[u8] = &[0xC4, 0xE3, 0xBA, 0xC3, 0x0D, 0x0A];
        let out = decode_text(NIHAO, false);
        assert!(
            !out.chars().any(|c| (0x80..=0xFF).contains(&(c as u32))),
            "a short high-high line was rescued into the system code page: {out:?}"
        );
        assert!(out.ends_with("\r\n"), "the newline must survive: {out:?}");
    }

    #[test]
    fn short_single_accent_no_longer_loses_to_a_stray_gbk_pairing() {
        // A short, real-world Windows-1252 fragment: "Straße\r\n".
        const STRASSE: &[u8] = &[0x53, 0x74, 0x72, 0x61, 0xDF, 0x65, 0x0D, 0x0A];
        let out = decode_text(STRASSE, false);
        assert!(
            !has_cjk(&out),
            "a single accented Latin-1 byte decoded as CJK: {out:?}"
        );
        assert!(
            out.starts_with("Stra") && out.ends_with("e\r\n"),
            "the surrounding ASCII text must survive verbatim: {out:?}"
        );
    }

    /// The fix above must not blunt genuine short CJK text: real Japanese still carries the
    /// kana that gives `cjk_score` its dominance bonus, which stays trusted even under the
    /// short-buffer rescue (the bonus is far above `SHORT_BUFFER_RESCUE_BYTES`'s bar).
    #[test]
    fn short_real_shift_jis_greeting_still_beats_the_system_codepage() {
        // "こんにちは" (konnichiwa) in Shift-JIS - 10 bytes, all kana, no kanji.
        const SJIS_GREETING: &[u8] = &[0x82, 0xB1, 0x82, 0xF1, 0x82, 0xC9, 0x82, 0xBF, 0x82, 0xCD];
        assert_eq!(decode_text(SJIS_GREETING, false), "こんにちは");
    }

    /// Bytes that are neither valid UTF-8 nor valid under any DBCS table must still decode to
    /// something via the system code page rather than panicking or silently dropping the
    /// trailing ASCII - this locks in the pre-existing fallback for a plain invalid-UTF-8
    /// fixture (`0xC3` is a UTF-8 lead byte with no valid continuation).
    #[test]
    fn invalid_utf8_garbage_falls_back_without_losing_trailing_ascii() {
        let out = decode_text(b"\xC3\x28\x41", false);
        assert!(
            out.ends_with("(A"),
            "trailing ASCII bytes must survive verbatim: {out:?}"
        );
    }
}
