//! DjVu annotation parser — phase 4.
//!
//! Parses ANTa (plain) and ANTz (BZZ-compressed) annotation chunks into
//! typed structures.
//!
//! ## Key public types
//!
//! - `Annotation` — page-level annotation (background, zoom, mode)
//! - `MapArea` — a clickable area with URL, description, and shape
//! - `Shape` — rect / oval / poly / line / text area shape
//! - `Color` — RGB color parsed from `#rrggbb` strings
//! - `AnnotationError` — typed errors from this module
//!
//! ## Format notes
//!
//! ANTa/ANTz contain S-expression-like text:
//! ```text
//! (background #ffffff)
//! (zoom 100)
//! (mode color)
//! (maparea "url" "desc" (rect x y w h) ...)
//! ```
//!
//! This parser handles only the subset documented in the DjVu v3 spec
//! (background, zoom, mode, maparea with rect/oval/poly/line/text shapes).

#[cfg(not(feature = "std"))]
use alloc::{
    format,
    string::{String, ToString},
    vec::Vec,
};

use crate::sexp::SExpr;

// ---- Error ------------------------------------------------------------------

/// Errors from annotation parsing.
#[derive(Debug, thiserror::Error)]
pub enum AnnotationError {
    /// A hex color string is malformed.
    #[error("invalid color value: {0}")]
    InvalidColor(String),

    /// A numeric value could not be parsed.
    #[error("invalid number: {0}")]
    InvalidNumber(String),

    /// The S-expression is malformed (missing closing paren, etc.).
    #[error("malformed s-expression: {0}")]
    Parse(String),
}

// ---- Public types -----------------------------------------------------------

/// An RGB color value.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// Bounding rectangle in DjVu coordinates.
///
/// Note: coordinates are in DjVu native space (bottom-left origin).
/// Integration with the text layer coordinate system requires manual remap.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Shape of a maparea.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Shape {
    Rect(Rect),
    Oval(Rect),
    Poly(Vec<(u32, u32)>),
    Line(u32, u32, u32, u32),
    Text(Rect),
}

/// A border style (currently stored as a raw string for forward-compat).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Border {
    pub style: String,
}

/// A highlight color for a maparea.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Highlight {
    pub color: Color,
}

/// A clickable map area (hyperlink or highlight region) in a DjVu page.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct MapArea {
    /// Target URL (empty string if no link).
    pub url: String,
    /// Human-readable description.
    pub description: String,
    /// Shape of the area.
    pub shape: Shape,
    /// Optional border style.
    pub border: Option<Border>,
    /// Optional highlight color.
    pub highlight: Option<Highlight>,
}

/// Page-level annotation data.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Annotation {
    /// Background color for the page view.
    pub background: Option<Color>,
    /// Zoom level (percentage, e.g. 100 = 100%).
    pub zoom: Option<u32>,
    /// Display mode string (e.g. "color", "bw", "fore", "back").
    pub mode: Option<String>,
}

// ---- Entry points -----------------------------------------------------------

/// Parse a decoded annotation payload (the bytes of an `ANTa` chunk, or a
/// BZZ-decompressed `ANTz` chunk).
///
/// This is a pure parser: BZZ decompression for `ANTz` is handled upstream by
/// [`DjVuPage::chunk_payload`](crate::DjVuPage::chunk_payload).
pub fn parse_annotations(data: &[u8]) -> Result<(Annotation, Vec<MapArea>), AnnotationError> {
    let text = crate::lenient_text::decode_lossy(data);
    parse_annotation_text(&text)
}

// ---- Annotation builder from S-expressions ----------------------------------

fn parse_annotation_text(text: &str) -> Result<(Annotation, Vec<MapArea>), AnnotationError> {
    if text.trim().is_empty() {
        return Ok((Annotation::default(), Vec::new()));
    }

    let exprs = crate::sexp::parse_sexprs(text);

    let mut annotation = Annotation::default();
    let mut mapareas = Vec::new();

    for expr in &exprs {
        if let SExpr::List(items) = expr {
            let head = match items.first() {
                Some(SExpr::Atom(s)) => s.as_str(),
                _ => continue,
            };

            match head {
                "background" => {
                    if let Some(SExpr::Atom(color_str)) = items.get(1) {
                        annotation.background = Some(parse_color(color_str)?);
                    }
                }
                "zoom" => {
                    if let Some(SExpr::Atom(n)) = items.get(1) {
                        annotation.zoom = Some(parse_uint(n)?);
                    }
                }
                "mode" => {
                    if let Some(SExpr::Atom(m)) = items.get(1) {
                        annotation.mode = Some(m.clone());
                    }
                }
                "maparea" => {
                    if let Some(ma) = parse_maparea(items)? {
                        mapareas.push(ma);
                    }
                }
                _ => {} // ignore unknown top-level forms
            }
        }
    }

    Ok((annotation, mapareas))
}

fn parse_maparea(items: &[SExpr]) -> Result<Option<MapArea>, AnnotationError> {
    // (maparea "url" "desc" (shape ...) [options...])
    let url = match items.get(1) {
        Some(SExpr::Atom(s)) => s.clone(),
        _ => String::new(),
    };
    let description = match items.get(2) {
        Some(SExpr::Atom(s)) => s.clone(),
        _ => String::new(),
    };

    let shape_expr = match items.get(3) {
        Some(SExpr::List(l)) => l,
        _ => return Ok(None),
    };

    let shape = parse_shape(shape_expr)?;

    // Optional border / highlight (items[4..])
    let mut border = None;
    let mut highlight = None;
    for item in items.get(4..).unwrap_or(&[]) {
        if let SExpr::List(opts) = item {
            match opts.first() {
                Some(SExpr::Atom(s)) if s == "border" => {
                    if let Some(SExpr::Atom(style)) = opts.get(1) {
                        border = Some(Border {
                            style: style.clone(),
                        });
                    }
                }
                Some(SExpr::Atom(s)) if s == "hilite" => {
                    if let Some(SExpr::Atom(color)) = opts.get(1) {
                        highlight = Some(Highlight {
                            color: parse_color(color)?,
                        });
                    }
                }
                _ => {}
            }
        }
    }

    Ok(Some(MapArea {
        url,
        description,
        shape,
        border,
        highlight,
    }))
}

fn parse_shape(items: &[SExpr]) -> Result<Shape, AnnotationError> {
    let kind = match items.first() {
        Some(SExpr::Atom(s)) => s.as_str(),
        _ => return Err(AnnotationError::Parse("shape has no kind".to_string())),
    };

    match kind {
        "rect" => {
            let x = get_uint(items, 1)?;
            let y = get_uint(items, 2)?;
            let w = get_uint(items, 3)?;
            let h = get_uint(items, 4)?;
            Ok(Shape::Rect(Rect {
                x,
                y,
                width: w,
                height: h,
            }))
        }
        "oval" => {
            let x = get_uint(items, 1)?;
            let y = get_uint(items, 2)?;
            let w = get_uint(items, 3)?;
            let h = get_uint(items, 4)?;
            Ok(Shape::Oval(Rect {
                x,
                y,
                width: w,
                height: h,
            }))
        }
        "text" => {
            let x = get_uint(items, 1)?;
            let y = get_uint(items, 2)?;
            let w = get_uint(items, 3)?;
            let h = get_uint(items, 4)?;
            Ok(Shape::Text(Rect {
                x,
                y,
                width: w,
                height: h,
            }))
        }
        "line" => {
            let x1 = get_uint(items, 1)?;
            let y1 = get_uint(items, 2)?;
            let x2 = get_uint(items, 3)?;
            let y2 = get_uint(items, 4)?;
            Ok(Shape::Line(x1, y1, x2, y2))
        }
        "poly" => {
            // (poly x1 y1 x2 y2 ...)
            let mut pts = Vec::new();
            let mut i = 1usize;
            while i + 1 < items.len() {
                let x = get_uint(items, i)?;
                let y = get_uint(items, i + 1)?;
                pts.push((x, y));
                i += 2;
            }
            Ok(Shape::Poly(pts))
        }
        other => Err(AnnotationError::Parse(format!(
            "unknown shape kind: {other}"
        ))),
    }
}

// ---- Helpers ----------------------------------------------------------------

fn get_uint(items: &[SExpr], idx: usize) -> Result<u32, AnnotationError> {
    match items.get(idx) {
        Some(SExpr::Atom(s)) => parse_uint(s),
        _ => Err(AnnotationError::Parse(format!(
            "expected uint at position {idx}"
        ))),
    }
}

fn parse_uint(s: &str) -> Result<u32, AnnotationError> {
    s.parse::<u32>()
        .map_err(|_| AnnotationError::InvalidNumber(s.to_string()))
}

fn parse_color(s: &str) -> Result<Color, AnnotationError> {
    let hex = s.strip_prefix('#').unwrap_or(s);
    if hex.len() != 6 {
        return Err(AnnotationError::InvalidColor(s.to_string()));
    }
    let r = u8::from_str_radix(&hex[0..2], 16)
        .map_err(|_| AnnotationError::InvalidColor(s.to_string()))?;
    let g = u8::from_str_radix(&hex[2..4], 16)
        .map_err(|_| AnnotationError::InvalidColor(s.to_string()))?;
    let b = u8::from_str_radix(&hex[4..6], 16)
        .map_err(|_| AnnotationError::InvalidColor(s.to_string()))?;
    Ok(Color { r, g, b })
}

// ---- Encoder ----------------------------------------------------------------

/// Serialize an [`Annotation`] and a list of [`MapArea`]s to ANTa text bytes.
///
/// Produces the plain-text S-expression format consumed by [`parse_annotations`].
/// Returns an empty `Vec` if there is nothing to encode.
pub fn encode_annotations(ann: &Annotation, areas: &[MapArea]) -> Vec<u8> {
    let mut out = String::new();

    if let Some(ref c) = ann.background {
        out.push_str(&format!(
            "(background #{:02x}{:02x}{:02x})\n",
            c.r, c.g, c.b
        ));
    }
    if let Some(z) = ann.zoom {
        out.push_str(&format!("(zoom {z})\n"));
    }
    if let Some(ref m) = ann.mode {
        out.push_str(&format!("(mode {m})\n"));
    }

    for ma in areas {
        out.push_str(&encode_maparea(ma));
        out.push('\n');
    }

    out.into_bytes()
}

/// Serialize an [`Annotation`] and map areas to ANTz bytes (BZZ-compressed ANTa).
#[cfg(feature = "std")]
pub fn encode_annotations_bzz(ann: &Annotation, areas: &[MapArea]) -> Vec<u8> {
    let plain = encode_annotations(ann, areas);
    if plain.is_empty() {
        return Vec::new();
    }
    crate::bzz_encode::bzz_encode(&plain)
}

/// Serialize one maparea to its S-expression string (without trailing newline).
fn encode_maparea(ma: &MapArea) -> String {
    let mut s = String::from("(maparea ");
    s.push_str(&quote_str(&ma.url));
    s.push(' ');
    s.push_str(&quote_str(&ma.description));
    s.push(' ');
    s.push_str(&encode_shape(&ma.shape));
    if let Some(ref b) = ma.border {
        s.push_str(&format!(" (border {})", b.style));
    }
    if let Some(ref h) = ma.highlight {
        s.push_str(&format!(
            " (hilite #{:02x}{:02x}{:02x})",
            h.color.r, h.color.g, h.color.b
        ));
    }
    s.push(')');
    s
}

/// Serialize a [`Shape`] to its S-expression string.
fn encode_shape(shape: &Shape) -> String {
    match shape {
        Shape::Rect(r) => format!("(rect {} {} {} {})", r.x, r.y, r.width, r.height),
        Shape::Oval(r) => format!("(oval {} {} {} {})", r.x, r.y, r.width, r.height),
        Shape::Text(r) => format!("(text {} {} {} {})", r.x, r.y, r.width, r.height),
        Shape::Line(x1, y1, x2, y2) => format!("(line {x1} {y1} {x2} {y2})"),
        Shape::Poly(pts) => {
            let mut s = String::from("(poly");
            for (x, y) in pts {
                s.push_str(&format!(" {x} {y}"));
            }
            s.push(')');
            s
        }
    }
}

/// Quote a string for use in the ANTa S-expression format.
///
/// Produces `"..."` with backslash-escaping for `"` and `\`.
fn quote_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tokenizer-level behaviour (atoms, quoted strings, comments, escapes,
    // depth guard) now lives in `crate::sexp` and is covered by its own tests.

    // ── Color parsing ───────────────────────────────────────────────────────

    #[test]
    fn test_parse_color_valid() {
        let c = parse_color("#ff0080").unwrap();
        assert_eq!(
            c,
            Color {
                r: 255,
                g: 0,
                b: 128
            }
        );
    }

    #[test]
    fn test_parse_color_no_hash() {
        let c = parse_color("00ff00").unwrap();
        assert_eq!(c, Color { r: 0, g: 255, b: 0 });
    }

    #[test]
    fn test_parse_color_invalid_length() {
        assert!(matches!(
            parse_color("#fff"),
            Err(AnnotationError::InvalidColor(_))
        ));
    }

    #[test]
    fn test_parse_color_invalid_hex() {
        assert!(matches!(
            parse_color("#gggggg"),
            Err(AnnotationError::InvalidColor(_))
        ));
    }

    // ── Number parsing ──────────────────────────────────────────────────────

    #[test]
    fn test_parse_uint_valid() {
        assert_eq!(parse_uint("42").unwrap(), 42);
        assert_eq!(parse_uint("0").unwrap(), 0);
    }

    #[test]
    fn test_parse_uint_invalid() {
        assert!(matches!(
            parse_uint("abc"),
            Err(AnnotationError::InvalidNumber(_))
        ));
        assert!(matches!(
            parse_uint("-5"),
            Err(AnnotationError::InvalidNumber(_))
        ));
    }

    // ── Full annotation parsing ─────────────────────────────────────────────

    #[test]
    fn test_parse_empty() {
        let (ann, areas) = parse_annotations(b"").unwrap();
        assert!(ann.background.is_none());
        assert!(areas.is_empty());
    }

    #[test]
    fn test_parse_background() {
        let (ann, _) = parse_annotations(b"(background #ff0000)").unwrap();
        assert_eq!(ann.background, Some(Color { r: 255, g: 0, b: 0 }));
    }

    #[test]
    fn test_parse_zoom_and_mode() {
        let (ann, _) = parse_annotations(b"(zoom 150)(mode color)").unwrap();
        assert_eq!(ann.zoom, Some(150));
        assert_eq!(ann.mode.as_deref(), Some("color"));
    }

    #[test]
    fn test_parse_maparea_rect() {
        let input = br#"(maparea "http://example.com" "Example" (rect 10 20 100 50))"#;
        let (_, areas) = parse_annotations(input).unwrap();
        assert_eq!(areas.len(), 1);
        assert_eq!(areas[0].url, "http://example.com");
        assert_eq!(areas[0].description, "Example");
        assert!(matches!(&areas[0].shape, Shape::Rect(r) if r.x == 10 && r.y == 20));
    }

    #[test]
    fn test_parse_maparea_decodes_cp1252_byte_leniently() {
        let input = b"(maparea \"http://example.com/a\x96b\" \"Example\" (rect 10 20 100 50))";
        let (_, areas) = parse_annotations(input).unwrap();
        assert_eq!(areas.len(), 1);
        assert_eq!(areas[0].url, "http://example.com/a–b");
        assert_eq!(areas[0].description, "Example");
    }

    #[test]
    fn test_parse_maparea_oval() {
        let input = br#"(maparea "" "" (oval 0 0 50 50))"#;
        let (_, areas) = parse_annotations(input).unwrap();
        assert!(matches!(&areas[0].shape, Shape::Oval(_)));
    }

    #[test]
    fn test_parse_maparea_poly() {
        let input = br#"(maparea "" "" (poly 0 0 10 0 10 10 0 10))"#;
        let (_, areas) = parse_annotations(input).unwrap();
        if let Shape::Poly(pts) = &areas[0].shape {
            assert_eq!(pts.len(), 4);
            assert_eq!(pts[0], (0, 0));
            assert_eq!(pts[2], (10, 10));
        } else {
            panic!("expected poly shape");
        }
    }

    #[test]
    fn test_parse_maparea_line() {
        let input = br#"(maparea "" "" (line 0 0 100 100))"#;
        let (_, areas) = parse_annotations(input).unwrap();
        assert!(matches!(&areas[0].shape, Shape::Line(0, 0, 100, 100)));
    }

    #[test]
    fn test_parse_maparea_with_border_and_hilite() {
        let input = br#"(maparea "" "" (rect 0 0 10 10) (border solid) (hilite #00ff00))"#;
        let (_, areas) = parse_annotations(input).unwrap();
        assert_eq!(areas[0].border.as_ref().unwrap().style, "solid");
        assert_eq!(
            areas[0].highlight.as_ref().unwrap().color,
            Color { r: 0, g: 255, b: 0 }
        );
    }

    #[test]
    fn test_parse_unknown_shape() {
        let input = br#"(maparea "" "" (circle 0 0 10))"#;
        assert!(matches!(
            parse_annotations(input),
            Err(AnnotationError::Parse(_))
        ));
    }

    #[test]
    fn test_parse_unknown_toplevel_ignored() {
        let input = b"(unknown_key value)(zoom 100)";
        let (ann, _) = parse_annotations(input).unwrap();
        assert_eq!(ann.zoom, Some(100));
    }

    #[test]
    fn test_parse_deeply_nested_does_not_overflow() {
        // 200 open parens deeper than the reader's depth guard — must not
        // stack-overflow. Goes through the public entry point now that the
        // reader lives in `crate::sexp`.
        let input = format!("{}{}", "(".repeat(200), ")".repeat(200));
        let _ = parse_annotations(input.as_bytes());
    }

    #[test]
    fn test_parse_multiple_mapareas() {
        let input = br#"(maparea "a" "" (rect 0 0 1 1))(maparea "b" "" (rect 2 2 3 3))"#;
        let (_, areas) = parse_annotations(input).unwrap();
        assert_eq!(areas.len(), 2);
        assert_eq!(areas[0].url, "a");
        assert_eq!(areas[1].url, "b");
    }

    // ── Encoder roundtrips ──────────────────────────────────────────────────

    #[test]
    fn encode_empty_is_empty() {
        let ann = Annotation::default();
        let out = encode_annotations(&ann, &[]);
        assert!(out.is_empty());
    }

    #[test]
    fn encode_background_roundtrip() {
        let ann = Annotation {
            background: Some(Color {
                r: 255,
                g: 128,
                b: 0,
            }),
            zoom: None,
            mode: None,
        };
        let bytes = encode_annotations(&ann, &[]);
        let (dec, _) = parse_annotations(&bytes).unwrap();
        assert_eq!(dec.background, ann.background);
    }

    #[test]
    fn encode_zoom_and_mode_roundtrip() {
        let ann = Annotation {
            background: None,
            zoom: Some(150),
            mode: Some("color".to_string()),
        };
        let bytes = encode_annotations(&ann, &[]);
        let (dec, _) = parse_annotations(&bytes).unwrap();
        assert_eq!(dec.zoom, Some(150));
        assert_eq!(dec.mode, Some("color".to_string()));
    }

    #[test]
    fn encode_maparea_rect_roundtrip() {
        let ann = Annotation::default();
        let areas = vec![MapArea {
            url: "http://example.com".to_string(),
            description: "a link".to_string(),
            shape: Shape::Rect(Rect {
                x: 10,
                y: 20,
                width: 100,
                height: 50,
            }),
            border: None,
            highlight: None,
        }];
        let bytes = encode_annotations(&ann, &areas);
        let (_, dec_areas) = parse_annotations(&bytes).unwrap();
        assert_eq!(dec_areas.len(), 1);
        assert_eq!(dec_areas[0].url, "http://example.com");
        assert_eq!(dec_areas[0].description, "a link");
        assert!(matches!(&dec_areas[0].shape, Shape::Rect(r) if r.x == 10 && r.y == 20));
    }

    #[test]
    fn encode_maparea_poly_roundtrip() {
        let areas = vec![MapArea {
            url: String::new(),
            description: String::new(),
            shape: Shape::Poly(vec![(0, 0), (10, 0), (5, 10)]),
            border: None,
            highlight: None,
        }];
        let bytes = encode_annotations(&Annotation::default(), &areas);
        let (_, dec_areas) = parse_annotations(&bytes).unwrap();
        assert_eq!(dec_areas.len(), 1);
        assert!(matches!(&dec_areas[0].shape, Shape::Poly(pts) if pts == &[(0,0),(10,0),(5,10)]));
    }

    #[test]
    fn encode_maparea_with_border_and_hilite_roundtrip() {
        let areas = vec![MapArea {
            url: "x".to_string(),
            description: String::new(),
            shape: Shape::Rect(Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            }),
            border: Some(Border {
                style: "xor".to_string(),
            }),
            highlight: Some(Highlight {
                color: Color { r: 255, g: 0, b: 0 },
            }),
        }];
        let bytes = encode_annotations(&Annotation::default(), &areas);
        let (_, dec_areas) = parse_annotations(&bytes).unwrap();
        assert_eq!(dec_areas[0].border.as_ref().unwrap().style, "xor");
        assert_eq!(dec_areas[0].highlight.as_ref().unwrap().color.r, 255);
    }

    #[test]
    fn encode_url_with_quotes_roundtrip() {
        let areas = vec![MapArea {
            url: r#"has "quotes" and \backslash"#.to_string(),
            description: String::new(),
            shape: Shape::Line(0, 0, 1, 1),
            border: None,
            highlight: None,
        }];
        let bytes = encode_annotations(&Annotation::default(), &areas);
        let (_, dec_areas) = parse_annotations(&bytes).unwrap();
        assert_eq!(dec_areas[0].url, r#"has "quotes" and \backslash"#);
    }

    // ── text / oval shapes (parse + encode) ──────────────────────────────────

    #[test]
    fn parse_text_shape_roundtrip() {
        let data = b"(maparea \"url\" \"desc\" (text 5 10 100 50))";
        let (_, areas) = parse_annotations(data).unwrap();
        assert_eq!(areas.len(), 1);
        assert!(matches!(&areas[0].shape, Shape::Text(r) if r.x == 5 && r.width == 100));
    }

    #[test]
    fn parse_oval_shape_roundtrip() {
        let data = b"(maparea \"url\" \"desc\" (oval 1 2 30 40))";
        let (_, areas) = parse_annotations(data).unwrap();
        assert_eq!(areas.len(), 1);
        assert!(matches!(&areas[0].shape, Shape::Oval(r) if r.x == 1 && r.height == 40));
    }

    #[test]
    fn encode_shape_oval_and_text_variants() {
        let oval = Shape::Oval(Rect {
            x: 3,
            y: 4,
            width: 50,
            height: 60,
        });
        let text = Shape::Text(Rect {
            x: 1,
            y: 2,
            width: 10,
            height: 20,
        });
        assert_eq!(encode_shape(&oval), "(oval 3 4 50 60)");
        assert_eq!(encode_shape(&text), "(text 1 2 10 20)");
    }

    // ── parse_maparea default url/desc / missing shape ────────────────────────

    #[test]
    fn parse_maparea_non_atom_url_and_desc_default_to_empty() {
        // url (items[1]) and description (items[2]) are lists → default to ""
        let data = b"(maparea () () (rect 0 0 10 10))";
        let (_, areas) = parse_annotations(data).unwrap();
        assert_eq!(areas.len(), 1);
        assert_eq!(areas[0].url, "");
        assert_eq!(areas[0].description, "");
    }

    #[test]
    fn parse_maparea_no_shape_list_is_skipped() {
        // items[3] is an atom, not a list → returns None → area is dropped
        let data = b"(maparea \"url\" \"desc\" notalist)";
        let (_, areas) = parse_annotations(data).unwrap();
        assert_eq!(areas.len(), 0);
    }

    // ── parse_shape error paths ───────────────────────────────────────────────

    #[test]
    fn parse_shape_empty_list_returns_error() {
        let data = b"(maparea \"url\" \"desc\" ())";
        assert!(parse_annotations(data).is_err());
    }

    #[test]
    fn parse_shape_bad_number_returns_error() {
        let data = b"(maparea \"url\" \"desc\" (rect 0 0 abc 10))";
        assert!(parse_annotations(data).is_err());
    }

    #[test]
    fn parse_shape_missing_coordinate_returns_error() {
        // get_uint called with idx beyond the list length → Parse error
        let data = b"(maparea \"url\" \"desc\" (rect 0 0))";
        assert!(parse_annotations(data).is_err());
    }

    // ── encode_annotations_bzz empty short-circuit ───────────────────────────

    #[test]
    fn encode_annotations_bzz_empty_annotation_returns_empty() {
        let result = encode_annotations_bzz(&Annotation::default(), &[]);
        assert!(result.is_empty());
    }
}
