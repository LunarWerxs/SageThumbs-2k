use super::*;

// ── RenderOptions edge-case helpers ──────────────────────────────────────

#[test]
fn fit_to_width_zero_page_width_uses_requested_width() {
    // Line 213: dw == 0 → height = width (same as requested width).
    let bytes = make_doc_with_dims(0, 100);
    let doc = DjVuDocument::parse(&bytes).unwrap();
    let page = doc.page(0).unwrap();
    let opts = RenderOptions::fit_to_width(page, 40);
    assert_eq!(opts.width, 40);
    assert_eq!(opts.height, 40); // because dw==0 → height = width
}

#[test]
fn fit_to_height_zero_page_height_uses_requested_height() {
    // Line 232: dh == 0 → width = height (same as requested height).
    let bytes = make_doc_with_dims(100, 0);
    let doc = DjVuDocument::parse(&bytes).unwrap();
    let page = doc.page(0).unwrap();
    let opts = RenderOptions::fit_to_height(page, 60);
    assert_eq!(opts.height, 60);
    assert_eq!(opts.width, 60); // because dh==0 → width = height
}

#[test]
fn fit_to_box_zero_page_dims_falls_back_to_box_max() {
    // Lines 255-259: dw==0 || dh==0 → return max_width × max_height with scale=1.
    let bytes = make_doc_with_dims(0, 0);
    let doc = DjVuDocument::parse(&bytes).unwrap();
    let page = doc.page(0).unwrap();
    let opts = RenderOptions::fit_to_box(page, 80, 60);
    assert_eq!(opts.width, 80);
    assert_eq!(opts.height, 60);
}

#[test]
fn can_stream_true_at_native_resolution_with_lanczos3() {
    // Line 290: the "|| native dims" branch — evaluated when Bilinear is false.
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    let opts = RenderOptions {
        width: page.width() as u32,
        height: page.height() as u32,
        resampling: Resampling::Lanczos3,
        ..Default::default()
    };
    assert!(opts.can_stream(page));
}

/// Rendering a JB2-only (no BG44) page at very small scale triggers
/// subsample >= 4 and exercises bg44_partial's empty-chunks path (line 918).
#[test]
fn render_bilevel_at_tiny_scale_exercises_bg44_partial_empty() {
    let doc = load_doc("boy_jb2.djvu");
    let page = doc.page(0).unwrap();
    // Use a tiny width so decode_scale << 0.25 and best_iw44_subsample >= 4.
    let opts = RenderOptions {
        width: 10,
        height: 10,
        ..Default::default()
    };
    let pm = render_pixmap(page, &opts).expect("tiny bilevel render should succeed");
    assert!(pm.width > 0 && pm.height > 0);
}

/// Bold dilation (opts.bold > 0) thickens the mask.
#[test]
fn render_with_bold_dilation_produces_output() {
    let doc = load_doc("boy_jb2.djvu");
    let page = doc.page(0).unwrap();
    let opts = RenderOptions {
        width: 40,
        height: 40,
        bold: 1,
        ..Default::default()
    };
    let pm = render_pixmap(page, &opts).expect("bold render should succeed");
    assert_eq!(pm.width, 40);
    assert_eq!(pm.height, 40);
}
