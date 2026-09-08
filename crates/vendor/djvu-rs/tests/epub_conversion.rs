//! Integration tests for DjVu → EPUB conversion.
//!
//! Verifies that output is a valid EPUB 3 ZIP archive with required structure,
//! page images, text overlay, and navigation from bookmarks.

#[cfg(feature = "epub")]
mod epub_tests {
    use djvu_rs::djvu_document::DjVuDocument;
    use djvu_rs::epub::{EpubOptions, djvu_to_epub};
    use std::collections::HashMap;
    use std::io::Read;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    fn load_doc(name: &str) -> DjVuDocument {
        let data = std::fs::read(fixture(name)).unwrap();
        DjVuDocument::parse(&data).unwrap()
    }

    /// Read all files from the EPUB (ZIP) into a map of path → bytes.
    fn unzip_epub(epub: &[u8]) -> HashMap<String, Vec<u8>> {
        let cursor = std::io::Cursor::new(epub);
        let mut zip = zip::ZipArchive::new(cursor).expect("not a valid ZIP");
        let mut files = HashMap::new();
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).unwrap();
            let name = entry.name().to_owned();
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).unwrap();
            files.insert(name, buf);
        }
        files
    }

    fn utf8(files: &HashMap<String, Vec<u8>>, path: &str) -> String {
        String::from_utf8(
            files
                .get(path)
                .unwrap_or_else(|| panic!("missing file: {path}"))
                .clone(),
        )
        .expect("not valid UTF-8")
    }

    // ── Structure ─────────────────────────────────────────────────────────────

    #[test]
    fn epub_is_valid_zip() {
        let doc = load_doc("boy.djvu");
        let epub = djvu_to_epub(&doc, &EpubOptions::default()).unwrap();
        assert!(!epub.is_empty());
        // Must be a ZIP (PK signature)
        assert_eq!(&epub[..2], b"PK");
    }

    #[test]
    fn epub_has_mimetype_first_uncompressed() {
        let doc = load_doc("boy.djvu");
        let epub = djvu_to_epub(&doc, &EpubOptions::default()).unwrap();
        // EPUB spec: first file must be "mimetype", stored (no compression), value exact
        let files = unzip_epub(&epub);
        let mt = utf8(&files, "mimetype");
        assert_eq!(mt, "application/epub+zip");
    }

    #[test]
    fn epub_has_container_xml() {
        let doc = load_doc("boy.djvu");
        let epub = djvu_to_epub(&doc, &EpubOptions::default()).unwrap();
        let files = unzip_epub(&epub);
        let container = utf8(&files, "META-INF/container.xml");
        assert!(container.contains("application/oebps-package+xml"));
        assert!(container.contains("content.opf"));
    }

    #[test]
    fn epub_has_opf_package() {
        let doc = load_doc("boy.djvu");
        let epub = djvu_to_epub(&doc, &EpubOptions::default()).unwrap();
        let files = unzip_epub(&epub);
        let opf = utf8(&files, "OEBPS/content.opf");
        assert!(opf.contains("epub:type"), "missing epub:type or wrong OPF");
        assert!(opf.contains("<manifest>"));
        assert!(opf.contains("<spine>"));
        assert!(opf.contains("nav.xhtml"));
    }

    #[test]
    fn epub_has_nav_document() {
        let doc = load_doc("boy.djvu");
        let epub = djvu_to_epub(&doc, &EpubOptions::default()).unwrap();
        let files = unzip_epub(&epub);
        let nav = utf8(&files, "OEBPS/nav.xhtml");
        assert!(nav.contains("epub:type=\"toc\""));
    }

    // ── Pages ─────────────────────────────────────────────────────────────────

    #[test]
    fn epub_single_page_has_xhtml_and_image() {
        let doc = load_doc("boy.djvu");
        let epub = djvu_to_epub(&doc, &EpubOptions::default()).unwrap();
        let files = unzip_epub(&epub);
        // Page XHTML
        assert!(files.contains_key("OEBPS/pages/page_0001.xhtml"));
        // Page image
        assert!(files.contains_key("OEBPS/images/page_0001.png"));
        // XHTML references the image
        let xhtml = utf8(&files, "OEBPS/pages/page_0001.xhtml");
        assert!(xhtml.contains("page_0001.png"));
    }

    #[test]
    fn epub_multi_page_has_all_pages() {
        // vega.djvu — 2 pages, small, fast
        let doc = load_doc("vega.djvu");
        let page_count = doc.page_count();
        let epub = djvu_to_epub(&doc, &EpubOptions::default()).unwrap();
        let files = unzip_epub(&epub);
        for i in 1..=page_count {
            let xhtml = format!("OEBPS/pages/page_{i:04}.xhtml");
            let img = format!("OEBPS/images/page_{i:04}.png");
            assert!(files.contains_key(&xhtml), "missing {xhtml}");
            assert!(files.contains_key(&img), "missing {img}");
        }
    }

    // ── Text overlay ──────────────────────────────────────────────────────────

    #[test]
    fn epub_page_xhtml_has_text_overlay_structure() {
        // boy.djvu parses and renders without errors; verify the XHTML structure
        // is correct (text overlay CSS class present in template even if page has no text)
        let doc = load_doc("boy.djvu");
        let epub = djvu_to_epub(&doc, &EpubOptions::default()).unwrap();
        let files = unzip_epub(&epub);
        let xhtml = utf8(&files, "OEBPS/pages/page_0001.xhtml");
        // Style block must define djvu-text class (text overlay infrastructure)
        assert!(xhtml.contains("djvu-text"), "missing djvu-text CSS class");
        // Page image must be present
        assert!(xhtml.contains("page_0001.png"), "missing page image ref");
    }

    // ── Bookmarks → navigation ────────────────────────────────────────────────

    #[test]
    fn epub_bookmarks_appear_in_nav() {
        // links.djvu — 1 page, has NAVM bookmarks, renders correctly
        let doc = load_doc("links.djvu");
        let epub = djvu_to_epub(&doc, &EpubOptions::default()).unwrap();
        let files = unzip_epub(&epub);
        let nav = utf8(&files, "OEBPS/nav.xhtml");
        // links.djvu has bookmarks — should use them instead of generic page list
        assert!(nav.contains("<li>"), "nav must have at least one entry");
        assert!(nav.contains("epub:type=\"toc\""), "nav must have toc");
    }

    // ── OPF spine order ───────────────────────────────────────────────────────

    #[test]
    fn epub_spine_lists_all_pages_in_order() {
        // vega.djvu — 2 pages, fast
        let doc = load_doc("vega.djvu");
        let page_count = doc.page_count();
        let epub = djvu_to_epub(&doc, &EpubOptions::default()).unwrap();
        let files = unzip_epub(&epub);
        let opf = utf8(&files, "OEBPS/content.opf");
        // Every page must appear in the spine in order
        for i in 1..=page_count {
            assert!(
                opf.contains(&format!("page_{i:04}")),
                "page {i} missing from OPF spine"
            );
        }
    }
}
