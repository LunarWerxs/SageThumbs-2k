use super::*;

// ── One render pipeline ──────────────────────────────────────────────────

/// Native-orientation options at `1/div` of the page size.
pub(super) fn opts_at(page: &DjVuPage, div: u32) -> RenderOptions {
    RenderOptions {
        width: (page.width() as u32 / div).max(1),
        height: (page.height() as u32 / div).max(1),
        ..Default::default()
    }
}

/// The `r` rectangle of `pm` (which must contain it), as RGBA rows.
pub(super) fn crop(pm: &Pixmap, r: RenderRect) -> Vec<u8> {
    let mut out = Vec::with_capacity(r.width as usize * r.height as usize * 4);
    for y in r.y..r.y + r.height {
        let start = (y as usize * pm.width as usize + r.x as usize) * 4;
        out.extend_from_slice(&pm.data[start..start + r.width as usize * 4]);
    }
    out
}

/// Mean absolute difference over the RGB channels of two same-sized pixmaps.
pub(super) fn mean_abs_diff(a: &Pixmap, b: &Pixmap) -> f64 {
    assert_eq!((a.width, a.height), (b.width, b.height));
    let (sum, n) = a
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .zip(b.data.as_chunks::<4>().0)
        .flat_map(|(p, q)| (0..3).map(move |c| p[c].abs_diff(q[c]) as u64))
        .fold((0u64, 0u64), |(s, n), d| (s + d, n + 1));
    sum as f64 / n as f64
}

/// Below a quarter of the native size a strict render reads only the
/// first BG44 chunk. A permissive render of an intact page reads the same
/// chunks, so both modes render alike and share cached tiles in either
/// order (the cache key has no permissive flag).
#[test]
fn permissive_matches_strict_below_quarter_scale() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    assert!(
        page.bg44_chunks().len() >= 2,
        "need a multi-chunk BG44 page"
    );
    let strict = opts_at(page, 5);
    let permissive = RenderOptions {
        permissive: true,
        ..strict.clone()
    };
    let want = render_pixmap(page, &strict).unwrap();
    assert!(render_pixmap(page, &permissive).unwrap().data == want.data);

    let whole = RenderRect {
        x: 0,
        y: 0,
        width: strict.width,
        height: strict.height,
    };
    // The permissive request fills the tile cache first.
    let first = render_region_tiled(page, whole, &permissive).unwrap();
    let second = render_region_tiled(page, whole, &strict).unwrap();
    assert!(first.data == want.data, "permissive tiles differ");
    assert!(second.data == want.data, "strict tiles differ");
}

/// A Lanczos-3 region is the matching crop of the Lanczos-3 page, and
/// white where it leaves the canvas.
#[test]
fn lanczos_region_is_a_crop_of_the_page() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    assert_eq!(page.rotation(), crate::info::Rotation::None);
    let opts = RenderOptions {
        resampling: Resampling::Lanczos3,
        ..opts_at(page, 3)
    };
    let full = render_pixmap(page, &opts).unwrap();
    let r = RenderRect {
        x: opts.width / 4,
        y: opts.height / 3,
        width: opts.width / 2,
        height: opts.height / 2,
    };
    assert!(render_region(page, r, &opts).unwrap().data == crop(&full, r));
    assert!(render_region_tiled(page, r, &opts).unwrap().data == crop(&full, r));

    let edge = RenderRect {
        x: opts.width - 4,
        y: 0,
        width: 8,
        height: 2,
    };
    let pm = render_region(page, edge, &opts).unwrap();
    let inside = RenderRect { width: 4, ..edge };
    for y in 0..2usize {
        let row = &pm.data[y * 32..(y + 1) * 32];
        assert!(row[..16] == crop(&full, inside)[y * 16..(y + 1) * 16]);
        assert!(
            row[16..].iter().all(|&b| b == 255),
            "outside the canvas is white"
        );
    }
}

/// A Lanczos-3 region filters only the part of the native page it reads,
/// yet stays byte-identical to the crop of the whole Lanczos-3 page:
/// downscaled and upscaled, colour and bilevel, at the corners, along the
/// edges and across tile seams, untiled and tiled.
#[test]
fn lanczos_window_matches_the_page_crop() {
    for name in ["chicken.djvu", "boy_jb2.djvu"] {
        let doc = load_doc(name);
        let page = doc.page(0).unwrap();
        assert_eq!(page.rotation(), crate::info::Rotation::None);
        let (pw, ph) = (page.width() as u32, page.height() as u32);
        for (w, h) in [(pw / 3, ph / 3), (pw * 2, ph * 2), (pw * 2 / 3 + 1, ph + 7)] {
            let opts = RenderOptions {
                width: w,
                height: h,
                resampling: Resampling::Lanczos3,
                ..Default::default()
            };
            let full = render_pixmap(page, &opts).unwrap();
            assert_eq!((full.width, full.height), (w, h));
            let seam = TILE_SIZE.min(w).min(h).saturating_sub(5);
            for (x, y, rw, rh) in [
                (0, 0, 1, 1),
                (w - 1, h - 1, 1, 1),
                (0, 0, w, 1),
                (w / 2, 0, 7, h),
                (seam, seam, 10, 10),
                (w / 5, h / 7, w / 2, h / 3),
            ] {
                let r = RenderRect {
                    x,
                    y,
                    width: rw.min(w - x),
                    height: rh.min(h - y),
                };
                let want = crop(&full, r);
                assert!(
                    render_region(page, r, &opts).unwrap().data == want,
                    "{name} {w}x{h} region {r:?}"
                );
                assert!(
                    render_region_tiled(page, r, &opts).unwrap().data == want,
                    "{name} {w}x{h} tiled region {r:?}"
                );
            }
        }
    }
}

/// Lanczos-3 turns an INFO-rotated page once, like bilinear, instead of
/// rotating its native-size canvas a second time.
#[test]
fn lanczos_turns_rotated_pages_once() {
    for name in [
        "boy_jb2_rotate90.djvu",
        "boy_jb2_rotate180.djvu",
        "boy_jb2_rotate270.djvu",
    ] {
        let doc = load_doc(name);
        let page = doc.page(0).unwrap();
        let bilinear = opts_at(page, 2);
        let lanczos = RenderOptions {
            resampling: Resampling::Lanczos3,
            ..bilinear.clone()
        };
        let want = render_pixmap(page, &bilinear).unwrap();
        let got = render_pixmap(page, &lanczos).unwrap();
        let mad = mean_abs_diff(&want, &got);
        assert!(mad < 4.0, "{name}: Lanczos differs from bilinear by {mad}");
        let last = render_progressive(page, &lanczos, progressive_steps(page) - 1).unwrap();
        assert!(last.data == got.data, "{name}: progressive Lanczos differs");
    }
}

/// `render_into` fills the buffer straight from the compositor, so it
/// refuses the whole-pixmap steps instead of skipping them.
#[test]
fn render_into_refuses_whole_pixmap_options() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    let base = opts_at(page, 2);
    let mut buf = vec![0u8; base.width as usize * base.height as usize * 4];
    for opts in [
        RenderOptions {
            aa: true,
            ..base.clone()
        },
        RenderOptions {
            resampling: Resampling::Lanczos3,
            ..base.clone()
        },
        RenderOptions {
            rotation: UserRotation::Cw90,
            ..base.clone()
        },
    ] {
        assert!(
            matches!(
                render_into(page, &opts, &mut buf),
                Err(RenderError::UnsupportedOption(_))
            ),
            "{opts:?}"
        );
    }

    // An INFO rotation needs a whole pixmap too; a user rotation that
    // cancels it does not.
    let doc = load_doc("boy_jb2_rotate90.djvu");
    let page = doc.page(0).unwrap();
    let opts = opts_at(page, 1);
    let mut buf = vec![0u8; opts.width as usize * opts.height as usize * 4];
    assert!(matches!(
        render_into(page, &opts, &mut buf),
        Err(RenderError::UnsupportedOption(_))
    ));
    let upright = [
        UserRotation::Cw90,
        UserRotation::Rot180,
        UserRotation::Ccw90,
    ]
    .into_iter()
    .map(|rotation| RenderOptions {
        rotation,
        ..opts.clone()
    })
    .find(|o| o.can_stream(page))
    .expect("one user rotation cancels the INFO rotation");
    render_into(page, &upright, &mut buf).unwrap();
    assert!(buf == render_pixmap(page, &upright).unwrap().data);
}

/// Anti-aliasing finishes every whole-page render: the progressive frames,
/// the streaming decoder's frames, and the coarse preview.
#[test]
fn anti_aliasing_finishes_every_whole_page_render() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    let plain = opts_at(page, 2);
    let aa = RenderOptions {
        aa: true,
        ..plain.clone()
    };
    let full = render_pixmap(page, &aa).unwrap();
    assert_eq!(
        (full.width, full.height),
        (plain.width / 2, plain.height / 2)
    );

    let steps = progressive_steps(page);
    assert!(render_progressive(page, &aa, steps - 1).unwrap().data == full.data);
    let mut dec = ProgressiveDecoder::new(page, &aa).unwrap();
    let mut last = None;
    for chunk in page.bg44_chunks() {
        last = Some(dec.push_bg44_chunk(chunk).unwrap());
    }
    assert!(last.unwrap().data == full.data);

    let coarse = render_coarse(page, &aa).unwrap().unwrap();
    let plain_coarse = render_coarse(page, &plain).unwrap().unwrap();
    assert!(coarse.data == aa_downscale(&plain_coarse).data);
}
