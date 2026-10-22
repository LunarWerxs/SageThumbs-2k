use super::*;

// ── BGjp / FGjp tests ─────────────────────────────────────────────────────

/// Load the synthetic bgjp_test.djvu fixture from the assets directory.
pub(super) fn load_bgjp_doc() -> DjVuDocument {
    load_doc("bgjp_test.djvu")
}

/// BGjp fixture loads without error and reports correct dimensions.
#[test]
fn bgjp_fixture_loads() {
    let doc = load_bgjp_doc();
    let page = doc.page(0).unwrap();
    assert_eq!(page.width(), 4);
    assert_eq!(page.height(), 4);
}

/// BGjp chunk is present in the fixture.
#[test]
fn bgjp_chunk_present() {
    let doc = load_bgjp_doc();
    let page = doc.page(0).unwrap();
    assert!(
        page.find_chunk(b"BGjp").is_some(),
        "fixture must have a BGjp chunk"
    );
    assert!(
        page.bg44_chunks().is_empty(),
        "fixture must NOT have BG44 chunks"
    );
}

/// `decode_bgjp` returns a non-None Pixmap for the BGjp fixture.
#[test]
fn decode_bgjp_returns_pixmap() {
    let doc = load_bgjp_doc();
    let page = doc.page(0).unwrap();
    let pm = decode_bgjp(page).expect("decode_bgjp must not error");
    assert!(pm.is_some(), "decode_bgjp must return Some(Pixmap)");
    let pm = pm.unwrap();
    assert_eq!(pm.width, 4);
    assert_eq!(pm.height, 4);
    assert_eq!(pm.data.len(), 4 * 4 * 4); // RGBA
}

/// `decode_bgjp` returns None for a page with no BGjp chunk.
#[test]
fn decode_bgjp_returns_none_without_chunk() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    let pm = decode_bgjp(page).expect("should not error");
    assert!(pm.is_none());
}

/// `decode_jpeg_to_pixmap` produces RGBA output with alpha=255.
#[test]
fn decode_jpeg_to_pixmap_alpha_is_255() {
    let doc = load_bgjp_doc();
    let page = doc.page(0).unwrap();
    let data = page.find_chunk(b"BGjp").unwrap();
    let pm = decode_jpeg_to_pixmap(data).expect("decode must succeed");
    for chunk in pm.data.as_chunks::<4>().0 {
        assert_eq!(chunk[3], 255, "alpha must be 255 for every pixel");
    }
}

/// render_pixmap falls back to BGjp when no BG44 chunks are present.
#[test]
fn render_pixmap_uses_bgjp_background() {
    let doc = load_bgjp_doc();
    let page = doc.page(0).unwrap();
    let opts = RenderOptions {
        width: 4,
        height: 4,
        ..Default::default()
    };
    let pm = render_pixmap(page, &opts).expect("render must succeed");
    assert_eq!(pm.width, 4);
    assert_eq!(pm.height, 4);
}

/// render_coarse also falls back to BGjp (no BG44 chunks).
#[test]
fn render_coarse_uses_bgjp_background() {
    let doc = load_bgjp_doc();
    let page = doc.page(0).unwrap();
    let opts = RenderOptions {
        width: 4,
        height: 4,
        ..Default::default()
    };
    let pm = render_coarse(page, &opts).expect("render_coarse must succeed");
    assert!(pm.is_some(), "must return Some when BGjp present");
    let pm = pm.unwrap();
    assert_eq!(pm.width, 4);
    assert_eq!(pm.height, 4);
}
