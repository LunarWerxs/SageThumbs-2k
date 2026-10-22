use super::*;

// ── Banded background (#811) ─────────────────────────────────────────────

/// Composite `page` at `opts` over `out` output rows starting at `offset`,
/// through `for_each_bg_band` with the given background, into a flat
/// buffer (`rows == false`) or through the row sink (`rows == true`).
pub(super) fn composite_with_bg(
    page: &DjVuPage,
    opts: &RenderOptions,
    bg: &Background,
    offset: (u32, u32),
    out: (u32, u32),
    rows: bool,
) -> Vec<u8> {
    let gamma_lut = build_gamma_lut(page.gamma());
    let DecodedLayers {
        bg: _,
        fg_palette,
        mask,
        blit_map,
        fg44,
    } = decode_layers(page, opts, 1, usize::MAX).unwrap();
    let stride = out.0 as usize * 4;
    let mut buf = vec![0u8; stride * out.1 as usize];
    for_each_bg_band(
        page,
        opts,
        bg,
        mask.as_deref(),
        0,
        fg_palette.as_ref(),
        blit_map.as_deref().map(Vec::as_slice),
        fg44.as_deref(),
        &gamma_lut,
        offset,
        out,
        |ctx, oy0| {
            if rows {
                composite_rows(ctx, |y, row| {
                    let at = (y + oy0 as usize) * stride;
                    buf[at..at + stride].copy_from_slice(row);
                })
            } else {
                composite_into(ctx, band_rows_mut(&mut buf, out.0, oy0, ctx.out_h))
            }
        },
    )
    .unwrap();
    buf
}

/// A background composited from bands of the wavelet image is
/// byte-identical to one composited from the whole RGB pixmap: at 1:1, on
/// an upscale, on a downscale, with a region offset, through the flat
/// buffer and through the row sink, with bands far smaller than the
/// production budget so every seam is exercised.
#[test]
fn banded_background_composites_like_the_whole_one() {
    // chicken: a small page, composited whole at every size. colorbook:
    // BG44 + JB2 mask, plane at page/3. history: plane at page/3 with a
    // ragged edge. carte: a wide page with a page/3 plane. The large pages
    // are composited as regions — the mapping is what matters, not the
    // area. (All four have colour backgrounds; banding is colour-only.)
    let subjects = [
        ("chicken.djvu", true),
        ("colorbook.djvu", false),
        ("history.djvu", false),
        ("carte.djvu", false),
    ];
    for (file, small) in subjects {
        let started = std::time::Instant::now();
        let doc = load_doc(file);
        let page = doc.page(0).unwrap();
        let img = page.decoded_bg44().expect("fixture has a BG44 background");
        assert!(
            img.rgb_band_rows().is_none(),
            "{file} is small: the production path must hold it whole"
        );
        let whole = Background::Whole(Arc::new(img.to_rgb_subsample(1).unwrap()));
        let (pw, ph) = (page.width() as u32, page.height() as u32);
        let sizes = [
            (pw, ph),
            (pw * 7 / 5, ph * 7 / 5),
            (pw * 5 / 7, ph * 5 / 7),
            (pw / 4, ph / 4),
        ];
        let band_sizes: &[u32] = if small { &[9, 37] } else { &[37, 300] };
        for (w, h) in sizes {
            let opts = RenderOptions {
                width: w,
                height: h,
                ..Default::default()
            };
            // The whole output, or regions: one off the top-left corner,
            // one at the bottom-right edge with a ragged height so the
            // last band is a partial one, and narrow ones in the middle,
            // whose bands decode only some columns of the plane.
            let cases: Vec<((u32, u32), (u32, u32))> = if small {
                vec![
                    ((0, 0), (w, h)),
                    // Saturating: at a quarter of the size the page
                    // is barely taller than this region's offset.
                    (
                        (13, 29),
                        (w.saturating_sub(40).max(1), h.saturating_sub(61).max(1)),
                    ),
                    ((w / 2 - 3, 7), (1, h - 7)),
                ]
            } else {
                let (rw, rh) = (200.min(w), 333.min(h));
                vec![
                    ((13, 29), (rw, rh)),
                    ((w - rw, h - rh), (rw, rh)),
                    ((w / 2 - 61, h / 3), (97, rh)),
                    ((w / 3 + 1, 0), (1, rh)),
                ]
            };
            for (offset, out) in cases {
                let expect = composite_with_bg(page, &opts, &whole, offset, out, false);
                for &band_rows in band_sizes {
                    let banded = Background::Banded {
                        image: img.clone(),
                        band_rows,
                    };
                    for rows in [false, true] {
                        let got = composite_with_bg(page, &opts, &banded, offset, out, rows);
                        assert!(
                            got == expect,
                            "{file} at {w}x{h}, offset {offset:?}, out {out:?}, \
                                 band_rows {band_rows}, rows={rows}: banded composite differs"
                        );
                    }
                }
            }
        }
        println!("{file}: checked in {:?}", started.elapsed());
    }
}

/// On a page whose background really is banded, a tile decodes only the
/// plane columns it reads. Each tile, through the tile cache and as a plain
/// region, must equal the same pixels of a strip of the whole page width,
/// at a downscale, at the page size and on an upscale, with bilinear and
/// with Lanczos-3.
#[test]
fn banded_tiles_match_a_full_width_strip() {
    let data = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/corpus/big_scanned_page.djvu"
    ))
    .expect("corpus page");
    let doc = DjVuDocument::parse(&data).unwrap();
    let page = doc.page(0).unwrap();
    let img = page.decoded_bg44().expect("a BG44 background");
    assert!(img.rgb_band_rows().is_some(), "the page must be banded");
    let (pw, ph) = (page.width() as u32, page.height() as u32);
    for (w, h) in [(pw / 4, ph / 4), (pw, ph), (pw * 2, ph * 2)] {
        for resampling in [Resampling::Bilinear, Resampling::Lanczos3] {
            let opts = RenderOptions {
                width: w,
                height: h,
                resampling,
                ..Default::default()
            };
            let y = h / 3;
            let strip = RenderRect {
                x: 0,
                y,
                width: w,
                height: TILE_SIZE,
            };
            let whole = render_region(page, strip, &opts).unwrap();
            let at = |x: u32, width: u32| RenderRect {
                x,
                y,
                width,
                height: TILE_SIZE,
            };
            for r in [
                at(0, TILE_SIZE),
                at(w / 2 - 77, 100),
                at(w - 1, 1),
                at(w - TILE_SIZE, TILE_SIZE),
            ] {
                let want = crop(&whole, RenderRect { y: 0, ..r });
                assert!(
                    render_region(page, r, &opts).unwrap().data == want,
                    "{w}x{h} {resampling:?} region {r:?}"
                );
                page.render_layers().clear_tile_cache();
                assert!(
                    render_region_tiled(page, r, &opts).unwrap().data == want,
                    "{w}x{h} {resampling:?} tiled region {r:?}"
                );
            }
        }
    }
}

/// `bg_rows_needed` returns the rows the samplers read, and they lie
/// inside the plane; bands from `bg_band_out_rows` respect the budget.
#[test]
fn bg_band_planning_stays_inside_the_plane_and_the_budget() {
    let page = (2260u32, 3669u32);
    let plane = (754u32, 1223u32);
    for full in [(2260, 3669), (3164, 5137), (1614, 2621), (753, 1223)] {
        let (lo, hi) = bg_rows_needed(page, full, plane, 0..full.1);
        assert_eq!(lo, 0);
        assert!(
            hi <= plane.1,
            "full render at {full:?} reads {hi} > {} rows",
            plane.1
        );
        if full == page {
            assert_eq!(hi, plane.1, "a 1:1 render reads every plane row");
        }
        let mut oy = 0;
        while oy < full.1 {
            let rows = bg_band_out_rows(page, full, plane, oy, full.1 - oy, 64);
            let (lo, hi) = bg_rows_needed(page, full, plane, oy..oy + rows);
            assert!(
                lo < hi && hi <= plane.1,
                "{full:?} band at {oy}: {lo}..{hi}"
            );
            assert!(
                hi - lo <= 64,
                "{full:?} band at {oy}: {lo}..{hi} exceeds 64 rows"
            );
            oy += rows;
        }
    }
    assert_eq!(bg_rows_needed(page, page, plane, 5..5), (0, 0));
}
