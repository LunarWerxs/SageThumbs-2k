use super::*;

// ── Issue #225: render_rows byte-identical to render_into (direct-write path) ──

// ── Issue #225 Phase 2: public render_streaming API ──────────────────────

/// `render_streaming` must produce byte-for-byte identical output to
/// `render_pixmap` when no post-processing options are set (no aa, no
/// Lanczos scaling, no rotation).
#[test]
fn render_streaming_byte_identical_to_render_pixmap_color() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    let w = 60u32;
    let h = 80u32;
    let opts = RenderOptions {
        width: w,
        height: h,
        ..Default::default()
    };

    let pm = render_pixmap(page, &opts).expect("render_pixmap should succeed");

    let row_stride = w as usize * 4;
    let mut streamed = vec![0u8; w as usize * h as usize * 4];
    render_streaming(page, &opts, |y, row| {
        assert_eq!(row.len(), row_stride);
        let start = y * row_stride;
        streamed[start..start + row_stride].copy_from_slice(row);
    })
    .expect("render_streaming should succeed");

    assert_eq!(
        pm.data, streamed,
        "render_streaming must be byte-identical to render_pixmap when no post-processing options are set"
    );
}

/// `render_streaming` must produce byte-for-byte identical output to
/// `render_pixmap` for a bilevel page.
#[test]
fn render_streaming_byte_identical_to_render_pixmap_bilevel() {
    let doc = load_doc("boy_jb2.djvu");
    let page = doc.page(0).unwrap();
    let w = 50u32;
    let h = 70u32;
    let opts = RenderOptions {
        width: w,
        height: h,
        ..Default::default()
    };

    let pm = render_pixmap(page, &opts).expect("render_pixmap should succeed");

    let row_stride = w as usize * 4;
    let mut streamed = vec![0u8; w as usize * h as usize * 4];
    render_streaming(page, &opts, |y, row| {
        let start = y * row_stride;
        streamed[start..start + row_stride].copy_from_slice(row);
    })
    .expect("render_streaming should succeed");

    assert_eq!(pm.data, streamed);
}

/// `render_streaming` rejects anti-aliasing with `UnsupportedOption`.
#[test]
fn render_streaming_rejects_aa() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    let opts = RenderOptions {
        width: 60,
        height: 80,
        aa: true,
        ..Default::default()
    };
    let err = render_streaming(page, &opts, |_, _| {}).unwrap_err();
    assert!(
        matches!(err, RenderError::UnsupportedOption(_)),
        "expected UnsupportedOption, got {err:?}"
    );
}

/// `render_streaming` rejects Lanczos-3 when scaling actually happens.
#[test]
fn render_streaming_rejects_lanczos_with_scaling() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    // Force scaling by picking dimensions ≠ the page's native size.
    let opts = RenderOptions {
        width: page.width() as u32 / 2,
        height: page.height() as u32 / 2,
        resampling: Resampling::Lanczos3,
        ..Default::default()
    };
    let err = render_streaming(page, &opts, |_, _| {}).unwrap_err();
    assert!(matches!(err, RenderError::UnsupportedOption(_)));
}

/// `render_streaming` allows Lanczos-3 when output matches native page
/// dimensions (Lanczos is a no-op in that case).
#[test]
fn render_streaming_allows_lanczos_at_native_size() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    let opts = RenderOptions {
        width: page.width() as u32,
        height: page.height() as u32,
        resampling: Resampling::Lanczos3,
        ..Default::default()
    };
    let mut row_count = 0usize;
    render_streaming(page, &opts, |_, _| row_count += 1).expect("should succeed at native");
    assert_eq!(row_count, page.height() as usize);
}

/// `render_streaming` rejects user rotation.
#[test]
fn render_streaming_rejects_user_rotation() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    let opts = RenderOptions {
        width: 60,
        height: 80,
        rotation: UserRotation::Cw90,
        ..Default::default()
    };
    let err = render_streaming(page, &opts, |_, _| {}).unwrap_err();
    assert!(matches!(err, RenderError::UnsupportedOption(_)));
}

/// A user rotation that cancels the page's INFO rotation streams: both
/// `can_stream` and `render_streaming` accept it, and the rows equal the
/// buffered render.
#[test]
fn render_streaming_accepts_rotation_that_cancels_info_rotation() {
    let data = std::fs::read(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/boy_jb2_rotate90.djvu"),
    )
    .unwrap();
    let doc = DjVuDocument::parse(&data).unwrap();
    let page = doc.page(0).unwrap();
    assert_eq!(page.rotation(), crate::info::Rotation::Cw90);
    let opts = RenderOptions {
        width: page.width() as u32,
        height: page.height() as u32,
        rotation: UserRotation::Ccw90,
        ..Default::default()
    };
    assert!(opts.can_stream(page));
    let mut rows = Vec::new();
    render_streaming(page, &opts, |_, row| rows.extend_from_slice(row)).unwrap();
    assert_eq!(rows, render_pixmap(page, &opts).unwrap().data);

    let upright = RenderOptions {
        rotation: UserRotation::None,
        ..opts
    };
    assert!(!upright.can_stream(page));
    assert!(render_streaming(page, &upright, |_, _| {}).is_err());
}

/// `render_streaming` rejects zero dimensions with `InvalidDimensions`.
#[test]
fn render_streaming_rejects_zero_dimensions() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    let opts = RenderOptions {
        width: 0,
        height: 80,
        ..Default::default()
    };
    let err = render_streaming(page, &opts, |_, _| {}).unwrap_err();
    assert!(matches!(err, RenderError::InvalidDimensions { .. }));
}
