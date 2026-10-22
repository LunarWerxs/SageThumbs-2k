use super::*;

// ── C4_TILE_CACHE: render_region_tiled ───────────────────────────────────

/// `render_region_tiled` is byte-identical to `render_region` across a
/// scripted "pan": many overlapping regions, some spanning multiple
/// `TILE_SIZE`-aligned tiles, some landing on partial edge tiles at the
/// full render's right/bottom border (full render size deliberately not a
/// multiple of `TILE_SIZE`).
#[test]
fn render_region_tiled_matches_render_region() {
    let doc = load_doc("colorbook.djvu");
    let page = doc.page(0).unwrap();
    // Not a multiple of TILE_SIZE (256), so the sequence below touches
    // partial edge tiles too.
    let opts = RenderOptions {
        width: 900,
        height: 700,
        ..Default::default()
    };

    // A little pan sequence: overlapping viewports sweeping across and
    // down the page, each straddling tile boundaries differently.
    let regions = [
        RenderRect {
            x: 0,
            y: 0,
            width: 300,
            height: 220,
        },
        RenderRect {
            x: 60,
            y: 0,
            width: 300,
            height: 220,
        },
        RenderRect {
            x: 200,
            y: 40,
            width: 300,
            height: 220,
        },
        RenderRect {
            x: 400,
            y: 40,
            width: 300,
            height: 220,
        },
        RenderRect {
            x: 600,
            y: 480,
            width: 300,
            height: 220,
        }, // right/bottom edge tiles
        RenderRect {
            x: 250,
            y: 250,
            width: 400,
            height: 300,
        }, // spans 2x2 tiles
        RenderRect {
            x: 1,
            y: 1,
            width: 5,
            height: 5,
        }, // sub-tile sliver
    ];

    for region in regions {
        let direct = render_region(page, region, &opts).expect("render_region");
        let tiled = render_region_tiled(page, region, &opts).expect("render_region_tiled");
        assert_eq!(tiled.width, direct.width);
        assert_eq!(tiled.height, direct.height);
        assert_eq!(
            tiled.data, direct.data,
            "render_region_tiled diverged from render_region for {region:?}"
        );
    }
}

/// A second call for a region already fully covered by previously-cached
/// tiles still reproduces the same bytes (exercises the cache-hit path,
/// not just cold tile composition).
#[test]
fn render_region_tiled_repeated_region_matches() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    let opts = RenderOptions {
        width: 640,
        height: 480,
        ..Default::default()
    };
    let region = RenderRect {
        x: 100,
        y: 100,
        width: 200,
        height: 150,
    };

    let first = render_region_tiled(page, region, &opts).unwrap();
    // Bytes should now be resident in the page's tile cache.
    assert!(page.render_cache_bytes() > 0);
    let second = render_region_tiled(page, region, &opts).unwrap();
    assert_eq!(first.data, second.data);

    // A neighbouring, overlapping region should also match a direct render.
    let overlapping = RenderRect {
        x: 150,
        y: 120,
        width: 200,
        height: 150,
    };
    let direct = render_region(page, overlapping, &opts).unwrap();
    let tiled = render_region_tiled(page, overlapping, &opts).unwrap();
    assert_eq!(direct.data, tiled.data);
}

/// Rotated and permissive requests go through the tile cache; scaled
/// Lanczos-3 falls back to `render_region`. Every mode produces the exact
/// `render_region` output.
#[test]
fn render_region_tiled_matches_render_region_in_every_mode() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    let region = RenderRect {
        x: 5,
        y: 5,
        width: 40,
        height: 30,
    };
    let tiles = || page.render_layers().tile_cache_len();

    let rotated_opts = RenderOptions {
        width: 200,
        height: 150,
        rotation: UserRotation::Cw90,
        ..Default::default()
    };
    assert_eq!(tiles(), 0);
    let tiled = render_region_tiled(page, region, &rotated_opts).unwrap();
    assert!(tiles() > 0, "a rotated request must fill the tile cache");
    assert_eq!((tiled.width, tiled.height), (30, 40));
    assert_eq!(
        render_region(page, region, &rotated_opts).unwrap().data,
        tiled.data
    );
    // Rotated tiles are the upright ones, turned: a second, upright
    // request reuses them.
    let upright_opts = RenderOptions {
        rotation: UserRotation::None,
        ..rotated_opts
    };
    let cached = tiles();
    assert_eq!(
        render_region(page, region, &upright_opts).unwrap().data,
        render_region_tiled(page, region, &upright_opts)
            .unwrap()
            .data
    );
    assert_eq!(tiles(), cached, "rotation must not be part of the tile key");

    let lanczos_opts = RenderOptions {
        width: 100,
        height: 75,
        resampling: Resampling::Lanczos3,
        ..Default::default()
    };
    assert_eq!(
        render_region(page, region, &lanczos_opts).unwrap().data,
        render_region_tiled(page, region, &lanczos_opts)
            .unwrap()
            .data
    );

    let permissive_opts = RenderOptions {
        width: 300,
        height: 225,
        permissive: true,
        ..Default::default()
    };
    let cached = tiles();
    assert_eq!(
        render_region(page, region, &permissive_opts).unwrap().data,
        render_region_tiled(page, region, &permissive_opts)
            .unwrap()
            .data
    );
    assert!(
        tiles() > cached,
        "a permissive request must fill the tile cache"
    );
}

/// The tile cache's byte accounting is bounded (FIFO eviction) and feeds
/// into `render_cache_bytes` / `evict_render_cache` like the other layer
/// caches (C5 integration).
#[test]
fn render_region_tiled_cache_is_budget_bounded_and_evictable() {
    let mut doc = load_doc("colorbook.djvu");
    let (native_w, native_h) = {
        let p = doc.page(0).unwrap();
        (p.width() as u32, p.height() as u32)
    };
    // A render large enough to have many more than
    // TILE_CACHE_MAX_BYTES / (TILE_SIZE*TILE_SIZE*4) tiles available.
    let opts = RenderOptions {
        width: native_w.max(4000),
        height: native_h.max(4000),
        ..Default::default()
    };
    let full_w = opts.width;

    {
        let page = doc.page(0).unwrap();
        // Touch many disjoint tiles by requesting a small region in each.
        let tiles_per_side = (full_w / TILE_SIZE).clamp(1, 12);
        for ty in 0..tiles_per_side {
            for tx in 0..tiles_per_side {
                let region = RenderRect {
                    x: tx * TILE_SIZE,
                    y: ty * TILE_SIZE,
                    width: 8,
                    height: 8,
                };
                let _ = render_region_tiled(page, region, &opts).unwrap();
            }
        }
        let bytes = page.render_layers().tile_cache_bytes();
        assert!(bytes > 0, "expected some tile bytes cached");
        assert!(
            bytes <= TILE_CACHE_MAX_BYTES,
            "tile cache exceeded its byte budget: {bytes} > {TILE_CACHE_MAX_BYTES}"
        );
    }

    // Evicting the whole page's render cache drops the tiles too.
    doc.evict_render_caches();
    assert_eq!(doc.page(0).unwrap().render_cache_bytes(), 0);
}

/// `render_region_tiled` with zero-size dimensions errors like
/// `render_region`.
#[test]
fn render_region_tiled_rejects_zero_dimensions() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    let opts = RenderOptions {
        width: 100,
        height: 80,
        ..Default::default()
    };
    let region = RenderRect {
        x: 0,
        y: 0,
        width: 0,
        height: 10,
    };
    assert!(render_region_tiled(page, region, &opts).is_err());
}

/// `render_region` with a byte-aligned x offset on a bilevel page matches the
/// full render — exercises the generalized P2 BILEVEL_RGBA fast path (#433),
/// which fires when `offset_x % 8 == 0`.
#[test]
fn render_region_bilevel_byte_aligned_offset_matches_full() {
    let doc = load_doc("boy_jb2.djvu");
    let page = doc.page(0).unwrap();
    let full_w = page.width() as u32;
    let full_h = page.height() as u32;
    let opts = RenderOptions {
        width: full_w,
        height: full_h,
        ..Default::default()
    };
    let full = render_pixmap(page, &opts).expect("full render should succeed");
    // x=16 is byte-aligned (16 % 8 == 0) → generalized P2 path; x=17 below isn't.
    for &x in &[16u32, 17u32] {
        let region = RenderRect {
            x,
            y: 8,
            width: 64,
            height: 32,
        };
        let part = render_region(page, region, &opts).expect("region render should succeed");
        for ry in 0..region.height {
            for rx in 0..region.width {
                let fb = (((region.y + ry) * full_w + (x + rx)) * 4) as usize;
                let pb = ((ry * region.width + rx) * 4) as usize;
                assert_eq!(
                    &full.data[fb..fb + 4],
                    &part.data[pb..pb + 4],
                    "mismatch at x={x} region ({rx},{ry})"
                );
            }
        }
    }
}

/// `render_region` with invalid dimensions returns an error.
#[test]
fn render_region_invalid_dimensions() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    let opts = RenderOptions {
        width: 100,
        height: 100,
        ..Default::default()
    };
    let region = RenderRect {
        x: 0,
        y: 0,
        width: 0,
        height: 50,
    };
    let err = render_region(page, region, &opts).unwrap_err();
    assert!(
        matches!(err, RenderError::InvalidDimensions { .. }),
        "expected InvalidDimensions, got {err:?}"
    );
}

/// `render_pixmap` still works correctly (regression guard).
#[test]
fn render_pixmap_still_works_after_refactor() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    let opts = RenderOptions {
        width: 80,
        height: 60,
        ..Default::default()
    };
    let pm = render_pixmap(page, &opts).expect("render_pixmap should succeed");
    assert_eq!(pm.width, 80);
    assert_eq!(pm.height, 60);
    assert_eq!(pm.data.len(), 80 * 60 * 4);
}

/// `best_iw44_subsample` returns expected power-of-2 values.
#[test]
fn best_iw44_subsample_values() {
    assert_eq!(best_iw44_subsample(1.0), 1, "scale=1.0 → subsample=1");
    assert_eq!(best_iw44_subsample(0.5), 2, "scale=0.5 → subsample=2");
    assert_eq!(
        best_iw44_subsample(0.375),
        4,
        "scale=0.375 → subsample=4 (1.5/0.375=4.0, allows 1.5× upscale)"
    );
    assert_eq!(best_iw44_subsample(0.25), 4, "scale=0.25 → subsample=4");
    assert_eq!(
        best_iw44_subsample(0.1),
        8,
        "scale=0.1 → subsample=8 (capped)"
    );
    assert_eq!(
        best_iw44_subsample(0.0),
        1,
        "scale=0.0 → subsample=1 (edge case)"
    );
    assert_eq!(
        best_iw44_subsample(-1.0),
        1,
        "scale<0 → subsample=1 (edge case)"
    );
    assert_eq!(
        best_iw44_subsample(2.0),
        1,
        "scale>1.0 → subsample=1 (no upscaling needed)"
    );
}

/// The IW44 decode subsample is derived from the output `width`, not from
/// the deprecated `scale` field — the regression guard for the PDF
/// over-decode (#377).
#[test]
#[allow(deprecated)] // deliberately writes the legacy `scale` field to prove it is ignored
fn decode_subsample_derives_from_width_not_scale_field() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    let (dw, _) = display_dimensions(page);

    // Quarter-width output. The PDF exporter built exactly this — a small
    // `width` with `scale` left at the 1.0 default — and the old pipeline
    // read `scale` and decoded at full wavelet resolution (subsample 1).
    // The decode scale is now derived from `width`, so it is 0.25 →
    // subsample 4, and the over-decode is gone.
    let opts = RenderOptions {
        width: dw / 4,
        ..Default::default()
    };
    assert!((opts.decode_scale(page) - 0.25).abs() < 0.01);
    assert_eq!(best_iw44_subsample(opts.decode_scale(page)), 4);

    // Writing the legacy `scale` field by hand — to any value — must not
    // change the width-derived subsample.
    for misleading in [1.0_f32, 0.0, 0.5, 4.0] {
        let mut o = RenderOptions {
            width: dw / 4,
            ..Default::default()
        };
        o.scale = misleading;
        assert_eq!(
            best_iw44_subsample(o.decode_scale(page)),
            4,
            "scale={misleading} must not change the width-derived subsample",
        );
    }
}

/// INFO-rotated page: the decode scale follows the *native* page width, not
/// the rotation-swapped display width. The compositor scales the native
/// raster into the output buffer before `rotate_pixmap` runs, so a downscaled
/// rotated page must not pick its IW44 subsample from the swapped dimension —
/// doing so over-subsampled a downscaled portrait background (#377 follow-up).
#[test]
fn decode_scale_uses_native_width_for_rotated_page() {
    // boy_jb2_rotate90 carries a 90° rotation in its INFO chunk.
    let doc = load_doc("boy_jb2_rotate90.djvu");
    let page = doc.page(0).unwrap();
    let pw = page.width() as u32;
    let (dw, _) = display_dimensions(page);
    assert_ne!(dw, pw, "fixture must be a non-square INFO-rotated page");

    // The raster exporters size the output from the native page width
    // (page.width() * s); at s = 0.5 the half-size render must decode at 0.5.
    let opts = RenderOptions {
        width: pw / 2,
        height: (page.height() as u32) / 2,
        ..Default::default()
    };
    let native = opts.width as f32 / pw as f32; // correct: width / native width
    let display = opts.width as f32 / dw as f32; // the old bug: width / display width
    assert!(
        (opts.decode_scale(page) - native).abs() < 1e-4,
        "decode_scale {} should equal the native-width ratio {native}",
        opts.decode_scale(page),
    );
    assert!(
        (opts.decode_scale(page) - display).abs() > 1e-2,
        "decode_scale must not follow the rotation-swapped display width ({display})",
    );
}

/// Rendering with bg_subsample=2 (scale=0.5) produces the correct output dimensions.
#[test]
fn render_pixmap_subsampled_bg_correct_dimensions() {
    let doc = load_doc("boy.djvu");
    let page = doc.page(0).unwrap();
    // width = half the page → decode_scale ≈ 0.5 → bg_subsample=2 internally
    let opts = RenderOptions {
        width: (page.width() as f32 * 0.5) as u32,
        height: (page.height() as f32 * 0.5) as u32,
        ..Default::default()
    };
    let pm = render_pixmap(page, &opts).expect("subsampled render should succeed");
    assert_eq!(pm.width, opts.width);
    assert_eq!(pm.height, opts.height);
    assert_eq!(
        pm.data.len() as u64,
        opts.width as u64 * opts.height as u64 * 4
    );
}

/// Second render of the same page produces identical pixels — confirms the
/// BG44 cache is used and does not corrupt output.
#[test]
fn decoded_bg44_cache_produces_identical_pixels_on_second_render() {
    let doc = load_doc("boy.djvu");
    let page = doc.page(0).unwrap();
    let opts = RenderOptions {
        width: page.width() as u32,
        height: page.height() as u32,
        ..Default::default()
    };
    let pm1 = render_pixmap(page, &opts).expect("first render should succeed");
    let pm2 = render_pixmap(page, &opts).expect("second render should succeed");
    assert_eq!(
        pm1.data, pm2.data,
        "cached render must produce identical pixels"
    );
}

/// After the first render the `decoded_bg44` cache is populated — the
/// image dimensions match the page's raw BG44 size.
#[test]
fn decoded_bg44_is_populated_after_render() {
    let doc = load_doc("boy.djvu");
    let page = doc.page(0).unwrap();
    let opts = RenderOptions {
        width: page.width() as u32,
        height: page.height() as u32,
        ..Default::default()
    };
    // Trigger cache population.
    render_pixmap(page, &opts).expect("render should succeed");
    // Cache must now hold an image whose size matches the page's native size.
    let cached = page
        .decoded_bg44()
        .expect("cache should be populated after render");
    assert_eq!(
        cached.width,
        page.width() as u32,
        "cached bg44 width must equal page width"
    );
    assert_eq!(
        cached.height,
        page.height() as u32,
        "cached bg44 height must equal page height"
    );
}

/// `downsample_mask_4x` is render-tier logic now testable on a hand-built
/// bitmap — no DjVu bytes to parse. Each 4×4 source block collapses to one
/// output bit that is set iff any source bit in the block is set.
#[test]
fn downsample_mask_4x_max_pools_each_block() {
    // 8×8 mask: set a single pixel in the top-left block and one in the
    // bottom-right block; the other two blocks stay clear.
    let mut src = crate::bitmap::Bitmap::new(8, 8);
    src.set(1, 2, true); // top-left 4×4 block
    src.set(6, 5, true); // bottom-right 4×4 block
    let out = downsample_mask_4x(&src);
    assert_eq!((out.width, out.height), (2, 2));
    assert!(out.get(0, 0), "top-left block had a set bit");
    assert!(!out.get(1, 0), "top-right block was empty");
    assert!(!out.get(0, 1), "bottom-left block was empty");
    assert!(out.get(1, 1), "bottom-right block had a set bit");
}

/// A non-multiple-of-4 mask rounds up: a 5×5 source yields a 2×2 result and
/// the ragged edge block still max-pools its single column/row.
#[test]
fn downsample_mask_4x_rounds_up_ragged_edges() {
    let mut src = crate::bitmap::Bitmap::new(5, 5);
    src.set(4, 4, true); // lone pixel in the ragged bottom-right block
    let out = downsample_mask_4x(&src);
    assert_eq!((out.width, out.height), (2, 2));
    assert!(out.get(1, 1), "ragged corner block must capture its bit");
    assert!(!out.get(0, 0));
}

/// `render_region` applies page rotation the same way as `render_pixmap`.
///
/// For a 90° CW rotation a non-square region of width×height is returned as
/// height×width — proving rotation was applied (not silently skipped).
#[test]
fn render_region_applies_rotation() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    // Request an explicit 90° CW user rotation.
    let opts = RenderOptions {
        width: 80,
        height: 60,
        rotation: UserRotation::Cw90,
        ..Default::default()
    };
    // Non-square region so swapped dimensions are detectable.
    let region = RenderRect {
        x: 0,
        y: 0,
        width: 40,
        height: 20,
    };
    let part = render_region(page, region, &opts).expect("region render should succeed");
    // After CW90 rotation a 40×20 region becomes 20×40.
    assert_eq!(
        part.width, 20,
        "expected width=20 (was region.height) after CW90 rotation"
    );
    assert_eq!(
        part.height, 40,
        "expected height=40 (was region.width) after CW90 rotation"
    );
}

/// #691: `render_region` matches the same crop of `render_pixmap` even
/// when the downscale activates the 1/4-resolution mask fast path
/// (`bg_subsample >= 4`, no bold, no FGbz) — the region path must take
/// the same `sub4_mask` decision as the full-page path.
#[test]
fn render_region_matches_full_render_crop_at_sub4() {
    let doc = load_doc("boy_jb2.djvu");
    let page = doc.page(0).unwrap();
    let opts = RenderOptions {
        width: 40,
        height: 52,
        ..Default::default()
    };
    let full = render_pixmap(page, &opts).unwrap();
    let region = RenderRect {
        x: 8,
        y: 8,
        width: 24,
        height: 24,
    };
    let reg = render_region(page, region, &opts).unwrap();
    let mut crop = Vec::new();
    for y in 0..24usize {
        let s = ((8 + y) * 40 + 8) * 4;
        crop.extend_from_slice(&full.data[s..s + 24 * 4]);
    }
    assert_eq!(
        reg.data, crop,
        "render_region must be byte-identical to the matching crop of render_pixmap"
    );
}
