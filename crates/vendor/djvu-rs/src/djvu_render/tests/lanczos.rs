use super::*;

// ── Lanczos-3 tests ───────────────────────────────────────────────────────
// (The raw resampler `scale_lanczos3` and its `lanczos3_kernel` now live in
// the `pixmap` module and are unit-tested there; these exercise the render
// path's use of Lanczos-3.)

/// `Resampling::Lanczos3` produces the correct output dimensions.
#[test]
fn render_pixmap_lanczos3_correct_dimensions() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    let pw = page.width() as u32;
    let ph = page.height() as u32;
    let tw = pw / 2;
    let th = ph / 2;

    let opts = RenderOptions {
        width: tw,
        height: th,
        resampling: Resampling::Lanczos3,
        ..Default::default()
    };
    let pm = render_pixmap(page, &opts).expect("Lanczos3 render must succeed");
    assert_eq!(pm.width, tw);
    assert_eq!(pm.height, th);
}

/// Lanczos-3 and bilinear renders differ (different algorithms produce different output).
#[test]
fn lanczos3_differs_from_bilinear_at_half_scale() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    let pw = page.width() as u32;
    let ph = page.height() as u32;
    let tw = pw / 2;
    let th = ph / 2;

    let bilinear = render_pixmap(
        page,
        &RenderOptions {
            width: tw,
            height: th,
            resampling: Resampling::Bilinear,
            ..Default::default()
        },
    )
    .unwrap();

    let lanczos = render_pixmap(
        page,
        &RenderOptions {
            width: tw,
            height: th,
            resampling: Resampling::Lanczos3,
            ..Default::default()
        },
    )
    .unwrap();

    // Dimensions must be the same.
    assert_eq!(bilinear.width, lanczos.width);
    assert_eq!(bilinear.height, lanczos.height);

    // But pixel values should differ (algorithms are not identical).
    let differ = bilinear
        .data
        .iter()
        .zip(lanczos.data.iter())
        .any(|(a, b)| a != b);
    assert!(
        differ,
        "Lanczos3 and bilinear must produce different pixel values"
    );
}

/// `Resampling::Bilinear` default is maintained for backward compat.
#[test]
fn resampling_default_is_bilinear() {
    let opts = RenderOptions::default();
    assert_eq!(opts.resampling, Resampling::Bilinear);
}
