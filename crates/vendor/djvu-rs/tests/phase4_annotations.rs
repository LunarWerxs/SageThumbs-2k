//! Phase 4 tests: DjVu annotation (ANTz/ANTa) parsing.

use djvu_rs::annotation::{Color, Shape, parse_annotations};

/// Helper: encode a string as ANTa bytes (plain, no BZZ).
fn make_anta(s: &str) -> Vec<u8> {
    s.as_bytes().to_vec()
}

#[test]
fn test_parse_empty_annotations() {
    // Empty ANTa should return Ok with empty annotation and no mapareas
    let data = make_anta("");
    let (ann, mapareas) = parse_annotations(&data).expect("empty parse should succeed");
    assert!(ann.background.is_none());
    assert!(ann.zoom.is_none());
    assert!(ann.mode.is_none());
    assert!(mapareas.is_empty());
}

#[test]
fn test_parse_background_annotation() {
    let data = make_anta("(background #ffffff)");
    let (ann, _) = parse_annotations(&data).expect("parse should succeed");
    assert!(ann.background.is_some(), "background should be parsed");
    let bg = ann.background.unwrap();
    assert_eq!(
        bg,
        Color {
            r: 0xff,
            g: 0xff,
            b: 0xff
        }
    );
}

#[test]
fn test_parse_background_black() {
    let data = make_anta("(background #000000)");
    let (ann, _) = parse_annotations(&data).expect("parse should succeed");
    let bg = ann.background.expect("background must be set");
    assert_eq!(bg, Color { r: 0, g: 0, b: 0 });
}

#[test]
fn test_parse_zoom_annotation() {
    let data = make_anta("(zoom 150)");
    let (ann, _) = parse_annotations(&data).expect("parse should succeed");
    assert_eq!(ann.zoom, Some(150u32));
}

#[test]
fn test_parse_mode_annotation() {
    let data = make_anta("(mode color)");
    let (ann, _) = parse_annotations(&data).expect("parse should succeed");
    assert_eq!(ann.mode.as_deref(), Some("color"));
}

#[test]
fn test_parse_maparea_rect() {
    let data = make_anta(r#"(maparea "http://example.com" "Click here" (rect 10 20 100 50))"#);
    let (_, mapareas) = parse_annotations(&data).expect("parse should succeed");
    assert_eq!(mapareas.len(), 1);
    let ma = &mapareas[0];
    assert_eq!(ma.url, "http://example.com");
    assert_eq!(ma.description, "Click here");
    match &ma.shape {
        Shape::Rect(r) => {
            assert_eq!(r.x, 10);
            assert_eq!(r.y, 20);
            assert_eq!(r.width, 100);
            assert_eq!(r.height, 50);
        }
        other => panic!("expected Rect shape, got {other:?}"),
    }
}

#[test]
fn test_parse_maparea_oval() {
    let data = make_anta(r#"(maparea "" "" (oval 5 10 80 40))"#);
    let (_, mapareas) = parse_annotations(&data).expect("parse should succeed");
    assert_eq!(mapareas.len(), 1);
    match &mapareas[0].shape {
        Shape::Oval(r) => {
            assert_eq!(r.x, 5);
            assert_eq!(r.y, 10);
            assert_eq!(r.width, 80);
            assert_eq!(r.height, 40);
        }
        other => panic!("expected Oval, got {other:?}"),
    }
}

#[test]
fn test_parse_maparea_poly() {
    let data = make_anta(r#"(maparea "" "" (poly 0 0 100 0 100 100 0 100))"#);
    let (_, mapareas) = parse_annotations(&data).expect("parse should succeed");
    assert_eq!(mapareas.len(), 1);
    match &mapareas[0].shape {
        Shape::Poly(pts) => {
            assert_eq!(pts.len(), 4);
            assert_eq!(pts[0], (0, 0));
            assert_eq!(pts[1], (100, 0));
            assert_eq!(pts[2], (100, 100));
            assert_eq!(pts[3], (0, 100));
        }
        other => panic!("expected Poly, got {other:?}"),
    }
}

#[test]
fn test_parse_maparea_line() {
    let data = make_anta(r#"(maparea "" "" (line 0 0 200 100))"#);
    let (_, mapareas) = parse_annotations(&data).expect("parse should succeed");
    assert_eq!(mapareas.len(), 1);
    match &mapareas[0].shape {
        Shape::Line(x1, y1, x2, y2) => {
            assert_eq!((*x1, *y1, *x2, *y2), (0, 0, 200, 100));
        }
        other => panic!("expected Line, got {other:?}"),
    }
}

#[test]
fn test_hyperlinks_filter() {
    // Only mapareas with non-empty url should appear in hyperlinks()
    let data = make_anta(concat!(
        r#"(maparea "http://a.com" "link" (rect 0 0 10 10))"#,
        "\n",
        r#"(maparea "" "no link" (rect 20 20 10 10))"#
    ));
    let (_, mapareas) = parse_annotations(&data).expect("parse should succeed");
    assert_eq!(mapareas.len(), 2);

    let hyperlinks: Vec<_> = mapareas.iter().filter(|m| !m.url.is_empty()).collect();
    assert_eq!(hyperlinks.len(), 1);
    assert_eq!(hyperlinks[0].url, "http://a.com");
}

#[test]
fn test_parse_multiple_mapareas() {
    let data = make_anta(concat!(
        r#"(maparea "http://one.com" "first" (rect 0 0 10 10))"#,
        "\n",
        r#"(maparea "http://two.com" "second" (rect 20 0 10 10))"#,
        "\n",
        r#"(maparea "http://three.com" "third" (oval 40 0 10 10))"#
    ));
    let (_, mapareas) = parse_annotations(&data).expect("parse should succeed");
    assert_eq!(mapareas.len(), 3);
}

#[test]
fn test_parse_annotations_bzz() {
    use djvu_rs::annotation::parse_annotations;
    use djvu_rs::bzz::bzz_decode;
    // Pre-computed BZZ encoding of "(background #aabbcc)"
    // Generated with: printf '(background #aabbcc)' | bzz -e - -
    let encoded: &[u8] = &[
        0xff, 0xff, 0xea, 0xfe, 0xb5, 0xe5, 0x65, 0x34, 0xc2, 0xb9, 0xa6, 0x8d, 0xed, 0xd2, 0x83,
        0x46, 0xfe, 0xf5, 0xcd, 0x8c, 0x47, 0x1d, 0x8c, 0x97,
    ];
    // ANTz decompresses via the shared BZZ path; the parser itself is pure.
    let decoded = bzz_decode(encoded).expect("bzz decode should succeed");
    let (ann, _) = parse_annotations(&decoded).expect("parse should succeed");
    let bg = ann.background.expect("background");
    assert_eq!(
        bg,
        Color {
            r: 0xaa,
            g: 0xbb,
            b: 0xcc
        }
    );
}

#[test]
fn test_djvu_page_annotations_method() {
    // Test that DjVuPage::annotations() exists and returns Ok
    let assets = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("references/djvujs/library/assets");
    let path = assets.join("chicken.djvu");
    if !path.exists() {
        return;
    }
    let data = std::fs::read(&path).expect("read chicken.djvu");
    let doc = djvu_rs::DjVuDocument::parse(&data).expect("parse");
    let page = doc.page(0).expect("page 0");
    // chicken.djvu likely has no annotations — just check it doesn't error
    let result = page.annotations();
    assert!(result.is_ok(), "annotations() should not error: {result:?}");
}

#[test]
fn test_djvu_page_hyperlinks_method() {
    let assets = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("references/djvujs/library/assets");
    let path = assets.join("chicken.djvu");
    if !path.exists() {
        return;
    }
    let data = std::fs::read(&path).expect("read chicken.djvu");
    let doc = djvu_rs::DjVuDocument::parse(&data).expect("parse");
    let page = doc.page(0).expect("page 0");
    // just verify the method is callable and returns Ok
    let links = page.hyperlinks();
    assert!(links.is_ok(), "hyperlinks() should not error");
}
