use super::*;

// ── Permissive render path ────────────────────────────────────────────────

/// Permissive render on a standard IW44+JB2 page completes without error.
#[test]
fn permissive_render_iw44_jb2_page() {
    let doc = load_doc("czech.djvu");
    let page = doc.page(0).unwrap();
    let opts = RenderOptions {
        width: 40,
        height: 40,
        permissive: true,
        ..Default::default()
    };
    let pm = render_pixmap(page, &opts).expect("permissive render should not error");
    assert_eq!(pm.width, 40);
    assert_eq!(pm.height, 40);
}

/// Oversized render dimensions must be rejected, not OOM / panic on an empty
/// overflow pixmap (security finding).
#[test]
fn oversized_render_dimensions_are_rejected() {
    let doc = load_doc("czech.djvu");
    let page = doc.page(0).unwrap();
    let opts = RenderOptions {
        width: 60_000,
        height: 60_000, // 3.6 G px > MAX_RENDER_PIXELS
        permissive: true,
        ..Default::default()
    };
    assert!(matches!(
        render_pixmap(page, &opts),
        Err(RenderError::ResourceLimit(_))
    ));
}

#[test]
fn render_into_rejects_oversized_output_with_typed_limit_error() {
    let doc = load_doc("czech.djvu");
    let page = doc.page(0).unwrap();
    let opts = RenderOptions {
        width: 60_000,
        height: 60_000,
        permissive: true,
        ..Default::default()
    };
    let mut buf = vec![0u8; 16];
    assert!(matches!(
        render_into(page, &opts, &mut buf),
        Err(RenderError::ResourceLimit(_))
    ));
}

#[test]
fn configurable_render_limit_overrides_inherited_ceiling() {
    let doc = load_doc("czech.djvu");
    let page = doc.page(0).unwrap();
    let opts = RenderOptions {
        width: 500,
        height: 500,
        permissive: true,
        ..Default::default()
    };
    let limits = crate::resource_limits::ResourceLimits {
        max_render_pixels: Some(100_000),
        ..Default::default()
    };
    assert!(matches!(
        render_pixmap_with_limits(page, &opts, Some(limits)),
        Err(RenderError::ResourceLimit(exceeded)) if exceeded.operation == "render_pixmap"
            && exceeded.axis == crate::resource_limits::ResourceLimitAxis::RenderOutputPixels
    ));
}

/// Permissive render on a BGjp page (no BG44) falls through to decode_bgjp.
#[test]
fn permissive_render_bgjp_page() {
    let doc = load_doc("bgjp_test.djvu");
    let page = doc.page(0).unwrap();
    let opts = RenderOptions {
        width: 4,
        height: 4,
        permissive: true,
        ..Default::default()
    };
    let pm = render_pixmap(page, &opts).expect("permissive render of BGjp page should succeed");
    assert_eq!(pm.width, 4);
    assert_eq!(pm.height, 4);
}

/// Permissive render on a page with FGbz palette exercises the indexed mask path.
#[test]
fn permissive_render_fgbz_page() {
    let doc = load_doc("navm_fgbz.djvu");
    let page = doc.page(0).unwrap();
    let opts = RenderOptions {
        width: 40,
        height: 40,
        permissive: true,
        ..Default::default()
    };
    let pm = render_pixmap(page, &opts).expect("permissive render of FGbz page should succeed");
    assert!(pm.width > 0 && pm.height > 0);
}

/// render_gray8 produces a grayscale pixmap.
#[test]
fn render_gray8_produces_grayscale_output() {
    let doc = load_doc("boy_jb2.djvu");
    let page = doc.page(0).unwrap();
    let opts = RenderOptions {
        width: 20,
        height: 20,
        ..Default::default()
    };
    let gpm = render_gray8(page, &opts).expect("render_gray8 should succeed");
    assert_eq!(gpm.width, 20);
    assert_eq!(gpm.height, 20);
    assert_eq!(gpm.data.len(), 20 * 20);
}

/// render_coarse rejects zero width.
#[test]
fn render_coarse_rejects_zero_dimensions() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    let opts = RenderOptions {
        width: 0,
        height: 50,
        ..Default::default()
    };
    let err = render_coarse(page, &opts).unwrap_err();
    assert!(matches!(err, RenderError::InvalidDimensions { .. }));
}

/// render_coarse on a JB2-only page returns None.
#[test]
fn render_coarse_jb2_only_page_returns_none() {
    let doc = load_doc("boy_jb2.djvu");
    let page = doc.page(0).unwrap();
    let opts = RenderOptions {
        width: 40,
        height: 40,
        ..Default::default()
    };
    let result = render_coarse(page, &opts).expect("should not error");
    assert!(
        result.is_none(),
        "JB2-only page has no BG44 so render_coarse yields None"
    );
}

/// render_progressive rejects zero dimensions.
#[test]
fn render_progressive_rejects_zero_dimensions() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    let opts = RenderOptions {
        width: 0,
        height: 50,
        ..Default::default()
    };
    let err = render_progressive(page, &opts, 0).unwrap_err();
    assert!(matches!(err, RenderError::InvalidDimensions { .. }));
}

/// render_progressive on a JB2-only page (no BG44) completes successfully.
#[test]
fn render_progressive_jb2_only_page_succeeds() {
    let doc = load_doc("boy_jb2.djvu");
    let page = doc.page(0).unwrap();
    let opts = RenderOptions {
        width: 40,
        height: 40,
        ..Default::default()
    };
    let result = render_progressive(page, &opts, 0)
        .expect("render_progressive should succeed even without BG44");
    assert_eq!(result.width, 40);
    assert_eq!(result.height, 40);
}

/// Lanczos3 at native resolution skips the re-render (need_scale=false path).
#[test]
fn lanczos3_at_native_resolution_skips_rerender() {
    let doc = load_doc("bgjp_test.djvu");
    let page = doc.page(0).unwrap();
    // bgjp_test.djvu is 4×4 — render at native size with Lanczos3
    let opts = RenderOptions {
        width: page.width() as u32,
        height: page.height() as u32,
        resampling: Resampling::Lanczos3,
        ..Default::default()
    };
    let pm = render_pixmap(page, &opts).expect("lanczos at native size must succeed");
    assert_eq!(pm.width, page.width() as u32);
    assert_eq!(pm.height, page.height() as u32);
}

/// render_pixmap rejects zero width.
#[test]
fn render_pixmap_rejects_zero_width() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    let opts = RenderOptions {
        width: 0,
        height: 50,
        ..Default::default()
    };
    let err = render_pixmap(page, &opts).unwrap_err();
    assert!(matches!(err, RenderError::InvalidDimensions { .. }));
}

/// Build a minimal single-page DJVU document with the given width and height.
pub(super) fn make_doc_with_dims(w: u16, h: u16) -> Vec<u8> {
    use crate::iff::{Chunk, DjvuFile, emit};
    let mut info = vec![0u8; 10];
    info[0] = (w >> 8) as u8;
    info[1] = w as u8;
    info[2] = (h >> 8) as u8;
    info[3] = h as u8;
    let file = DjvuFile {
        root: Chunk::Form {
            secondary_id: *b"DJVU",
            length: 0,
            children: vec![Chunk::Leaf {
                id: *b"INFO",
                data: info,
            }],
        },
    };
    emit(&file)
}
