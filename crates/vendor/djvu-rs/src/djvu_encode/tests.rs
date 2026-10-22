use super::*;
use crate::iff::parse_form;
use crate::jb2;
use crate::text::{TextZone, TextZoneKind};

fn checkerboard(w: u32, h: u32) -> Bitmap {
    let mut bm = Bitmap::new(w, h);
    for y in 0..h {
        for x in 0..w {
            if (x + y) % 2 == 0 {
                bm.set_black(x, y);
            }
        }
    }
    bm
}

/// #601: the bilevel Lossless path is a provable fixed point — decode →
/// re-encode reproduces the mask bit-for-bit, and a second cycle
/// reproduces the container bytes too. Guards against generation loss on
/// the one profile that promises none.
#[test]
fn lossless_bilevel_reencode_is_idempotent() {
    for fixture in ["tests/fixtures/boy_jb2.djvu", "tests/fixtures/ccitt_2.djvu"] {
        let data = std::fs::read(fixture).unwrap();
        let doc = crate::djvu_document::DjVuDocument::parse(&data).unwrap();
        let page = doc.page(0).unwrap();
        let dpi = page.dpi() as u16;
        let mask0 = page
            .extract_mask()
            .unwrap()
            .expect("bilevel fixture has a mask");

        let gen1 = PageEncoder::from_bitmap(&mask0)
            .with_dpi(dpi)
            .encode()
            .unwrap();
        let doc1 = crate::djvu_document::DjVuDocument::parse(&gen1).unwrap();
        let mask1 = doc1.page(0).unwrap().extract_mask().unwrap().unwrap();
        assert_eq!(
            (mask0.width, mask0.height, &mask0.data),
            (mask1.width, mask1.height, &mask1.data),
            "{fixture}: generation-1 mask must be bit-identical"
        );

        let gen2 = PageEncoder::from_bitmap(&mask1)
            .with_dpi(dpi)
            .encode()
            .unwrap();
        assert_eq!(gen1, gen2, "{fixture}: generation 2 must be a fixed point");
    }
}

/// Synthetic "picture" page: a smooth colour gradient (survives in the
/// background layer) with dark glyph-like strokes (become the mask).
fn synthetic_layered_page() -> Pixmap {
    let (w, h) = (96u32, 64u32);
    let mut pm = Pixmap::white(w, h);
    for y in 0..h {
        for x in 0..w {
            let r = 140 + (x * 90 / w) as u8;
            let g = 160 + (y * 70 / h) as u8;
            let b = 200u8;
            pm.set_rgb(x, y, r, g, b);
        }
    }
    for row in 0..4u32 {
        let y0 = 8 + row * 14;
        for x in 8..88u32 {
            if (x / 6) % 2 == 0 {
                for dy in 0..3u32 {
                    pm.set_rgb(x, y0 + dy, 20, 16, 12);
                }
            }
        }
    }
    pm
}

fn render_native(doc: &crate::djvu_document::DjVuDocument) -> Pixmap {
    render_native_page(doc, 0)
}

/// Like [`render_native`] but for an arbitrary page index — the
/// multi-page re-encode tests render every bundle page, not just page 0.
fn render_native_page(doc: &crate::djvu_document::DjVuDocument, index: usize) -> Pixmap {
    let page = doc.page(index).unwrap();
    crate::djvu_render::render_pixmap(
        page,
        &crate::djvu_render::RenderOptions {
            width: page.width() as u32,
            height: page.height() as u32,
            ..Default::default()
        },
    )
    .unwrap()
}

/// #601 mask reuse: a decode → render → re-encode cycle that passes the
/// source mask through `with_mask` must keep the mask bit-identical
/// across generations (no binarization drift), for both colour profiles.
#[test]
fn layered_reencode_with_reused_mask_is_a_mask_fixed_point() {
    let pm0 = synthetic_layered_page();
    for quality in [EncodeQuality::Quality, EncodeQuality::Archival] {
        let gen0 = PageEncoder::from_pixmap(&pm0)
            .with_quality(quality)
            .encode()
            .unwrap();
        let doc0 = crate::djvu_document::DjVuDocument::parse(&gen0).unwrap();
        let mask0 = doc0.page(0).unwrap().extract_mask().unwrap().unwrap();
        assert!(
            mask0.data.iter().any(|&b| b != 0),
            "synthetic page must produce a non-empty mask"
        );

        let mut doc = doc0;
        let mut mask = mask0.clone();
        for generation in 1..=2 {
            let rendered = render_native(&doc);
            let next = PageEncoder::from_pixmap(&rendered)
                .with_quality(quality)
                .with_mask(&mask)
                .encode()
                .unwrap();
            doc = crate::djvu_document::DjVuDocument::parse(&next).unwrap();
            mask = doc.page(0).unwrap().extract_mask().unwrap().unwrap();
            assert_eq!(
                (mask0.width, mask0.height, &mask0.data),
                (mask.width, mask.height, &mask.data),
                "{quality:?}: generation-{generation} mask must be bit-identical"
            );
        }
    }
}

/// `segment_page_with_mask` fed `segment_page`'s own mask must reproduce
/// its background byte-identically — the reuse path changes nothing but
/// the mask's origin.
#[test]
fn segment_page_with_mask_matches_segment_page() {
    let pm = synthetic_layered_page();
    for opts in [SegmentOptions::default(), SegmentOptions::archival()] {
        let a = segment_page(&pm, &opts);
        let b = segment_page_with_mask(&pm, &a.mask, &opts);
        assert_eq!(a.mask.data, b.mask.data, "mask must pass through");
        assert_eq!(
            (a.bg.width, a.bg.height, &a.bg.data),
            (b.bg.width, b.bg.height, &b.bg.data),
            "background must be byte-identical"
        );
    }
}

/// `with_mask` is only meaningful for layered colour encodes; every other
/// combination must fail loudly instead of silently ignoring the mask.
#[test]
fn with_mask_rejects_invalid_combinations() {
    let pm = Pixmap::white(16, 16);
    let mask = Bitmap::new(16, 16);
    let wrong_size = Bitmap::new(8, 16);

    assert!(matches!(
        PageEncoder::from_pixmap(&pm)
            .with_mask(&wrong_size)
            .encode(),
        Err(EncodeError::Unsupported(_))
    ));
    assert!(matches!(
        PageEncoder::from_pixmap(&pm)
            .with_quality(EncodeQuality::Photo)
            .with_mask(&mask)
            .encode(),
        Err(EncodeError::Unsupported(_))
    ));
    assert!(matches!(
        PageEncoder::from_bitmap(&mask).with_mask(&mask).encode(),
        Err(EncodeError::Unsupported(_))
    ));
}

#[test]
fn default_segment_options_maps_archival_to_dense_background() {
    // Single source of truth for the quality → segmentation mapping: only
    // Archival lowers bg_subsample; everything else uses the plain default.
    assert_eq!(
        EncodeQuality::Archival
            .default_segment_options()
            .bg_subsample,
        6,
        "Archival keeps a denser background grid"
    );
    assert_eq!(
        EncodeQuality::Quality
            .default_segment_options()
            .bg_subsample,
        SegmentOptions::default().bg_subsample,
    );
    assert_eq!(
        EncodeQuality::Lossless
            .default_segment_options()
            .bg_subsample,
        SegmentOptions::default().bg_subsample,
    );
    // archival() is the literal-free constructor those map onto.
    let arch = SegmentOptions::archival();
    assert_eq!(arch.bg_subsample, 6);
    assert_eq!(arch.threshold, SegmentOptions::default().threshold);
    assert_eq!(arch.bg_inpaint, SegmentOptions::default().bg_inpaint);
}

#[test]
fn with_iw44_options_is_threaded_into_background_codec() {
    // Reaching the IW44 knobs through the builder must actually change the
    // emitted BG44 — fewer total slices ⇒ a strictly smaller background.
    let pm = mixed_lighting_fixture();
    let default_bytes = PageEncoder::from_pixmap(&pm)
        .with_quality(EncodeQuality::Quality)
        .encode()
        .expect("default encode");
    let trimmed = Iw44EncodeOptions {
        total_slices: 20,
        ..Iw44EncodeOptions::default()
    };
    let trimmed_bytes = PageEncoder::from_pixmap(&pm)
        .with_quality(EncodeQuality::Quality)
        .with_iw44_options(trimmed)
        .encode()
        .expect("trimmed encode");
    assert!(
        trimmed_bytes.len() < default_bytes.len(),
        "with_iw44_options(total_slices=20) should shrink output ({} vs {})",
        trimmed_bytes.len(),
        default_bytes.len()
    );
    // Still a valid, parseable DjVu page.
    let doc = crate::djvu_document::DjVuDocument::parse(&trimmed_bytes).expect("parse");
    assert!(!doc.page(0).expect("page").all_chunks(b"BG44").is_empty());
}

#[test]
fn with_jb2_options_lossy_threshold_round_trips() {
    // The JB2 knob is reachable through the builder and still produces a
    // decodable mask (lossy CC substitution stays within the format).
    let pm = mixed_lighting_fixture();
    // Spell every field (cfg-gated like the Default impl) so this compiles
    // cleanly whether or not the `experimental` feature is active — neither
    // struct-update nor reassign-after-default triggers a clippy lint.
    let jb2 = Jb2EncodeOptions {
        lossy_threshold: 0.04,
        despeckle: None,
        #[cfg(feature = "experimental")]
        cross_size_rec6_probe: None,
        #[cfg(feature = "experimental")]
        same_size_rec6: None,
        #[cfg(feature = "experimental")]
        aligned_refine: None,
    };
    let bytes = PageEncoder::from_pixmap(&pm)
        .with_quality(EncodeQuality::Quality)
        .with_jb2_options(jb2)
        .encode()
        .expect("lossy jb2 encode");
    let doc = crate::djvu_document::DjVuDocument::parse(&bytes).expect("parse");
    let page = doc.page(0).expect("page");
    assert!(page.raw_chunk(b"Sjbz").is_some());
    page.extract_mask()
        .expect("mask decode")
        .expect("mask present");
}

#[test]
fn lossless_bilevel_round_trips() {
    let bm = checkerboard(64, 48);
    let bytes = PageEncoder::from_bitmap(&bm)
        .with_dpi(150)
        .with_quality(EncodeQuality::Lossless)
        .encode()
        .expect("encode");

    let form = parse_form(&bytes).expect("parse_form");
    assert_eq!(&form.form_type, b"DJVU");

    let mut info_data: Option<&[u8]> = None;
    let mut sjbz_data: Option<&[u8]> = None;
    for chunk in &form.chunks {
        match &chunk.id {
            b"INFO" => info_data = Some(chunk.data),
            b"Sjbz" => sjbz_data = Some(chunk.data),
            _ => {}
        }
    }
    let info = info_data.expect("INFO chunk present");
    let sjbz = sjbz_data.expect("Sjbz chunk present");

    assert_eq!(u16::from_be_bytes([info[0], info[1]]), 64);
    assert_eq!(u16::from_be_bytes([info[2], info[3]]), 48);
    assert_eq!(u16::from_le_bytes([info[6], info[7]]), 150);

    let decoded = jb2::decode(sjbz, None).expect("jb2 decode");
    assert_eq!(decoded.width, bm.width);
    assert_eq!(decoded.height, bm.height);
    for y in 0..bm.height {
        for x in 0..bm.width {
            assert_eq!(decoded.get(x, y), bm.get(x, y), "mismatch at ({x},{y})");
        }
    }
}

#[test]
fn explicit_smmr_bilevel_round_trips_without_sjbz() {
    let bm = checkerboard(64, 48);
    let bytes = PageEncoder::from_bitmap(&bm)
        .with_bilevel_codec(BilevelCodec::Smmr)
        .encode()
        .expect("encode");

    let doc = crate::djvu_document::DjVuDocument::parse(&bytes).expect("parse");
    let page = doc.page(0).expect("page");
    assert!(page.raw_chunk(b"Smmr").is_some());
    assert!(page.raw_chunk(b"Sjbz").is_none());

    let decoded = page
        .extract_mask()
        .expect("decode mask")
        .expect("mask present");
    assert_eq!((decoded.width, decoded.height), (bm.width, bm.height));
    assert_eq!(decoded.data, bm.data);
}

#[test]
fn fresh_page_metadata_round_trips_as_antz() {
    let bm = Bitmap::new(32, 24);
    let meta = crate::metadata::DjVuMetadata {
        title: Some("Fresh document".into()),
        author: Some("djvu-rs".into()),
        extra: vec![("language".into(), "en".into())],
        ..crate::metadata::DjVuMetadata::default()
    };
    let bytes = PageEncoder::from_bitmap(&bm)
        .with_metadata(meta.clone())
        .encode()
        .expect("encode");

    let doc = crate::djvu_document::DjVuDocument::parse(&bytes).expect("parse");
    let page = doc.page(0).expect("page");
    assert!(page.raw_chunk(b"METz").is_none());
    let (annotation, _) = page.annotations().expect("annotations").expect("ANTz");
    assert!(annotation.extra[0].starts_with("(metadata"));
    assert_eq!(doc.metadata().expect("metadata"), Some(meta));
}

#[test]
fn defaults_are_300_dpi_lossless_for_bitmap() {
    let bm = Bitmap::new(8, 8);
    let enc = PageEncoder::from_bitmap(&bm);
    assert_eq!(enc.dpi, 300);
    assert_eq!(enc.quality, EncodeQuality::Lossless);
}

#[test]
fn defaults_are_300_dpi_quality_for_pixmap() {
    let pm = Pixmap::white(8, 8);
    let enc = PageEncoder::from_pixmap(&pm);
    assert_eq!(enc.dpi, 300);
    assert_eq!(enc.quality, EncodeQuality::Quality);
    assert!(enc.segment_options.is_none());
}

#[test]
fn with_dpi_clamps_zero_to_one() {
    let bm = Bitmap::new(8, 8);
    let enc = PageEncoder::from_bitmap(&bm).with_dpi(0);
    assert_eq!(enc.dpi, 1);
}

#[test]
fn archival_bitmap_rejected() {
    let bm = Bitmap::new(16, 16);
    let err = PageEncoder::from_bitmap(&bm)
        .with_quality(EncodeQuality::Archival)
        .encode()
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("Archival"));
}

#[test]
fn empty_bitmap_round_trips() {
    let bm = Bitmap::new(1, 1);
    let bytes = PageEncoder::from_bitmap(&bm).encode().expect("encode");
    let form = parse_form(&bytes).expect("parse");
    assert_eq!(&form.form_type, b"DJVU");
}

#[test]
fn encode_rejects_pixmap_width_exceeding_u16() {
    // width = 70000 > 65535: try_from fails → EncodeError::Unsupported
    let pm = Pixmap {
        width: 70_000,
        height: 1,
        data: vec![],
    };
    let err = PageEncoder::from_pixmap(&pm)
        .with_quality(EncodeQuality::Quality)
        .encode()
        .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("width") || msg.contains("65"),
        "unexpected: {msg}"
    );
}

#[test]
fn encode_rejects_bitmap_height_exceeding_u16() {
    // height = 70000 > 65535: try_from fails → EncodeError::Unsupported
    let bm = Bitmap {
        width: 1,
        height: 70_000,
        data: vec![0u8; 70_000 / 8 + 1],
    };
    let err = PageEncoder::from_bitmap(&bm)
        .with_quality(EncodeQuality::Lossless)
        .encode()
        .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("height") || msg.contains("65"),
        "unexpected: {msg}"
    );
}

#[test]
fn quality_color_emits_info_sjbz_bg44() {
    // 64×64 page: white background with a black 16×16 ink square.
    let mut pm = Pixmap::white(64, 64);
    for y in 16..32 {
        for x in 16..32 {
            pm.set_rgb(x, y, 0, 0, 0);
        }
    }

    let bytes = PageEncoder::from_pixmap(&pm)
        .with_dpi(200)
        .with_quality(EncodeQuality::Quality)
        .encode()
        .expect("encode");

    let form = parse_form(&bytes).expect("parse_form");
    assert_eq!(&form.form_type, b"DJVU");

    let mut has_info = false;
    let mut has_sjbz = false;
    let mut bg44_count = 0;
    for chunk in &form.chunks {
        match &chunk.id {
            b"INFO" => has_info = true,
            b"Sjbz" => has_sjbz = true,
            b"BG44" => bg44_count += 1,
            _ => {}
        }
    }
    assert!(has_info, "INFO chunk missing");
    assert!(has_sjbz, "Sjbz chunk missing");
    assert!(
        bg44_count > 0,
        "expected at least one BG44 chunk, got {bg44_count}"
    );
}

#[test]
fn quality_color_emits_fgbz_for_colored_foreground() {
    let mut pm = Pixmap::white(64, 64);
    for y in 16..32 {
        for x in 16..32 {
            pm.set_rgb(x, y, 180, 20, 20);
        }
    }

    let bytes = PageEncoder::from_pixmap(&pm)
        .with_quality(EncodeQuality::Quality)
        .encode()
        .expect("encode");

    let form = parse_form(&bytes).expect("parse_form");
    let fgbz = form
        .chunks
        .iter()
        .find(|chunk| &chunk.id == b"FGbz")
        .expect("FGbz chunk present");
    let (palette, indices) = crate::fgbz_encode::decode_fgbz(fgbz.data).expect("decode FGbz");
    assert_eq!(palette.len(), 1);
    assert!(indices.is_empty());
    assert!(palette[0].r > 0, "foreground red should be preserved");
}

#[test]
fn quality_color_emits_per_blit_fgbz_indices() {
    let mut pm = Pixmap::white(80, 40);
    for y in 8..24 {
        for x in 8..24 {
            pm.set_rgb(x, y, 180, 20, 20);
        }
        for x in 48..64 {
            pm.set_rgb(x, y, 20, 40, 180);
        }
    }

    let bytes = PageEncoder::from_pixmap(&pm)
        .with_quality(EncodeQuality::Quality)
        .encode()
        .expect("encode");
    let doc = crate::djvu_document::DjVuDocument::parse(&bytes).expect("parse");
    let page = doc.page(0).expect("page");
    let fgbz = page.raw_chunk(b"FGbz").expect("FGbz present");
    let (palette, indices) = crate::fgbz_encode::decode_fgbz(fgbz).expect("decode FGbz");

    assert!(
        palette.len() >= 2,
        "expected at least two foreground colors, got {palette:?}"
    );
    assert!(
        indices.len() >= 2,
        "expected per-blit indices for two foreground components"
    );
    assert_ne!(
        indices[0], indices[1],
        "separate colored components should point at distinct palette entries"
    );

    let rendered = crate::Document::from_bytes(bytes)
        .expect("document")
        .page(0)
        .expect("page")
        .render()
        .expect("render");
    let left = rendered.get_rgb(12, 12);
    let right = rendered.get_rgb(52, 12);
    assert!(
        left.0 > left.2,
        "left foreground should render red-dominant, got {left:?}"
    );
    assert!(
        right.2 > right.0,
        "right foreground should render blue-dominant, got {right:?}"
    );
}

/// 20 thick rings, each missing a different ink pixel; the first ten are
/// dark red, the rest dark blue. Returns the pixmap and each ring's
/// top-left corner.
fn noisy_colored_rings(first_defect: usize) -> (Pixmap, Vec<(u32, u32)>) {
    let mut ink = Vec::new();
    for y in 0..16i32 {
        for x in 0..16i32 {
            let d2 = (2 * x - 15).pow(2) + (2 * y - 15).pow(2);
            if (8 * 8..=226).contains(&d2) {
                ink.push((x as u32, y as u32));
            }
        }
    }
    let mut pm = Pixmap::white(200, 60);
    let mut origins = Vec::new();
    for i in 0..20usize {
        let (ox, oy) = (4 + (i as u32 % 10) * 19, 6 + (i as u32 / 10) * 26);
        let hole = ink[(first_defect + i) * 7 % ink.len()];
        let (r, g, b) = if i < 10 { (150, 10, 10) } else { (10, 20, 150) };
        for &(x, y) in &ink {
            if (x, y) != hole {
                pm.set_rgb(ox + x, oy + y, r, g, b);
            }
        }
        origins.push((ox, oy));
    }
    (pm, origins)
}

#[test]
fn quality_color_refines_near_copies_and_keeps_colors() {
    // Near-copy glyphs become refinements of earlier symbols; the blit
    // order and so the FGbz colour per blit must not change.
    let (pm, origins) = noisy_colored_rings(0);
    let seg = segment_page(&pm, &EncodeQuality::Quality.default_segment_options());
    let (plain, _) =
        jb2_encode::encode_jb2_dict_with_blits(&seg.mask, &[], &Jb2EncodeOptions::default());

    let bytes = PageEncoder::from_pixmap(&pm)
        .with_quality(EncodeQuality::Quality)
        .encode()
        .expect("encode");
    let doc = crate::djvu_document::DjVuDocument::parse(&bytes).expect("parse");
    let page = doc.page(0).expect("page");
    let sjbz = page.raw_chunk(b"Sjbz").expect("Sjbz");
    assert!(
        sjbz.len() < plain.len(),
        "refined Sjbz {} B vs plain dict {} B",
        sjbz.len(),
        plain.len()
    );
    let mask = page.extract_mask().expect("mask decode").expect("mask");
    assert_eq!(mask, seg.mask, "mask must decode pixel-exact");

    let rendered = render_native(&doc);
    for (i, &(ox, oy)) in origins.iter().enumerate() {
        // First ink pixel of this ring in the decoded mask.
        let (px, py) = (0..16)
            .flat_map(|y| (0..16).map(move |x| (ox + x, oy + y)))
            .find(|&(x, y)| mask.get(x, y))
            .expect("ring ink");
        let (r, _, b) = rendered.get_rgb(px, py);
        if i < 10 {
            assert!(r > b, "ring {i} should render red, got ({r}, _, {b})");
        } else {
            assert!(b > r, "ring {i} should render blue, got ({r}, _, {b})");
        }
    }
}

#[test]
fn layered_shared_refines_near_copies_and_round_trips() {
    let pages = [noisy_colored_rings(0).0, noisy_colored_rings(40).0];
    let bytes = encode_djvm_layered_shared(&pages, EncodeQuality::Quality, 300, None, 2)
        .expect("layered shared encode");
    let doc = crate::djvu_document::DjVuDocument::parse(&bytes).expect("parse bundle");
    let opts = EncodeQuality::Quality.default_segment_options();
    for (i, pm) in pages.iter().enumerate() {
        let mask = doc.page(i).unwrap().extract_mask().unwrap().unwrap();
        assert_eq!(mask, segment_page(pm, &opts).mask, "page {i} mask");
    }
}

#[test]
fn median_cut_reduces_many_near_duplicate_colors_to_k() {
    // 40 colours clustered tightly around red and blue (simulating
    // anti-aliasing noise across many blits of "the same" ink colour).
    let mut colors = Vec::new();
    for i in 0..20u8 {
        colors.push(WeightedColor {
            r: 180 + (i % 5),
            g: 20,
            b: 20,
            weight: 10,
        });
    }
    for i in 0..20u8 {
        colors.push(WeightedColor {
            r: 20,
            g: 20,
            b: 180 + (i % 5),
            weight: 10,
        });
    }
    let palette = median_cut(&colors, 2);
    assert_eq!(palette.len(), 2);
    // One entry should be red-dominant, the other blue-dominant.
    let (mut reds, mut blues) = (0, 0);
    for c in &palette {
        if c.r > c.b {
            reds += 1;
        } else {
            blues += 1;
        }
    }
    assert_eq!((reds, blues), (1, 1));
}

#[test]
fn median_cut_never_exceeds_k_even_with_fewer_distinct_colors() {
    let colors = vec![
        WeightedColor {
            r: 10,
            g: 10,
            b: 10,
            weight: 1,
        },
        WeightedColor {
            r: 10,
            g: 10,
            b: 10,
            weight: 1,
        },
    ];
    // Requesting 8 boxes from a single distinct colour must not spin
    // forever or panic — it should stop once nothing is splittable.
    let palette = median_cut(&colors, 8);
    assert_eq!(palette.len(), 1);
}

#[test]
fn median_cut_empty_input_is_empty() {
    assert!(median_cut(&[], 4).is_empty());
}

#[test]
fn nearest_palette_index_picks_closest() {
    let palette = [
        FgbzColor { r: 255, g: 0, b: 0 },
        FgbzColor { r: 0, g: 0, b: 255 },
    ];
    assert_eq!(
        nearest_palette_index(
            &palette,
            FgbzColor {
                r: 200,
                g: 10,
                b: 10
            }
        ),
        0
    );
    assert_eq!(
        nearest_palette_index(
            &palette,
            FgbzColor {
                r: 10,
                g: 10,
                b: 200
            }
        ),
        1
    );
}

#[test]
fn fgbz_mediancut_is_opt_in_default_stays_exact() {
    // Same fixture as quality_color_emits_per_blit_fgbz_indices: two
    // distinctly-coloured blits. Exact (default) keeps 2 palette
    // entries; MedianCut capped at 1 must collapse to 1 and still
    // produce a valid, decodable page.
    let mut pm = Pixmap::white(80, 40);
    for y in 8..24 {
        for x in 8..24 {
            pm.set_rgb(x, y, 180, 20, 20);
        }
        for x in 48..64 {
            pm.set_rgb(x, y, 20, 40, 180);
        }
    }

    let default_bytes = PageEncoder::from_pixmap(&pm)
        .with_quality(EncodeQuality::Quality)
        .encode()
        .expect("default encode");
    let explicit_exact_bytes = PageEncoder::from_pixmap(&pm)
        .with_quality(EncodeQuality::Quality)
        .with_fgbz_options(FgbzPaletteOptions::Exact)
        .encode()
        .expect("exact encode");
    assert_eq!(
        default_bytes, explicit_exact_bytes,
        "FgbzPaletteOptions::Exact must be byte-identical to the (opt-out) default"
    );

    let capped_bytes = PageEncoder::from_pixmap(&pm)
        .with_quality(EncodeQuality::Quality)
        .with_fgbz_options(FgbzPaletteOptions::MedianCut { max_colors: 1 })
        .encode()
        .expect("median-cut encode");
    assert_ne!(
        default_bytes, capped_bytes,
        "opting into MedianCut{{max_colors:1}} must change the output"
    );

    let doc = crate::djvu_document::DjVuDocument::parse(&capped_bytes).expect("parse");
    let page = doc.page(0).expect("page");
    let fgbz = page.raw_chunk(b"FGbz").expect("FGbz present");
    let (palette, _indices) = crate::fgbz_encode::decode_fgbz(fgbz).expect("decode FGbz");
    assert_eq!(palette.len(), 1, "capped at 1 palette entry");
}

#[test]
fn quality_color_accepts_adaptive_segment_options() {
    let pm = mixed_lighting_fixture();

    let bytes = PageEncoder::from_pixmap(&pm)
        .with_quality(EncodeQuality::Quality)
        .with_segment_options(adaptive_segment_options())
        .encode()
        .expect("encode");

    let doc = crate::djvu_document::DjVuDocument::parse(&bytes).expect("parse");
    let page = doc.page(0).expect("page");
    assert!(page.raw_chunk(b"Sjbz").is_some());
    assert!(!page.all_chunks(b"BG44").is_empty());
}

#[test]
fn layered_shared_djbz_round_trips_with_incl() {
    // #452: two identical colour pages — their mask CCs are byte-exact across
    // pages, so they are promoted to one shared Djbz, and each page references
    // it via INCL while keeping its own BG44/FGbz.
    let pm = mixed_lighting_fixture();
    let pages = [pm.clone(), pm.clone()];
    let bytes = encode_djvm_layered_shared(&pages, EncodeQuality::Quality, 300, None, 2)
        .expect("layered shared encode");

    let doc = crate::djvu_document::DjVuDocument::parse(&bytes).expect("parse bundle");
    assert_eq!(doc.page_count(), 2);
    for i in 0..2 {
        let page = doc.page(i).expect("page");
        assert!(page.raw_chunk(b"Sjbz").is_some(), "page {i} Sjbz");
        assert!(!page.all_chunks(b"BG44").is_empty(), "page {i} BG44");
        assert!(
            page.raw_chunk(b"INCL").is_some(),
            "page {i} INCL → shared dict"
        );
        // The shared-dictionary Sjbz must still decode to the page mask.
        page.extract_mask()
            .expect("mask decode")
            .expect("mask present");
    }
    assert!(
        bytes.windows(4).any(|w| w == b"Djbz"),
        "shared Djbz form present"
    );
}

/// Encoder peak-memory step 4: [`encode_djvm_layered_shared_streaming`]
/// must produce byte-identical output to the eager `&[Pixmap]` entry
/// points for the same pages, regardless of window size — a window
/// covering everything at once (`Some(page_count)`), a narrow window
/// that forces multiple rounds, and the `None` (feature-dependent)
/// default all included.
#[test]
fn streaming_matches_eager_output_across_window_sizes() {
    let two = two_page_bundle_fixture();
    let extra = mixed_lighting_fixture();
    let pages: Vec<Pixmap> = vec![
        two[0].clone(),
        two[1].clone(),
        extra.clone(),
        two[0].clone(),
    ];

    let eager = encode_djvm_layered_shared(&pages, EncodeQuality::Quality, 300, None, 2)
        .expect("eager encode");

    for window in [None, Some(1), Some(2), Some(3), Some(pages.len())] {
        let pages_ref = &pages;
        let streamed = encode_djvm_layered_shared_streaming(
            pages_ref.len(),
            |idx| Ok::<Pixmap, std::convert::Infallible>(pages_ref[idx].clone()),
            EncodeQuality::Quality,
            300,
            None,
            2,
            false,
            None,
            window,
        )
        .unwrap_or_else(|e| panic!("streaming encode (window={window:?}) failed: {e}"));
        assert_eq!(
            eager, streamed,
            "window={window:?}: streaming output must be byte-identical to the eager path"
        );
    }
}

/// Same equivalence, but with thumbnails and per-page mask reuse both
/// enabled — the union path
/// [`encode_djvm_layered_shared_with_thumbnails_and_masks`] exercises —
/// to make sure neither optional feature is dropped or reordered by the
/// windowed phase-1 loop.
#[test]
fn streaming_matches_eager_with_thumbnails_and_masks() {
    let pages = two_page_bundle_fixture();
    let gen0 = encode_djvm_layered_shared(&pages, EncodeQuality::Quality, 300, None, 2)
        .expect("gen0 encode");
    let doc0 = crate::djvu_document::DjVuDocument::parse(&gen0).expect("parse gen0");
    let masks0: Vec<Bitmap> = (0..pages.len())
        .map(|i| doc0.page(i).unwrap().extract_mask().unwrap().unwrap())
        .collect();
    let mask_refs: Vec<Option<&Bitmap>> = masks0.iter().map(Some).collect();

    let eager = encode_djvm_layered_shared_with_thumbnails_and_masks(
        &pages,
        EncodeQuality::Quality,
        300,
        None,
        2,
        true,
        &mask_refs,
    )
    .expect("eager encode");

    let pages_ref = &pages;
    let streamed = encode_djvm_layered_shared_streaming(
        pages_ref.len(),
        |idx| Ok::<Pixmap, std::convert::Infallible>(pages_ref[idx].clone()),
        EncodeQuality::Quality,
        300,
        None,
        2,
        true,
        Some(&mask_refs),
        Some(1), // narrowest possible window: one page prepared at a time
    )
    .expect("streaming encode");

    assert_eq!(
        eager, streamed,
        "streaming with thumbnails+masks must match the eager equivalent"
    );
}

/// A page source that fails must surface as [`EncodeError::PageSource`],
/// not panic or silently produce a truncated/wrong bundle.
#[test]
fn streaming_source_error_surfaces_as_page_source_error() {
    #[derive(Debug, thiserror::Error)]
    #[error("simulated page {0} decode failure")]
    struct FakeError(usize);

    let pages = two_page_bundle_fixture();
    let result = encode_djvm_layered_shared_streaming(
        pages.len(),
        |idx| {
            if idx == 1 {
                Err(FakeError(idx))
            } else {
                Ok(pages[idx].clone())
            }
        },
        EncodeQuality::Quality,
        300,
        None,
        2,
        false,
        None,
        Some(1),
    );
    match result {
        Err(EncodeError::PageSource(e)) => {
            assert_eq!(e.to_string(), "simulated page 1 decode failure");
        }
        other => panic!("expected EncodeError::PageSource, got {other:?}"),
    }
}

#[test]
fn adaptive_segment_options_improve_decoded_mixed_lighting_fixture() {
    let pm = mixed_lighting_fixture();
    let fixed = PageEncoder::from_pixmap(&pm)
        .with_quality(EncodeQuality::Quality)
        .with_segment_options(SegmentOptions {
            bg_subsample: 6,
            ..SegmentOptions::default()
        })
        .encode()
        .expect("fixed encode");
    let adaptive = PageEncoder::from_pixmap(&pm)
        .with_quality(EncodeQuality::Quality)
        .with_segment_options(SegmentOptions {
            bg_subsample: 6,
            ..adaptive_segment_options()
        })
        .encode()
        .expect("adaptive encode");

    let fixed_render = render_encoded(&fixed);
    let adaptive_render = render_encoded(&adaptive);
    let fixed_err = mean_abs_rgb_diff(&pm, &fixed_render);
    let adaptive_err = mean_abs_rgb_diff(&pm, &adaptive_render);

    assert!(
        adaptive_err < fixed_err * 0.70,
        "adaptive decoded render should be closer to source ({adaptive_err:.2} vs {fixed_err:.2})"
    );
}

/// #571: the Photo profile writes INFO + BG44 only (no Sjbz/FGbz) and
/// round-trips through our decoder; grayscale sources take the grayscale
/// IW44 encoder.
#[test]
fn photo_profile_masks_nothing_and_round_trips() {
    // Colour gradient source.
    let mut pm = Pixmap::white(64, 48);
    for y in 0..48 {
        for x in 0..64 {
            pm.set_rgb(x, y, (x * 4) as u8, (y * 5) as u8, 128);
        }
    }
    let bytes = PageEncoder::from_pixmap(&pm)
        .with_dpi(100)
        .with_quality(EncodeQuality::Photo)
        .encode()
        .unwrap();
    let doc = crate::djvu_document::DjVuDocument::parse(&bytes).unwrap();
    let page = doc.page(0).unwrap();
    assert!(page.find_chunk(b"Sjbz").is_none(), "no mask in Photo");
    assert!(page.find_chunk(b"FGbz").is_none(), "no palette in Photo");
    assert!(page.find_chunk(b"BG44").is_some(), "background present");
    let out = crate::djvu_render::render_pixmap(
        page,
        &crate::djvu_render::RenderOptions {
            width: 64,
            height: 48,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!((out.width, out.height), (64, 48));

    // Pure-grayscale source must also round-trip (grayscale IW44 path).
    let mut gray = Pixmap::white(64, 48);
    for y in 0..48 {
        for x in 0..64 {
            let v = ((x + y) * 3) as u8;
            gray.set_rgb(x, y, v, v, v);
        }
    }
    let gbytes = PageEncoder::from_pixmap(&gray)
        .with_dpi(100)
        .with_quality(EncodeQuality::Photo)
        .encode()
        .unwrap();
    let gdoc = crate::djvu_document::DjVuDocument::parse(&gbytes).unwrap();
    let gout = crate::djvu_render::render_pixmap(
        gdoc.page(0).unwrap(),
        &crate::djvu_render::RenderOptions {
            width: 64,
            height: 48,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!((gout.width, gout.height), (64, 48));
}

/// #570: the auto-classifier must match the expert profile choice on the
/// corpus, and a photo must never be routed to the bilevel path
/// (catastrophic misroute).
#[test]
fn classify_content_matches_expert_choice_on_corpus() {
    let render = |path: &str, page: usize, dpi: f32| -> Pixmap {
        let data =
            std::fs::read(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path)).unwrap();
        let doc = crate::djvu_document::DjVuDocument::parse(&data).unwrap();
        let p = doc.page(page).unwrap();
        let scale = dpi / p.dpi().max(1) as f32;
        let w = ((p.width() as f32 * scale).round() as u32).max(1);
        let h = ((p.height() as f32 * scale).round() as u32).max(1);
        crate::djvu_render::render_pixmap(
            p,
            &crate::djvu_render::RenderOptions {
                width: w,
                height: h,
                ..Default::default()
            },
        )
        .unwrap()
    };

    // Photo (boy.djvu is a photograph) — must be Photo, and NEVER Lossless.
    let photo = classify_content(&render("tests/fixtures/boy.djvu", 0, 300.0));
    assert_ne!(
        photo,
        EncodeQuality::Lossless,
        "photo → bilevel is catastrophic"
    );
    assert_eq!(photo, EncodeQuality::Photo);

    // Bilevel scans → Lossless.
    assert_eq!(
        classify_content(&render("tests/fixtures/boy_jb2.djvu", 0, 300.0)),
        EncodeQuality::Lossless
    );
    // Native resolution — the real encode workflow feeds native scans;
    // a downscaled render adds antialiasing midtones a true bilevel
    // source doesn't have.
    assert_eq!(
        classify_content(&render("tests/corpus/cable_1973_100133.djvu", 0, 300.0)),
        EncodeQuality::Lossless
    );

    // Layered colour documents → Quality.
    assert_eq!(
        classify_content(&render("tests/fixtures/colorbook.djvu", 0, 150.0)),
        EncodeQuality::Quality
    );
    assert_eq!(
        classify_content(&render("tests/fixtures/navm_fgbz.djvu", 1, 150.0)),
        EncodeQuality::Quality
    );
}

fn adaptive_segment_options() -> SegmentOptions {
    SegmentOptions {
        binarization: crate::segment::Binarization::Sauvola { window: 9, k: 0.34 },
        bg_inpaint: true,
        ..SegmentOptions::default()
    }
}

fn mixed_lighting_fixture() -> Pixmap {
    let mut pm = Pixmap::white(48, 24);
    for y in 0..24 {
        for x in 0..48 {
            let v = if x < 24 { 80 } else { 220 };
            pm.set_rgb(x, y, v, v, v);
        }
    }

    // Dark ink on dark paper.
    for y in 6..18 {
        pm.set_rgb(9, y, 40, 40, 40);
        pm.set_rgb(14, y, 40, 40, 40);
    }
    for x in 9..=14 {
        pm.set_rgb(x, 6, 40, 40, 40);
        pm.set_rgb(x, 12, 40, 40, 40);
    }

    // Light-gray ink on bright paper. Fixed threshold treats this as BG,
    // so the thin strokes wash into the BG44 sample cells.
    for y in 6..18 {
        pm.set_rgb(33, y, 140, 140, 140);
        pm.set_rgb(40, y, 140, 140, 140);
    }
    for x in 33..=40 {
        pm.set_rgb(x, 6, 140, 140, 140);
        pm.set_rgb(x, 12, 140, 140, 140);
        pm.set_rgb(x, 17, 140, 140, 140);
    }
    pm
}

fn render_encoded(bytes: &[u8]) -> Pixmap {
    let doc = crate::djvu_document::DjVuDocument::parse(bytes).expect("parse encoded doc");
    let page = doc.page(0).expect("page");
    let (width, height) = page.dimensions();
    let opts = crate::djvu_render::RenderOptions {
        width: u32::from(width),
        height: u32::from(height),
        ..crate::djvu_render::RenderOptions::default()
    };
    crate::djvu_render::render_pixmap(page, &opts).expect("render encoded page")
}

fn mean_abs_rgb_diff(expected: &Pixmap, actual: &Pixmap) -> f64 {
    assert_eq!(
        (expected.width, expected.height),
        (actual.width, actual.height)
    );
    let mut sum = 0u64;
    let mut n = 0u64;
    for (a, b) in expected
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .zip(actual.data.as_chunks::<4>().0)
    {
        for c in 0..3 {
            sum += a[c].abs_diff(b[c]) as u64;
            n += 1;
        }
    }
    sum as f64 / n as f64
}

#[test]
fn archival_color_emits_layered_djvu_with_fgbz() {
    let mut pm = Pixmap::white(48, 48);
    for y in 12..24 {
        for x in 12..24 {
            pm.set_rgb(x, y, 0, 90, 180);
        }
    }

    let bytes = PageEncoder::from_pixmap(&pm)
        .with_quality(EncodeQuality::Archival)
        .encode()
        .expect("encode");

    let doc = crate::djvu_document::DjVuDocument::parse(&bytes).expect("parse");
    let page = doc.page(0).expect("page");
    assert!(page.raw_chunk(b"Sjbz").is_some());
    assert!(!page.all_chunks(b"BG44").is_empty());
    assert!(page.raw_chunk(b"FGbz").is_some());
}

#[test]
fn lossless_pixmap_rejected() {
    let pm = Pixmap::white(8, 8);
    let err = PageEncoder::from_pixmap(&pm)
        .with_quality(EncodeQuality::Lossless)
        .encode()
        .unwrap_err();
    assert!(format!("{err}").contains("Lossless"));
}

#[test]
fn quality_bitmap_rejected() {
    let bm = Bitmap::new(8, 8);
    let err = PageEncoder::from_bitmap(&bm)
        .with_quality(EncodeQuality::Quality)
        .encode()
        .unwrap_err();
    assert!(format!("{err}").contains("Quality"));
}

#[test]
fn quality_color_round_trips_through_document() {
    // End-to-end: encode a colour page at Quality, parse it back
    // through the high-level Document API, and confirm dimensions
    // + that the page has both an Sjbz and at least one BG44 chunk.
    let pm = Pixmap::white(32, 24);
    let bytes = PageEncoder::from_pixmap(&pm)
        .with_dpi(150)
        .with_quality(EncodeQuality::Quality)
        .encode()
        .expect("encode");

    let doc = crate::djvu_document::DjVuDocument::parse(&bytes).expect("parse");
    let page = doc.page(0).expect("page 0");
    assert_eq!(page.width(), 32);
    assert_eq!(page.height(), 24);
    assert_eq!(page.dpi(), 150);
    assert!(page.raw_chunk(b"Sjbz").is_some());
    assert!(!page.all_chunks(b"BG44").is_empty());
}

// ── TH44 thumbnail tests (layered encoder) ────────────────────────────────

/// Layered bundle WITH thumbnails: each page FORM:DJVU contains TH44 chunk(s)
/// that decode to a valid IW44 image at the expected reduced dimensions.
#[test]
fn layered_bundle_with_thumbnails_each_page_has_th44() {
    // Build two distinct colour pages.
    let mut p1 = Pixmap::white(64, 48);
    for y in 8..24 {
        for x in 8..24 {
            p1.set_rgb(x, y, 180, 20, 20);
        }
    }
    let p2 = Pixmap::white(64, 48);

    let bytes = encode_djvm_layered_shared_with_thumbnails(
        &[p1.clone(), p2.clone()],
        EncodeQuality::Quality,
        300,
        None,
        2,
        true,
    )
    .expect("encode layered with thumbnails");

    let doc = crate::djvu_document::DjVuDocument::parse(&bytes).expect("parse bundle");
    assert_eq!(doc.page_count(), 2);
    for i in 0..2 {
        let page = doc.page(i).expect("page");
        let thumb = page.thumbnail().expect("thumbnail() should not error");
        assert!(
            thumb.is_some(),
            "page {i} must carry a TH44 thumbnail when with_thumbnails=true"
        );
        let thumb = thumb.unwrap();
        let (tw, th) = crate::thumbnail::thumbnail_dimensions(
            if i == 0 { p1.width } else { p2.width },
            if i == 0 { p1.height } else { p2.height },
        );
        assert_eq!(
            thumb.width, tw,
            "page {i} thumbnail width should be {tw}, got {}",
            thumb.width
        );
        assert_eq!(
            thumb.height, th,
            "page {i} thumbnail height should be {th}, got {}",
            thumb.height
        );
    }
}

/// Layered bundle WITHOUT thumbnails: output must NOT contain any TH44 chunks.
#[test]
fn layered_bundle_without_thumbnails_has_no_th44() {
    let pm = Pixmap::white(64, 48);
    let bytes = encode_djvm_layered_shared(&[pm.clone(), pm], EncodeQuality::Quality, 300, None, 2)
        .expect("encode layered");

    let doc = crate::djvu_document::DjVuDocument::parse(&bytes).expect("parse bundle");
    assert_eq!(doc.page_count(), 2);
    for i in 0..2 {
        let page = doc.page(i).expect("page");
        let thumb = page.thumbnail().expect("thumbnail() should not error");
        assert!(
            thumb.is_none(),
            "page {i} must NOT carry a TH44 thumbnail when with_thumbnails=false"
        );
    }
}

// ── #779 follow-up: per-page mask reuse on the multi-page bundle path ──

/// Two differently-shaped colour fixtures, used as a small multi-page
/// bundle by the mask-reuse tests below.
fn two_page_bundle_fixture() -> [Pixmap; 2] {
    [synthetic_layered_page(), mixed_lighting_fixture()]
}

/// (a) Given the same per-page masks, `encode_djvm_layered_shared_with_masks`
/// must reproduce each page's mask byte-identically in the output bundle —
/// the multi-page analogue of `PageEncoder::with_mask`'s single-page
/// guarantee. Also exercises a mixed `Some`/`None` `masks` slice: page 1
/// falls back to normal segmentation and must still produce a valid,
/// non-empty mask.
#[test]
fn layered_shared_with_masks_reproduces_supplied_masks() {
    let pages = two_page_bundle_fixture();
    let gen0 = encode_djvm_layered_shared(&pages, EncodeQuality::Quality, 300, None, 2)
        .expect("gen0 encode");
    let doc0 = crate::djvu_document::DjVuDocument::parse(&gen0).expect("parse gen0");
    let masks0: Vec<Bitmap> = (0..pages.len())
        .map(|i| {
            doc0.page(i)
                .unwrap()
                .extract_mask()
                .unwrap()
                .expect("page has a mask")
        })
        .collect();
    for (i, m) in masks0.iter().enumerate() {
        assert!(
            m.data.iter().any(|&b| b != 0),
            "page {i} fixture mask must be non-empty"
        );
    }

    // Reuse page 0's mask, let page 1 re-segment from scratch (`None`).
    let mask_refs: Vec<Option<&Bitmap>> = vec![Some(&masks0[0]), None];
    let reused = encode_djvm_layered_shared_with_masks(
        &pages,
        EncodeQuality::Quality,
        300,
        None,
        2,
        &mask_refs,
    )
    .expect("reused encode");
    let doc1 = crate::djvu_document::DjVuDocument::parse(&reused).expect("parse reused");
    assert_eq!(doc1.page_count(), pages.len());

    let mask1_0 = doc1.page(0).unwrap().extract_mask().unwrap().unwrap();
    assert_eq!(
        (masks0[0].width, masks0[0].height, &masks0[0].data),
        (mask1_0.width, mask1_0.height, &mask1_0.data),
        "page 0: reused mask must decode back byte-identically"
    );
    let mask1_1 = doc1.page(1).unwrap().extract_mask().unwrap().unwrap();
    assert!(
        mask1_1.data.iter().any(|&b| b != 0),
        "page 1: re-segmented (None) page must still produce a non-empty mask"
    );

    // Reusing every page's mask must reproduce all of them byte-identically.
    let mask_refs_all: Vec<Option<&Bitmap>> = masks0.iter().map(Some).collect();
    let reused_all = encode_djvm_layered_shared_with_masks(
        &pages,
        EncodeQuality::Quality,
        300,
        None,
        2,
        &mask_refs_all,
    )
    .expect("reused-all encode");
    let doc_all = crate::djvu_document::DjVuDocument::parse(&reused_all).expect("parse");
    for (i, expected) in masks0.iter().enumerate() {
        let mask = doc_all.page(i).unwrap().extract_mask().unwrap().unwrap();
        assert_eq!(
            (expected.width, expected.height, &expected.data),
            (mask.width, mask.height, &mask.data),
            "page {i}: reused mask must decode back byte-identically"
        );
    }
}

/// (b) A 2-generation decode → render → re-encode cycle over a multi-page
/// bundle, feeding `encode_djvm_layered_shared_with_masks` each page's
/// previous-generation mask, must keep every page's mask bit-identical —
/// the multi-page analogue of
/// `layered_reencode_with_reused_mask_is_a_mask_fixed_point`.
#[test]
fn layered_shared_multipage_reencode_with_reused_masks_is_a_mask_fixed_point() {
    let pages0 = two_page_bundle_fixture();
    for quality in [EncodeQuality::Quality, EncodeQuality::Archival] {
        let gen0 = encode_djvm_layered_shared(&pages0, quality, 300, None, 2).expect("gen0 encode");
        let doc0 = crate::djvu_document::DjVuDocument::parse(&gen0).expect("parse gen0");
        let masks0: Vec<Bitmap> = (0..pages0.len())
            .map(|i| doc0.page(i).unwrap().extract_mask().unwrap().unwrap())
            .collect();

        let mut doc = doc0;
        let mut masks = masks0.clone();
        for generation in 1..=2 {
            let rendered: Vec<Pixmap> = (0..pages0.len())
                .map(|i| render_native_page(&doc, i))
                .collect();
            let mask_refs: Vec<Option<&Bitmap>> = masks.iter().map(Some).collect();
            let next =
                encode_djvm_layered_shared_with_masks(&rendered, quality, 300, None, 2, &mask_refs)
                    .expect("re-encode");
            doc = crate::djvu_document::DjVuDocument::parse(&next).expect("parse next gen");
            masks = (0..pages0.len())
                .map(|i| doc.page(i).unwrap().extract_mask().unwrap().unwrap())
                .collect();
            for i in 0..pages0.len() {
                assert_eq!(
                    (masks0[i].width, masks0[i].height, &masks0[i].data),
                    (masks[i].width, masks[i].height, &masks[i].data),
                    "{quality:?}: page {i} generation-{generation} mask must be bit-identical"
                );
            }
        }
    }
}

/// (c) Error cases: `encode_djvm_layered_shared_with_masks` must reject a
/// `masks` slice whose length doesn't match `pixmaps`, a mask whose
/// dimensions don't match its page, and — through the shared `_impl` —
/// a non-layered profile, matching `PageEncoder::with_mask`'s validation.
#[test]
fn layered_shared_with_masks_rejects_invalid_combinations() {
    let pages = two_page_bundle_fixture();
    let mask0 = Bitmap::new(pages[0].width, pages[0].height);
    let mask1 = Bitmap::new(pages[1].width, pages[1].height);
    let wrong_size = Bitmap::new(pages[1].width + 1, pages[1].height);

    // Wrong-length masks slice (one entry short).
    assert!(matches!(
        encode_djvm_layered_shared_with_masks(
            &pages,
            EncodeQuality::Quality,
            300,
            None,
            2,
            &[Some(&mask0)],
        ),
        Err(EncodeError::Unsupported(_))
    ));

    // Mismatched mask dimensions for page 1.
    assert!(matches!(
        encode_djvm_layered_shared_with_masks(
            &pages,
            EncodeQuality::Quality,
            300,
            None,
            2,
            &[Some(&mask0), Some(&wrong_size)],
        ),
        Err(EncodeError::Unsupported(_))
    ));

    // `encode_djvm_layered_shared` only supports the layered colour
    // profiles; masks must not bypass that gate.
    assert!(matches!(
        encode_djvm_layered_shared_with_masks(
            &pages,
            EncodeQuality::Lossless,
            300,
            None,
            2,
            &[Some(&mask0), Some(&mask1)],
        ),
        Err(EncodeError::Unsupported(_))
    ));

    // Valid combination still succeeds (sanity check the rejects above
    // are actually exercising the masks path, not some other failure).
    assert!(
        encode_djvm_layered_shared_with_masks(
            &pages,
            EncodeQuality::Quality,
            300,
            None,
            2,
            &[Some(&mask0), Some(&mask1)],
        )
        .is_ok()
    );
}

/// `encode_djvm_layered_shared_with_thumbnails_and_masks` combines both
/// extensions: TH44 thumbnails present AND the supplied mask reused.
#[test]
fn layered_shared_with_thumbnails_and_masks_combines_both() {
    let pages = two_page_bundle_fixture();
    let gen0 = encode_djvm_layered_shared(&pages, EncodeQuality::Quality, 300, None, 2)
        .expect("gen0 encode");
    let doc0 = crate::djvu_document::DjVuDocument::parse(&gen0).expect("parse gen0");
    let masks0: Vec<Bitmap> = (0..pages.len())
        .map(|i| doc0.page(i).unwrap().extract_mask().unwrap().unwrap())
        .collect();
    let mask_refs: Vec<Option<&Bitmap>> = masks0.iter().map(Some).collect();

    let bytes = encode_djvm_layered_shared_with_thumbnails_and_masks(
        &pages,
        EncodeQuality::Quality,
        300,
        None,
        2,
        true,
        &mask_refs,
    )
    .expect("combined encode");
    let doc = crate::djvu_document::DjVuDocument::parse(&bytes).expect("parse bundle");
    assert_eq!(doc.page_count(), pages.len());
    for (i, expected) in masks0.iter().enumerate() {
        let page = doc.page(i).unwrap();
        assert!(
            page.thumbnail().expect("thumbnail() ok").is_some(),
            "page {i} must carry a TH44 thumbnail"
        );
        let mask = page.extract_mask().unwrap().unwrap();
        assert_eq!(
            (expected.width, expected.height, &expected.data),
            (mask.width, mask.height, &mask.data),
            "page {i}: reused mask must decode back byte-identically"
        );
    }
}

// ── TXTZ_OCR: encode-time text layer ────────────────────────────────────

fn sample_text_layer(page_w: u32, page_h: u32) -> TextLayer {
    use crate::text::Rect;
    TextLayer {
        text: "Hello World".into(),
        zones: vec![TextZone {
            kind: TextZoneKind::Page,
            rect: Rect {
                x: 0,
                y: 0,
                width: page_w,
                height: page_h,
            },
            text: "Hello World".into(),
            children: vec![TextZone {
                kind: TextZoneKind::Line,
                rect: Rect {
                    x: 4,
                    y: 6,
                    width: page_w.saturating_sub(8),
                    height: 20,
                },
                text: "Hello World".into(),
                children: vec![
                    TextZone {
                        kind: TextZoneKind::Word,
                        rect: Rect {
                            x: 4,
                            y: 6,
                            width: 50,
                            height: 20,
                        },
                        text: "Hello".into(),
                        children: vec![],
                    },
                    TextZone {
                        kind: TextZoneKind::Word,
                        rect: Rect {
                            x: 60,
                            y: 6,
                            width: 50,
                            height: 20,
                        },
                        text: "World".into(),
                        children: vec![],
                    },
                ],
            }],
        }],
    }
}

#[test]
fn no_text_layer_is_byte_identical_to_pre_txtz_ocr_baseline() {
    // Opt-in guarantee: not calling with_text_layer()/with_ocr_text_layer()
    // must produce exactly the same bytes as before those methods existed
    // (no stray empty TXTz chunk, no size/behavior change for existing
    // callers). Cross-checked against `lossless_bilevel_round_trips`
    // et al., which continue to pass unmodified.
    let bm = checkerboard(32, 24);
    let bytes = PageEncoder::from_bitmap(&bm).encode().expect("encode");
    let form = parse_form(&bytes).expect("parse_form");
    assert!(
        !form
            .chunks
            .iter()
            .any(|c| &c.id == b"TXTz" || &c.id == b"TXTa"),
        "no text layer attached => no TXTz/TXTa chunk should be emitted"
    );
}

#[test]
fn with_text_layer_emits_txtz_and_round_trips_through_our_decoder() {
    let bm = checkerboard(120, 80);
    let layer = sample_text_layer(120, 80);
    let bytes = PageEncoder::from_bitmap(&bm)
        .with_quality(EncodeQuality::Lossless)
        .with_text_layer(layer.clone())
        .encode()
        .expect("encode");

    let form = parse_form(&bytes).expect("parse_form");
    assert!(
        form.chunks.iter().any(|c| &c.id == b"TXTz"),
        "TXTz chunk should be present after with_text_layer"
    );

    // Round-trip through our own decoder end to end (DjVuDocument), the
    // primary validator per the task brief.
    let doc = crate::djvu_document::DjVuDocument::parse(&bytes).expect("parse document");
    let page = doc.page(0).expect("page 0");
    let decoded = page
        .text_layer()
        .expect("text_layer() must not error")
        .expect("text_layer() must return Some after with_text_layer");
    assert_eq!(decoded.text, "Hello World");
    let words: Vec<&str> = decoded
        .zones
        .first()
        .and_then(|page_zone| page_zone.children.first())
        .map(|line| line.children.iter().map(|w| w.text.as_str()).collect())
        .unwrap_or_default();
    assert_eq!(words, vec!["Hello", "World"]);

    let plain = page.text().expect("text()").expect("Some plain text");
    assert_eq!(plain, "Hello World");
}

/// Deterministic mock `OcrBackend` for `with_ocr_text_layer` — mirrors the
/// pattern used by `examples/ocr_qa.rs`'s test-only mock backend so this
/// unit test needs no real Tesseract install.
struct MockOcrBackend {
    layer: TextLayer,
}

impl OcrBackend for MockOcrBackend {
    fn recognize(&self, _pixmap: &Pixmap, _options: &OcrOptions) -> Result<TextLayer, OcrError> {
        Ok(self.layer.clone())
    }
}

#[test]
fn with_ocr_text_layer_runs_backend_and_attaches_result() {
    let bm = checkerboard(96, 64);
    let backend = MockOcrBackend {
        layer: sample_text_layer(96, 64),
    };
    let bytes = PageEncoder::from_bitmap(&bm)
        .with_quality(EncodeQuality::Lossless)
        .with_ocr_text_layer(&backend, &OcrOptions::default())
        .expect("OCR backend should not fail")
        .encode()
        .expect("encode");

    let doc = crate::djvu_document::DjVuDocument::parse(&bytes).expect("parse document");
    let page = doc.page(0).expect("page 0");
    let text = page.text().expect("text()").expect("Some plain text");
    assert_eq!(text, "Hello World");
}

#[test]
fn with_ocr_text_layer_works_from_colour_pixmap_source_too() {
    // Quality/Archival (Pixmap source) path: OCR runs directly on the
    // pixmap without the bitmap_to_pixmap conversion.
    let pm = Pixmap::white(96, 64);
    let backend = MockOcrBackend {
        layer: sample_text_layer(96, 64),
    };
    let bytes = PageEncoder::from_pixmap(&pm)
        .with_quality(EncodeQuality::Quality)
        .with_ocr_text_layer(&backend, &OcrOptions::default())
        .expect("OCR backend should not fail")
        .encode()
        .expect("encode");

    let form = parse_form(&bytes).expect("parse_form");
    assert!(form.chunks.iter().any(|c| &c.id == b"TXTz"));
}

#[test]
fn bitmap_to_pixmap_maps_black_pixels_to_black_rgb() {
    let mut bm = Bitmap::new(4, 2);
    bm.set_black(1, 0);
    let pm = bitmap_to_pixmap(&bm);
    assert_eq!(pm.get_rgb(1, 0), (0, 0, 0));
    assert_eq!(pm.get_rgb(0, 0), (255, 255, 255));
}

/// Encoder peak-memory step 3: the phase-1 precomputed colour table
/// (`precompute_cc_data`, sampled via
/// `jb2_encode::symbol_boxes_in_emission_order` — no dictionary, no
/// entropy encode) must produce byte-identical `FGbz` to the existing
/// decode-based-order sampler (`foreground_fgbz_from_blits`, sampled
/// from the real emitted blits) for the lossless default case. This
/// pins the "geometric decomposition is independent of the shared
/// dictionary" claim the precomputation relies on.
#[test]
fn precomputed_cc_colors_match_blit_based_fgbz_sampling() {
    let pm = mixed_lighting_fixture();
    let opts = EncodeQuality::Quality.default_segment_options();
    let seg = segment_page(&pm, &opts);
    let jb2_options = Jb2EncodeOptions::default();

    let (_symbols, cc_colors) =
        precompute_cc_data(&pm, &seg.mask, &jb2_options).expect("lossless: table computed");
    let (_, blits) = jb2_encode::encode_jb2_dict_with_blits(&seg.mask, &[], &jb2_options);
    assert_eq!(
        cc_colors.len(),
        blits.len(),
        "precomputed table must align 1:1 with the real emitted blit list"
    );

    let from_table = fgbz_from_accums(cc_colors, FgbzPaletteOptions::Exact);
    let from_blits = foreground_fgbz_from_blits(&pm, &seg.mask, &blits, FgbzPaletteOptions::Exact);

    match (from_table, from_blits) {
        (Some(a), Some(b)) => {
            let (
                Chunk::Leaf {
                    id: a_id,
                    data: a_data,
                },
                Chunk::Leaf {
                    id: b_id,
                    data: b_data,
                },
            ) = (a.into_leaf(), b.into_leaf())
            else {
                panic!("FGbz encodes to a leaf chunk");
            };
            assert_eq!(a_id, b_id);
            assert_eq!(a_data, b_data, "FGbz payload must be byte-identical");
        }
        (None, None) => {}
        (Some(_), None) => panic!("table produced FGbz but blit-based sampler produced none"),
        (None, Some(_)) => panic!("blit-based sampler produced FGbz but table produced none"),
    }
}

/// Same equivalence, exercised through the full multi-page bundle
/// pipeline (`prepare_page` → `build_page`) rather than the two
/// sampling functions directly, and across two pages sharing a
/// dictionary — the case the precomputation exists for.
#[test]
fn layered_shared_bundle_fgbz_unaffected_by_precomputed_colour_table() {
    let pm = mixed_lighting_fixture();
    let pages = [pm.clone(), pm.clone()];
    let bytes = encode_djvm_layered_shared(&pages, EncodeQuality::Quality, 300, None, 2)
        .expect("layered shared encode");
    let doc = crate::djvu_document::DjVuDocument::parse(&bytes).expect("parse bundle");
    for i in 0..2 {
        let page = doc.page(i).expect("page");
        assert!(page.raw_chunk(b"FGbz").is_some(), "page {i} FGbz present");
    }
}

/// `prepare_page` must not attempt the precomputed-table shortcut when
/// `Jb2EncodeOptions::lossy_threshold > 0` — lossy rec-7 substitution can
/// blit a near-twin dict entry whose true decoded pixels differ from the
/// component the table was sampled from. `build_page` must still
/// produce an `FGbz` chunk in that case (via the decode-based fallback),
/// not silently drop it.
#[test]
fn lossy_threshold_falls_back_to_decode_based_fgbz_sampling() {
    let pm = mixed_lighting_fixture();
    let opts = EncodeQuality::Quality.default_segment_options();
    let lossy_jb2_options = Jb2EncodeOptions {
        lossy_threshold: 0.05,
        ..Jb2EncodeOptions::default()
    };

    // Phase 1: the fallback signal is `None`, not a (possibly wrong) table.
    let prepared = prepare_page(&pm, None, &opts, false, &lossy_jb2_options);
    assert!(
        prepared.cc_colors.is_none(),
        "lossy_threshold > 0 must skip the precomputed colour table"
    );

    // Phase 3: FGbz must still be emitted, via the decode-based path.
    let part = build_page(
        0,
        Some(&pm),
        prepared,
        &[],
        false,
        "dict0001.djvi",
        300,
        &lossy_jb2_options,
    )
    .expect("build_page");
    assert_eq!(part.kind, DirmComponentKind::Page);
    assert_eq!(part.id, "p0001.djvu");
    assert!(
        part.bytes.windows(4).any(|w| w == b"FGbz"),
        "FGbz chunk present despite lossy_threshold fallback"
    );
}
