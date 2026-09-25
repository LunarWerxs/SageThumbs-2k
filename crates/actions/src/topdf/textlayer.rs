//! The invisible text layer that makes a combined PDF searchable.
//!
//! WHY: a scan or phone photo turned into a PDF is only pixels; nothing in any PDF viewer can
//! search, select or copy it. The idea comes from Tesseract's PDF renderer (tesseract-ocr/
//! tesseract, `src/api/pdfrenderer.cpp`, Apache-2.0) and is written fresh here: every word the
//! Windows OCR engine recognised is drawn in text render mode 3 (neither filled nor stroked, so
//! nothing appears over the picture) with a "glyphless" CID font whose every CID maps to glyph
//! 1 (Acrobat refuses to select text drawn with glyph 0). The string is the word's UTF-16 code
//! units and the ToUnicode map is the identity, so search, select and copy return the real
//! text; a per-word horizontal scale (`Tz`) stretches each word to exactly its box, so a search
//! highlight lands on the pixels the word came from.

use std::fmt::Write as _;
use std::io::{self, Write};

use flate2::write::ZlibEncoder;
use flate2::Compression;
use st2k_codecs::ocr::WordBox;

use super::{mark, Counted};

/// Objects the shared font adds after the pages: the Type0 font, its CIDFont, the font
/// descriptor, the CIDToGIDMap, the ToUnicode CMap and the embedded font file.
pub(super) const FONT_OBJS: usize = 6;

/// Every glyph's advance in the font's em. Arbitrary (each word's `Tz` rescales it), but `/DW`,
/// `hmtx` and [`text_ops`]' width sum must agree on it.
const ADVANCE: u16 = 500;
/// Font units per em. The descriptor's ascent is a full em and its descent zero, so a word set
/// at its box height on the box's bottom edge highlights exactly the box.
const EM: u16 = 1000;

/// Where a page's image is drawn: its page-space origin and size (points) and its pixel size,
/// which together map an OCR box (image pixels, y down) onto the page (points, y up).
#[derive(Clone, Copy, Debug)]
pub(super) struct Frame {
    pub dx: f64,
    pub dy: f64,
    pub dw: f64,
    pub dh: f64,
    pub iw: f64,
    pub ih: f64,
}

/// The content-stream operators that lay `lines` of recognised words over the image in `f`,
/// invisibly. Empty when there is no text, so a blank page carries no text object at all.
pub(super) fn text_ops(lines: &[Vec<WordBox>], f: Frame) -> String {
    let mut ops = String::new();
    for line in lines {
        for (i, word) in line.iter().enumerate() {
            push_word(&mut ops, word, line.get(i + 1), f);
        }
    }
    if ops.is_empty() {
        return ops;
    }
    format!("BT\n3 Tr\n{ops}ET\n")
}

/// One word: font size from the box height, baseline on the box's bottom edge, and `Tz` so the
/// string spans the box. A word followed by another on the same line carries a trailing space
/// (copied text keeps its word breaks) and spans to that next word's left edge instead.
fn push_word(ops: &mut String, word: &WordBox, next: Option<&WordBox>, f: Frame) {
    let text = word.text.trim();
    let (sx, sy) = (f.dw / f.iw, f.dh / f.ih);
    let size = f64::from(word.h) * sy;
    let positive = |v: f64| v.is_finite() && v > 0.0;
    if text.is_empty() || !positive(size) || !positive(sx) {
        return;
    }
    let mut s = text.to_string();
    let mut span = f64::from(word.w);
    if let Some(n) = next {
        s.push(' ');
        span = span.max(f64::from(n.x - word.x));
    }
    let units: Vec<u16> = s.encode_utf16().collect();
    let natural = units.len() as f64 * f64::from(ADVANCE) / f64::from(EM) * size;
    let tz = 100.0 * span.max(0.0) * sx / natural;
    let x = f.dx + f64::from(word.x) * sx;
    let y = f.dy + f.dh - f64::from(word.y + word.h) * sy;
    let _ = write!(
        ops,
        "/F1 {size:.3} Tf {tz:.3} Tz 1 0 0 1 {x:.3} {y:.3} Tm <"
    );
    for u in units {
        let _ = write!(ops, "{u:04X}");
    }
    ops.push_str("> Tj\n");
}

/// Write the shared font as objects `first..first + FONT_OBJS`; pages name it `/F1`.
pub(super) fn write_font<W: Write>(
    w: &mut Counted<W>,
    off: &mut [usize],
    first: usize,
) -> io::Result<()> {
    let (cid, desc, map, cmap, file) = (first + 1, first + 2, first + 3, first + 4, first + 5);
    mark(off, first, w.pos);
    write!(w, "{first} 0 obj\n<< /Type /Font /Subtype /Type0 /BaseFont /GlyphLessFont /Encoding /Identity-H /DescendantFonts [{cid} 0 R] /ToUnicode {cmap} 0 R >>\nendobj\n")?;
    mark(off, cid, w.pos);
    write!(w, "{cid} 0 obj\n<< /Type /Font /Subtype /CIDFontType2 /BaseFont /GlyphLessFont /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /FontDescriptor {desc} 0 R /DW {ADVANCE} /CIDToGIDMap {map} 0 R >>\nendobj\n")?;
    mark(off, desc, w.pos);
    write!(w, "{desc} 0 obj\n<< /Type /FontDescriptor /FontName /GlyphLessFont /Flags 5 /FontBBox [0 0 {ADVANCE} {EM}] /ItalicAngle 0 /Ascent {EM} /Descent 0 /CapHeight {EM} /StemV 80 /FontFile2 {file} 0 R >>\nendobj\n")?;
    write_stream(w, off, map, "/Filter /FlateDecode ", &cid_to_gid_map()?)?;
    write_stream(w, off, cmap, "", to_unicode_cmap().as_bytes())?;
    let ttf = glyphless_ttf();
    write_stream(w, off, file, &format!("/Length1 {} ", ttf.len()), &ttf)
}

/// One stream object: `extra` dictionary entries, then `data` verbatim.
fn write_stream<W: Write>(
    w: &mut Counted<W>,
    off: &mut [usize],
    id: usize,
    extra: &str,
    data: &[u8],
) -> io::Result<()> {
    mark(off, id, w.pos);
    write!(
        w,
        "{id} 0 obj\n<< {extra}/Length {} >>\nstream\n",
        data.len()
    )?;
    w.write_all(data)?;
    w.write_all(b"\nendstream\nendobj\n")
}

/// Every CID (a UTF-16 code unit) maps to glyph 1: two big-endian bytes per CID, 128 KiB that
/// Flate shrinks to a few hundred bytes.
fn cid_to_gid_map() -> io::Result<Vec<u8>> {
    let mut z = ZlibEncoder::new(Vec::new(), Compression::best());
    z.write_all(&[0u8, 1].repeat(0x1_0000))?;
    z.finish()
}

/// The identity ToUnicode CMap: CID n is UTF-16 code unit n. A `bfrange` may only vary its last
/// byte and a block holds at most 100 ranges, so it is one range per high byte, in blocks.
fn to_unicode_cmap() -> String {
    let mut s = String::from(
        "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
         /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
         /CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n\
         1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n",
    );
    let highs: Vec<u16> = (0..=0xFF).collect();
    for block in highs.chunks(100) {
        let _ = writeln!(s, "{} beginbfrange", block.len());
        for h in block {
            let _ = writeln!(s, "<{h:02X}00> <{h:02X}FF> <{h:02X}00>");
        }
        s.push_str("endbfrange\n");
    }
    s.push_str("endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n");
    s
}

/// The glyphless TrueType font's tables as big-endian 16-bit words, in the tag order the table
/// directory must list them: two glyphs (.notdef and glyph 1), both empty and [`ADVANCE`] wide.
/// Only the tables a CIDFontType2 program needs (no `cmap`: the CIDToGIDMap picks the glyph).
const TTF_TABLES: [(&[u8; 4], &[u16]); 7] = [
    (b"glyf", &[]),
    (
        b"head",
        // version, revision, checkSumAdjustment (patched in), magic, flags, unitsPerEm,
        // created, modified, bbox, macStyle, lowestRecPPEM, direction hint, short loca, glyf 0.
        &[
            1, 0, 1, 0, 0, 0, 0x5F0F, 0x3CF5, 0x000B, EM, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, ADVANCE,
            EM, 0, 3, 2, 0, 0,
        ],
    ),
    (
        b"hhea",
        // version, ascender, descender, lineGap, advanceWidthMax, minLSB, minRSB, xMaxExtent,
        // caret rise/run/offset, 4 reserved, metricDataFormat, numberOfHMetrics.
        &[
            1, 0, EM, 0, 0, ADVANCE, 0, 0, ADVANCE, 1, 0, 0, 0, 0, 0, 0, 0, 2,
        ],
    ),
    (b"hmtx", &[ADVANCE, 0, ADVANCE, 0]),
    (b"loca", &[0, 0, 0]),
    // version 1.0, numGlyphs 2, then the thirteen limits (maxZones 2, the rest 0).
    (b"maxp", &[1, 0, 2, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0]),
    // version 3.0 (no glyph names), italicAngle, underline position -100 / thickness 50,
    // isFixedPitch, four memory hints.
    (
        b"post",
        &[3, 0, 0, 0, 0xFF9C, 50, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0],
    ),
];

/// Assemble [`TTF_TABLES`] into an sfnt: offset table, table directory, 4-byte-aligned table
/// data, per-table checksums and the whole-font `checkSumAdjustment` in `head`.
fn glyphless_ttf() -> Vec<u8> {
    let n = TTF_TABLES.len();
    let sel = n.ilog2() as usize;
    let range = (1usize << sel) * 16;
    let mut out = Vec::new();
    push_u16s(
        &mut out,
        &[
            1,
            0,
            n as u16,
            range as u16,
            sel as u16,
            (n * 16 - range) as u16,
        ],
    );
    let mut offset = 12 + 16 * n;
    let mut body = Vec::new();
    let mut head_at = 0;
    for (tag, words) in TTF_TABLES {
        let mut data = Vec::new();
        push_u16s(&mut data, words);
        if tag == b"head" {
            head_at = offset;
        }
        out.extend_from_slice(tag);
        out.extend_from_slice(&sfnt_checksum(&data).to_be_bytes());
        out.extend_from_slice(&(offset as u32).to_be_bytes());
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        data.resize(data.len().next_multiple_of(4), 0);
        offset += data.len();
        body.extend_from_slice(&data);
    }
    out.extend_from_slice(&body);
    let adjust = 0xB1B0_AFBAu32.wrapping_sub(sfnt_checksum(&out));
    if let Some(slot) = out.get_mut(head_at + 8..head_at + 12) {
        slot.copy_from_slice(&adjust.to_be_bytes());
    }
    out
}

fn push_u16s(out: &mut Vec<u8>, words: &[u16]) {
    for w in words {
        out.extend_from_slice(&w.to_be_bytes());
    }
}

/// The sfnt checksum: the wrapping sum of the data as big-endian u32s, zero-padded.
fn sfnt_checksum(data: &[u8]) -> u32 {
    data.chunks(4).fold(0u32, |sum, c| {
        let mut word = [0u8; 4];
        word[..c.len()].copy_from_slice(c);
        sum.wrapping_add(u32::from_be_bytes(word))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(text: &str, x: f32, w: f32) -> WordBox {
        WordBox {
            text: text.into(),
            x,
            y: 50.0,
            w,
            h: 20.0,
        }
    }

    /// A 1000x500 image drawn at half size, 10 pt in from the left and 20 pt up.
    const FRAME: Frame = Frame {
        dx: 10.0,
        dy: 20.0,
        dw: 500.0,
        dh: 250.0,
        iw: 1000.0,
        ih: 500.0,
    };

    /// The layer's whole contract in one line: invisible (`3 Tr`), sized to the box height,
    /// stretched by `Tz` to the box width (2 glyphs x 500/1000 em x 10 pt = 10 pt natural, 20 pt
    /// wanted, so 200%), placed on the box's bottom-left corner in PDF space (y up), and spelled
    /// as UTF-16 code units the identity ToUnicode map turns back into "Hi".
    #[test]
    fn a_word_is_invisible_and_spans_exactly_its_box() {
        let ops = text_ops(&[vec![word("Hi", 100.0, 40.0)]], FRAME);
        assert!(
            ops.starts_with("BT\n3 Tr\n") && ops.ends_with("ET\n"),
            "{ops}"
        );
        assert!(
            ops.contains("/F1 10.000 Tf 200.000 Tz 1 0 0 1 60.000 235.000 Tm <00480069> Tj"),
            "{ops}"
        );
    }

    /// Words on one line keep a real space between them when copied, and the first word's span
    /// runs to the second's left edge so the space sits in the gap rather than overlapping it.
    #[test]
    fn words_on_a_line_are_joined_by_a_space_that_fills_the_gap() {
        let ops = text_ops(
            &[vec![word("a", 100.0, 20.0), word("b", 160.0, 20.0)]],
            FRAME,
        );
        // "a " = 2 units -> 10 pt natural; span 60 px = 30 pt -> 300%.
        assert!(
            ops.contains("300.000 Tz") && ops.contains("<00610020>"),
            "{ops}"
        );
        assert!(
            ops.contains("<0062> Tj"),
            "the last word carries no space: {ops}"
        );
    }

    /// No words, no text object: a page with nothing recognised stays byte-identical to the
    /// plain combine's.
    #[test]
    fn no_words_writes_no_text_object() {
        assert!(text_ops(&[], FRAME).is_empty());
        assert!(text_ops(&[vec![word("  ", 0.0, 5.0)]], FRAME).is_empty());
    }

    /// The embedded font program must be a well-formed sfnt, or a strict viewer drops the whole
    /// font and with it the text layer: the checkSumAdjustment makes the whole file sum to the
    /// magic 0xB1B0AFBA, and every directory entry's checksum matches its table.
    #[test]
    fn the_glyphless_font_is_a_checksummed_sfnt() {
        let ttf = glyphless_ttf();
        assert_eq!(sfnt_checksum(&ttf), 0xB1B0_AFBA);
        assert_eq!(&ttf[..6], &[0, 1, 0, 0, 0, 7]);
        for rec in ttf[12..12 + 16 * 7].chunks(16) {
            let sum = u32::from_be_bytes(rec[4..8].try_into().unwrap());
            let at = u32::from_be_bytes(rec[8..12].try_into().unwrap()) as usize;
            let len = u32::from_be_bytes(rec[12..16].try_into().unwrap()) as usize;
            if !rec.starts_with(b"head") {
                assert_eq!(sfnt_checksum(&ttf[at..at + len]), sum, "{:?}", &rec[..4]);
            }
            assert_eq!(at % 4, 0, "tables must be 4-byte aligned");
        }
    }
}
