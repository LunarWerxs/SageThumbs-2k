//! Phase 4 tests: DjVu text layer (TXTz/TXTa) parsing.

use djvu_rs::{
    DjVuDocument,
    text::{TextZoneKind, parse_text_layer},
};

/// Helper: build a minimal TXTa binary payload.
///
/// TXTa format:
///   [u24be text_len][utf8 text][u8 version]
///   followed by zone binary tree (delta-encoded)
///
/// For simplicity in tests we create payloads without zone tree (just text).
fn make_txta_no_zones(text: &str) -> Vec<u8> {
    let text_bytes = text.as_bytes();
    let mut buf = Vec::new();
    let len = text_bytes.len();
    buf.push(((len >> 16) & 0xff) as u8);
    buf.push(((len >> 8) & 0xff) as u8);
    buf.push((len & 0xff) as u8);
    buf.extend_from_slice(text_bytes);
    // version byte
    buf.push(0);
    buf
}

/// Helper: build a TXTa binary payload with one page zone and one word child.
///
/// Zone binary format (per DjVu spec, legacy implementation):
/// type(u8) x(i16+bias) y(i16+bias) w(i16+bias) h(i16+bias)
/// text_start(i16+bias) text_len(i24) children_count(i24)
/// then children recursively.
///
/// We create: PAGE zone covering full page, containing one WORD zone.
fn make_txta_with_zones(text: &str, page_height: u32) -> Vec<u8> {
    let text_bytes = text.as_bytes();
    let text_len = text_bytes.len();

    let mut buf = Vec::new();
    // text length (u24be)
    buf.push(((text_len >> 16) & 0xff) as u8);
    buf.push(((text_len >> 8) & 0xff) as u8);
    buf.push((text_len & 0xff) as u8);
    buf.extend_from_slice(text_bytes);
    // version byte
    buf.push(0);

    // PAGE zone: type=1, x=0, y=0 (bottom-left), w=200, h=page_height
    // Using bias encoding: value = raw - 0x8000, so raw = value + 0x8000
    let ph = page_height as i32;
    let encode_i16 = |v: i32| -> [u8; 2] {
        let raw = (v + 0x8000) as u16;
        raw.to_be_bytes()
    };
    // First zone has no parent/prev — values are absolute
    // PAGE zone: x=0, y=0, w=200, h=ph
    // text_start=0, text_len=text_len
    // children_count=1 (one WORD child)
    buf.push(1u8); // TextZoneKind::Page
    buf.extend_from_slice(&encode_i16(0)); // x = 0
    buf.extend_from_slice(&encode_i16(0)); // y = 0  (bottom-left in DjVu)
    buf.extend_from_slice(&encode_i16(200)); // width
    buf.extend_from_slice(&encode_i16(ph)); // height = page_height
    buf.extend_from_slice(&encode_i16(0)); // text_start = 0
    // text_len as i24
    let tl = text_len as i32;
    buf.push(((tl >> 16) & 0xff) as u8);
    buf.push(((tl >> 8) & 0xff) as u8);
    buf.push((tl & 0xff) as u8);
    // children_count = 1
    buf.push(0);
    buf.push(0);
    buf.push(1);

    // WORD child: type=6, relative to parent
    // Per legacy delta encoding for WORD (type 6 = COLUMN/REGION/WORD/CHARACTER):
    // x += prev.x + prev.width; y += prev.y (no prev → use parent)
    // With parent: x += parent.x; y = parent.y + parent.height - (y + height)
    // We want the word at x=10, y=10 in bottom-left coords (will be remapped to top-left)
    // parent.x=0, parent.y=0, parent.h=ph
    // new_y = 0 + ph - (dy + word_h) => dy = ph - new_y - word_h
    // but stored value is relative: x_stored = new_x - parent.x = 10
    // y_stored = parent.y + parent.height - (new_y + word_h) = ph - (10 + 20) = ph - 30
    let word_x = 10i32;
    let word_y_bl = 10i32; // bottom-left y
    let word_w = 50i32;
    let word_h = 20i32;
    // stored delta: x_delta = word_x - parent.x = 10 - 0 = 10
    let x_delta = word_x;
    // y_delta: parent.y + parent.height - (y_delta + word_h) = word_y_bl
    //          0 + ph - (y_delta + word_h) = word_y_bl
    //          y_delta = ph - word_y_bl - word_h
    let y_delta = ph - word_y_bl - word_h;
    buf.push(6u8); // TextZoneKind::Word
    buf.extend_from_slice(&encode_i16(x_delta));
    buf.extend_from_slice(&encode_i16(y_delta));
    buf.extend_from_slice(&encode_i16(word_w));
    buf.extend_from_slice(&encode_i16(word_h));
    buf.extend_from_slice(&encode_i16(0)); // text_start delta relative to parent.text_start=0
    // text_len = text_len
    buf.push(((tl >> 16) & 0xff) as u8);
    buf.push(((tl >> 8) & 0xff) as u8);
    buf.push((tl & 0xff) as u8);
    // children_count = 0
    buf.push(0);
    buf.push(0);
    buf.push(0);

    buf
}

#[test]
fn test_parse_text_layer_basic() {
    let text = "hello world";
    let data = make_txta_no_zones(text);
    let layer = parse_text_layer(&data, 300).expect("parse should succeed");
    assert_eq!(layer.text, text);
    assert!(layer.zones.is_empty(), "no zones in this payload");
}

#[test]
fn test_plain_text_extraction() {
    let text = "The quick brown fox";
    let data = make_txta_no_zones(text);
    let layer = parse_text_layer(&data, 400).expect("parse should succeed");
    assert_eq!(layer.text, "The quick brown fox");
}

#[test]
fn test_parse_text_layer_empty_text() {
    let data = make_txta_no_zones("");
    let layer = parse_text_layer(&data, 300).expect("parse empty should succeed");
    assert_eq!(layer.text, "");
    assert!(layer.zones.is_empty());
}

#[test]
fn test_parse_text_layer_too_short_returns_error() {
    // Data shorter than 3 bytes (can't even read text length)
    let result = parse_text_layer(&[0x00, 0x00], 300);
    assert!(result.is_err(), "too-short data should return error");
}

#[test]
fn test_text_zone_coordinate_remap() {
    // Build a page of height 300, word at bottom-left y=10 with height=20.
    // After top-left remap: top_y = page_height - (bl_y + h) = 300 - (10 + 20) = 270
    let text = "word";
    let page_height = 300u32;
    let data = make_txta_with_zones(text, page_height);
    let layer = parse_text_layer(&data, page_height).expect("parse with zones should succeed");

    // Should have a root PAGE zone with one WORD child
    assert_eq!(layer.zones.len(), 1, "should have 1 root zone");
    let page_zone = &layer.zones[0];
    assert_eq!(page_zone.kind, TextZoneKind::Page);
    assert_eq!(page_zone.rect.y, 0, "page zone top-left y should be 0");

    assert_eq!(page_zone.children.len(), 1, "page zone should have 1 child");
    let word_zone = &page_zone.children[0];
    assert_eq!(word_zone.kind, TextZoneKind::Word);
    // Expected top-left y = page_height - (word_y_bl + word_h) = 300 - (10 + 20) = 270
    assert_eq!(
        word_zone.rect.y, 270,
        "word top-left y should be 270 after remap"
    );
    assert_eq!(word_zone.rect.x, 10, "word x should be 10");
    assert_eq!(word_zone.rect.width, 50, "word width should be 50");
    assert_eq!(word_zone.rect.height, 20, "word height should be 20");
}

#[test]
fn test_parse_text_layer_bzz() {
    // Pre-computed BZZ encoding of:
    //   \x00\x00\x0f  (text_len = 15)
    //   "compressed text"  (15 bytes)
    //   \x00  (version)
    // Generated with: printf '\x00\x00\x0fcompressed text\x00' | bzz -e - -
    use djvu_rs::bzz::bzz_decode;
    use djvu_rs::text::parse_text_layer;
    let encoded: &[u8] = &[
        0xff, 0xff, 0xeb, 0xbf, 0x8b, 0x1f, 0xc5, 0x04, 0x22, 0xcf, 0xef, 0xba, 0x6b, 0x2b, 0x4e,
        0x9f, 0x25, 0x6a, 0x9a, 0xa3, 0x86, 0x3f,
    ];
    // TXTz decompresses via the shared BZZ path; the parser itself is pure.
    let decoded = bzz_decode(encoded).expect("bzz decode should succeed");
    let layer = parse_text_layer(&decoded, 300).expect("parse should succeed");
    assert_eq!(layer.text, "compressed text");
}

#[test]
fn test_djvu_page_text_layer_from_real_file() {
    let assets = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("references/djvujs/library/assets");
    let golden = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/text");

    // DjVu3Spec_bundled.djvu is a multi-page bundled document with TXTz chunks
    // on every page and is known to parse correctly with the new DjVuDocument model.
    let path = assets.join("DjVu3Spec_bundled.djvu");
    if !path.exists() {
        return; // skip if asset not available
    }
    let data = std::fs::read(&path).expect("read DjVu3Spec_bundled.djvu");
    let doc = DjVuDocument::parse(&data).expect("parse DjVu3Spec_bundled.djvu");
    let page = doc.page(0).expect("page 0");
    let layer = page.text_layer().expect("text_layer should not error");

    // Page 0 (== spec page 1) has a TXTz chunk — verify text is extracted correctly.
    // The golden file (djvu3spec_p1.txt) contains djvused S-expression output, not
    // plain text, so we only verify content properties here.
    match layer {
        Some(ref l) => {
            assert!(
                !l.text.is_empty(),
                "DjVu3Spec page 0 text should be non-empty"
            );
            // The spec intro starts with "Introduction" on page 1
            assert!(
                l.text.contains("Introduction"),
                "expected 'Introduction' in DjVu3Spec page 1 text"
            );
        }
        None => {
            // If no layer is returned, something is wrong
            panic!("DjVu3Spec_bundled.djvu page 0 should have a text layer");
        }
    }
    let _ = golden; // suppress unused warning
}

#[test]
fn test_djvu_page_text_method() {
    let assets = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("references/djvujs/library/assets");
    // Use DjVu3Spec_bundled which is known to parse correctly with the new model
    let path = assets.join("DjVu3Spec_bundled.djvu");
    if !path.exists() {
        return;
    }
    let data = std::fs::read(&path).expect("read DjVu3Spec_bundled.djvu");
    let doc = DjVuDocument::parse(&data).expect("parse DjVu3Spec_bundled.djvu");
    let page = doc.page(0).expect("page 0");

    // text() convenience method — should not error
    let text_opt = page.text().expect("text() should not error");
    // DjVu3Spec page 0 has a TXTz chunk
    assert!(
        text_opt.is_some(),
        "DjVu3Spec_bundled.djvu page 0 should have text layer"
    );
    let text = text_opt.unwrap();
    assert!(!text.is_empty(), "extracted text should be non-empty");
}

// ── Reflowable text (#228) ───────────────────────────────────────────────────

/// On a real OCR'd page, `reflowable_text()` should:
///  - return a non-empty list of paragraphs
///  - join the per-line strings within a paragraph into a single `text` field
///    that contains no `\n` and no DjVu separator codes (\x00, \x0b, \x1d, \x1f)
///  - apply the trim-each-line rule (no leading/trailing spaces around lines)
#[test]
fn test_reflowable_text_on_corpus_watchmaker() {
    let path =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/watchmaker.djvu");
    let Ok(data) = std::fs::read(&path) else {
        eprintln!("watchmaker.djvu not present, skipping");
        return;
    };
    let doc = DjVuDocument::parse(&data).expect("parse");
    let page = doc.page(1).expect("page 1");
    let layer = page
        .text_layer()
        .expect("text_layer no error")
        .expect("page 1 has text");

    let paras = layer.reflowable_text();
    assert!(
        paras.len() >= 3,
        "expected at least 3 paragraphs on this page, got {}",
        paras.len()
    );

    for p in &paras {
        assert!(!p.text.is_empty(), "paragraph text should not be empty");
        assert!(
            !p.text.contains('\n'),
            "joined paragraph text must not contain raw newlines"
        );
        for sep in ['\u{0000}', '\u{000b}', '\u{001d}', '\u{001f}'] {
            assert!(
                !p.text.contains(sep),
                "joined paragraph text must not contain DjVu separator U+{:04X}",
                sep as u32
            );
        }
        for line in &p.lines {
            assert_eq!(line.trim(), line, "each line must be already trimmed");
            assert!(!line.is_empty(), "no empty lines should slip through");
        }
    }

    // The opening paragraph is the running header "98 Living in the LambLight"
    // (single line). Confirm the first paragraph round-trips that.
    assert_eq!(paras[0].text, "98 Living in the LambLight");
}
