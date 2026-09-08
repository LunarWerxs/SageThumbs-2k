//! Tesseract OCR backend (requires `ocr-tesseract` feature).
//!
//! Uses the system Tesseract installation via the `tesseract` crate.

use crate::ocr::{OcrBackend, OcrError, OcrOptions};
use crate::pixmap::Pixmap;
use crate::text::{Rect, TextLayer, TextZone, TextZoneKind};

/// Tesseract OCR backend.
///
/// Requires Tesseract and Leptonica to be installed on the system.
/// Language data files must be available (e.g. `tessdata/eng.traineddata`).
pub struct TesseractBackend {
    /// Path to tessdata directory. `None` uses the system default.
    pub tessdata_dir: Option<String>,
}

impl TesseractBackend {
    /// Create a new Tesseract backend with the default tessdata location.
    pub fn new() -> Self {
        Self { tessdata_dir: None }
    }

    /// Create a Tesseract backend pointing to a custom tessdata directory.
    pub fn with_tessdata(path: impl Into<String>) -> Self {
        Self {
            tessdata_dir: Some(path.into()),
        }
    }
}

impl Default for TesseractBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl OcrBackend for TesseractBackend {
    fn recognize(&self, pixmap: &Pixmap, options: &OcrOptions) -> Result<TextLayer, OcrError> {
        let rgb = pixmap.to_rgb();

        // Initialize Tesseract
        let data_path = self.tessdata_dir.as_deref();
        let mut api = tesseract::Tesseract::new(data_path, Some(&options.languages))
            .map_err(|e| OcrError::InitFailed(e.to_string()))?;

        api = api
            .set_frame(
                &rgb,
                pixmap.width as i32,
                pixmap.height as i32,
                3,
                (pixmap.width * 3) as i32,
            )
            .map_err(|e| OcrError::RecognitionFailed(e.to_string()))?;

        api = api.set_source_resolution(options.dpi as i32);

        // Get full text
        let text = api
            .get_text()
            .map_err(|e| OcrError::RecognitionFailed(e.to_string()))?;

        // Build word-level zones from hOCR output
        let hocr = api
            .get_hocr_text(0)
            .map_err(|e| OcrError::RecognitionFailed(e.to_string()))?;

        let zones = parse_hocr_to_zones(&hocr, pixmap.width, pixmap.height);

        Ok(TextLayer { text, zones })
    }
}

/// Parse Tesseract hOCR output into DjVu text zones.
fn parse_hocr_to_zones(hocr: &str, page_w: u32, page_h: u32) -> Vec<TextZone> {
    // Build a page-level zone containing line and word zones extracted from hOCR.
    // hOCR bbox format: "bbox x1 y1 x2 y2" (top-left origin, matching our Rect).
    let mut words = Vec::new();
    let mut lines = Vec::new();

    for line in hocr.lines() {
        let line = line.trim();
        // Detect word spans
        if line.contains("ocrx_word") || line.contains("ocr_word") {
            if let Some(zone) = parse_hocr_element(line, TextZoneKind::Word) {
                words.push(zone);
            }
        } else if line.contains("ocr_line") {
            // If we have accumulated words, flush them into a line zone
            if !words.is_empty()
                && let Some(mut line_zone) = parse_hocr_element(line, TextZoneKind::Line)
            {
                line_zone.children = core::mem::take(&mut words);
                // Concatenate children text
                line_zone.text = line_zone
                    .children
                    .iter()
                    .map(|w| w.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
                lines.push(line_zone);
            }
            words.clear();
        }
    }

    // Flush remaining words into a synthetic line
    if !words.is_empty() {
        let text: String = words
            .iter()
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let rect = bounding_rect(&words);
        lines.push(TextZone {
            kind: TextZoneKind::Line,
            rect,
            text,
            children: words,
        });
    }

    // Wrap all lines in a page zone
    let page_text: String = lines
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    vec![TextZone {
        kind: TextZoneKind::Page,
        rect: Rect {
            x: 0,
            y: 0,
            width: page_w,
            height: page_h,
        },
        text: page_text,
        children: lines,
    }]
}

/// Parse a single hOCR element's bbox and text content.
fn parse_hocr_element(html: &str, kind: TextZoneKind) -> Option<TextZone> {
    let bbox = parse_bbox(html)?;
    let text = extract_inner_text(html);
    Some(TextZone {
        kind,
        rect: bbox,
        text,
        children: Vec::new(),
    })
}

/// Extract "bbox x1 y1 x2 y2" from an hOCR title attribute.
fn parse_bbox(s: &str) -> Option<Rect> {
    let idx = s.find("bbox ")?;
    let rest = &s[idx + 5..];
    let end = rest.find([';', '"', '\'']).unwrap_or(rest.len());
    let coords: Vec<u32> = rest[..end]
        .split_whitespace()
        .filter_map(|n| n.parse().ok())
        .collect();
    if coords.len() >= 4 {
        Some(Rect {
            x: coords[0],
            y: coords[1],
            width: coords[2].saturating_sub(coords[0]),
            height: coords[3].saturating_sub(coords[1]),
        })
    } else {
        None
    }
}

/// Strip HTML tags to get inner text.
fn extract_inner_text(html: &str) -> String {
    let mut result = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => result.push(c),
            _ => {}
        }
    }
    result.trim().to_string()
}

/// Compute bounding rect of a set of zones.
fn bounding_rect(zones: &[TextZone]) -> Rect {
    if zones.is_empty() {
        return Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        };
    }
    let mut x1 = u32::MAX;
    let mut y1 = u32::MAX;
    let mut x2 = 0u32;
    let mut y2 = 0u32;
    for z in zones {
        x1 = x1.min(z.rect.x);
        y1 = y1.min(z.rect.y);
        x2 = x2.max(z.rect.x + z.rect.width);
        y2 = y2.max(z.rect.y + z.rect.height);
    }
    Rect {
        x: x1,
        y: y1,
        width: x2.saturating_sub(x1),
        height: y2.saturating_sub(y1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- parse_bbox -----------------------------------------------------------

    #[test]
    fn parse_bbox_basic() {
        let html = r#"<span class='ocrx_word' title='bbox 10 20 50 40; x_wconf 95'>hello</span>"#;
        let r = parse_bbox(html).unwrap();
        assert_eq!(r.x, 10);
        assert_eq!(r.y, 20);
        assert_eq!(r.width, 40); // 50 - 10
        assert_eq!(r.height, 20); // 40 - 20
    }

    #[test]
    fn parse_bbox_no_semicolon() {
        let html = r#"title="bbox 0 5 100 50""#;
        let r = parse_bbox(html).unwrap();
        assert_eq!(r.x, 0);
        assert_eq!(r.y, 5);
        assert_eq!(r.width, 100);
        assert_eq!(r.height, 45);
    }

    #[test]
    fn parse_bbox_missing_returns_none() {
        assert!(parse_bbox("no bbox here").is_none());
    }

    #[test]
    fn parse_bbox_too_few_coords() {
        // Only 3 numbers after "bbox" — not a valid bbox
        assert!(parse_bbox("title='bbox 10 20 50'").is_none());
    }

    #[test]
    fn parse_bbox_inverted_clamps_to_zero() {
        // x2 < x1 would underflow — saturating_sub returns 0
        let html = "title='bbox 50 40 10 20'";
        let r = parse_bbox(html).unwrap();
        assert_eq!(r.x, 50);
        assert_eq!(r.y, 40);
        assert_eq!(r.width, 0);
        assert_eq!(r.height, 0);
    }

    // ---- extract_inner_text --------------------------------------------------

    #[test]
    fn extract_inner_text_basic() {
        let html = "<span class='ocrx_word'>hello</span>";
        assert_eq!(extract_inner_text(html), "hello");
    }

    #[test]
    fn extract_inner_text_nested_tags() {
        let html = "<p><span>foo</span> <em>bar</em></p>";
        assert_eq!(extract_inner_text(html), "foo bar");
    }

    #[test]
    fn extract_inner_text_trims_whitespace() {
        let html = "  <span>  word  </span>  ";
        assert_eq!(extract_inner_text(html), "word");
    }

    #[test]
    fn extract_inner_text_no_tags() {
        assert_eq!(extract_inner_text("plain text"), "plain text");
    }

    #[test]
    fn extract_inner_text_empty() {
        assert_eq!(extract_inner_text("<span></span>"), "");
    }

    // ---- parse_hocr_to_zones -------------------------------------------------

    #[test]
    fn parse_hocr_empty_input() {
        let zones = parse_hocr_to_zones("", 100, 200);
        assert_eq!(zones.len(), 1);
        assert_eq!(zones[0].kind, TextZoneKind::Page);
        assert!(zones[0].children.is_empty());
    }

    #[test]
    fn parse_hocr_single_word() {
        let hocr = "<span class='ocrx_word' title='bbox 10 20 50 40'>hello</span>";
        let zones = parse_hocr_to_zones(hocr, 100, 200);
        assert_eq!(zones[0].kind, TextZoneKind::Page);
        assert_eq!(zones[0].rect.width, 100);
        assert_eq!(zones[0].rect.height, 200);
        // Word is flushed into a synthetic line
        let lines = &zones[0].children;
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].kind, TextZoneKind::Line);
        assert_eq!(lines[0].children.len(), 1);
        assert_eq!(lines[0].children[0].kind, TextZoneKind::Word);
        assert_eq!(lines[0].children[0].text, "hello");
    }

    #[test]
    fn parse_hocr_word_with_line() {
        // A line followed by its word — the parser flushes words accumulated
        // before the line marker into a line zone.
        let hocr = [
            "<span class='ocrx_word' title='bbox 10 20 50 40'>foo</span>",
            "<span class='ocr_line' title='bbox 5 15 200 50'>...</span>",
        ]
        .join("\n");
        let zones = parse_hocr_to_zones(&hocr, 800, 600);
        let lines = &zones[0].children;
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].kind, TextZoneKind::Line);
        assert_eq!(lines[0].children[0].text, "foo");
    }

    #[test]
    fn parse_hocr_page_rect_matches_input_dimensions() {
        let zones = parse_hocr_to_zones("", 640, 480);
        assert_eq!(zones[0].rect.x, 0);
        assert_eq!(zones[0].rect.y, 0);
        assert_eq!(zones[0].rect.width, 640);
        assert_eq!(zones[0].rect.height, 480);
    }

    // ---- bounding_rect -------------------------------------------------------

    #[test]
    fn bounding_rect_empty() {
        let r = bounding_rect(&[]);
        assert_eq!((r.x, r.y, r.width, r.height), (0, 0, 0, 0));
    }

    #[test]
    fn bounding_rect_single() {
        let zone = TextZone {
            kind: TextZoneKind::Word,
            rect: Rect {
                x: 10,
                y: 20,
                width: 30,
                height: 10,
            },
            text: String::new(),
            children: vec![],
        };
        let r = bounding_rect(&[zone]);
        assert_eq!(r.x, 10);
        assert_eq!(r.y, 20);
        assert_eq!(r.width, 30);
        assert_eq!(r.height, 10);
    }

    #[test]
    fn bounding_rect_two_non_overlapping() {
        let make = |x, y, w, h| TextZone {
            kind: TextZoneKind::Word,
            rect: Rect {
                x,
                y,
                width: w,
                height: h,
            },
            text: String::new(),
            children: vec![],
        };
        // First: x=0..30, y=0..10; Second: x=50..80, y=5..15
        let r = bounding_rect(&[make(0, 0, 30, 10), make(50, 5, 30, 10)]);
        assert_eq!(r.x, 0);
        assert_eq!(r.y, 0);
        assert_eq!(r.width, 80); // max x2=80, min x1=0
        assert_eq!(r.height, 15); // max y2=15, min y1=0
    }
}
