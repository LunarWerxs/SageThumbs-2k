//! DjVu text layer — data model and parser.
//!
//! Defines the types shared by the text encoder, serialisers, and OCR
//! backends, and provides the pure [`parse_text_layer`] parser for TXTa
//! (plain) and TXTz (BZZ-compressed) chunks.
//!
//! ## Key types
//!
//! - [`TextLayer`] — full text content and zone hierarchy of a page
//! - [`TextZone`] — single zone node (page/column/para/line/word/char)
//! - [`TextZoneKind`] — enum discriminating zone types
//! - [`Rect`] — bounding rectangle in top-left-origin coordinates
//! - [`Paragraph`] — reflowable paragraph extracted from a [`TextLayer`]
//! - [`TextError`] — typed errors from text layer parsing
//!
//! ## Format notes
//!
//! The TXTa/TXTz binary format stores:
//!   `[u24be text_len][utf8 text][u8 version][zone tree]`
//!
//! Zone coordinates use DjVu's bottom-left origin. The parser remaps all
//! coordinates to a top-left origin using the provided page height.
//! Zone fields are delta-encoded relative to a parent or previous sibling.

#[cfg(not(feature = "std"))]
use alloc::{
    string::{String, ToString},
    vec::Vec,
};

use crate::info::Rotation;

// ---- Error ------------------------------------------------------------------

/// Errors from text layer parsing.
#[derive(Debug, thiserror::Error)]
pub enum TextError {
    /// The binary data is too short to be a valid text layer.
    #[error("text layer data too short")]
    TooShort,

    /// A text length field points past the end of the data.
    #[error("text length overflows data")]
    TextOverflow,

    /// The text bytes are not valid UTF-8.
    ///
    /// No longer produced since #524: invalid bytes are decoded leniently
    /// (CP1252 fallback). Kept so matching code keeps compiling.
    #[error("invalid UTF-8 in text layer")]
    InvalidUtf8,

    /// A zone record is truncated (not enough bytes for a field).
    #[error("zone record truncated at offset {0}")]
    ZoneTruncated(usize),

    /// An unknown zone type byte was encountered.
    #[error("unknown zone type {0}")]
    UnknownZoneType(u8),

    /// The zone hierarchy nests deeper than [`MAX_ZONE_DEPTH`] (#589).
    #[error("zone tree too deep (> {MAX_ZONE_DEPTH})")]
    ZoneTooDeep,
}

/// Maximum text-zone nesting depth (#589). The DjVu hierarchy is
/// page→column→region→para→line→word→character = 7 real levels; the generous
/// cap tolerates degenerate-but-legitimate nesting while stopping a crafted
/// single-child chain from overflowing the stack. Mirrors `MAX_NAVM_DEPTH`.
pub const MAX_ZONE_DEPTH: usize = 64;

/// Smallest possible child zone record in bytes: 1 type byte + five
/// 2-byte biased coordinates + a 3-byte `text_len` + a 3-byte
/// `children_count` = 17. Used to reject a `children_count` that cannot fit in
/// the remaining input before reserving for it (#589).
const MIN_ZONE_RECORD_BYTES: usize = 17;

// ---- Public types -----------------------------------------------------------

/// Zone type discriminant in the DjVu text layer hierarchy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum TextZoneKind {
    Page,
    Column,
    Region,
    Para,
    Line,
    Word,
    Character,
}

/// Bounding rectangle in top-left-origin coordinates (pixels).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// A single node in the text zone hierarchy.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TextZone {
    /// Zone type.
    pub kind: TextZoneKind,
    /// Bounding box (top-left origin, after coordinate remap).
    pub rect: Rect,
    /// Text covered by this zone (substring of [`TextLayer::text`]).
    pub text: String,
    /// Child zones (columns inside page, words inside line, etc.).
    pub children: Vec<TextZone>,
}

/// The complete text layer of a DjVu page.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TextLayer {
    /// Full plain-text content of the page, UTF-8.
    pub text: String,
    /// Top-level zone nodes (usually a single `Page` zone).
    pub zones: Vec<TextZone>,
}

/// A reflowable paragraph: the original lines as they appear on the page,
/// plus a single joined `text` string with line-break and hyphenation rules
/// applied (see [`TextLayer::reflowable_text`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Paragraph {
    /// Each physical line as it appeared on the page (trimmed of trailing
    /// whitespace, ordered top-to-bottom).
    pub lines: Vec<String>,
    /// Lines joined into a single string. Hyphenated line breaks (line ends
    /// with `-` and next line starts with a lowercase letter) are joined with
    /// no separator and the hyphen is dropped. Other line breaks become a
    /// single ASCII space.
    pub text: String,
}

// ---- TextLayer methods ------------------------------------------------------

impl TextLayer {
    /// Return a copy of this text layer with all zone rectangles transformed to
    /// match a rendered page of size `render_w × render_h`.
    ///
    /// - `page_w`, `page_h` — native page dimensions from the INFO chunk.
    /// - `rotation` — page rotation from the INFO chunk.
    /// - `render_w`, `render_h` — the pixel size of the rendered output.
    ///
    /// Applies rotation first (in native pixel space), then scales the result
    /// proportionally to the requested render size.  The text content is
    /// preserved unchanged.
    pub fn transform(
        &self,
        page_w: u32,
        page_h: u32,
        rotation: Rotation,
        render_w: u32,
        render_h: u32,
    ) -> Self {
        let (disp_w, disp_h) = match rotation {
            Rotation::Cw90 | Rotation::Ccw90 => (page_h, page_w),
            _ => (page_w, page_h),
        };
        let t = ZoneTransform {
            page_w,
            page_h,
            rotation,
            disp_w,
            disp_h,
            render_w,
            render_h,
        };
        let zones = self.zones.iter().map(|z| transform_zone(z, &t)).collect();
        TextLayer {
            text: self.text.clone(),
            zones,
        }
    }

    /// Group the page text into reading-order paragraphs (#228).
    ///
    /// Uses the DjVu zone separator characters carried in `self.text`:
    ///
    /// - `\x00` (NUL), `\x0b` (VT), `\x1d` (GS), `\x1f` (US) — paragraph /
    ///   region / column / page boundary. Each starts a new [`Paragraph`].
    /// - `\x0a` (LF) — line break within a paragraph.
    ///
    /// Line joining: trailing whitespace is dropped from each line. If a line
    /// ends with `-` and the next line starts with an ASCII lowercase letter,
    /// the hyphen is dropped and the lines are joined with no separator
    /// (soft-hyphen at line end). Otherwise lines are joined with a single
    /// ASCII space.
    ///
    /// Empty paragraphs (only whitespace) are skipped. The returned vector
    /// preserves zone-stream order — for the typical OCR'd single-column page
    /// this is reading order.
    pub fn reflowable_text(&self) -> Vec<Paragraph> {
        let mut out = Vec::new();
        for chunk in self
            .text
            .split(['\u{0000}', '\u{000b}', '\u{001d}', '\u{001f}'])
        {
            let lines: Vec<String> = chunk
                .split('\n')
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect();
            if lines.is_empty() {
                continue;
            }
            let text = join_paragraph_lines(&lines);
            out.push(Paragraph { lines, text });
        }
        out
    }
}

fn join_paragraph_lines(lines: &[String]) -> String {
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        if i == 0 {
            out.push_str(line);
            continue;
        }
        let prev_hyphen =
            out.ends_with('-') && line.chars().next().is_some_and(|c| c.is_ascii_lowercase());
        if prev_hyphen {
            out.pop();
            out.push_str(line);
        } else {
            out.push(' ');
            out.push_str(line);
        }
    }
    out
}

// ---- Rect methods -----------------------------------------------------------

impl Rect {
    /// Rotate this rectangle within a `page_w × page_h` native coordinate space.
    ///
    /// Coordinates are in top-left origin.  Returns the transformed rect in the
    /// rotated display space (which has dimensions `page_h × page_w` for 90°
    /// rotations and `page_w × page_h` for 0°/180°).
    pub fn rotate(&self, page_w: u32, page_h: u32, rotation: Rotation) -> Self {
        match rotation {
            Rotation::None => self.clone(),
            Rotation::Rot180 => Rect {
                x: page_w.saturating_sub(self.x.saturating_add(self.width)),
                y: page_h.saturating_sub(self.y.saturating_add(self.height)),
                width: self.width,
                height: self.height,
            },
            // Clockwise 90°: displayed page is page_h wide × page_w tall.
            // (x, y, w, h) → (page_h - y - h,  x,  h,  w)
            Rotation::Cw90 => Rect {
                x: page_h.saturating_sub(self.y.saturating_add(self.height)),
                y: self.x,
                width: self.height,
                height: self.width,
            },
            // Counter-clockwise 90°: displayed page is page_h wide × page_w tall.
            // (x, y, w, h) → (y,  page_w - x - w,  h,  w)
            Rotation::Ccw90 => Rect {
                x: self.y,
                y: page_w.saturating_sub(self.x.saturating_add(self.width)),
                width: self.height,
                height: self.width,
            },
        }
    }

    /// Scale this rectangle from a `from_w × from_h` space to `to_w × to_h`.
    pub fn scale(&self, from_w: u32, from_h: u32, to_w: u32, to_h: u32) -> Self {
        if from_w == 0 || from_h == 0 {
            return self.clone();
        }
        Rect {
            x: (self.x as u64 * to_w as u64 / from_w as u64) as u32,
            y: (self.y as u64 * to_h as u64 / from_h as u64) as u32,
            width: (self.width as u64 * to_w as u64 / from_w as u64) as u32,
            height: (self.height as u64 * to_h as u64 / from_h as u64) as u32,
        }
    }
}

// ---- Private zone transform helpers -----------------------------------------

/// Parameters for `transform_zone` — groups the 7 invariants so we stay
/// under clippy's `too_many_arguments` limit.
struct ZoneTransform {
    page_w: u32,
    page_h: u32,
    rotation: Rotation,
    disp_w: u32,
    disp_h: u32,
    render_w: u32,
    render_h: u32,
}

fn transform_zone(zone: &TextZone, t: &ZoneTransform) -> TextZone {
    let rotated = zone.rect.rotate(t.page_w, t.page_h, t.rotation);
    let scaled = rotated.scale(t.disp_w, t.disp_h, t.render_w, t.render_h);
    let children = zone.children.iter().map(|c| transform_zone(c, t)).collect();
    TextZone {
        kind: zone.kind,
        rect: scaled,
        text: zone.text.clone(),
        children,
    }
}

// ---- Entry point ------------------------------------------------------------

/// Parse a decoded text-layer payload (the bytes of a `TXTa` chunk, or a
/// BZZ-decompressed `TXTz` chunk).
///
/// This is a pure parser: BZZ decompression for `TXTz` is handled upstream by
/// [`DjVuPage::chunk_payload`](crate::DjVuPage::chunk_payload).  `page_height`
/// is used to remap DjVu bottom-left coordinates to top-left.
pub fn parse_text_layer(data: &[u8], page_height: u32) -> Result<TextLayer, TextError> {
    parse_text_layer_inner(data, page_height)
}

// ---- Internal parsing -------------------------------------------------------

fn parse_text_layer_inner(data: &[u8], page_height: u32) -> Result<TextLayer, TextError> {
    if data.len() < 3 {
        return Err(TextError::TooShort);
    }

    let mut pos = 0usize;

    // Read text length (u24be)
    let text_len = read_u24(data, &mut pos).ok_or(TextError::TooShort)?;

    // Read the text blob. Nominally UTF-8; legacy files carry CP1252 bytes
    // (#524), so decode leniently instead of aborting the whole layer. Zone
    // records index this blob by ON-DISK BYTE offset, so when the bytes are
    // not valid UTF-8 the zones must slice the original bytes (and decode
    // each slice), never a re-encoded String whose offsets no longer line up.
    let text_end = pos.checked_add(text_len).ok_or(TextError::TextOverflow)?;
    if text_end > data.len() {
        return Err(TextError::TextOverflow);
    }
    let text_bytes = data.get(pos..text_end).ok_or(TextError::TextOverflow)?;
    // One validation pass decides both views (review of #524: don't scan a
    // multi-megabyte blob twice).
    let (full_text, text) = match core::str::from_utf8(text_bytes) {
        Ok(s) => (FullText::Utf8(s), s.to_string()),
        Err(_) => (
            FullText::Legacy(text_bytes),
            crate::lenient_text::decode_lossy(text_bytes).into_owned(),
        ),
    };
    pos = text_end;

    // Consume version byte (if present)
    if pos < data.len() {
        pos += 1; // version byte — currently unused
    }

    // Parse zone tree
    let mut zones = Vec::new();
    if pos < data.len() {
        let zone = parse_zone(data, &mut pos, None, None, &full_text, page_height, 0)?;
        zones.push(zone);
    }

    Ok(TextLayer { text, zones })
}

/// The page's text blob as zone parsing sees it (#524).
///
/// Zone records address the text by on-disk byte offset, so the two variants
/// preserve exact offsets in both worlds: valid UTF-8 slices the `&str`
/// directly (with char-boundary clamping), legacy bytes are sliced raw and
/// each slice is decoded leniently on extraction.
enum FullText<'a> {
    Utf8(&'a str),
    Legacy(&'a [u8]),
}

// ---- Zone parsing -----------------------------------------------------------

/// Delta-encoding context carried from one zone parse to the next.
#[derive(Clone)]
struct ZoneCtx {
    x: i32,
    y: i32, // bottom-left y (DjVu native)
    width: i32,
    height: i32,
    text_start: i32,
    text_len: i32,
}

fn parse_zone(
    data: &[u8],
    pos: &mut usize,
    parent: Option<&ZoneCtx>,
    prev: Option<&ZoneCtx>,
    full_text: &FullText<'_>,
    page_height: u32,
    depth: usize,
) -> Result<TextZone, TextError> {
    if depth > MAX_ZONE_DEPTH {
        return Err(TextError::ZoneTooDeep);
    }
    if *pos >= data.len() {
        return Err(TextError::ZoneTruncated(*pos));
    }

    let type_byte = *data.get(*pos).ok_or(TextError::ZoneTruncated(*pos))?;
    *pos += 1;

    let kind = match type_byte {
        1 => TextZoneKind::Page,
        2 => TextZoneKind::Column,
        3 => TextZoneKind::Region,
        4 => TextZoneKind::Para,
        5 => TextZoneKind::Line,
        6 => TextZoneKind::Word,
        7 => TextZoneKind::Character,
        other => return Err(TextError::UnknownZoneType(other)),
    };

    let mut x = read_i16_biased(data, pos).ok_or(TextError::ZoneTruncated(*pos))?;
    let mut y = read_i16_biased(data, pos).ok_or(TextError::ZoneTruncated(*pos))?;
    let width = read_i16_biased(data, pos).ok_or(TextError::ZoneTruncated(*pos))?;
    let height = read_i16_biased(data, pos).ok_or(TextError::ZoneTruncated(*pos))?;
    let mut text_start = read_i16_biased(data, pos).ok_or(TextError::ZoneTruncated(*pos))?;
    let text_len = read_i24(data, pos).ok_or(TextError::ZoneTruncated(*pos))?;

    // Apply delta encoding (matches djvujs DjVuText.js decodeZone logic)
    if let Some(prev) = prev {
        match type_byte {
            1 | 4 | 5 => {
                // PAGE, PARAGRAPH, LINE
                x += prev.x;
                y = prev.y - (y + height);
            }
            _ => {
                // COLUMN, REGION, WORD, CHARACTER
                x += prev.x + prev.width;
                y += prev.y;
            }
        }
        text_start += prev.text_start + prev.text_len;
    } else if let Some(parent) = parent {
        x += parent.x;
        y = parent.y + parent.height - (y + height);
        text_start += parent.text_start;
    }

    // Remap y from DjVu bottom-left to top-left
    // top_left_y = page_height - (bl_y + height)
    let tl_y = (page_height as i32)
        .saturating_sub(y.saturating_add(height))
        .max(0) as u32;
    let tl_x = x.max(0) as u32;
    let tl_w = width.max(0) as u32;
    let tl_h = height.max(0) as u32;

    let rect = Rect {
        x: tl_x,
        y: tl_y,
        width: tl_w,
        height: tl_h,
    };

    // Extract zone text
    let ts = text_start.max(0) as usize;
    let tl = text_len.max(0) as usize;
    let zone_text = extract_text_slice(full_text, ts, tl);

    let children_count = read_i24(data, pos)
        .ok_or(TextError::ZoneTruncated(*pos))?
        .max(0) as usize;
    // Cap the up-front reservation to what the remaining bytes could actually
    // encode (#589): each child needs >= MIN_ZONE_RECORD_BYTES, so a crafted
    // `children_count` (i24, up to ~16.7M) can no longer reserve ~1.5 GB
    // before any child is read. The loop below still iterates the full
    // `children_count` and fails with `ZoneTruncated` on the first missing
    // child, so genuinely-truncated files error exactly as before — only the
    // allocation is bounded to O(remaining input).
    let remaining = data.len().saturating_sub(*pos);
    let reserve = children_count.min(remaining / MIN_ZONE_RECORD_BYTES);

    let ctx = ZoneCtx {
        x,
        y,
        width,
        height,
        text_start,
        text_len,
    };

    let mut children = Vec::with_capacity(reserve);
    let mut prev_child: Option<ZoneCtx> = None;

    for _ in 0..children_count {
        let child = parse_zone(
            data,
            pos,
            Some(&ctx),
            prev_child.as_ref(),
            full_text,
            page_height,
            depth + 1,
        )?;
        prev_child = Some(ZoneCtx {
            x: child.rect.x as i32,
            y: {
                // We need to store the original bottom-left y for delta calc.
                // Inverse remap: bl_y = page_height - (tl_y + height)
                (page_height as i32).saturating_sub(child.rect.y as i32 + child.rect.height as i32)
            },
            width: child.rect.width as i32,
            height: child.rect.height as i32,
            text_start: ts as i32,
            text_len: tl as i32,
        });
        children.push(child);
    }

    Ok(TextZone {
        kind,
        rect,
        text: zone_text,
        children,
    })
}

/// Extract a substring from `full_text` starting at byte offset `start` with byte length `len`.
///
/// UTF-8 text clamps to valid char boundaries to avoid panics on multi-byte
/// chars. Legacy (non-UTF-8) text slices the on-disk bytes exactly and
/// decodes the slice leniently (#524). Known tradeoff: when a legacy blob
/// contains a valid multi-byte UTF-8 run and a zone edge lands inside it,
/// that zone's text decodes the split bytes as CP1252 and can genuinely
/// differ from the same region of [`TextLayer::text`] (e.g. a zone covering
/// the second byte of "é" reads "©"). Byte-exact offsets for the dominant
/// pure-CP1252 case are worth that edge; no panic, no out-of-bounds either
/// way. Pinned by `test_legacy_zone_split_of_utf8_run_diverges`.
fn extract_text_slice(full_text: &FullText<'_>, start: usize, len: usize) -> String {
    match *full_text {
        FullText::Utf8(text) => {
            let end = start.saturating_add(len).min(text.len());
            let start = start.min(end);
            // Walk back to a valid char boundary
            let safe_start = (0..=start)
                .rev()
                .find(|&i| text.is_char_boundary(i))
                .unwrap_or(0);
            let safe_end = (end..=text.len())
                .find(|&i| text.is_char_boundary(i))
                .unwrap_or(text.len());
            text[safe_start..safe_end].to_string()
        }
        FullText::Legacy(bytes) => {
            let end = start.saturating_add(len).min(bytes.len());
            let start = start.min(end);
            crate::lenient_text::decode_lossy(&bytes[start..end]).into_owned()
        }
    }
}

// ---- Low-level readers (no indexing, no unwrap) -----------------------------

/// Read 3 bytes as a u24 big-endian value; advance `pos` by 3. Returns None if truncated.
fn read_u24(data: &[u8], pos: &mut usize) -> Option<usize> {
    let b0 = *data.get(*pos)?;
    let b1 = *data.get(*pos + 1)?;
    let b2 = *data.get(*pos + 2)?;
    *pos += 3;
    Some(((b0 as usize) << 16) | ((b1 as usize) << 8) | (b2 as usize))
}

/// Read 2 bytes as a biased i16 (raw u16 − 0x8000). Returns None if truncated.
fn read_i16_biased(data: &[u8], pos: &mut usize) -> Option<i32> {
    let b0 = *data.get(*pos)?;
    let b1 = *data.get(*pos + 1)?;
    *pos += 2;
    let raw = u16::from_be_bytes([b0, b1]);
    Some(raw as i32 - 0x8000)
}

/// Read 3 bytes as a signed i24 big-endian. Returns None if truncated.
fn read_i24(data: &[u8], pos: &mut usize) -> Option<i32> {
    let b0 = *data.get(*pos)? as i32;
    let b1 = *data.get(*pos + 1)? as i32;
    let b2 = *data.get(*pos + 2)? as i32;
    *pos += 3;
    Some((b0 << 16) | (b1 << 8) | b2)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Low-level reader tests ──────────────────────────────────────────────

    #[test]
    fn test_read_u24() {
        let data = [0x01, 0x02, 0x03];
        let mut pos = 0;
        assert_eq!(read_u24(&data, &mut pos), Some(0x010203));
        assert_eq!(pos, 3);
    }

    #[test]
    fn test_read_u24_truncated() {
        let data = [0x01, 0x02];
        let mut pos = 0;
        assert_eq!(read_u24(&data, &mut pos), None);
    }

    #[test]
    fn test_read_i16_biased() {
        let data = [0x80, 0x00]; // 0x8000 - 0x8000 = 0
        let mut pos = 0;
        assert_eq!(read_i16_biased(&data, &mut pos), Some(0));
        assert_eq!(pos, 2);
    }

    #[test]
    fn test_read_i16_biased_negative() {
        let data = [0x00, 0x00]; // 0x0000 - 0x8000 = -32768
        let mut pos = 0;
        assert_eq!(read_i16_biased(&data, &mut pos), Some(-0x8000));
    }

    #[test]
    fn test_read_i16_biased_truncated() {
        let data = [0x80];
        let mut pos = 0;
        assert_eq!(read_i16_biased(&data, &mut pos), None);
    }

    #[test]
    fn test_read_i24() {
        let data = [0x00, 0x01, 0x00];
        let mut pos = 0;
        assert_eq!(read_i24(&data, &mut pos), Some(256));
    }

    // ── extract_text_slice ──────────────────────────────────────────────────

    #[test]
    fn test_extract_text_slice_basic() {
        let t = FullText::Utf8("hello world");
        assert_eq!(extract_text_slice(&t, 0, 5), "hello");
        assert_eq!(extract_text_slice(&t, 6, 5), "world");
    }

    #[test]
    fn test_extract_text_slice_out_of_bounds() {
        let t = FullText::Utf8("hello");
        assert_eq!(extract_text_slice(&t, 10, 5), "");
        assert_eq!(extract_text_slice(&t, 0, 100), "hello");
    }

    #[test]
    fn test_extract_text_slice_utf8_boundary() {
        // Multi-byte char: each char is 2 bytes
        let s = FullText::Utf8("\u{00e9}\u{00e8}"); // é è — 2 bytes each
        // Slicing at byte 1 (mid-char) should snap to boundary
        let result = extract_text_slice(&s, 1, 2);
        assert!(result.is_char_boundary(0));
    }

    #[test]
    fn test_extract_text_slice_empty() {
        assert_eq!(extract_text_slice(&FullText::Utf8(""), 0, 0), "");
        assert_eq!(extract_text_slice(&FullText::Utf8("abc"), 1, 0), "");
    }

    #[test]
    fn test_legacy_zone_split_of_utf8_run_diverges() {
        // Documented tradeoff (#524 review): blob = valid UTF-8 "é" + "A" +
        // stray 0x96 → whole blob is invalid, Legacy mode. A zone edge inside
        // the "é" run decodes the split byte as CP1252 ("©") and differs from
        // the whole-text view ("éA–"). This pins the tradeoff so a future
        // change that alters it does so deliberately.
        let bytes = [0xC3, 0xA9, b'A', 0x96];
        let t = FullText::Legacy(&bytes);
        assert_eq!(
            crate::lenient_text::decode_lossy(&bytes).as_ref(),
            "\u{E9}A\u{2013}"
        );
        assert_eq!(extract_text_slice(&t, 1, 1), "\u{A9}");
        // Aligned slices still decode cleanly.
        assert_eq!(extract_text_slice(&t, 0, 2), "\u{E9}");
        assert_eq!(extract_text_slice(&t, 3, 1), "\u{2013}");
    }

    #[test]
    fn test_extract_text_slice_legacy_exact_offsets() {
        // CP1252 bytes: zone offsets address the raw bytes, one byte per char.
        let bytes = b"a\x96b\x97c";
        let t = FullText::Legacy(bytes);
        assert_eq!(extract_text_slice(&t, 1, 1), "\u{2013}");
        assert_eq!(extract_text_slice(&t, 3, 1), "\u{2014}");
        assert_eq!(extract_text_slice(&t, 0, 5), "a\u{2013}b\u{2014}c");
        assert_eq!(extract_text_slice(&t, 10, 3), "");
    }

    // ── Error paths ─────────────────────────────────────────────────────────

    #[test]
    fn test_too_short_data() {
        assert!(matches!(
            parse_text_layer(&[0x00], 100),
            Err(TextError::TooShort)
        ));
        assert!(matches!(
            parse_text_layer(&[], 100),
            Err(TextError::TooShort)
        ));
    }

    #[test]
    fn test_text_overflow() {
        // text_len = 0x00_00_FF (255) but only 3+1 bytes available
        let data = [0x00, 0x00, 0xFF, 0x41];
        assert!(matches!(
            parse_text_layer(&data, 100),
            Err(TextError::TextOverflow)
        ));
    }

    #[test]
    fn test_invalid_utf8_decodes_leniently() {
        // text_len = 2, then 2 bytes that are not valid UTF-8. Legacy CP1252
        // text must not abort the layer (#524): 0xFF = ÿ, 0xFE = þ.
        let data = [0x00, 0x00, 0x02, 0xFF, 0xFE];
        let result = parse_text_layer(&data, 100).unwrap();
        assert_eq!(result.text, "ÿþ");
        assert!(result.zones.is_empty());
    }

    #[test]
    fn test_cp1252_text_zone_offsets_stay_exact() {
        // CP1252 text "a\x96b" with a Page zone covering all 3 on-disk bytes.
        // Offsets address the original bytes, so the zone text must decode to
        // the full "a–b" even though the decoded String is 5 bytes (#524).
        let data = [
            0x00, 0x00, 0x03, // text_len = 3
            b'a', 0x96, b'b', // CP1252 text (0x96 = en dash)
            0x00, // version
            0x01, // zone type = Page
            0x80, 0x00, // x = 0 (biased)
            0x80, 0x00, // y = 0
            0x80, 0x03, // width = 3
            0x80, 0x0A, // height = 10
            0x80, 0x00, // text_start = 0 (biased)
            0x00, 0x00, 0x03, // text_len = 3 (i24)
            0x00, 0x00, 0x00, // children = 0 (i24)
        ];
        let result = parse_text_layer(&data, 100).unwrap();
        assert_eq!(result.text, "a\u{2013}b");
        assert_eq!(result.zones.len(), 1);
        assert_eq!(result.zones[0].text, "a\u{2013}b");
    }

    #[test]
    fn test_unknown_zone_type() {
        // text_len=1, text="A", version=0, then zone type=99 (invalid)
        let data = [
            0x00, 0x00, 0x01, // text_len = 1
            b'A', // text
            0x00, // version
            99,   // invalid zone type
        ];
        assert!(matches!(
            parse_text_layer(&data, 100),
            Err(TextError::UnknownZoneType(99))
        ));
    }

    #[test]
    fn test_zone_truncated() {
        // text_len=1, text="A", version=0, zone type=1 (Page), then truncated
        let data = [
            0x00, 0x00, 0x01, // text_len = 1
            b'A', // text
            0x00, // version
            0x01, // zone type = Page
            0x80, 0x00, // x (only partial fields)
        ];
        assert!(matches!(
            parse_text_layer(&data, 100),
            Err(TextError::ZoneTruncated(_))
        ));
    }

    // ── Successful parse ────────────────────────────────────────────────────

    #[test]
    fn test_empty_text_no_zones() {
        // text_len=0, no zones after that
        let data = [0x00, 0x00, 0x00];
        let result = parse_text_layer(&data, 100).unwrap();
        assert_eq!(result.text, "");
        assert!(result.zones.is_empty());
    }

    #[test]
    fn test_text_only_no_zones() {
        // text_len=5, text="Hello", version byte, then no zone data
        let data = [
            0x00, 0x00, 0x05, // text_len = 5
            b'H', b'e', b'l', b'l', b'o', // text
            0x00, // version
        ];
        let result = parse_text_layer(&data, 100).unwrap();
        assert_eq!(result.text, "Hello");
        assert!(result.zones.is_empty());
    }

    // ── TextLayer::transform ─────────────────────────────────────────────────

    fn make_layer(x: u32, y: u32, w: u32, h: u32) -> TextLayer {
        TextLayer {
            text: "test".to_string(),
            zones: vec![TextZone {
                kind: TextZoneKind::Page,
                rect: Rect {
                    x,
                    y,
                    width: w,
                    height: h,
                },
                text: "test".to_string(),
                children: vec![],
            }],
        }
    }

    fn rect0(layer: &TextLayer) -> &Rect {
        &layer.zones[0].rect
    }

    #[test]
    fn transform_none_identity() {
        use crate::info::Rotation;
        // No rotation, 1:1 scale — rects unchanged
        let layer = make_layer(10, 20, 30, 40);
        let out = layer.transform(100, 200, Rotation::None, 100, 200);
        assert_eq!(
            *rect0(&out),
            Rect {
                x: 10,
                y: 20,
                width: 30,
                height: 40
            }
        );
    }

    #[test]
    fn transform_none_scale_2x() {
        use crate::info::Rotation;
        let layer = make_layer(10, 20, 30, 40);
        let out = layer.transform(100, 200, Rotation::None, 200, 400);
        assert_eq!(
            *rect0(&out),
            Rect {
                x: 20,
                y: 40,
                width: 60,
                height: 80
            }
        );
    }

    #[test]
    fn transform_rot180() {
        use crate::info::Rotation;
        // page 100×200, rect (10, 20, 30, 40)
        // new_x = 100 - 10 - 30 = 60
        // new_y = 200 - 20 - 40 = 140
        let layer = make_layer(10, 20, 30, 40);
        let out = layer.transform(100, 200, Rotation::Rot180, 100, 200);
        assert_eq!(
            *rect0(&out),
            Rect {
                x: 60,
                y: 140,
                width: 30,
                height: 40
            }
        );
    }

    #[test]
    fn transform_cw90() {
        use crate::info::Rotation;
        // page 100×200, rect (x=10, y=20, w=30, h=40)
        // displayed: 200 wide × 100 tall
        // new_x = page_h - y - h = 200 - 20 - 40 = 140
        // new_y = x = 10
        // new_w = h = 40,  new_h = w = 30
        let layer = make_layer(10, 20, 30, 40);
        let out = layer.transform(100, 200, Rotation::Cw90, 200, 100);
        assert_eq!(
            *rect0(&out),
            Rect {
                x: 140,
                y: 10,
                width: 40,
                height: 30
            }
        );
    }

    #[test]
    fn transform_ccw90() {
        use crate::info::Rotation;
        // page 100×200, rect (x=10, y=20, w=30, h=40)
        // displayed: 200 wide × 100 tall
        // new_x = y = 20
        // new_y = page_w - x - w = 100 - 10 - 30 = 60
        // new_w = h = 40,  new_h = w = 30
        let layer = make_layer(10, 20, 30, 40);
        let out = layer.transform(100, 200, Rotation::Ccw90, 200, 100);
        assert_eq!(
            *rect0(&out),
            Rect {
                x: 20,
                y: 60,
                width: 40,
                height: 30
            }
        );
    }

    #[test]
    fn transform_cw90_then_scale() {
        use crate::info::Rotation;
        // page 100×200, rect (10, 20, 30, 40), render at 2× (400×200)
        // After Cw90: (140, 10, 40, 30) in 200×100 space
        // Scale ×2: (280, 20, 80, 60)
        let layer = make_layer(10, 20, 30, 40);
        let out = layer.transform(100, 200, Rotation::Cw90, 400, 200);
        assert_eq!(
            *rect0(&out),
            Rect {
                x: 280,
                y: 20,
                width: 80,
                height: 60
            }
        );
    }

    #[test]
    fn transform_text_preserved() {
        use crate::info::Rotation;
        let layer = make_layer(0, 0, 10, 10);
        let out = layer.transform(100, 100, Rotation::Cw90, 100, 100);
        assert_eq!(out.text, "test");
        assert_eq!(out.zones[0].text, "test");
    }

    #[test]
    fn test_single_word_zone() {
        // Build a minimal text layer with one Page zone containing "Hi"
        let text = b"Hi";
        let mut data = Vec::new();
        // text_len = 2 (u24be)
        data.extend_from_slice(&[0x00, 0x00, 0x02]);
        data.extend_from_slice(text);
        data.push(0x00); // version

        // Page zone (type=1)
        data.push(0x01);
        // x=0, y=0, w=100, h=50 (biased i16: value + 0x8000)
        data.extend_from_slice(&0x8000u16.to_be_bytes()); // x=0
        data.extend_from_slice(&0x8000u16.to_be_bytes()); // y=0
        data.extend_from_slice(&(100u16 + 0x8000u16).wrapping_add(0).to_be_bytes()); // w=100
        let h_val = 50i32 + 0x8000;
        data.extend_from_slice(&(h_val as u16).to_be_bytes()); // h=50
        data.extend_from_slice(&0x8000u16.to_be_bytes()); // text_start=0
        // text_len = 2 (i24)
        data.extend_from_slice(&[0x00, 0x00, 0x02]);
        // children_count = 0 (i24)
        data.extend_from_slice(&[0x00, 0x00, 0x00]);

        let result = parse_text_layer(&data, 100).unwrap();
        assert_eq!(result.text, "Hi");
        assert_eq!(result.zones.len(), 1);
        assert_eq!(result.zones[0].kind, TextZoneKind::Page);
        assert_eq!(result.zones[0].text, "Hi");
        assert_eq!(result.zones[0].rect.width, 100);
        assert_eq!(result.zones[0].rect.height, 50);
    }

    // ── Paragraph reflow tests (#228) ───────────────────────────────────────

    fn layer_with(text: &str) -> TextLayer {
        TextLayer {
            text: text.to_string(),
            zones: Vec::new(),
        }
    }

    #[test]
    fn reflowable_text_splits_on_paragraph_separator() {
        // Two paragraphs separated by US (\x1f), each with two lines.
        let layer = layer_with("first line\nsecond line\u{001f}third line\nfourth line");
        let paras = layer.reflowable_text();
        assert_eq!(paras.len(), 2);
        assert_eq!(paras[0].lines, vec!["first line", "second line"]);
        assert_eq!(paras[0].text, "first line second line");
        assert_eq!(paras[1].lines, vec!["third line", "fourth line"]);
        assert_eq!(paras[1].text, "third line fourth line");
    }

    #[test]
    fn reflowable_text_joins_soft_hyphen() {
        // "compre-" + "hensive" → "comprehensive" (lowercase next, hyphen drop).
        let layer = layer_with("a compre-\nhensive guide");
        let paras = layer.reflowable_text();
        assert_eq!(paras.len(), 1);
        assert_eq!(paras[0].text, "a comprehensive guide");
    }

    #[test]
    fn reflowable_text_keeps_hyphen_before_uppercase() {
        // "Anglo-" + "Saxon" — the hyphen is part of the word, not a soft
        // line-break. Uppercase next ⇒ keep the hyphen, replace newline with
        // a space.
        let layer = layer_with("Anglo-\nSaxon roots");
        let paras = layer.reflowable_text();
        assert_eq!(paras.len(), 1);
        assert_eq!(paras[0].text, "Anglo- Saxon roots");
    }

    #[test]
    fn reflowable_text_treats_all_separator_codes_as_break() {
        // NUL, VT, GS, US should each break paragraphs.
        let layer = layer_with("a\u{0000}b\u{000b}c\u{001d}d\u{001f}e");
        let paras = layer.reflowable_text();
        assert_eq!(paras.len(), 5);
        assert_eq!(paras[0].text, "a");
        assert_eq!(paras[4].text, "e");
    }

    #[test]
    fn reflowable_text_skips_empty_paragraphs() {
        let layer = layer_with("\u{001f}\u{001f}only one\u{001f}");
        let paras = layer.reflowable_text();
        assert_eq!(paras.len(), 1);
        assert_eq!(paras[0].text, "only one");
    }

    #[test]
    fn reflowable_text_trims_per_line_whitespace() {
        let layer = layer_with("  leading\n  trailing  \n   middle   ");
        let paras = layer.reflowable_text();
        assert_eq!(paras.len(), 1);
        assert_eq!(paras[0].lines, vec!["leading", "trailing", "middle"]);
        assert_eq!(paras[0].text, "leading trailing middle");
    }

    // ── Character zone (type_byte=7) ─────────────────────────────────────────

    #[test]
    fn parse_character_zone_type_7() {
        // text "X", then a Character zone (type=7), no children
        let mut data = Vec::new();
        data.extend_from_slice(&[0x00, 0x00, 0x01]); // text_len = 1
        data.push(b'X'); // text
        data.push(0x00); // version
        data.push(0x07); // zone type = Character (7)
        data.extend_from_slice(&[0x80, 0x00]); // x = 0 (biased)
        data.extend_from_slice(&[0x80, 0x00]); // y = 0
        data.extend_from_slice(&[0x80, 0x0A]); // w = 10
        data.extend_from_slice(&[0x80, 0x14]); // h = 20
        data.extend_from_slice(&[0x80, 0x00]); // text_start = 0
        data.extend_from_slice(&[0x00, 0x00, 0x01]); // text_len (i24) = 1
        data.extend_from_slice(&[0x00, 0x00, 0x00]); // children_count = 0
        let result = parse_text_layer(&data, 100).unwrap();
        assert_eq!(result.zones.len(), 1);
        assert_eq!(result.zones[0].kind, TextZoneKind::Character);
    }

    // ── ZoneTruncated from recursive child call ──────────────────────────────

    #[test]
    fn zone_truncated_when_child_data_missing() {
        // A Page zone with children_count=1 but no child bytes follow
        let mut data = Vec::new();
        data.extend_from_slice(&[0x00, 0x00, 0x01]); // text_len = 1
        data.push(b'A'); // text
        data.push(0x00); // version
        data.push(0x01); // zone type = Page (1)
        data.extend_from_slice(&[0x80, 0x00]); // x
        data.extend_from_slice(&[0x80, 0x00]); // y
        data.extend_from_slice(&[0x80, 0x64]); // w = 100
        data.extend_from_slice(&[0x80, 0x32]); // h = 50
        data.extend_from_slice(&[0x80, 0x00]); // text_start
        data.extend_from_slice(&[0x00, 0x00, 0x01]); // text_len (i24) = 1
        data.extend_from_slice(&[0x00, 0x00, 0x01]); // children_count = 1 → triggers child
        // NO child data → ZoneTruncated
        assert!(matches!(
            parse_text_layer(&data, 100),
            Err(TextError::ZoneTruncated(_))
        ));
    }

    // ── Rect::scale zero-dimension guard ────────────────────────────────────

    #[test]
    fn rect_scale_zero_from_w_returns_clone() {
        let r = Rect {
            x: 5,
            y: 10,
            width: 20,
            height: 30,
        };
        assert_eq!(r.scale(0, 100, 200, 200), r);
    }

    #[test]
    fn rect_scale_zero_from_h_returns_clone() {
        let r = Rect {
            x: 5,
            y: 10,
            width: 20,
            height: 30,
        };
        assert_eq!(r.scale(100, 0, 200, 200), r);
    }

    // ── #589 resource-ceiling regression seeds ────────────────────────────

    /// A zone declaring a huge `children_count` (i24) that cannot fit in the
    /// remaining bytes is rejected up front — no ~1.5 GB `Vec::with_capacity`.
    #[test]
    fn zone_child_count_amplification_is_rejected() {
        // One Page zone: type=0x00, then 5×2-byte biased coords, 3-byte
        // text_len=0, 3-byte children_count = 0xFFFFFF. Then nothing.
        // text-layer header: 3-byte text_len = 0, then a version byte
        let mut d = vec![0u8, 0, 0, 0];
        // zone: type + 5 biased-i16 coords (0x8000 = bias 0) + text_len(0) + children
        d.push(0x01);
        for _ in 0..5 {
            d.extend_from_slice(&[0x80, 0x00]);
        }
        d.extend_from_slice(&[0, 0, 0]); // text_len = 0
        d.extend_from_slice(&[0xFF, 0xFF, 0xFF]); // children_count = 16,777,215
        // The huge count is not backed by data, so the parse fails fast with
        // ZoneTruncated — and crucially the `Vec::with_capacity` reservation is
        // capped to `remaining / MIN_ZONE_RECORD_BYTES` (≈0 here), so no ~1.5 GB
        // allocation happens on the way to that error.
        let err = parse_text_layer(&d, 1000).unwrap_err();
        assert!(
            matches!(err, TextError::ZoneTruncated(_)),
            "huge child count must fail fast without over-reserving, got {err:?}"
        );
    }

    /// A single-child chain deeper than `MAX_ZONE_DEPTH` errors instead of
    /// recursing to a stack overflow.
    #[test]
    fn zone_depth_is_bounded() {
        fn zone_with_one_child(children: u32) -> Vec<u8> {
            let mut z = Vec::new();
            z.push(0x01); // type
            for _ in 0..5 {
                z.extend_from_slice(&[0x80, 0x00]); // biased coords = 0
            }
            z.extend_from_slice(&[0, 0, 0]); // text_len = 0
            z.extend_from_slice(&(children).to_be_bytes()[1..4]); // 3-byte children_count
            z
        }
        // Header (text_len=0) + a chain of MAX_ZONE_DEPTH+5 single-child zones,
        // deepest one having 0 children.
        let mut d = vec![0, 0, 0, 0]; // text_len(0) + version byte
        let n = MAX_ZONE_DEPTH + 5;
        for i in 0..n {
            d.extend_from_slice(&zone_with_one_child(if i + 1 < n { 1 } else { 0 }));
        }
        let err = parse_text_layer(&d, 1000).unwrap_err();
        assert!(
            matches!(err, TextError::ZoneTooDeep),
            "over-deep chain must error, got {err:?}"
        );
    }

    /// A legitimate shallow tree with a realistic child count still parses.
    #[test]
    fn zone_normal_tree_still_parses() {
        // Page with 2 word children, each 0 grandchildren.
        let mut d = vec![0, 0, 0, 0]; // text_len(0) + version byte
        d.push(0x01); // Page
        for _ in 0..5 {
            d.extend_from_slice(&[0x80, 0x00]);
        }
        d.extend_from_slice(&[0, 0, 0]); // text_len
        d.extend_from_slice(&[0, 0, 2]); // 2 children
        for _ in 0..2 {
            d.push(0x06); // Word
            for _ in 0..5 {
                d.extend_from_slice(&[0x80, 0x00]);
            }
            d.extend_from_slice(&[0, 0, 0]); // text_len
            d.extend_from_slice(&[0, 0, 0]); // 0 children
        }
        let tl = parse_text_layer(&d, 1000).unwrap();
        assert_eq!(tl.zones.len(), 1);
        assert_eq!(tl.zones[0].children.len(), 2);
    }
}
