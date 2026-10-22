use super::*;

// ── render_region tests ───────────────────────────────────────────────────

/// `render_region` allocates only the region-sized buffer (≤ 512 KB for 256×256).
#[test]
fn render_region_allocates_proportionally() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    let opts = RenderOptions::fit_to_width(page, 1000);
    let region = RenderRect {
        x: 0,
        y: 0,
        width: 256,
        height: 256,
    };
    let pm = render_region(page, region, &opts).expect("render_region should succeed");
    assert_eq!(pm.width, 256);
    assert_eq!(pm.height, 256);
    assert_eq!(pm.data.len(), 256 * 256 * 4);
    assert!(
        pm.data.len() <= 512 * 1024,
        "region allocation {} exceeds 512 KB",
        pm.data.len()
    );
}

/// `render_region` pixels match the same pixels from `render_pixmap`.
#[test]
fn render_region_matches_full_render() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    let opts = RenderOptions {
        width: 100,
        height: 80,
        ..Default::default()
    };
    let full = render_pixmap(page, &opts).expect("full render should succeed");
    let region = RenderRect {
        x: 10,
        y: 10,
        width: 30,
        height: 20,
    };
    let part = render_region(page, region, &opts).expect("region render should succeed");

    assert_eq!(part.width, 30);
    assert_eq!(part.height, 20);

    for ry in 0..20u32 {
        for rx in 0..30u32 {
            let full_base = ((10 + ry) as usize * 100 + (10 + rx) as usize) * 4;
            let part_base = (ry as usize * 30 + rx as usize) * 4;
            assert_eq!(
                &full.data[full_base..full_base + 4],
                &part.data[part_base..part_base + 4],
                "pixel mismatch at region ({rx},{ry}) / full ({},{} )",
                10 + rx,
                10 + ry
            );
        }
    }
}

/// `region` of `full`, white where it leaves `full`: the reference crop,
/// pixel by pixel.
pub(super) fn reference_crop(full: &Pixmap, region: RenderRect) -> Vec<u8> {
    let mut out = Pixmap::white(region.width, region.height);
    for y in 0..region.height {
        for x in 0..region.width {
            let (fx, fy) = (region.x + x, region.y + y);
            if fx < full.width && fy < full.height {
                let (r, g, b) = full.get_rgb(fx, fy);
                out.set_rgb(x, y, r, g, b);
            }
        }
    }
    out.data
}

/// Regions inside the page, across its right and bottom edges, and
/// wholly outside it.
pub(super) fn regions_around(w: u32, h: u32) -> [RenderRect; 4] {
    let rect = |x, y, width, height| RenderRect {
        x,
        y,
        width,
        height,
    };
    [
        rect(3, 5, 17, 11),
        rect(w.saturating_sub(7), h.saturating_sub(4), 12, 9),
        rect(0, 0, w + 1, h + 1),
        rect(w + 2, 0, 5, 5),
    ]
}

/// Lanczos-3 at a scaled size ignores `aa` on its fallback too: when the
/// native composite fails, the bilinear canvas keeps the requested size
/// rather than being halved.
#[test]
fn lanczos_fallback_ignores_aa() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    let bilinear = RenderOptions {
        width: 70,
        height: 90,
        ..Default::default()
    };
    let opts = RenderOptions {
        aa: true,
        resampling: Resampling::Lanczos3,
        ..bilinear
    };
    // Room for the output, not for the native composite Lanczos-3 rescales.
    let limits = crate::resource_limits::ResourceLimits {
        max_render_pixels: Some(70 * 90),
        ..Default::default()
    };
    assert!(page.width() as u64 * page.height() as u64 > 70 * 90);

    let fallback = render_pixmap_with_limits(page, &opts, Some(limits)).unwrap();
    let expected = render_pixmap(page, &bilinear).unwrap();
    assert_eq!((fallback.width, fallback.height), (70, 90));
    assert!(fallback.data == expected.data);

    // `region` and the tiled display render take the same rule.
    assert!(!aa_halves(page, &opts));
    assert_eq!(unrotated_size(page, &opts), (70, 90));
}

/// Every option applies to `render_region` and to a progressive region
/// request: each returns the exact crop of the matching whole-page render.
/// Anti-aliasing crops the halved page (odd sizes too), Lanczos-3 the
/// rescaled page (which ignores `aa`, as `render_pixmap` does).
#[test]
fn render_region_matches_full_render_in_every_mode() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    let aa = |width, height| RenderOptions {
        width,
        height,
        aa: true,
        ..Default::default()
    };
    let modes = [
        aa(101, 81),
        aa(1, 1),
        RenderOptions {
            rotation: UserRotation::Cw90,
            ..aa(90, 120)
        },
        RenderOptions {
            resampling: Resampling::Lanczos3,
            ..aa(70, 90)
        },
        RenderOptions {
            resampling: Resampling::Lanczos3,
            aa: false,
            ..aa(70, 90)
        },
    ];
    let last = progressive_steps(page) - 1;
    assert!(last > 0, "chicken.djvu must have several BG44 chunks");
    for opts in &modes {
        let upright = RenderOptions {
            rotation: UserRotation::None,
            ..opts.clone()
        };
        let full = render_pixmap(page, &upright).unwrap();
        let first = render_progressive(page, &upright, 0).unwrap();
        for region in regions_around(full.width, full.height) {
            let rotation = opts.output_rotation(page);
            let expect = |pm: &Pixmap| {
                let crop = Pixmap {
                    width: region.width,
                    height: region.height,
                    data: reference_crop(pm, region),
                };
                rotate_pixmap(crop, rotation).data
            };
            let what = format!("{opts:?} at {region:?}");
            assert!(
                render_region(page, region, opts).unwrap().data == expect(&full),
                "render_region, {what}"
            );
            for (step, whole) in [(0, &first), (last, &full)] {
                let part = RenderRequest::new(upright.clone())
                    .region(region)
                    .quality(Quality::Step(step))
                    .pixmap(page)
                    .unwrap();
                assert!(
                    part.data == reference_crop(whole, region),
                    "step {step}, {what}"
                );
            }
        }
    }
}

/// A request region is the display-space crop of the whole-page request
/// in every mode (rotation, anti-aliasing, Lanczos-3), at every quality,
/// cached or not; and each whole-page request equals its legacy function.
#[test]
fn render_request_regions_crop_the_whole_page() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    let size = |width, height| RenderOptions {
        width,
        height,
        ..Default::default()
    };
    let modes = [
        size(91, 121),
        RenderOptions {
            aa: true,
            rotation: UserRotation::Cw90,
            ..size(91, 121)
        },
        RenderOptions {
            resampling: Resampling::Lanczos3,
            rotation: UserRotation::Rot180,
            ..size(70, 90)
        },
    ];
    for opts in &modes {
        for quality in [Quality::Full, Quality::Step(0), Quality::Coarse] {
            let request = RenderRequest::new(opts.clone()).quality(quality);
            let whole = request.pixmap(page).unwrap();
            let legacy = match quality {
                Quality::Full => render_pixmap(page, opts).unwrap(),
                Quality::Step(k) => render_progressive(page, opts, k).unwrap(),
                _ => render_coarse(page, opts).unwrap().unwrap(),
            };
            assert!(whole == legacy, "{opts:?} {quality:?}: legacy");
            for region in regions_around(whole.width, whole.height) {
                for cached in [false, true] {
                    let part = request
                        .clone()
                        .region(region)
                        .cached(cached)
                        .pixmap(page)
                        .unwrap();
                    assert!(
                        part.data == reference_crop(&whole, region),
                        "{opts:?} {quality:?} {region:?} cached={cached}"
                    );
                }
            }
        }
    }
}

/// The buffer and row outputs give the pixmap's bytes, for the whole page
/// and for a region; a streamed region must lie inside the page.
#[test]
fn render_request_streams_match_the_pixmap() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    let opts = RenderOptions {
        width: 91,
        height: 121,
        ..Default::default()
    };
    let region = RenderRect {
        x: 10,
        y: 20,
        width: 50,
        height: 40,
    };
    for quality in [Quality::Full, Quality::Step(1), Quality::Coarse] {
        for region in [None, Some(region)] {
            let mut request = RenderRequest::new(opts.clone()).quality(quality);
            if let Some(region) = region {
                request = request.region(region);
            }
            let pm = request.pixmap(page).unwrap();
            let mut buf = vec![0u8; pm.data.len()];
            request.write_rgba(page, &mut buf).unwrap();
            assert!(buf == pm.data, "{quality:?} {region:?}: buffer");
            let row_len = pm.width as usize * 4;
            let mut rows = Vec::new();
            request
                .rows(page, |y, row| {
                    assert_eq!(y * row_len, rows.len());
                    rows.extend_from_slice(row);
                })
                .unwrap();
            assert!(rows == pm.data, "{quality:?} {region:?}: rows");
        }
    }
    let past_edge = RenderRequest::new(opts.clone()).region(RenderRect {
        x: 80,
        y: 0,
        width: 20,
        height: 10,
    });
    assert!(matches!(
        past_edge.write_rgba(page, &mut [0u8; 800]),
        Err(RenderError::UnsupportedOption(_))
    ));
    let rotated = RenderRequest::new(RenderOptions {
        rotation: UserRotation::Cw90,
        ..opts
    });
    assert!(matches!(
        rotated.rows(page, |_, _| {}),
        Err(RenderError::UnsupportedOption(_))
    ));
}

/// A cancelled token stops every output with `Cancelled`; quality errors
/// name the problem.
#[test]
fn deprecated_progressive_step_matches_render_request() {
    let opts = RenderOptions {
        width: 60,
        height: 80,
        ..Default::default()
    };
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    for step in 0..progressive_steps(page) {
        let request = RenderRequest::new(opts.clone()).quality(Quality::Step(step));
        assert_eq!(
            render_progressive_step(page, &opts, step).unwrap().data,
            request.pixmap(page).unwrap().data,
            "step {step}"
        );
    }
    // A page without background chunks has one full frame, whatever step
    // the old entry point is asked for.
    let doc = load_doc("boy_jb2.djvu");
    let bilevel = doc.page(0).unwrap();
    let full = render_pixmap(bilevel, &opts).unwrap();
    assert_eq!(
        render_progressive_step(bilevel, &opts, 3).unwrap().data,
        full.data
    );
    assert_eq!(
        render_progressive_all(bilevel, &opts).unwrap()[0].data,
        full.data
    );
}

#[test]
fn render_request_cancel_and_quality_errors() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    let opts = RenderOptions {
        width: 60,
        height: 80,
        ..Default::default()
    };
    let token = CancelToken::new();
    let request = RenderRequest::new(opts.clone()).cancel(token.clone());
    assert!(request.pixmap(page).is_ok());
    token.cancel();
    let region = RenderRect {
        x: 0,
        y: 0,
        width: 10,
        height: 10,
    };
    let cancelled = |r: Result<(), RenderError>| matches!(r, Err(RenderError::Cancelled));
    assert!(cancelled(request.pixmap(page).map(drop)));
    for cached in [false, true] {
        let part = request.clone().region(region).cached(cached);
        assert!(cancelled(part.pixmap(page).map(drop)), "cached={cached}");
    }
    assert!(cancelled(
        request.write_rgba(page, &mut vec![0; 60 * 80 * 4])
    ));
    assert!(cancelled(request.rows(page, |_, _| {})));

    let steps = progressive_steps(page);
    let past_last = RenderRequest::new(opts.clone()).quality(Quality::Step(steps));
    assert!(matches!(
        past_last.pixmap(page),
        Err(RenderError::ChunkOutOfRange { chunk_n, max }) if chunk_n == steps && max == steps - 1
    ));

    let doc = load_doc("boy_jb2.djvu");
    let bilevel = doc.page(0).unwrap();
    let coarse = RenderRequest::new(opts.clone()).quality(Quality::Coarse);
    assert!(matches!(
        coarse.pixmap(bilevel),
        Err(RenderError::NoBackground)
    ));
    // A page without BG44 chunks has one frame: the full render.
    assert!(
        RenderRequest::new(opts.clone())
            .quality(Quality::Step(0))
            .pixmap(bilevel)
            .unwrap()
            == render_pixmap(bilevel, &opts).unwrap()
    );
}

/// Cached display-space regions of a rotated, anti-aliased page are the
/// matching crops of the whole render.
#[test]
fn display_region_tiled_crops_the_anti_aliased_page() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    let opts = RenderOptions {
        width: 91,
        height: 121,
        aa: true,
        rotation: UserRotation::Cw90,
        ..Default::default()
    };
    let upright = RenderOptions {
        rotation: UserRotation::None,
        ..opts.clone()
    };
    let full = render_pixmap(page, &opts).unwrap();
    assert_eq!((full.width, full.height), (60, 45));
    for region in regions_around(full.width, full.height) {
        let tiled = RenderRequest::new(opts.clone())
            .region(region)
            .cached(true)
            .pixmap(page)
            .unwrap();
        assert!(
            tiled.data == reference_crop(&full, region),
            "display region {region:?}"
        );
        assert!(
            render_region_tiled(page, region, &upright).unwrap().data
                == render_region(page, region, &upright).unwrap().data,
            "tiled region {region:?}"
        );
    }
}
