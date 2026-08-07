//! Content pipeline for the viewer: classify a path, decode images on a budgeted worker
//! thread (never on the UI thread — hard constraint §4/#3), build a DIB, and aspect-fit
//! paint it. Ported from `previewhandler.rs` (`make_dib` / `draw` / the budgeted-decode
//! worker), which does exactly this for the Explorer preview pane.

use core::ffi::c_void;
use core::time::Duration;
use std::cell::RefCell;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    AlphaBlend, CreateCompatibleDC, CreateDIBSection, CreateSolidBrush, DeleteDC, DeleteObject,
    FillRect, SelectObject, SetStretchBltMode, StretchBlt, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO,
    BITMAPINFOHEADER, BLENDFUNCTION, DIB_RGB_COLORS, HALFTONE, HBITMAP, HDC, SRCCOPY,
};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use super::window::{ContentKind, WM_APP_ANIM, WM_APP_PDFINFO, WM_APP_RENDER};

/// Wall-clock budget for a single decode (plan §7 uses 12 s, matching the preview pane).
const DECODE_BUDGET: Duration = Duration::from_secs(12);

/// A decoded image ready to become a DIB. `Send`, so it crosses the worker→UI post.
pub(super) struct DecodedRgba {
    pub w: i32,
    pub h: i32,
    pub rgba: Vec<u8>,
}

/// The current image render installed in the window (the DIB + its natural dims + the bg
/// it was composited over). Sole owner of `hbmp`; freed when replaced or on window destroy.
pub(super) struct RenderData {
    pub hbmp: HBITMAP,
    pub iw: i32,
    pub ih: i32,
    /// The bitmap holds PREMULTIPLIED alpha and must be composed with `AlphaBlend`, not blitted.
    /// Only ever true for the main image pane (see [`make_render`]); cover art and inline Markdown
    /// images stay flattened, so they keep the plain blit and want no checkerboard.
    pub alpha: bool,
    /// The premultiplied pixels of `hbmp`, which is a DIB SECTION, so this is its own backing
    /// memory rather than a second copy. Valid exactly as long as `hbmp`. Null unless `alpha`.
    src: *const u8,
    /// Last box-filtered downscale, keyed by the size it was built for. See [`scaled_for`] for
    /// why the resampling is done here rather than left to GDI.
    scaled: RefCell<Option<(i32, i32, HBITMAP)>>,
}

impl RenderData {
    /// A fully opaque render: plain `StretchBlt`, no checkerboard, no resampling cache.
    pub(super) fn opaque(hbmp: HBITMAP, iw: i32, ih: i32) -> Self {
        Self {
            hbmp,
            iw,
            ih,
            alpha: false,
            src: core::ptr::null(),
            scaled: RefCell::new(None),
        }
    }
}

impl Drop for RenderData {
    fn drop(&mut self) {
        unsafe {
            if let Some((_, _, s)) = self.scaled.borrow_mut().take() {
                let _ = DeleteObject(s.into());
            }
            let _ = DeleteObject(self.hbmp.into());
        }
    }
}

/// Decide how to present `path`: directory / unsupported → InfoCard; text/markdown (gated on
/// the settings) → Text; any of the ~315 supported formats → Image; an unknown-but-textual
/// file → Text. Phase 3's text branch shows the file as readable monospace text; rendered
/// GitHub-style Markdown + syntax highlighting (WebView2 + syntect) is a later enhancement.
pub(super) fn classify(path: &str) -> ContentKind {
    use sagethumbs2k_core::{formats, settings};
    let p = std::path::Path::new(path);
    if p.is_dir() {
        return ContentKind::InfoCard;
    }
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    // Markdown (rendered) + text/code, ahead of the image path (a `.md`/`.txt` is never an image).
    if settings::preview_markdown() && formats::is_preview_markdown(&ext) {
        return ContentKind::Markdown;
    }
    // Structured docs ride the markdown PIPELINE (converted at load — see `docconv`), but each
    // honors the toggle a user would expect to govern it: a notebook is a markdown document,
    // CSV/TSV are data/text files (review finding, 2026-07-13 — with Markdown off + Text on,
    // csv used to fall through to the raw-text sniff and lose its table view).
    if formats::is_preview_doc(&ext) {
        let on = if ext.eq_ignore_ascii_case("ipynb") {
            settings::preview_markdown()
        } else {
            settings::preview_text()
        };
        if on {
            return ContentKind::Markdown;
        }
    }
    if settings::preview_text() && formats::is_preview_text(&ext) {
        return ContentKind::Text;
    }
    if formats::is_known(&ext) {
        // Video AND audio play in-viewer via the shared Media-Foundation engine + transport strip
        // (audio is a video with no picture — same seek/volume/play controls). Everything else
        // (documents, images, incl. embedded album art) takes the decoded-image path.
        if matches!(
            formats::category(&ext),
            formats::Category::Video | formats::Category::Audio
        ) {
            return ContentKind::Video;
        }
        return ContentKind::Image;
    }
    // Unknown extension: if it sniffs as text (and text preview is on), show it as text.
    if settings::preview_text() && looks_like_text(path) {
        return ContentKind::Text;
    }
    // Still unknown, so fall back to the CONTENT. A file with no extension (or a wrong one) that
    // is really a picture used to land on the info card, which reads as "we can't open this" when
    // in fact every decoder we own would have handled it. The decoders all content-sniff anyway;
    // this only gets them the chance to run.
    if looks_like_image(path) {
        return ContentKind::Image;
    }
    ContentKind::InfoCard
}

/// Magic-number sniff for the common raster containers, used ONLY as the last resort in
/// [`classify`] when the extension told us nothing.
///
/// Deliberately a short list of unambiguous, fixed-offset signatures rather than a general
/// detector: this runs on the UI thread, and being wrong here costs a decode attempt that ends in
/// the same info card we would have shown anyway.
fn looks_like_image(path: &str) -> bool {
    let Some((head, _)) = read_capped(path, 64) else {
        return false;
    };
    magic_is_image(&head)
}

/// The signature table behind [`looks_like_image`], split out so it is testable without a file.
fn magic_is_image(h: &[u8]) -> bool {
    let at = |off: usize, sig: &[u8]| h.len() >= off + sig.len() && &h[off..off + sig.len()] == sig;
    at(0, b"\x89PNG\r\n\x1a\n")                                   // PNG / APNG
        || at(0, b"\xFF\xD8\xFF")                                 // JPEG
        || at(0, b"GIF87a")
        || at(0, b"GIF89a")
        || at(0, b"BM")                                           // BMP
        || (at(0, b"RIFF") && at(8, b"WEBP"))
        || at(0, b"qoif")                                         // QOI
        || at(4, b"ftypavif")                                     // AVIF
        || at(4, b"ftypheic")
        || at(4, b"ftypheix")
        || at(4, b"ftypmif1")                                     // HEIF
        || at(0, b"II*\0")                                        // TIFF little-endian
        || at(0, b"MM\0*") // TIFF big-endian
}

/// Read a text/code file for preview: cap at 5 MB, reject binaries, decode (BOM-aware, lossy),
/// truncate absurdly long lines, and mark a capped file. `None` if unreadable or binary.
pub(super) fn read_text(path: &str) -> Option<String> {
    const CAP: usize = 5 * 1024 * 1024;
    let (bytes, capped) = read_capped(path, CAP)?;
    if is_binary(&bytes) {
        return None;
    }
    let mut text = truncate_long_lines(&decode_text(&bytes), 10_000);
    if capped {
        text.push_str("\n\n… (file truncated at 5 MB)");
    }
    Some(text)
}

/// Like [`read_text`] but WITHOUT the long-line truncation — for structured documents
/// (CSV/TSV/`.ipynb`) that must be parsed whole. A minified single-line notebook JSON or a wide
/// CSV row would otherwise be cut at 10 000 chars, breaking the parse. Same 5 MB cap + binary
/// reject + BOM-aware decode. `None` if unreadable or binary.
pub(super) fn read_doc(path: &str) -> Option<String> {
    const CAP: usize = 5 * 1024 * 1024;
    let (bytes, _capped) = read_capped(path, CAP)?;
    if is_binary(&bytes) {
        return None;
    }
    Some(decode_text(&bytes))
}

/// Extensions shown as a file LISTING (container formats with no cover/thumbnail — deliberately
/// NOT comics/ebooks/office, which already preview their embedded image). The long tail here is
/// all just zip-in-disguise: appx/msix (Windows packages), xapk (split APKs), oxt (LibreOffice
/// extensions) — `list_archive` sniffs the signature, so a mislabeled file falls through safely.
pub(super) fn is_archive_ext(ext: &str) -> bool {
    matches!(
        ext,
        "zip"
            | "7z"
            | "rar"
            | "jar"
            | "apk"
            | "war"
            | "xpi"
            | "whl"
            | "nupkg"
            | "vsix"
            | "ipa"
            | "aar"
            | "appx"
            | "msix"
            | "appxbundle"
            | "msixbundle"
            | "xapk"
            | "oxt"
    )
}

/// Read an archive and format its entries (name + size) as a scrollable text listing, sorted with
/// directories first then case-insensitively by path. Never extracts (header/central-dir read only).
/// `None` if unreadable, not a recognized archive, or larger than the read cap (keeps the UI snappy).
pub(super) fn archive_listing(path: &str) -> Option<String> {
    // Cap the in-memory read: list_archive needs the whole byte slice, and this runs on the UI
    // thread. 64 MB covers the vast majority of previewed .zip/.jar/.apk without a visible hang.
    const CAP: u64 = 64 * 1024 * 1024;
    if std::fs::metadata(path).ok()?.len() > CAP {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let mut entries = sagethumbs2k_core::list_archive(&bytes)?;
    entries.sort_by(|a, b| {
        b.2.cmp(&a.2) // directories (is_dir=true) first
            .then_with(|| a.0.to_ascii_lowercase().cmp(&b.0.to_ascii_lowercase()))
    });
    let files = entries.iter().filter(|e| !e.2).count();
    let total: u64 = entries.iter().map(|e| e.1).sum();
    let name = std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut out = format!(
        "{name}\n{files} file(s) · {} uncompressed\n\n",
        human_size(total)
    );
    for (n, sz, is_dir) in &entries {
        if *is_dir {
            out.push_str(&format!("             {}/\n", n.trim_end_matches('/')));
        } else {
            out.push_str(&format!("{:>10}   {n}\n", human_size(*sz)));
        }
    }
    Some(out)
}

/// Human-readable byte size (B/KB/MB/GB/TB, one decimal above bytes).
pub(super) fn human_size(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let (mut v, mut i) = (b as f64, 0usize);
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

/// Quick "is this a text file" sniff for unknown extensions: read the first 16 KB and treat it
/// as text unless it has two consecutive NUL bytes (the standard binary heuristic).
fn looks_like_text(path: &str) -> bool {
    match read_capped(path, 16 * 1024) {
        Some((bytes, _)) => !bytes.is_empty() && !is_binary(&bytes),
        None => false,
    }
}

/// Read up to `cap` bytes of `path`; the bool is whether the file was longer (i.e. truncated).
fn read_capped(path: &str, cap: usize) -> Option<(Vec<u8>, bool)> {
    use std::io::Read;
    let f = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    // Read one byte past the cap so we can tell "exactly cap" from "longer than cap".
    f.take(cap as u64 + 1).read_to_end(&mut buf).ok()?;
    let capped = buf.len() > cap;
    buf.truncate(cap);
    Some((buf, capped))
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
/// [`decode_legacy`].
///
/// The UTF-8-lossy-everything shortcut this replaced turned every non-Unicode CJK file into a
/// solid wall of U+FFFD: GBK/GB18030 is still a Chinese national standard, and Shift-JIS,
/// Big5 and EUC-KR are all over real-world `.txt`/`.csv`/`.srt` files. Those users saw
/// nothing but replacement characters.
fn decode_text(bytes: &[u8]) -> String {
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
        Err(_) => decode_legacy(bytes),
    }
}

/// Decode `bytes` as UTF-32 (`le` picks the byte order); a trailing partial unit and any invalid
/// scalar (a surrogate, or above U+10FFFF) become U+FFFD rather than failing the whole file.
/// BOM-only, never sniffed: a BOM-less UTF-32 file is vanishingly rare and guessing one would
/// misread ordinary ASCII that happens to contain NULs.
fn utf32(bytes: &[u8], le: bool) -> String {
    bytes
        .chunks_exact(4)
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
    let u: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&u)
}

/// Decode `bytes` as UTF-16BE (odd trailing byte dropped).
fn utf16_be(bytes: &[u8]) -> String {
    let u: Vec<u16> = bytes
        .chunks_exact(2)
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
    for c in head.chunks_exact(2) {
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
/// Latin-1 file with isolated accented characters fails that immediately.
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
    if let Some((_, s)) = best {
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
    let bonus = if dominant { 1_000 } else { 0 };
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

/// Kick off an async decode of `path` on a detached worker thread. The result (or `None`
/// on failure/timeout) is posted back to `hwnd` as `WM_APP_RENDER` carrying a boxed
/// `(gen, Option<DecodedRgba>)`; `gen` lets the UI thread drop a stale result after the
/// user has already switched files. The UI thread NEVER blocks on the decode.
pub(super) unsafe fn spawn_decode(hwnd: HWND, path: String, gen: u64) {
    let hwnd_raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        // Reconstruct the HWND inside the worker (HWND isn't `Send`; the raw pointer is).
        let hwnd = HWND(hwnd_raw as *mut c_void);
        // Formats that stream + downscale off the file handle (OpenEXR) skip the read
        // entirely — a 12K render pass is past every in-memory cap. Never animated, so
        // this can post the single-frame result straight away.
        if let Some(decoded) = streamed_decode(&path) {
            let payload: Box<(u64, Option<DecodedRgba>)> = Box::new((gen, Some(decoded)));
            let raw = Box::into_raw(payload);
            if PostMessageW(
                Some(hwnd),
                WM_APP_RENDER,
                WPARAM(gen as usize),
                LPARAM(raw as isize),
            )
            .is_err()
            {
                drop(Box::from_raw(raw));
            }
            return;
        }
        // One bounded/path-aware read for BOTH the animation probe and static fallback.
        // This also gives the standalone viewer the core's PSD/PSB/Blender head-preview and
        // oversized streamed-cover fast paths instead of blindly buffering the whole file.
        let bytes = sagethumbs2k_core::decode::read_preview_capped(&path).ok();
        // Animated GIF/APNG/animated-WebP → post the whole frame list (WM_APP_ANIM). A static
        // file of the same extension returns None and falls through to the single-frame path.
        let ext = std::path::Path::new(&path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if matches!(ext.as_str(), "gif" | "png" | "apng" | "webp") {
            if let Some(bytes) = bytes.as_deref() {
                if let Some(frames) = super::anim::decode_animation(bytes, &ext) {
                    let payload: Box<(u64, Vec<(DecodedRgba, u32)>)> = Box::new((gen, frames));
                    let raw = Box::into_raw(payload);
                    if PostMessageW(
                        Some(hwnd),
                        WM_APP_ANIM,
                        WPARAM(gen as usize),
                        LPARAM(raw as isize),
                    )
                    .is_err()
                    {
                        drop(Box::from_raw(raw));
                    }
                    return;
                }
            }
        }
        let decoded = bytes.and_then(decode_loaded);
        let payload: Box<(u64, Option<DecodedRgba>)> = Box::new((gen, decoded));
        let raw = Box::into_raw(payload);
        if PostMessageW(
            Some(hwnd),
            WM_APP_RENDER,
            WPARAM(gen as usize),
            LPARAM(raw as isize),
        )
        .is_err()
        {
            // Window died between the decode and the post — reclaim the box so it can't leak.
            drop(Box::from_raw(raw));
        }
    });
}

/// Synchronous decode for the headless `--shot` path (off the UI hot path, no worker).
pub(super) fn decode_sync(path: &str) -> Option<DecodedRgba> {
    read_and_decode(path)
}

/// Markdown remote-image fetch cap: badges are a few KB, hotlinked art rarely tops 8 MB.
const MD_IMG_MAX_BYTES: usize = 8 * 1024 * 1024;
/// Per-phase network timeout for one markdown image (seconds).
const MD_IMG_TIMEOUT_SECS: u64 = 8;

/// Fetch + decode one REMOTE markdown image on a worker thread (opt-in toggle path).
/// HTTPS-only + byte-capped via `http_fetch_capped`; decode is budget-bounded; the result
/// posts back as `WM_APP_MDIMG` with `Box<(gen, src, Option<DecodedRgba>)>` (a stale `gen`
/// is dropped by the handler). The UI thread never blocks.
pub(super) unsafe fn spawn_md_img(hwnd: HWND, src: String, gen: u64) {
    let hwnd_raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        let hwnd = HWND(hwnd_raw as *mut c_void);
        let decoded =
            crate::sponsors::http_fetch_capped(&src, false, MD_IMG_MAX_BYTES, MD_IMG_TIMEOUT_SECS)
                .and_then(decode_preview_budgeted)
                .map(|img| {
                    // Same display-cap policy as local markdown images (bounds the cached DIB).
                    let img = if img.width() > 2048 || img.height() > 4096 {
                        img.thumbnail(2048, 4096)
                    } else {
                        img
                    };
                    let rgba = img.to_rgba8();
                    let (w, h) = (rgba.width() as i32, rgba.height() as i32);
                    DecodedRgba {
                        w,
                        h,
                        rgba: rgba.into_raw(),
                    }
                });
        let payload: Box<(u64, String, Option<DecodedRgba>)> = Box::new((gen, src, decoded));
        let raw = Box::into_raw(payload);
        if PostMessageW(
            Some(hwnd),
            super::window::WM_APP_MDIMG,
            WPARAM(gen as usize),
            LPARAM(raw as isize),
        )
        .is_err()
        {
            drop(Box::from_raw(raw)); // window died before the post — reclaim
        }
    });
}

/// Decode PDF `page` (0-based) via the OS renderer + fetch the page count, posting the count
/// (`WM_APP_PDFINFO`) and then the page image (`WM_APP_RENDER`, reusing the normal install path).
pub(super) unsafe fn spawn_decode_pdf(hwnd: HWND, path: String, page: u32, gen: u64) {
    let hwnd_raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        let hwnd = HWND(hwnd_raw as *mut c_void);
        let rendered = sagethumbs2k_core::decode::read_capped(&path)
            .ok()
            .and_then(|bytes| sagethumbs2k_core::pdf::render_page_counted(&bytes, page, 1600));
        let (rgba, count) = match rendered {
            Some((png, count)) => {
                let d = image::load_from_memory(&png).ok().map(|img| {
                    let rgba = img.to_rgba8();
                    let (w, h) = (rgba.width() as i32, rgba.height() as i32);
                    DecodedRgba {
                        w,
                        h,
                        rgba: rgba.into_raw(),
                    }
                });
                (d, Some(count))
            }
            None => (None, None),
        };
        if let Some(c) = count {
            let cb: Box<(u64, u32)> = Box::new((gen, c));
            let raw = Box::into_raw(cb);
            if PostMessageW(
                Some(hwnd),
                WM_APP_PDFINFO,
                WPARAM(gen as usize),
                LPARAM(raw as isize),
            )
            .is_err()
            {
                drop(Box::from_raw(raw));
            }
        }
        let payload: Box<(u64, Option<DecodedRgba>)> = Box::new((gen, rgba));
        let raw = Box::into_raw(payload);
        if PostMessageW(
            Some(hwnd),
            WM_APP_RENDER,
            WPARAM(gen as usize),
            LPARAM(raw as isize),
        )
        .is_err()
        {
            drop(Box::from_raw(raw));
        }
    });
}

/// Read the file and run the budgeted decoder, converting the result to tight RGBA8.
fn read_and_decode(path: &str) -> Option<DecodedRgba> {
    // Formats that stream + downscale off the file handle (OpenEXR) never go
    // through the bounded whole-file read — a 12K render pass is past every cap.
    if let Some(img) = streamed_decode(path) {
        return Some(img);
    }
    let bytes = sagethumbs2k_core::decode::read_preview_capped(path).ok()?;
    decode_loaded(bytes)
}

/// The by-path streaming decode (see `decode::decode_preview_streamed`), converted
/// to tight RGBA8. `None` when the path isn't one of those formats.
fn streamed_decode(path: &str) -> Option<DecodedRgba> {
    let img = sagethumbs2k_core::decode::decode_preview_streamed(
        path,
        sagethumbs2k_core::decode::EXR_PATH_EDGE,
    )?;
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width() as i32, rgba.height() as i32);
    Some(DecodedRgba {
        w,
        h,
        rgba: rgba.into_raw(),
    })
}

/// Decode bytes already acquired by the path-aware reader. Keeping this separate lets the
/// animation probe fall through without issuing a second file read for ordinary PNG/WebP/GIF.
fn decode_loaded(bytes: Vec<u8>) -> Option<DecodedRgba> {
    let img = decode_preview_budgeted(bytes)?;
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width() as i32, rgba.height() as i32);
    Some(DecodedRgba {
        w,
        h,
        rgba: rgba.into_raw(),
    })
}

/// Run `decode::decode_preview` on a detached sub-thread, returning its result only if it
/// finishes within [`DECODE_BUDGET`]. On timeout returns `None` and abandons the
/// sub-thread (it sends into a dropped channel and exits on its own). The sub-thread holds
/// a COM MTA apartment because the WIC decode tier (HEIC/RAW/JPEG-XR) needs it — verbatim
/// from `previewhandler::decode_preview_budgeted`, minus the DLL `ModuleRef` pin (this is
/// an EXE, not the shell-loaded DLL).
fn decode_preview_budgeted(bytes: Vec<u8>) -> Option<image::DynamicImage> {
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let inited = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        let out = sagethumbs2k_core::decode::decode_preview(&bytes).ok();
        if inited {
            unsafe { CoUninitialize() };
        }
        let _ = tx.send(out);
    });
    rx.recv_timeout(DECODE_BUDGET).ok().flatten()
}

/// Build a top-down 32bpp DIB of `rgba` composited over the opaque `bg` (`COLORREF`
/// 0x00BBGGRR), so painting is a plain `StretchBlt`. `None` on a malformed size /
/// allocation failure (never panics on attacker-controlled dims). Verbatim port of
/// `previewhandler::make_dib`.
pub(super) unsafe fn make_dib(iw: i32, ih: i32, rgba: &[u8], bg: u32) -> Option<HBITMAP> {
    if iw <= 0 || ih <= 0 {
        return None;
    }
    let px = (iw as usize).checked_mul(ih as usize)?;
    if rgba.len() < px.checked_mul(4)? {
        return None;
    }
    let mut bmi = BITMAPINFO::default();
    bmi.bmiHeader.biSize = core::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = iw;
    bmi.bmiHeader.biHeight = -ih; // top-down
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = 0; // BI_RGB

    let mut bits: *mut c_void = core::ptr::null_mut();
    let hbmp = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
    if bits.is_null() {
        let _ = DeleteObject(hbmp.into());
        return None;
    }
    let (bg_r, bg_g, bg_b) = (bg & 0xFF, (bg >> 8) & 0xFF, (bg >> 16) & 0xFF);
    let dst = core::slice::from_raw_parts_mut(bits as *mut u8, px * 4);
    for i in 0..px {
        let r = rgba[i * 4] as u32;
        let g = rgba[i * 4 + 1] as u32;
        let b = rgba[i * 4 + 2] as u32;
        let a = rgba[i * 4 + 3] as u32;
        let comp = |s: u32, d: u32| (((s * a) + (d * (255 - a)) + 127) / 255) as u8;
        dst[i * 4] = comp(b, bg_b); // B
        dst[i * 4 + 1] = comp(g, bg_g); // G
        dst[i * 4 + 2] = comp(r, bg_r); // R
        dst[i * 4 + 3] = 255;
    }
    Some(hbmp)
}

/// The same DIB, but for the MAIN image pane, where transparency has to survive to paint time so a
/// checkerboard can show through it. Returns `(bitmap, has_alpha)`.
///
/// When the image is fully opaque this produces byte-identical output to [`make_dib`] and the
/// caller keeps using the plain `StretchBlt` path, so the overwhelmingly common case (a photo) is
/// completely unchanged, HALFTONE downscaling included. Only when some pixel is actually
/// translucent does it emit PREMULTIPLIED BGRA for `AlphaBlend`; premultiplied data is also what
/// makes the intermediate `StretchBlt` in [`paint_image`] filter correctly, since premultiplied
/// channels are linearly interpolatable and straight ones are not.
pub(super) unsafe fn make_render(iw: i32, ih: i32, rgba: &[u8], bg: u32) -> Option<RenderData> {
    if iw <= 0 || ih <= 0 {
        return None;
    }
    let px = (iw as usize).checked_mul(ih as usize)?;
    if rgba.len() < px.checked_mul(4)? {
        return None;
    }
    let has_alpha = (0..px).any(|i| rgba[i * 4 + 3] != 255);
    if !has_alpha {
        return make_dib(iw, ih, rgba, bg).map(|h| RenderData::opaque(h, iw, ih));
    }
    let mut bmi = BITMAPINFO::default();
    bmi.bmiHeader.biSize = core::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = iw;
    bmi.bmiHeader.biHeight = -ih; // top-down
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = 0; // BI_RGB

    let mut bits: *mut c_void = core::ptr::null_mut();
    let hbmp = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
    if bits.is_null() {
        let _ = DeleteObject(hbmp.into());
        return None;
    }
    let dst = core::slice::from_raw_parts_mut(bits as *mut u8, px * 4);
    for i in 0..px {
        let a = rgba[i * 4 + 3] as u32;
        let pm = |s: u8| (((s as u32 * a) + 127) / 255) as u8;
        dst[i * 4] = pm(rgba[i * 4 + 2]); // B
        dst[i * 4 + 1] = pm(rgba[i * 4 + 1]); // G
        dst[i * 4 + 2] = pm(rgba[i * 4]); // R
        dst[i * 4 + 3] = a as u8;
    }
    Some(RenderData {
        hbmp,
        iw,
        ih,
        alpha: true,
        src: bits as *const u8,
        scaled: RefCell::new(None),
    })
}

/// A `dw`x`dh` copy of `rd`, box-filtered (a true area average), cached until the size changes.
/// `None` when it is not worth doing or could not be built, and the caller then lets GDI scale.
///
/// **Why this exists.** A translucent image cannot go through `StretchBlt`'s good `HALFTONE`
/// filter, because that mode treats the surface as plain RGB and destroys the alpha byte. The only
/// GDI call that respects alpha is `AlphaBlend`, and it ignores the stretch mode entirely, so it
/// point-samples when shrinking. Measured on a concentric-ring test pattern at a 3x downscale:
/// `AlphaBlend` produced 58% more spurious high-frequency energy than the true area average, versus
/// `HALFTONE`'s 21%, i.e. visible aliasing on fine detail. Averaging the pixels here fixes that and
/// is actually CLOSER to ground truth than `HALFTONE` is.
///
/// Only used when SHRINKING. Enlarging point-samples, which is what you want for inspecting pixels.
unsafe fn scaled_for(rd: &RenderData, dw: i32, dh: i32) -> Option<HBITMAP> {
    if rd.src.is_null() || dw <= 0 || dh <= 0 || dw >= rd.iw || dh >= rd.ih {
        return None;
    }
    if let Some((cw, ch, h)) = *rd.scaled.borrow() {
        if cw == dw && ch == dh {
            return Some(h);
        }
    }
    let mut bmi = BITMAPINFO::default();
    bmi.bmiHeader.biSize = core::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = dw;
    bmi.bmiHeader.biHeight = -dh;
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = 0;
    let mut bits: *mut c_void = core::ptr::null_mut();
    let out = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
    if bits.is_null() {
        let _ = DeleteObject(out.into());
        return None;
    }
    let (sw, sh) = (rd.iw as usize, rd.ih as usize);
    let src = core::slice::from_raw_parts(rd.src, sw * sh * 4);
    let dst = core::slice::from_raw_parts_mut(bits as *mut u8, (dw * dh) as usize * 4);
    // Source span of each destination row/column, precomputed so the inner loop stays tight.
    let xs: Vec<(usize, usize)> = (0..dw as usize)
        .map(|x| {
            let a = x * sw / dw as usize;
            let b = ((x + 1) * sw).div_ceil(dw as usize).min(sw);
            (a, b.max(a + 1))
        })
        .collect();
    for y in 0..dh as usize {
        let y0 = y * sh / dh as usize;
        let y1 = (((y + 1) * sh).div_ceil(dh as usize)).min(sh).max(y0 + 1);
        for (x, &(x0, x1)) in xs.iter().enumerate() {
            let (mut b, mut g, mut r, mut a) = (0u32, 0u32, 0u32, 0u32);
            for sy in y0..y1 {
                let row = sy * sw * 4;
                for sx in x0..x1 {
                    let i = row + sx * 4;
                    b += src[i] as u32;
                    g += src[i + 1] as u32;
                    r += src[i + 2] as u32;
                    a += src[i + 3] as u32;
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            let o = (y * dw as usize + x) * 4;
            dst[o] = (b / n) as u8;
            dst[o + 1] = (g / n) as u8;
            dst[o + 2] = (r / n) as u8;
            dst[o + 3] = (a / n) as u8;
        }
    }
    if let Some((_, _, old)) = rd.scaled.borrow_mut().replace((dw, dh, out)) {
        let _ = DeleteObject(old.into());
    }
    Some(out)
}

/// Aspect-fit scale (image px -> screen px) of `rd` inside `(cw, ch)`. Shared by the paint
/// and the zoom-at-cursor math so they never disagree.
pub(super) fn fit_scale(iw: i32, ih: i32, cw: i32, ch: i32) -> f64 {
    if iw <= 0 || ih <= 0 || cw <= 0 || ch <= 0 {
        return 1.0;
    }
    f64::min(cw as f64 / iw as f64, ch as f64 / ih as f64)
}

/// Paint the image `rd` into `rc`, letterboxed with `bg`, at `zoom`x the aspect-fit scale and
/// offset by `pan` (device px). `zoom == 1.0`, `pan == (0,0)` is the plain aspect-fit centered
/// draw. Ported from `previewhandler::draw` (fill = letterbox, then `HALFTONE` `StretchBlt`).
pub(super) unsafe fn paint_image(
    hdc: HDC,
    rc: &RECT,
    rd: &RenderData,
    bg: u32,
    zoom: f64,
    pan: (i32, i32),
    checker: Option<i32>,
) {
    let brush = CreateSolidBrush(COLORREF(bg));
    FillRect(hdc, rc, brush);
    let _ = DeleteObject(brush.into());

    let cw = rc.right - rc.left;
    let ch = rc.bottom - rc.top;
    if cw <= 0 || ch <= 0 || rd.iw <= 0 || rd.ih <= 0 {
        return;
    }
    let scale = fit_scale(rd.iw, rd.ih, cw, ch) * zoom;
    let dw = ((rd.iw as f64 * scale).round() as i32).max(1);
    let dh = ((rd.ih as f64 * scale).round() as i32).max(1);
    let dx = rc.left + (cw - dw) / 2 + pan.0;
    let dy = rc.top + (ch - dh) / 2 + pan.1;

    let memdc = CreateCompatibleDC(Some(hdc));
    let old = SelectObject(memdc, rd.hbmp.into());
    SetStretchBltMode(hdc, HALFTONE);
    // The branch MUST key on `rd.alpha`, never on the checkerboard setting. A translucent bitmap
    // holds PREMULTIPLIED, un-composited pixels (see `make_render`), and `StretchBlt` ignores the
    // alpha byte entirely, so blitting one paints the premultiplied values as if they were opaque:
    // a 50%-alpha white pixel lands as mid-grey. Turning the checkerboard OFF must therefore still
    // take the blend path, just without the pattern under it (the flat `bg` fill above is what it
    // composites onto instead).
    match rd.alpha {
        // Translucent: optional checkerboard, then compose the premultiplied bitmap over it.
        //
        // `AlphaBlend` does its own scaling and ignores the stretch mode, and there is no way to
        // pre-scale through HALFTONE first: `StretchBlt` in HALFTONE mode treats the surface as
        // plain RGB and DESTROYS the alpha byte, so an intermediate scratch surface comes out
        // fully transparent and nothing draws at all (measured, not assumed). So the blend reads
        // straight from the source bitmap. Only translucent images take this path.
        true => {
            if let Some(cell) = checker {
                let (c0, c1) = sagethumbs2k_core::checker::checker_shades(bg);
                let cr = RECT {
                    left: dx,
                    top: dy,
                    right: dx + dw,
                    bottom: dy + dh,
                };
                sagethumbs2k_core::checker::fill_checker(hdc, &cr, c0, c1, cell);
            }
            let bf = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            // Shrinking: area-average it ourselves first and blend 1:1, because AlphaBlend's own
            // scaler aliases badly (see `scaled_for`). Enlarging, or if the scratch could not be
            // built, blend straight from the source.
            match scaled_for(rd, dw, dh) {
                Some(s) => {
                    let sdc = CreateCompatibleDC(Some(hdc));
                    let olds = SelectObject(sdc, s.into());
                    let _ = AlphaBlend(hdc, dx, dy, dw, dh, sdc, 0, 0, dw, dh, bf);
                    SelectObject(sdc, olds);
                    let _ = DeleteDC(sdc);
                }
                None => {
                    let _ = AlphaBlend(hdc, dx, dy, dw, dh, memdc, 0, 0, rd.iw, rd.ih, bf);
                }
            }
        }
        // Opaque: unchanged from before any of this existed. `make_render` produced byte-identical
        // output to the old `make_dib` for these, so photos take exactly the old HALFTONE path.
        false => {
            let _ = StretchBlt(
                hdc,
                dx,
                dy,
                dw,
                dh,
                Some(memdc),
                0,
                0,
                rd.iw,
                rd.ih,
                SRCCOPY,
            );
        }
    }
    SelectObject(memdc, old);
    let _ = DeleteDC(memdc);
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
        assert_eq!(decode_text("hello — 世界 🌏".as_bytes()), "hello — 世界 🌏");
        assert_eq!(decode_text(b"plain ascii\n"), "plain ascii\n");
        assert_eq!(decode_text(b""), "");
    }

    #[test]
    fn strips_boms() {
        let mut b = vec![0xEF, 0xBB, 0xBF];
        b.extend_from_slice("héllo".as_bytes());
        assert_eq!(decode_text(&b), "héllo");

        let mut le = vec![0xFF, 0xFE];
        le.extend("hi".encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(decode_text(&le), "hi");

        let mut be = vec![0xFE, 0xFF];
        be.extend("hi".encode_utf16().flat_map(u16::to_be_bytes));
        assert_eq!(decode_text(&be), "hi");
    }

    #[test]
    fn bomless_utf16_is_sniffed() {
        let text = "the quick brown fox jumps over the lazy dog";
        let le: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(decode_text(&le), text);
        let be: Vec<u8> = text.encode_utf16().flat_map(u16::to_be_bytes).collect();
        assert_eq!(decode_text(&be), text);
    }

    /// GBK is still a Chinese national standard; before this these bytes were a wall of U+FFFD.
    #[test]
    fn gbk_chinese_decodes() {
        const GBK: &[u8] = &[
            0xC4, 0xE3, 0xBA, 0xC3, 0xA3, 0xAC, 0xCA, 0xC0, 0xBD, 0xE7, 0xA3, 0xA1, 0xD5, 0xE2,
            0xCA, 0xC7, 0xD2, 0xBB, 0xB8, 0xF6, 0xB2, 0xE2, 0xCA, 0xD4, 0xCE, 0xC4, 0xBC, 0xFE,
            0xA1, 0xA3,
        ];
        assert_eq!(decode_text(GBK), "你好，世界！这是一个测试文件。");
    }

    /// Kana is the signal that keeps Shift-JIS from being read as GBK.
    #[test]
    fn shift_jis_japanese_decodes() {
        const SJIS: &[u8] = &[
            0x82, 0xB1, 0x82, 0xEA, 0x82, 0xCD, 0x93, 0xFA, 0x96, 0x7B, 0x8C, 0xEA, 0x82, 0xCC,
            0x83, 0x65, 0x83, 0x4C, 0x83, 0x58, 0x83, 0x67, 0x82, 0xC5, 0x82, 0xB7, 0x81, 0x42,
        ];
        assert_eq!(decode_text(SJIS), "これは日本語のテキストです。");
    }

    /// Hangul is the equivalent signal for EUC-KR.
    #[test]
    fn euc_kr_korean_decodes() {
        const EUCKR: &[u8] = &[
            0xBE, 0xC8, 0xB3, 0xE7, 0xC7, 0xCF, 0xBC, 0xBC, 0xBF, 0xE4, 0x20, 0xC7, 0xD1, 0xB1,
            0xB9, 0xBE, 0xEE, 0x20, 0xC5, 0xD8, 0xBD, 0xBA, 0xC6, 0xAE, 0xC0, 0xD4, 0xB4, 0xCF,
            0xB4, 0xD9, 0x2E,
        ];
        assert_eq!(decode_text(EUCKR), "안녕하세요 한국어 텍스트입니다.");
    }

    /// The regression that matters in the other direction: ordinary accented Latin text must
    /// never be dragged into a CJK reading just because some table accepted the bytes.
    #[test]
    fn latin1_is_not_mangled_into_cjk() {
        const L1: &[u8] = &[
            0x43, 0x61, 0x66, 0xE9, 0x20, 0x72, 0xE9, 0x73, 0x75, 0x6D, 0xE9, 0x20, 0x6E, 0x61,
            0xEF, 0x76, 0x65, 0x20, 0x73, 0x65, 0xF1, 0x6F, 0x72,
        ];
        let out = decode_text(L1);
        assert!(!has_cjk(&out), "Latin-1 text decoded as CJK: {out:?}");
        assert!(out.starts_with("Caf"), "unexpected decode: {out:?}");
    }

    /// A pure-ASCII byte stream must come back byte-identical whatever the machine's ACP is.
    #[test]
    fn ascii_is_never_reinterpreted() {
        let src = "fn main() { println!(\"hi\"); }\n";
        assert_eq!(decode_text(src.as_bytes()), src);
    }

    #[test]
    fn cjk_score_rejects_halfwidth_katakana_soup() {
        // What Shift-JIS makes of arbitrary high bytes — must not qualify as CJK text.
        assert_eq!(cjk_score("ﾖﾐﾄﾄﾊﾟ"), 0);
        // Real Japanese does.
        assert!(cjk_score("これは日本語です") > 0);
    }
}
