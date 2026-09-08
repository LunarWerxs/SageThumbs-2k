//! Integration tests for hOCR and ALTO XML text-layer serialization.

#[cfg(feature = "std")]
mod tests {
    use std::path::PathBuf;

    use djvu_rs::{
        DjVuDocument,
        text_serialize::{AltoOptions, HocrOptions, to_alto, to_hocr},
    };

    fn chicken_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("references/djvujs/library/assets/chicken.djvu")
    }

    fn doc_with_text() -> Option<DjVuDocument> {
        // The chicken.djvu may or may not have a text layer.
        // Find any test file that does.
        let data = std::fs::read(chicken_path()).ok()?;
        DjVuDocument::parse(&data).ok()
    }

    // ---- hOCR tests ---------------------------------------------------------

    #[test]
    fn hocr_output_starts_with_doctype() {
        let doc = doc_with_text().expect("need a test document");
        let hocr = to_hocr(&doc, &HocrOptions::default()).expect("hOCR generation should succeed");
        assert!(
            hocr.contains("<!DOCTYPE html>") || hocr.contains("<html"),
            "hOCR must contain an HTML root element"
        );
    }

    #[test]
    fn hocr_contains_ocr_page_class() {
        let doc = doc_with_text().expect("need a test document");
        let hocr = to_hocr(&doc, &HocrOptions::default()).expect("hOCR generation");
        // Every hOCR document with pages must have the ocr_page class
        assert!(
            hocr.contains("ocr_page"),
            "hOCR must contain ocr_page elements"
        );
    }

    #[test]
    fn hocr_contains_bbox_attributes() {
        let doc = doc_with_text().expect("need a test document");
        let hocr = to_hocr(&doc, &HocrOptions::default()).expect("hOCR generation");
        // Bounding boxes should be present in title attributes
        assert!(hocr.contains("bbox"), "hOCR must contain bbox coordinates");
    }

    #[test]
    fn hocr_all_pages_included() {
        let doc = doc_with_text().expect("need a test document");
        let page_count = doc.page_count();
        let hocr = to_hocr(&doc, &HocrOptions::default()).expect("hOCR generation");
        // Count ocr_page class occurrences (the div elements, not the meta tag)
        let page_occurrences = hocr.matches("class=\"ocr_page\"").count();
        assert_eq!(
            page_occurrences, page_count,
            "hOCR must include one ocr_page per document page"
        );
    }

    #[test]
    fn hocr_page_index_in_options() {
        let doc = doc_with_text().expect("need a test document");
        let opts = HocrOptions {
            page_index: Some(0),
            ..HocrOptions::default()
        };
        let hocr = to_hocr(&doc, &opts).expect("hOCR single-page generation");
        // Only one page should be included (count div elements, not meta tag mentions)
        assert_eq!(hocr.matches("class=\"ocr_page\"").count(), 1);
    }

    // ---- ALTO tests ---------------------------------------------------------

    #[test]
    fn alto_output_contains_root_element() {
        let doc = doc_with_text().expect("need a test document");
        let alto = to_alto(&doc, &AltoOptions::default()).expect("ALTO generation should succeed");
        assert!(
            alto.contains("<alto") || alto.contains("<ALTO"),
            "ALTO must contain root alto element"
        );
    }

    #[test]
    fn alto_contains_page_element() {
        let doc = doc_with_text().expect("need a test document");
        let alto = to_alto(&doc, &AltoOptions::default()).expect("ALTO generation");
        assert!(alto.contains("<Page"), "ALTO must contain Page elements");
    }

    #[test]
    fn alto_contains_width_height() {
        let doc = doc_with_text().expect("need a test document");
        let alto = to_alto(&doc, &AltoOptions::default()).expect("ALTO generation");
        assert!(
            alto.contains("WIDTH=") || alto.contains("width="),
            "ALTO Page must have WIDTH attribute"
        );
    }

    #[test]
    fn alto_all_pages_included() {
        let doc = doc_with_text().expect("need a test document");
        let page_count = doc.page_count();
        let alto = to_alto(&doc, &AltoOptions::default()).expect("ALTO generation");
        let page_count_in_output = alto.matches("<Page").count();
        assert_eq!(
            page_count_in_output, page_count,
            "ALTO must include one Page per document page"
        );
    }

    #[test]
    fn alto_namespace_present() {
        let doc = doc_with_text().expect("need a test document");
        let alto = to_alto(&doc, &AltoOptions::default()).expect("ALTO generation");
        // ALTO 4.x uses the alto schema
        assert!(
            alto.contains("alto") || alto.contains("ALTO"),
            "ALTO output must reference the ALTO namespace/schema"
        );
    }

    // ---- CLI text format tests ---------------------------------------------

    #[test]
    fn hocr_is_well_formed_xml_or_html() {
        let doc = doc_with_text().expect("need a test document");
        let hocr = to_hocr(&doc, &HocrOptions::default()).expect("hOCR generation");
        // Must have balanced root tags
        assert!(hocr.contains("</html>"), "hOCR must have closing html tag");
    }

    #[test]
    fn alto_is_well_formed_xml() {
        let doc = doc_with_text().expect("need a test document");
        let alto = to_alto(&doc, &AltoOptions::default()).expect("ALTO generation");
        // Must have XML declaration or root tag
        assert!(
            alto.contains("<?xml") || alto.contains("<alto"),
            "ALTO must be XML"
        );
        assert!(
            alto.contains("</alto>") || alto.contains("</ALTO>"),
            "ALTO must have closing tag"
        );
    }
}
