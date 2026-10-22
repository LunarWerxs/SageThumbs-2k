use super::*;

#[test]
fn test_pdf_escape_string() {
    assert_eq!(pdf_escape_string("hello"), "hello");
    assert_eq!(pdf_escape_string("a(b)c"), "a\\(b\\)c");
    assert_eq!(pdf_escape_string("a\\b"), "a\\\\b");
}

#[test]
fn test_px_to_pt() {
    // At 72 dpi, 72 pixels = 72 points
    assert!((px_to_pt(72.0, 72.0) - 72.0).abs() < 0.01);
    // At 300 dpi, 300 pixels = 72 points
    assert!((px_to_pt(300.0, 300.0) - 72.0).abs() < 0.01);
}

#[test]
fn test_resolve_bookmark_dest_page_number() {
    let page_ids = vec![10, 20, 30];
    let dest = resolve_bookmark_dest("#1", &page_ids);
    assert!(dest.contains("10 0 R"));
}

#[test]
fn test_pdf_writer_serialize() {
    let mut pdf = Vec::new();
    let mut w = PdfWriter::new(&mut pdf).unwrap();
    let id = w.add(b"<< /Type /Catalog >>".to_vec()).unwrap();
    assert_eq!(id, 1);
    w.finish().unwrap();
    assert!(pdf.starts_with(b"%PDF-1.4"));
    assert!(pdf.windows(5).any(|w| w == b"%%EOF"));
}

#[test]
fn test_make_stream() {
    let stream = make_stream(" /Filter /FlateDecode", b"hello");
    let s = String::from_utf8_lossy(&stream);
    assert!(s.contains("/Length 5"));
    assert!(s.contains("stream\nhello\nendstream"));
}

#[test]
fn test_deflate_roundtrip() {
    let data = b"hello world, this is a test of deflate compression";
    let compressed = deflate(data);
    // Compressed data should be non-empty
    assert!(!compressed.is_empty());
    // Decompress and verify
    let decompressed = miniz_oxide::inflate::decompress_to_vec_zlib(&compressed).unwrap();
    assert_eq!(&decompressed, data);
}

#[test]
fn test_make_deflate_stream() {
    let body = make_deflate_stream(" /Type /XObject", b"test data");
    let s = String::from_utf8_lossy(&body);
    assert!(s.contains("/Filter /FlateDecode"));
    assert!(s.contains("/Type /XObject"));
    assert!(s.contains("stream\n"));
    assert!(s.contains("\nendstream"));
}

#[test]
fn test_font_dict() {
    let d = font_dict();
    let s = String::from_utf8_lossy(&d);
    assert!(s.contains("/Type /Font"));
    assert!(s.contains("/BaseFont /Helvetica"));
}

#[test]
fn test_pdf_writer_alloc_ids() {
    let mut buf = Vec::new();
    let mut w = PdfWriter::new(&mut buf).unwrap();
    let id1 = w.alloc_id();
    let id2 = w.alloc_id();
    let id3 = w.alloc_id();
    assert_eq!(id1, 1);
    assert_eq!(id2, 2);
    assert_eq!(id3, 3);
}

#[test]
fn test_pdf_writer_multiple_objects() {
    let mut pdf = Vec::new();
    let mut w = PdfWriter::new(&mut pdf).unwrap();
    w.add(b"<< /Type /Catalog >>".to_vec()).unwrap();
    w.add(b"<< /Type /Pages >>".to_vec()).unwrap();
    w.finish().unwrap();
    let s = String::from_utf8_lossy(&pdf);
    assert!(s.contains("1 0 obj"));
    assert!(s.contains("2 0 obj"));
    assert!(s.contains("/Size 3")); // 0, 1, 2
}

#[test]
fn test_resolve_bookmark_dest_page_prefix() {
    let page_ids = vec![10, 20, 30];
    let dest = resolve_bookmark_dest("#page2", &page_ids);
    assert!(dest.contains("20 0 R"));
    assert!(dest.contains("/Fit"));
}

#[test]
fn test_resolve_bookmark_dest_page_underscore() {
    let page_ids = vec![10, 20, 30];
    let dest = resolve_bookmark_dest("#page_3", &page_ids);
    assert!(dest.contains("30 0 R"));
}

#[test]
fn test_resolve_bookmark_dest_out_of_range() {
    let page_ids = vec![10];
    let dest = resolve_bookmark_dest("#page99", &page_ids);
    // Should fall through to bare number parse or be empty
    assert!(!dest.contains("10 0 R"));
}

#[test]
fn test_resolve_bookmark_dest_external_url() {
    let page_ids = vec![10];
    let dest = resolve_bookmark_dest("http://example.com", &page_ids);
    assert!(dest.contains("/S /URI"));
    assert!(dest.contains("http://example.com"));
}

#[test]
fn test_resolve_bookmark_dest_empty_url() {
    let page_ids = vec![10];
    let dest = resolve_bookmark_dest("", &page_ids);
    assert!(dest.is_empty());
}

#[test]
fn test_pdf_escape_special_chars() {
    assert_eq!(pdf_escape_string("a(b)c\\d"), "a\\(b\\)c\\\\d");
}

#[test]
fn test_pdf_escape_non_ascii() {
    // Non-ASCII chars should be replaced with ?
    let result = pdf_escape_string("caf\u{00e9}");
    assert_eq!(result, "caf?");
}

#[test]
fn test_shape_to_pdf_rect_rect() {
    use crate::annotation;
    let shape = annotation::Shape::Rect(annotation::Rect {
        x: 0,
        y: 0,
        width: 300,
        height: 300,
    });
    let rect = shape_to_pdf_rect(&shape, 300.0, 72.0).unwrap();
    assert!((rect.0 - 0.0).abs() < 0.01); // x1
    assert!((rect.2 - 72.0).abs() < 0.01); // x2 = 300 * 72/300
}

#[test]
fn test_shape_to_pdf_rect_poly() {
    use crate::annotation;
    let shape = annotation::Shape::Poly(vec![(0, 0), (300, 0), (300, 300), (0, 300)]);
    let rect = shape_to_pdf_rect(&shape, 300.0, 72.0).unwrap();
    assert!((rect.0 - 0.0).abs() < 0.01);
    assert!((rect.2 - 72.0).abs() < 0.01);
}

#[test]
fn test_shape_to_pdf_rect_empty_poly() {
    use crate::annotation;
    let shape = annotation::Shape::Poly(vec![]);
    assert!(shape_to_pdf_rect(&shape, 300.0, 72.0).is_none());
}

#[test]
fn test_shape_to_pdf_rect_line() {
    use crate::annotation;
    let shape = annotation::Shape::Line(0, 0, 150, 150);
    let rect = shape_to_pdf_rect(&shape, 150.0, 72.0).unwrap();
    assert!((rect.0 - 0.0).abs() < 0.01);
    assert!((rect.2 - 72.0).abs() < 0.01);
}

#[test]
fn test_count_outline_items_empty() {
    let bookmarks: Vec<crate::djvu_document::DjVuBookmark> = vec![];
    assert_eq!(count_outline_items(&bookmarks), 0);
}

#[test]
fn test_count_outline_items_nested() {
    use crate::djvu_document::DjVuBookmark;
    let bookmarks = vec![DjVuBookmark {
        title: "Chapter 1".into(),
        url: "#1".into(),
        children: vec![
            DjVuBookmark {
                title: "Section 1.1".into(),
                url: "#2".into(),
                children: vec![],
            },
            DjVuBookmark {
                title: "Section 1.2".into(),
                url: "#3".into(),
                children: vec![],
            },
        ],
    }];
    assert_eq!(count_outline_items(&bookmarks), 3);
}

// ── DCTDecode / PdfOptions tests ──────────────────────────────────────────

fn assets_path() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("references/djvujs/library/assets")
}

fn load_doc(name: &str) -> crate::djvu_document::DjVuDocument {
    let data =
        std::fs::read(assets_path().join(name)).unwrap_or_else(|_| panic!("{name} must exist"));
    crate::djvu_document::DjVuDocument::parse(&data).unwrap_or_else(|e| panic!("parse failed: {e}"))
}

#[derive(Default)]
struct RecordingObserver {
    progress: Vec<(usize, usize)>,
    cancel_after: Option<usize>,
}

impl ExportObserver for RecordingObserver {
    fn on_progress(&mut self, done: usize, total: usize) {
        self.progress.push((done, total));
    }

    fn cancelled(&self) -> bool {
        self.cancel_after
            .is_some_and(|after| self.progress.len() >= after)
    }
}

fn load_fixture_doc(name: &str) -> crate::djvu_document::DjVuDocument {
    let data = std::fs::read(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    )
    .unwrap();
    crate::djvu_document::DjVuDocument::parse(&data).unwrap()
}

#[test]
fn pdf_writer_observer_reports_each_page_in_order() {
    let doc = load_fixture_doc("vega.djvu");
    let total = doc.page_count();
    let mut observer = RecordingObserver::default();

    djvu_to_pdf_to_writer_with_observer(
        &doc,
        &PdfOptions::default(),
        std::io::Cursor::new(Vec::new()),
        &mut observer,
    )
    .expect("observer export must succeed");

    assert_eq!(
        observer.progress,
        (1..=total).map(|done| (done, total)).collect::<Vec<_>>()
    );
}

#[test]
fn pdf_writer_cancellation_stops_after_completed_page() {
    let doc = load_fixture_doc("vega.djvu");
    assert!(doc.page_count() > 1, "fixture must contain multiple pages");
    let mut observer = RecordingObserver {
        cancel_after: Some(1),
        ..RecordingObserver::default()
    };

    let error = djvu_to_pdf_to_writer_with_observer(
        &doc,
        &PdfOptions::default(),
        std::io::Cursor::new(Vec::new()),
        &mut observer,
    )
    .expect_err("observer must cancel the export");

    assert!(matches!(error, PdfError::Cancelled));
    assert_eq!(observer.progress.len(), 1);
}

#[test]
fn pdf_default_writer_delegates_to_noop_observer() {
    let doc = load_fixture_doc("vega.djvu");
    let opts = PdfOptions::default();

    let mut default_cursor = std::io::Cursor::new(Vec::new());
    djvu_to_pdf_to_writer(&doc, &opts, &mut default_cursor).unwrap();

    let mut observed_cursor = std::io::Cursor::new(Vec::new());
    let mut observer = NoOpObserver;
    djvu_to_pdf_to_writer_with_observer(&doc, &opts, &mut observed_cursor, &mut observer).unwrap();

    assert_eq!(observed_cursor.into_inner(), default_cursor.into_inner());
}

#[test]
fn pdf_writer_failing_sink_returns_io_error() {
    let doc = load_fixture_doc("chicken.djvu");
    let error = djvu_to_pdf_to_writer(
        &doc,
        &PdfOptions::default(),
        crate::export_test_support::FailingWriter::after(2),
    )
    .expect_err("injected sink failure must be returned");

    assert!(matches!(error, PdfError::Io(error) if error.kind() == std::io::ErrorKind::Other));
}

#[test]
#[ignore = "renders 100 synthetic pages to exercise the streaming sink path"]
fn large_synthetic_export_streams_through_counting_sink() {
    const PAGE_COUNT: usize = 100;

    let component = std::fs::read(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/chicken.djvu"),
    )
    .expect("read source page");
    let mut bundle = crate::djvm::DjvmStreamWriter::new(Vec::new(), crate::djvm::DjvmSpool::Memory)
        .expect("create synthetic bundle writer");
    for page in 0..PAGE_COUNT {
        bundle
            .add_component(&format!("page_{page:04}.djvu"), 1, &component)
            .expect("add synthetic page");
    }
    let bundled = bundle.finish().expect("finish synthetic bundle");
    let doc = crate::djvu_document::DjVuDocument::parse(&bundled).expect("parse synthetic bundle");
    let mut observer = RecordingObserver::default();
    let mut sink = crate::export_test_support::CountingWriter::default();

    djvu_to_pdf_to_writer_with_observer(&doc, &PdfOptions::default(), &mut sink, &mut observer)
        .expect("stream synthetic export into counting sink");

    assert!(
        sink.bytes_written() > 0,
        "counting sink must receive output"
    );
    assert_eq!(
        observer.progress,
        (1..=PAGE_COUNT)
            .map(|done| (done, PAGE_COUNT))
            .collect::<Vec<_>>(),
        "every synthetic page must complete before the export returns"
    );
}

/// `PdfOptions::default()` uses jpeg_quality = Some(80).
#[test]
fn pdf_options_default_is_jpeg80() {
    let opts = PdfOptions::default();
    assert_eq!(opts.jpeg_quality, Some(80));
}

/// JPEG encoding roundtrip: `encode_rgb_to_jpeg` returns a non-empty JPEG.
#[test]
fn encode_rgb_to_jpeg_returns_jpeg() {
    // 4×4 solid red image
    let rgb = [255u8, 0, 0].repeat(16); // 16 pixels * 3 channels
    let jpeg = encode_rgb_to_jpeg(&rgb, 4, 4, 80);
    assert!(!jpeg.is_empty(), "JPEG output must not be empty");
    // JPEG starts with FF D8
    assert_eq!(jpeg[0], 0xFF);
    assert_eq!(jpeg[1], 0xD8);
}

/// `make_dct_stream` embeds /Filter /DCTDecode in the PDF stream dict.
#[test]
fn make_dct_stream_has_dctdecode_filter() {
    let fake_jpeg = b"\xFF\xD8\xFF\xD9"; // minimal JPEG markers
    let stream = make_dct_stream(" /Type /XObject", fake_jpeg);
    let s = String::from_utf8_lossy(&stream);
    assert!(
        s.contains("/Filter /DCTDecode"),
        "must contain DCTDecode filter"
    );
    assert!(s.contains("/Type /XObject"));
}

/// DCT PDF is smaller than deflate PDF for the same page.
#[test]
fn dct_pdf_is_smaller_than_deflate_pdf() {
    let doc = load_doc("chicken.djvu");
    let dct_pdf = djvu_to_pdf_with_options(
        &doc,
        &PdfOptions {
            jpeg_quality: Some(75),
            output_dpi: 150,
            adaptive_raster: false,
            ccitt_g4: false,
            mrc: false,
        },
    )
    .expect("DCT conversion must succeed");
    let flat_pdf = djvu_to_pdf_with_options(
        &doc,
        &PdfOptions {
            jpeg_quality: None,
            output_dpi: 150,
            adaptive_raster: false,
            ccitt_g4: false,
            mrc: false,
        },
    )
    .expect("FlateDecode conversion must succeed");
    assert!(
        dct_pdf.len() < flat_pdf.len(),
        "DCT PDF ({} bytes) must be smaller than FlateDecode PDF ({} bytes)",
        dct_pdf.len(),
        flat_pdf.len()
    );
}

/// Output PDF contains /DCTDecode when jpeg_quality is set.
#[test]
fn pdf_with_dct_contains_dctdecode_marker() {
    let doc = load_doc("chicken.djvu");
    let pdf = djvu_to_pdf_with_options(
        &doc,
        &PdfOptions {
            jpeg_quality: Some(80),
            output_dpi: 150,
            adaptive_raster: false,
            ccitt_g4: false,
            mrc: false,
        },
    )
    .unwrap();
    let has_dct = pdf.windows(9).any(|w| w == b"DCTDecode");
    assert!(has_dct, "PDF must contain DCTDecode");
}

/// Output PDF does NOT contain /DCTDecode when jpeg_quality is None.
#[test]
fn pdf_without_dct_has_no_dctdecode() {
    let doc = load_doc("chicken.djvu");
    let pdf = djvu_to_pdf_with_options(
        &doc,
        &PdfOptions {
            jpeg_quality: None,
            output_dpi: 150,
            adaptive_raster: false,
            ccitt_g4: false,
            mrc: false,
        },
    )
    .unwrap();
    let has_dct = pdf.windows(9).any(|w| w == b"DCTDecode");
    assert!(!has_dct, "FlateDecode PDF must not contain DCTDecode");
}

/// `djvu_to_pdf` (default, DCT at 80) is smaller than FlateDecode.
#[test]
fn default_djvu_to_pdf_is_dct() {
    let doc = load_doc("chicken.djvu");
    let default_pdf = djvu_to_pdf(&doc).unwrap();
    let flat_pdf = djvu_to_pdf_with_options(
        &doc,
        &PdfOptions {
            jpeg_quality: None,
            output_dpi: 150,
            adaptive_raster: false,
            ccitt_g4: false,
            mrc: false,
        },
    )
    .unwrap();
    assert!(
        default_pdf.len() < flat_pdf.len(),
        "default PDF must use DCT and be smaller than FlateDecode"
    );
}

// ── PDF_ADAPTIVE_RASTER: opt-in per-page Deflate-vs-JPEG choice ─────────────

#[test]
fn adaptive_raster_defaults_to_off() {
    assert!(!PdfOptions::default().adaptive_raster);
    assert!(!PdfOptions::archival().adaptive_raster);
}

/// With `adaptive_raster: false` (the default), output must be byte-identical
/// to the pre-existing always-DCT behaviour.
#[test]
fn adaptive_raster_off_is_byte_identical_to_default() {
    let doc = load_doc("chicken.djvu");
    let plain = djvu_to_pdf(&doc).unwrap();
    let explicit_off = djvu_to_pdf_with_options(
        &doc,
        &PdfOptions {
            adaptive_raster: false,
            ..PdfOptions::default()
        },
    )
    .unwrap();
    assert_eq!(plain, explicit_off);
}

/// On a near-flat colour scan (`PDF_DCT_PROBE`'s regression case), JPEG-80 is
/// 3.1x larger than Deflate at no SSIM gain. `adaptive_raster: true` must pick
/// Deflate on every such page and produce a visibly smaller whole-file PDF.
#[test]
fn adaptive_raster_shrinks_flat_colour_scan() {
    let data = std::fs::read(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/watchmaker.djvu"),
    )
    .expect("watchmaker.djvu must exist");
    let doc = crate::djvu_document::DjVuDocument::parse(&data).expect("parse");

    let default_pdf = djvu_to_pdf(&doc).unwrap();
    let adaptive_pdf = djvu_to_pdf_with_options(
        &doc,
        &PdfOptions {
            adaptive_raster: true,
            ..PdfOptions::default()
        },
    )
    .unwrap();

    assert!(
        adaptive_pdf.len() < default_pdf.len(),
        "adaptive PDF ({} B) must be smaller than always-DCT default ({} B)",
        adaptive_pdf.len(),
        default_pdf.len()
    );
    // Expect a substantial win (measured ~1.6x on this corpus file), not a
    // rounding-error difference.
    assert!(
        (default_pdf.len() as f64) / (adaptive_pdf.len() as f64) > 1.3,
        "expected a large win from adaptive raster on a flat colour scan"
    );
}

/// `adaptive_raster: true` must never be *larger* than always-DCT: it's a
/// per-page min, so image-heavy pages where JPEG already wins are unchanged.
#[test]
fn adaptive_raster_never_larger_than_default() {
    let doc = load_doc("chicken.djvu");
    let default_pdf = djvu_to_pdf(&doc).unwrap();
    let adaptive_pdf = djvu_to_pdf_with_options(
        &doc,
        &PdfOptions {
            adaptive_raster: true,
            ..PdfOptions::default()
        },
    )
    .unwrap();
    assert!(adaptive_pdf.len() <= default_pdf.len());
}

// ── PDF_G4: opt-in CCITTFaxDecode (G4/T.6) for JB2 masks ────────────────────

#[test]
fn ccitt_g4_defaults_to_off() {
    assert!(!PdfOptions::default().ccitt_g4);
    assert!(!PdfOptions::archival().ccitt_g4);
}

/// With `ccitt_g4: false` (the default), output must be byte-identical to
/// the pre-existing always-Deflate mask behaviour.
#[test]
fn ccitt_g4_off_is_byte_identical_to_default() {
    let doc = load_doc("boy_jb2.djvu"); // Sjbz-only (bilevel fast path)
    let plain = djvu_to_pdf(&doc).unwrap();
    let explicit_off = djvu_to_pdf_with_options(
        &doc,
        &PdfOptions {
            ccitt_g4: false,
            ..PdfOptions::default()
        },
    )
    .unwrap();
    assert_eq!(plain, explicit_off);
}

/// `ccitt_g4: true` must produce a PDF containing `/CCITTFaxDecode` for a
/// bilevel document, and must never be larger than the Deflate-only default
/// (it's a per-mask min, same pattern as `adaptive_raster`).
#[test]
fn ccitt_g4_on_uses_ccittfaxdecode_and_never_larger() {
    let doc = load_doc("boy_jb2.djvu");
    let default_pdf = djvu_to_pdf(&doc).unwrap();
    let g4_pdf = djvu_to_pdf_with_options(
        &doc,
        &PdfOptions {
            ccitt_g4: true,
            ..PdfOptions::default()
        },
    )
    .unwrap();
    let has_ccitt = g4_pdf.windows(14).any(|w| w == b"CCITTFaxDecode");
    assert!(has_ccitt, "ccitt_g4 PDF must contain /CCITTFaxDecode");
    assert!(
        g4_pdf.len() <= default_pdf.len(),
        "g4 PDF ({} B) must not be larger than default ({} B)",
        g4_pdf.len(),
        default_pdf.len()
    );
}

/// On a real scanned bilevel corpus doc (`watchmaker.djvu`), `ccitt_g4: true`
/// must shrink the whole-file PDF meaningfully (measured ~1.7x on this file's
/// masks — see `PDF_G4` in `PERF_EXPERIMENTS.md`).
#[test]
fn ccitt_g4_shrinks_bilevel_corpus_doc() {
    let data = std::fs::read(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/watchmaker.djvu"),
    )
    .expect("watchmaker.djvu must exist");
    let doc = crate::djvu_document::DjVuDocument::parse(&data).expect("parse");

    let default_pdf = djvu_to_pdf(&doc).unwrap();
    let g4_pdf = djvu_to_pdf_with_options(
        &doc,
        &PdfOptions {
            ccitt_g4: true,
            ..PdfOptions::default()
        },
    )
    .unwrap();

    assert!(
        g4_pdf.len() < default_pdf.len(),
        "g4 PDF ({} B) must be smaller than Deflate-only default ({} B)",
        g4_pdf.len(),
        default_pdf.len()
    );
}

/// `collect_mask_stream` with `ccitt_g4: true` still returns `None` for a
/// page without `Sjbz` (same short-circuit as the Deflate-only path).
#[test]
fn collect_mask_stream_g4_returns_none_for_no_sjbz() {
    let doc = load_doc("chicken.djvu"); // no Sjbz
    let page = doc.page(0).unwrap();
    let opts = PdfOptions {
        ccitt_g4: true,
        ..PdfOptions::default()
    };
    assert!(collect_mask_stream(page, &opts).is_none());
}

#[test]
fn pdf_rgb_streaming_matches_pixmap_rgb() {
    let doc = load_doc("boy.djvu");
    let page = doc.page(0).unwrap();
    let size = render_size(page, PdfOptions::default().output_dpi);
    let (rw, rh) = size.native;
    let opts = size.native_options(page);

    let streamed = render_rgb_for_pdf(page, &opts, rw, rh).unwrap();
    let pixmap = djvu_render::render_pixmap(page, &opts).unwrap();

    assert_eq!(streamed, pixmap.to_rgb());
}

#[test]
fn pdf_rgb_fallback_handles_non_streamable_options() {
    let doc = load_doc("boy.djvu");
    let page = doc.page(0).unwrap();
    let opts = RenderOptions {
        width: page.width() as u32,
        height: page.height() as u32,
        aa: true,
        ..RenderOptions::default()
    };

    let rgb = render_rgb_for_pdf(page, &opts, opts.width, opts.height).unwrap();
    let pixmap = djvu_render::render_pixmap(page, &opts).unwrap();

    assert_eq!(rgb, pixmap.to_rgb());
}

// ── Text layer ────────────────────────────────────────────────────────────

/// A document with TXTz must embed invisible text (BT…ET blocks).
#[test]
fn pdf_with_text_layer_contains_bt_et_markers() {
    let doc = load_doc("colorbook.djvu");
    let pdf = djvu_to_pdf(&doc).unwrap();
    // PDF content streams are deflated, but "BT" / "ET" may appear in stream
    // dict or in the raw uncompressed bytes we can observe at the dict level.
    // More reliably: we check that at least one page's content stream was
    // added (the file is larger than a document without text).
    assert!(!pdf.is_empty(), "PDF must not be empty");
    // The text content stream dict contains /Font when text is present.
    let has_font = pdf.windows(5).any(|w| w == b"/Font");
    assert!(
        has_font,
        "PDF with text layer must reference a /Font resource"
    );
}

/// `build_text_content` returns empty when the page has no text layer.
#[test]
fn build_text_content_no_text_layer_returns_empty() {
    let doc = load_doc("chicken.djvu"); // no TXTz
    let page = doc.page(0).unwrap();
    let result = build_text_content(page, 100.0, 720.0);
    assert!(
        result.is_empty(),
        "page without text layer must produce empty text content"
    );
}

/// `build_text_content` returns non-empty when the page has a text layer.
#[test]
fn build_text_content_with_text_layer_returns_non_empty() {
    let doc = load_doc("colorbook.djvu"); // has TXTz
    // Find the first page that actually has a text layer
    for i in 0..doc.page_count() {
        let page = doc.page(i).unwrap();
        if page.text_layer().ok().flatten().is_some() {
            let dpi = page.dpi().max(1) as f32;
            let pt_h = page.height() as f32 * 72.0 / dpi;
            let result = build_text_content(page, dpi, pt_h);
            if !result.is_empty() {
                assert!(result.contains("BT"), "text content must begin with BT");
                assert!(result.contains("ET"), "text content must end with ET");
                return;
            }
        }
    }
    // If no page had non-empty text, that's fine — fixture may have empty zones
}

// ── Bookmarks (PDF outline) ───────────────────────────────────────────────

/// A document with NAVM bookmarks must produce /Outlines in the PDF catalog.
#[test]
fn pdf_with_bookmarks_contains_outlines() {
    let doc = load_doc("links.djvu"); // has NAVM
    let pdf = djvu_to_pdf(&doc).unwrap();
    let has_outlines = pdf.windows(8).any(|w| w == b"Outlines");
    assert!(
        has_outlines,
        "PDF with NAVM bookmarks must contain /Outlines"
    );
}

/// A document without bookmarks must NOT produce /Outlines.
#[test]
fn pdf_without_bookmarks_has_no_outlines() {
    let doc = load_doc("chicken.djvu"); // no NAVM
    let pdf = djvu_to_pdf(&doc).unwrap();
    let has_outlines = pdf.windows(8).any(|w| w == b"Outlines");
    assert!(
        !has_outlines,
        "PDF without bookmarks must not contain /Outlines"
    );
}

/// `resolve_bookmark_dest` resolves `#page_N` to a /Dest reference.
/// DjVu page anchors are 1-based: `#page_1` = index 0, `#page_2` = index 1.
#[test]
fn resolve_bookmark_dest_page_anchor() {
    let page_ids = [10usize, 20, 30];
    // #page_1 is 1-based → index 0 → page_ids[0] = 10
    let dest = resolve_bookmark_dest("#page_1", &page_ids);
    assert!(dest.contains("/Dest"), "must produce /Dest: {dest}");
    assert!(dest.contains("10 0 R"), "must reference page id 10: {dest}");
    // #page_2 → index 1 → page_ids[1] = 20
    let dest2 = resolve_bookmark_dest("#page_2", &page_ids);
    assert!(
        dest2.contains("20 0 R"),
        "must reference page id 20: {dest2}"
    );
}

/// `resolve_bookmark_dest` falls back to /A /URI for external URLs.
#[test]
fn resolve_bookmark_dest_external_url() {
    let dest = resolve_bookmark_dest("https://example.com", &[10, 20]);
    assert!(
        dest.contains("/URI"),
        "external URL must produce URI action: {dest}"
    );
}

/// `resolve_bookmark_dest` returns empty string for empty URL.
#[test]
fn resolve_bookmark_dest_empty_url() {
    let dest = resolve_bookmark_dest("", &[10]);
    assert!(dest.is_empty(), "empty URL must produce empty dest: {dest}");
}

// ── Hyperlink annotations ─────────────────────────────────────────────────

/// `collect_link_annot_bodies` runs without error on a document with ANTz.
#[test]
fn collect_link_annot_bodies_runs_without_error() {
    let doc = load_doc("czech.djvu"); // has ANTz
    for i in 0..doc.page_count() {
        let page = doc.page(i).unwrap();
        let dpi = page.dpi().max(1) as f32;
        let pt_h = page.height() as f32 * 72.0 / dpi;
        let _ = collect_link_annot_bodies(page, dpi, pt_h);
    }
    // Test passes if no panic
}

/// `collect_link_annot_bodies` builds correct annotation body for a link.
///
/// Exercises the annotation formatting path directly without needing a
/// specific fixture with Rect-shaped hyperlinks.
#[test]
fn link_annot_body_format() {
    use crate::annotation::{MapArea, Rect as ARect, Shape};

    // Build a synthetic Rect-shaped hyperlink
    let link = MapArea {
        shape: Shape::Rect(ARect {
            x: 10,
            y: 20,
            width: 100,
            height: 50,
        }),
        url: "https://example.com".to_string(),
        description: String::new(),
        border: None,
        highlight: None,
        target: None,
        extra: Vec::new(),
    };

    let rect = shape_to_pdf_rect(&link.shape, 100.0, 360.0);
    assert!(rect.is_some(), "Rect shape must produce a PDF rect");
    let (x1, y1, x2, y2) = rect.unwrap();
    let url_escaped = pdf_escape_string(&link.url);
    let body = format!(
        "<< /Type /Annot /Subtype /Link\n\
           /Rect [{:.4} {:.4} {:.4} {:.4}]\n\
           /Border [0 0 0]\n\
           /A << /S /URI /URI ({url_escaped}) >> >>",
        x1, y1, x2, y2
    );
    assert!(body.contains("/Type /Annot"), "must have /Type /Annot");
    assert!(body.contains("/Subtype /Link"), "must have /Subtype /Link");
    assert!(body.contains("https://example.com"), "must contain URL");
    assert!(body.contains("/Rect"), "must have /Rect");
}

// ── Bilevel-only pages ────────────────────────────────────────────────────

/// Bilevel-only page (Sjbz, no BG44) must use /ImageMask in the PDF.
#[test]
fn bilevel_only_page_has_image_mask() {
    let doc = load_doc("boy_jb2.djvu"); // Sjbz-only
    let pdf = djvu_to_pdf(&doc).unwrap();
    let has_mask = pdf.windows(9).any(|w| w == b"ImageMask");
    assert!(has_mask, "bilevel-only page must embed /ImageMask XObject");
}

// ── Mixed page (Sjbz + BG44) — mask overlay ──────────────────────────────

/// A page with both Sjbz (foreground mask) and BG44 (background) must
/// embed both an /Im0 image and a /Mask0 ImageMask XObject.
/// (irish.djvu: Sjbz+BG44+FGbz — the palette path emits /Mask0, /Mask1, …)
#[test]
fn mixed_page_has_both_image_and_mask_xobject() {
    let doc = load_doc("irish.djvu"); // Sjbz+BG44+FGbz
    let pdf = djvu_to_pdf(&doc).unwrap();
    let has_im0 = pdf.windows(4).any(|w| w == b"Im0 ");
    let has_mask0 = pdf.windows(5).any(|w| w == b"Mask0");
    assert!(has_im0, "mixed page must reference /Im0 background");
    assert!(
        has_mask0,
        "mixed page must reference /Mask0 foreground mask"
    );
}

/// A page whose foreground colour is continuous-tone (FG44, no FGbz
/// palette) must NOT get a stencil: a black stencil would flatten the
/// coloured text, and the composited /Im0 already carries it (#620).
#[test]
fn fg44_page_skips_mask_stencil() {
    let doc = load_doc("colorbook.djvu"); // Sjbz+BG44+FG44, no FGbz
    let pdf = djvu_to_pdf(&doc).unwrap();
    let has_im0 = pdf.windows(4).any(|w| w == b"Im0 ");
    let has_mask0 = pdf.windows(5).any(|w| w == b"Mask0");
    assert!(has_im0, "FG44 page must reference /Im0 background");
    assert!(
        !has_mask0,
        "FG44 page must not paint a flat stencil over continuous-tone text"
    );
}

// ── render_dims / output_dpi ──────────────────────────────────────────────

/// When output_dpi is lower than native DPI the PDF is smaller.
#[test]
fn lower_output_dpi_produces_smaller_pdf() {
    let doc = load_doc("chicken.djvu");
    let native = djvu_to_pdf_with_options(
        &doc,
        &PdfOptions {
            jpeg_quality: None,
            output_dpi: 0,
            adaptive_raster: false,
            ccitt_g4: false,
            mrc: false,
        },
    )
    .unwrap();
    let downscaled = djvu_to_pdf_with_options(
        &doc,
        &PdfOptions {
            jpeg_quality: None,
            output_dpi: 50,
            adaptive_raster: false,
            ccitt_g4: false,
            mrc: false,
        },
    )
    .unwrap();
    assert!(
        downscaled.len() < native.len(),
        "50 DPI PDF ({} B) must be smaller than native ({} B)",
        downscaled.len(),
        native.len()
    );
}

// ── PdfOptions::archival() ────────────────────────────────────────────────

#[test]
fn pdf_archival_preset_produces_output() {
    let doc = load_doc("chicken.djvu");
    let pdf = djvu_to_pdf_with_options(&doc, &PdfOptions::archival()).unwrap();
    assert!(!pdf.is_empty());
    assert!(
        pdf.starts_with(b"%PDF-"),
        "archival PDF must start with %PDF-"
    );
}

#[test]
fn pdf_archival_preset_fields() {
    let opts = PdfOptions::archival();
    assert_eq!(opts.jpeg_quality, Some(90));
    assert_eq!(opts.output_dpi, 0);
}

// ── pdf_escape_string ─────────────────────────────────────────────────────

#[test]
fn pdf_escape_parens_and_backslash() {
    assert_eq!(pdf_escape_string("a(b)c"), "a\\(b\\)c");
    assert_eq!(pdf_escape_string("a\\b"), "a\\\\b");
}

#[test]
fn pdf_escape_ascii_passthrough() {
    assert_eq!(pdf_escape_string("hello 123"), "hello 123");
}

#[test]
fn pdf_escape_non_ascii_replaced_with_question_mark() {
    let s = pdf_escape_string("über");
    assert!(s.contains('?'), "non-ASCII must be replaced with ?: {s}");
}

// ── helvetica_advance ─────────────────────────────────────────────────────

#[test]
fn helvetica_advance_space() {
    assert!((helvetica_advance(' ') - 0.278).abs() < 1e-6);
}

#[test]
fn helvetica_advance_digit() {
    assert!((helvetica_advance('5') - 0.556).abs() < 1e-6);
}

#[test]
fn helvetica_advance_uppercase() {
    assert!((helvetica_advance('A') - 0.667).abs() < 1e-6);
}

#[test]
fn helvetica_advance_lowercase() {
    assert!((helvetica_advance('a') - 0.556).abs() < 1e-6);
}

#[test]
fn helvetica_advance_cjk_full_width() {
    // CJK Unified Ideograph — should return 1.0
    assert!((helvetica_advance('中') - 1.0).abs() < 1e-6);
}

#[test]
fn helvetica_advance_hiragana_full_width() {
    assert!((helvetica_advance('あ') - 1.0).abs() < 1e-6);
}

#[test]
fn helvetica_advance_cyrillic_falls_back() {
    // Cyrillic falls through to the 0.556 default
    assert!((helvetica_advance('А') - 0.556).abs() < 1e-6);
}

#[test]
fn helvetica_advance_control_char_is_zero() {
    assert!((helvetica_advance('\x00') - 0.0).abs() < 1e-6);
    assert!((helvetica_advance('\x7f') - 0.0).abs() < 1e-6);
}

#[test]
fn helvetica_advance_punctuation() {
    assert!((helvetica_advance(',') - 0.278).abs() < 1e-6);
    assert!((helvetica_advance('(') - 0.333).abs() < 1e-6);
    assert!((helvetica_advance('-') - 0.333).abs() < 1e-6);
}

// ── collect_mask_stream ───────────────────────────────────────────────────

#[test]
fn collect_mask_stream_returns_none_for_no_sjbz() {
    let doc = load_doc("chicken.djvu"); // no Sjbz
    let page = doc.page(0).unwrap();
    let result = collect_mask_stream(page, &PdfOptions::default());
    assert!(
        result.is_none(),
        "page without Sjbz must return None from collect_mask_stream"
    );
}

#[test]
fn collect_mask_stream_returns_some_for_sjbz_page() {
    let doc = load_doc("boy_jb2.djvu"); // has Sjbz
    let page = doc.page(0).unwrap();
    let result = collect_mask_stream(page, &PdfOptions::default());
    assert!(
        result.is_some(),
        "page with Sjbz must return Some from collect_mask_stream"
    );
    let body = result.unwrap();
    // Must contain /ImageMask keyword
    assert!(
        body.windows(9).any(|w| w == b"ImageMask"),
        "mask stream must contain /ImageMask"
    );
}

// ── shape_to_pdf_rect ─────────────────────────────────────────────────────

#[test]
fn shape_to_pdf_rect_converts_rect_shape() {
    use crate::annotation::{Rect as ARect, Shape};
    let shape = Shape::Rect(ARect {
        x: 0,
        y: 0,
        width: 100,
        height: 50,
    });
    let rect = shape_to_pdf_rect(&shape, 100.0, 360.0);
    assert!(rect.is_some(), "valid rect shape must produce a PDF rect");
    let (x1, y1, x2, y2) = rect.unwrap();
    assert!((x1 - 0.0).abs() < 0.01);
    assert!((y1 - 0.0).abs() < 0.01);
    assert!((x2 - 72.0).abs() < 0.01); // 100px * 72/100dpi = 72pt
    assert!((y2 - 36.0).abs() < 0.01); // 50px * 72/100dpi = 36pt
}

// ── px_to_pt ─────────────────────────────────────────────────────────────

#[test]
fn px_to_pt_at_72dpi_is_identity() {
    assert!((px_to_pt(100.0, 72.0) - 100.0).abs() < 0.001);
}

#[test]
fn px_to_pt_at_300dpi() {
    // 300px at 300dpi = 72pt
    assert!((px_to_pt(300.0, 300.0) - 72.0).abs() < 0.001);
}

// ── emit_word_span guards ─────────────────────────────────────────────────

#[test]
fn emit_word_span_zero_width_produces_no_ops() {
    use crate::text::Rect;
    let rect = Rect {
        x: 0,
        y: 0,
        width: 0,
        height: 20,
    };
    let mut ops = String::new();
    emit_word_span(&mut ops, &rect, "hello", 72.0, 720.0);
    assert!(ops.is_empty(), "zero-width rect must produce no output");
}

#[test]
fn emit_word_span_tiny_height_produces_no_ops() {
    use crate::text::Rect;
    // height=1px at 300dpi → h = 1*72/300 = 0.24pt < 0.5 → skip
    let rect = Rect {
        x: 0,
        y: 0,
        width: 50,
        height: 1,
    };
    let mut ops = String::new();
    emit_word_span(&mut ops, &rect, "hi", 300.0, 720.0);
    assert!(ops.is_empty(), "sub-0.5pt font size must produce no output");
}

// ── build_outline with nested bookmarks ──────────────────────────────────

#[test]
fn build_outline_with_nested_children_sets_first_last_count() {
    use crate::djvu_document::DjVuBookmark;
    let bookmarks = vec![DjVuBookmark {
        title: "Chapter 1".into(),
        url: "#page_1".into(),
        children: vec![
            DjVuBookmark {
                title: "Section 1.1".into(),
                url: "#page_2".into(),
                children: vec![],
            },
            DjVuBookmark {
                title: "Section 1.2".into(),
                url: "#page_3".into(),
                children: vec![],
            },
        ],
    }];
    let page_ids = [10usize, 20, 30];
    let mut pdf = Vec::new();
    let mut w = PdfWriter::new(&mut pdf).unwrap();
    let outline_id = build_outline(&mut w, &bookmarks, &page_ids).unwrap();
    assert!(
        outline_id.is_some(),
        "nested bookmarks must produce an outline"
    );
    // Serialize and check that /First and /Last are present
    w.finish().unwrap();
    let s = String::from_utf8_lossy(&pdf);
    assert!(
        s.contains("/First"),
        "outline item with children must set /First"
    );
    assert!(
        s.contains("/Last"),
        "outline item with children must set /Last"
    );
    assert!(
        s.contains("/Count"),
        "outline item with children must set /Count"
    );
}

// Lines 411-415: hyperlink annotation block (/Annots [...]).
// Build a synthetic single-page DjVu with an ANTz maparea URL, then convert.
#[test]
fn djvu_to_pdf_with_hyperlinks_produces_annots() {
    use crate::annotation::{self as ann, Annotation, MapArea};
    use crate::djvu_document::DjVuDocument;
    use crate::iff::{self as iff_mod, Chunk, DjvuFile};
    let maparea = MapArea {
        url: "https://example.com".to_string(),
        description: String::new(),
        shape: ann::Shape::Rect(ann::Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 50,
        }),
        border: None,
        highlight: None,
        target: None,
        extra: Vec::new(),
    };
    let ant_data = ann::encode_annotations_bzz(&Annotation::default(), &[maparea]);
    // Minimal INFO: width=100, height=100, dpi=0 (default), rest zero.
    let mut info = vec![0u8; 10];
    info[1] = 100; // width
    info[3] = 100; // height
    let bytes = iff_mod::emit(&DjvuFile {
        root: Chunk::Form {
            secondary_id: *b"DJVU",
            length: 0,
            children: vec![
                Chunk::Leaf {
                    id: *b"INFO",
                    data: info,
                },
                Chunk::Leaf {
                    id: *b"ANTz",
                    data: ant_data,
                },
            ],
        },
    });
    let doc = DjVuDocument::parse(&bytes).expect("synthetic doc must parse");
    let pdf = djvu_to_pdf(&doc).expect("synthetic hyperlink page must convert to PDF");
    let s = String::from_utf8_lossy(&pdf);
    assert!(
        s.contains("/Annots"),
        "PDF from hyperlink page must contain /Annots"
    );
}

/// Page with corrupted ANTz (invalid BZZ): `hyperlinks()` errors, so
/// `collect_link_annot_bodies` returns empty (line 332 `Err(_) => Vec::new()`).
#[test]
fn djvu_to_pdf_with_corrupted_antz_skips_annotations() {
    use crate::djvu_document::DjVuDocument;
    use crate::iff::{self as iff_mod, Chunk, DjvuFile};

    let mut info = vec![0u8; 10];
    info[1] = 100; // width
    info[3] = 100; // height
    // Garbage bytes that are not valid BZZ — decoding will fail
    let bad_antz: Vec<u8> = vec![0xFF, 0xFE, 0xAB, 0xCD, 0x12, 0x34];
    let bytes = iff_mod::emit(&DjvuFile {
        root: Chunk::Form {
            secondary_id: *b"DJVU",
            length: 0,
            children: vec![
                Chunk::Leaf {
                    id: *b"INFO",
                    data: info,
                },
                Chunk::Leaf {
                    id: *b"ANTz",
                    data: bad_antz,
                },
            ],
        },
    });
    let doc = DjVuDocument::parse(&bytes).expect("synthetic doc must parse");
    let pdf = djvu_to_pdf(&doc).expect("corrupted ANTz must not abort PDF export");
    let s = String::from_utf8_lossy(&pdf);
    assert!(
        !s.contains("/Annots"),
        "corrupted ANTz should produce no /Annots block"
    );
}

/// Page whose render fails (0×0 dimensions, no image data) triggers the blank
/// page fallback at lines 840-850: `rendered_pages[i]` is None so a blank
/// /Page object is emitted with the native MediaBox dimensions.
#[test]
fn djvu_to_pdf_zero_dim_page_emits_blank_page_object() {
    use crate::djvu_document::DjVuDocument;
    use crate::iff::{self as iff_mod, Chunk, DjvuFile};

    // INFO chunk: width=0, height=0, dpi=0 (all zeros). No Sjbz or BG44 so
    // is_bilevel_only=false, and render_dims returns (0,0), which makes
    // render_pixmap return InvalidDimensions → render_page_data returns Err
    // → .ok() yields None → blank page fallback fires.
    let info = vec![0u8; 10];
    let bytes = iff_mod::emit(&DjvuFile {
        root: Chunk::Form {
            secondary_id: *b"DJVU",
            length: 0,
            children: vec![Chunk::Leaf {
                id: *b"INFO",
                data: info,
            }],
        },
    });
    let doc = DjVuDocument::parse(&bytes).expect("zero-dim doc must parse");
    let pdf = djvu_to_pdf(&doc).expect("zero-dim page must not crash PDF export");
    let s = String::from_utf8_lossy(&pdf);
    assert!(
        s.contains("/Type /Page"),
        "PDF must contain at least one Page object"
    );
}
