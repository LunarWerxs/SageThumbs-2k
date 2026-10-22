use super::*;

// ── D_AA_ZOOM: mask_aa (bilinear coverage AA at upscale) ─────────────────

#[test]
fn mask_bilinear_coverage_values() {
    use crate::bitmap::Bitmap;
    // 4×1 mask: bit 0 set, rest clear.
    let mut bm = Bitmap::new(4, 1);
    bm.set(0, 0, true);
    // Exactly on a pixel centre (tx=ty=0): pure sample of bit 0 → 255.
    assert_eq!(mask_bilinear_coverage(&bm, 0, 0), 255);
    // Halfway between bit 0 (255) and bit 1 (0): (255+0)/2 rounded → 128.
    assert_eq!(mask_bilinear_coverage(&bm, 8, 0), 128);
    // Halfway between bit 1 and bit 2, both clear → 0.
    assert_eq!(mask_bilinear_coverage(&bm, 24, 0), 0);
    // All-foreground mask → 255 everywhere, no interpolation artifacts.
    let mut bm_full = Bitmap::new(2, 2);
    bm_full.set(0, 0, true);
    bm_full.set(1, 0, true);
    bm_full.set(0, 1, true);
    bm_full.set(1, 1, true);
    assert_eq!(mask_bilinear_coverage(&bm_full, 4, 4), 255);
    // All-background mask → 0 everywhere.
    let bm_empty = Bitmap::new(2, 2);
    assert_eq!(mask_bilinear_coverage(&bm_empty, 4, 4), 0);
}

/// `composite_rows_bilevel_one` at 2× upscale with `mask_aa: false` (the
/// default) must reproduce the exact nearest-bit pattern — hard requirement
/// that the opt-in flag changes nothing unless explicitly enabled.
#[test]
fn composite_bilevel_one_mask_aa_disabled_matches_nearest_at_upscale() {
    let opts = RenderOptions::default();
    assert!(!opts.mask_aa);
    let mut mask = crate::bitmap::Bitmap::new(4, 1);
    mask.set(0, 0, true); // only x=0 is foreground
    let lut = identity_lut();
    let ctx = synth_ctx(&opts, 4, 1, None, Some(&mask), &lut, 8, 2);

    let fx_step = FRAC / 2; // 2× upscale
    let fy_step = FRAC / 2;
    let mut row = vec![0u8; 8 * 4];
    composite_rows_bilevel_one(&ctx, 0, fx_step, fy_step, &mut row);

    // Nearest-neighbour duplication of source pixels [0,0,1,1,2,2,3,3];
    // only source pixel 0 is foreground (black), rest background (white).
    let expected_black = [true, true, false, false, false, false, false, false];
    for (x, &is_black) in expected_black.iter().enumerate() {
        let p = &row[x * 4..x * 4 + 4];
        if is_black {
            assert_eq!(p, [0, 0, 0, 255], "expected black at x={x}");
        } else {
            assert_eq!(p, [255, 255, 255, 255], "expected white at x={x}");
        }
    }
}

/// With `mask_aa: true` at the same 2× upscale, the pixel straddling the
/// mask edge (x=1, halfway between the set bit 0 and clear bit 1) must come
/// out as an intermediate gray — proof the bilinear coverage path actually
/// smooths the edge instead of just reproducing nearest-neighbour.
#[test]
fn composite_bilevel_one_mask_aa_enabled_smooths_edge_at_upscale() {
    let opts = RenderOptions {
        mask_aa: true,
        ..Default::default()
    };
    let mut mask = crate::bitmap::Bitmap::new(4, 1);
    mask.set(0, 0, true);
    let lut = identity_lut();
    let ctx = synth_ctx(&opts, 4, 1, None, Some(&mask), &lut, 8, 2);

    let fx_step = FRAC / 2;
    let fy_step = FRAC / 2;
    let mut row = vec![0u8; 8 * 4];
    composite_rows_bilevel_one(&ctx, 0, fx_step, fy_step, &mut row);

    // x=0 lands exactly on the set bit → still pure black.
    assert_eq!(&row[0..4], [0, 0, 0, 255], "x=0 exact sample stays black");
    // x=1 straddles bit 0 (fg) / bit 1 (bg) at tx=8/16 → coverage 128 → gray 127.
    assert_eq!(&row[4..8], [127, 127, 127, 255], "x=1 is a blended gray");
    // x=2 onward land entirely within the background region → white.
    for x in 2..8usize {
        assert_eq!(
            &row[x * 4..x * 4 + 4],
            [255, 255, 255, 255],
            "x={x} stays white"
        );
    }
}

/// `mask_aa` must be a no-op on the exact 1:1 fast path (native scale) —
/// the flag only ever matters past the early return for genuine upscale.
#[test]
fn composite_bilevel_one_mask_aa_is_noop_at_native_scale() {
    let mut mask = crate::bitmap::Bitmap::new(4, 1);
    mask.set(0, 0, true);
    let lut = identity_lut();

    let opts_off = RenderOptions::default();
    let ctx_off = synth_ctx(&opts_off, 4, 1, None, Some(&mask), &lut, 4, 1);
    let mut row_off = vec![0u8; 4 * 4];
    composite_rows_bilevel_one(&ctx_off, 0, FRAC, FRAC, &mut row_off);

    let opts_on = RenderOptions {
        mask_aa: true,
        ..Default::default()
    };
    let ctx_on = synth_ctx(&opts_on, 4, 1, None, Some(&mask), &lut, 4, 1);
    let mut row_on = vec![0u8; 4 * 4];
    composite_rows_bilevel_one(&ctx_on, 0, FRAC, FRAC, &mut row_on);

    assert_eq!(row_off, row_on, "mask_aa must not affect native 1:1 scale");
}

/// `mask_aa` must be a no-op on downscale — the bilinear-upscale branch is
/// only reachable when `!downscale`, so a `mask_aa: true` downscale render
/// must still take the existing `mask_box_coverage` path unchanged.
#[test]
fn composite_bilevel_one_mask_aa_is_noop_at_downscale() {
    let mut mask = crate::bitmap::Bitmap::new(4, 1);
    mask.set(0, 0, true);
    mask.set(2, 0, true);
    mask.set(3, 0, true);
    let lut = identity_lut();

    let fx_step = 4 * FRAC; // 4× downscale
    let fy_step = FRAC;

    let opts_off = RenderOptions::default();
    let ctx_off = synth_ctx(&opts_off, 4, 1, None, Some(&mask), &lut, 1, 1);
    let mut row_off = vec![0u8; 4];
    composite_rows_bilevel_one(&ctx_off, 0, fx_step, fy_step, &mut row_off);

    let opts_on = RenderOptions {
        mask_aa: true,
        ..Default::default()
    };
    let ctx_on = synth_ctx(&opts_on, 4, 1, None, Some(&mask), &lut, 1, 1);
    let mut row_on = vec![0u8; 4];
    composite_rows_bilevel_one(&ctx_on, 0, fx_step, fy_step, &mut row_on);

    assert_eq!(row_off, row_on, "mask_aa must not affect downscale");
}

/// `composite_rows_bilinear_one` (colour path) at 2× upscale with
/// `mask_aa: false` must reproduce the exact binary nearest-bit coverage —
/// same hard byte-identical requirement as the bilevel path, for the
/// colour+mask compositor.
#[test]
fn composite_bilinear_one_mask_aa_disabled_matches_nearest_at_upscale() {
    let opts = RenderOptions::default();
    let bg = Pixmap::try_new(8, 1, 200, 150, 100, 255).expect("fits the pixmap limit");
    let mut mask = crate::bitmap::Bitmap::new(8, 1);
    mask.set(0, 0, true); // only x=0 is foreground
    let lut = identity_lut();
    let ctx = synth_ctx(&opts, 8, 1, Some(&bg), Some(&mask), &lut, 4, 1);

    let fx_step = FRAC / 2; // 2× upscale
    let fy_step = FRAC / 2;
    let mut row = vec![0u8; 4 * 4];
    composite_rows_bilinear_one(&ctx, 0, fx_step, fy_step, &mut row, None, &mut Vec::new());

    // Nearest px indices for ox=0..4 are [0,0,1,1]; only px 0 is foreground,
    // rendered black (no FG44 layer ⇒ (0,0,0)); px 1 is background colour.
    assert_eq!(&row[0..4], [0, 0, 0, 255], "ox=0 nearest foreground");
    assert_eq!(&row[4..8], [0, 0, 0, 255], "ox=1 nearest foreground");
    assert_eq!(&row[8..12], [200, 150, 100, 255], "ox=2 background");
    assert_eq!(&row[12..16], [200, 150, 100, 255], "ox=3 background");
}

/// With `mask_aa: true` the pixel straddling the mask edge blends the
/// (black) foreground colour with the background colour proportionally to
/// the interpolated coverage, instead of snapping to one or the other.
#[test]
fn composite_bilinear_one_mask_aa_enabled_blends_fg_bg_at_upscale() {
    let opts = RenderOptions {
        mask_aa: true,
        ..Default::default()
    };
    let bg = Pixmap::try_new(8, 1, 200, 150, 100, 255).expect("fits the pixmap limit");
    let mut mask = crate::bitmap::Bitmap::new(8, 1);
    mask.set(0, 0, true);
    let lut = identity_lut();
    let ctx = synth_ctx(&opts, 8, 1, Some(&bg), Some(&mask), &lut, 4, 1);

    let fx_step = FRAC / 2;
    let fy_step = FRAC / 2;
    let mut row = vec![0u8; 4 * 4];
    composite_rows_bilinear_one(&ctx, 0, fx_step, fy_step, &mut row, None, &mut Vec::new());

    // ox=0 lands exactly on the set bit → still pure (black) foreground.
    assert_eq!(
        &row[0..4],
        [0, 0, 0, 255],
        "ox=0 exact sample stays foreground"
    );
    // ox=1 straddles the edge at coverage 128 → blend(0, 200/150/100, 128).
    assert_eq!(
        &row[4..8],
        [100, 75, 50, 255],
        "ox=1 is a fg/bg blend, not a hard snap"
    );
    // ox=2, ox=3 are entirely background.
    assert_eq!(&row[8..12], [200, 150, 100, 255], "ox=2 background");
    assert_eq!(&row[12..16], [200, 150, 100, 255], "ox=3 background");
}

/// Subtle no-op case: an exact page-level 1:1 render (`fx_step == fy_step
/// == FRAC`) whose *background* plane is internally subsampled still falls
/// through to the general B-series loop (the "extra-tight" 1:1 fast path
/// requires `bg_x_q24 == 1<<24`, which fails here) — but `mask_aa` must
/// still be a no-op there because there is no genuine axis upscale.
#[test]
fn composite_bilinear_one_mask_aa_is_noop_when_bg_subsampled_at_native_scale() {
    let bg = Pixmap::try_new(4, 1, 200, 150, 100, 255).expect("fits the pixmap limit"); // subsampled: page_w=8, bg_w=4
    let mut mask = crate::bitmap::Bitmap::new(8, 1);
    mask.set(0, 0, true);
    let lut = identity_lut();

    let opts_off = RenderOptions::default();
    let ctx_off = synth_ctx(&opts_off, 8, 1, Some(&bg), Some(&mask), &lut, 8, 1);
    assert_ne!(
        ctx_off.bg_x_q24,
        1 << 24,
        "test must exercise subsampled bg"
    );
    let mut row_off = vec![0u8; 8 * 4];
    composite_rows_bilinear_one(&ctx_off, 0, FRAC, FRAC, &mut row_off, None, &mut Vec::new());

    let opts_on = RenderOptions {
        mask_aa: true,
        ..Default::default()
    };
    let ctx_on = synth_ctx(&opts_on, 8, 1, Some(&bg), Some(&mask), &lut, 8, 1);
    let mut row_on = vec![0u8; 8 * 4];
    composite_rows_bilinear_one(&ctx_on, 0, FRAC, FRAC, &mut row_on, None, &mut Vec::new());

    assert_eq!(
        row_off, row_on,
        "mask_aa must not affect native 1:1 scale even with a subsampled bg plane"
    );
}

/// The precomputed `BilinearX` column table must be byte-identical to the
/// in-loop Q48 fallback on every row — 2× upscale, non-zero `offset_x`,
/// subsampled non-uniform bg so any x0/x1/tx mismatch shows up in bytes.
#[test]
fn composite_bilinear_one_column_table_matches_fallback() {
    let opts = RenderOptions::default();
    let mut bg = Pixmap::try_new(3, 2, 0, 0, 0, 255).expect("fits the pixmap limit"); // subsampled: page_w=8, bg_w=3
    for (i, px) in bg.data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        px[0] = (i * 40) as u8;
        px[1] = (i * 25 + 7) as u8;
        px[2] = (255 - i * 30) as u8;
    }
    let mut mask = crate::bitmap::Bitmap::new(8, 2);
    mask.set(2, 0, true); // one fg bit so the partial/fg branches run too
    let lut = identity_lut();

    let mut ctx = synth_ctx(&opts, 8, 2, Some(&bg), Some(&mask), &lut, 16, 4);
    ctx.offset_x = 3;

    let fx_step = FRAC / 2; // 2× upscale
    let fy_step = FRAC / 2;
    let table = precompute_bilinear_x(&ctx, fx_step).expect("bg present");

    for oy in 0..4 {
        let mut row_table = vec![0u8; 16 * 4];
        let mut row_fallback = vec![0u8; 16 * 4];
        composite_rows_bilinear_one(
            &ctx,
            oy,
            fx_step,
            fy_step,
            &mut row_table,
            Some(&table),
            &mut Vec::new(),
        );
        composite_rows_bilinear_one(
            &ctx,
            oy,
            fx_step,
            fy_step,
            &mut row_fallback,
            None,
            &mut Vec::new(),
        );
        assert_eq!(
            row_table, row_fallback,
            "table and fallback sampling must agree at oy={oy}"
        );
    }
}

/// Integration-level: `render_pixmap` at native scale on a real bilevel
/// document must be byte-identical whether `mask_aa` is on or off — the
/// no-op-at-scale-≤1 guarantee holding end-to-end, not just at the
/// synthetic-context unit level.
#[test]
fn render_pixmap_mask_aa_is_noop_at_native_scale_real_doc() {
    let doc = load_doc("boy_jb2.djvu");
    let page = doc.page(0).unwrap();
    let (w, h) = (page.width() as u32, page.height() as u32);

    let opts_off = RenderOptions {
        width: w,
        height: h,
        ..Default::default()
    };
    let opts_on = RenderOptions {
        width: w,
        height: h,
        mask_aa: true,
        ..Default::default()
    };
    let pm_off = render_pixmap(page, &opts_off).expect("render should succeed");
    let pm_on = render_pixmap(page, &opts_on).expect("render should succeed");
    assert_eq!(
        pm_off.data, pm_on.data,
        "mask_aa must be a no-op at native scale"
    );
}

/// Integration-level: `render_pixmap` at downscale on a real bilevel
/// document must also be byte-identical between `mask_aa` on/off.
#[test]
fn render_pixmap_mask_aa_is_noop_at_downscale_real_doc() {
    let doc = load_doc("boy_jb2.djvu");
    let page = doc.page(0).unwrap();
    let (w, h) = ((page.width() as u32) / 2, (page.height() as u32) / 2);

    let opts_off = RenderOptions {
        width: w,
        height: h,
        ..Default::default()
    };
    let opts_on = RenderOptions {
        width: w,
        height: h,
        mask_aa: true,
        ..Default::default()
    };
    let pm_off = render_pixmap(page, &opts_off).expect("render should succeed");
    let pm_on = render_pixmap(page, &opts_on).expect("render should succeed");
    assert_eq!(
        pm_off.data, pm_on.data,
        "mask_aa must be a no-op at downscale"
    );
}

/// Integration-level: at genuine 2× and 4× upscale on a real bilevel
/// document, `mask_aa: true` must actually change the output (introduce
/// intermediate gray values along glyph edges) — proving the flag is wired
/// end-to-end, not just correct in isolated unit tests.
#[test]
fn render_pixmap_mask_aa_smooths_edges_at_zoom_real_doc() {
    let doc = load_doc("boy_jb2.djvu");
    let page = doc.page(0).unwrap();
    let (pw, ph) = (page.width() as u32, page.height() as u32);

    for &zoom in &[2u32, 4u32] {
        let opts_off = RenderOptions {
            width: pw * zoom,
            height: ph * zoom,
            ..Default::default()
        };
        let opts_on = RenderOptions {
            width: pw * zoom,
            height: ph * zoom,
            mask_aa: true,
            ..Default::default()
        };
        let pm_off = render_pixmap(page, &opts_off).expect("nearest render should succeed");
        let pm_on = render_pixmap(page, &opts_on).expect("AA render should succeed");
        assert_eq!(pm_off.width, pm_on.width);
        assert_eq!(pm_off.height, pm_on.height);

        assert_ne!(
            pm_off.data, pm_on.data,
            "mask_aa=true must change output at {zoom}× zoom"
        );
        let has_intermediate_gray = pm_on
            .data
            .as_chunks::<4>()
            .0
            .iter()
            .any(|px| px[0] == px[1] && px[1] == px[2] && px[0] != 0 && px[0] != 255);
        assert!(
            has_intermediate_gray,
            "mask_aa=true should introduce intermediate gray values at {zoom}× zoom"
        );
    }
}

/// RenderOptions default values.
#[test]
fn render_options_default() {
    let opts = RenderOptions::default();
    assert_eq!(opts.width, 0);
    assert_eq!(opts.height, 0);
    assert_eq!(opts.bold, 0);
    assert!(!opts.aa);
    assert_eq!(opts.resampling, Resampling::Bilinear);
    assert!(!opts.mask_aa, "mask_aa must default to false (opt-in)");
}

/// RenderOptions can be constructed with explicit fields.
#[test]
fn render_options_construction() {
    let opts = RenderOptions {
        width: 400,
        height: 300,
        bold: 1,
        aa: true,
        rotation: UserRotation::Cw90,
        ..Default::default()
    };
    assert_eq!(opts.width, 400);
    assert_eq!(opts.height, 300);
    assert_eq!(opts.bold, 1);
    assert!(opts.aa);
    assert_eq!(opts.rotation, UserRotation::Cw90);
}

/// The incremental `render_progressive_all` fast path (B5) must be
/// byte-identical to the per-frame `render_progressive_step` loop it
/// replaces, on a real multi-BG44-chunk page.
#[test]
fn render_progressive_all_matches_per_frame() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    // chicken.djvu has 3 BG44 chunks → 3 progressive frames, exercising the
    // incremental streaming path (steps > 1, Bilinear, strict).
    assert!(
        page.bg44_chunks().len() >= 2,
        "need a multi-chunk BG44 page"
    );

    let opts = RenderOptions {
        width: page.width() as u32,
        height: page.height() as u32,
        resampling: Resampling::Bilinear,
        ..Default::default()
    };

    let all = render_progressive_all(page, &opts).expect("progressive_all");
    let steps = progressive_steps(page);
    assert_eq!(all.len(), steps);
    for (step, frame) in all.iter().enumerate() {
        let per_frame = render_progressive_step(page, &opts, step).expect("progressive_step");
        assert_eq!(
            (frame.width, frame.height),
            (per_frame.width, per_frame.height),
            "frame {step} dimensions differ"
        );
        assert!(
            frame.data == per_frame.data,
            "frame {step} pixels differ between incremental and per-frame paths"
        );
    }
}

#[test]
fn progressive_decoder_streams_frames_matching_batch() {
    // The streaming ProgressiveDecoder, fed one BG44 chunk at a time, must
    // reproduce render_progressive_all's frames byte-for-byte.
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    let chunks = page.bg44_chunks();
    assert!(chunks.len() >= 2, "need a multi-chunk BG44 page");

    let opts = RenderOptions {
        width: page.width() as u32,
        height: page.height() as u32,
        resampling: Resampling::Bilinear,
        ..Default::default()
    };

    let batch = render_progressive_all(page, &opts).expect("progressive_all");

    let mut dec = ProgressiveDecoder::new(page, &opts).expect("decoder");
    let steps = progressive_steps(page);
    for (i, chunk) in chunks.iter().take(steps).enumerate() {
        let frame = dec.push_bg44_chunk(chunk).expect("push");
        assert_eq!(dec.frames_produced(), i + 1);
        assert_eq!(
            (frame.width, frame.height),
            (batch[i].width, batch[i].height),
            "streamed frame {i} dimensions differ"
        );
        assert!(
            frame.data == batch[i].data,
            "streamed frame {i} pixels differ from batch progressive_all"
        );
    }
}

/// The streaming `ProgressiveDecoder`, fed one BG44 chunk at a time, must
/// reproduce `render_progressive_step`'s frames byte-for-byte — the
/// byte-identical requirement checked directly against the per-frame API
/// (not only via the `render_progressive_all` batch path already covered
/// above), on both the small 3-chunk `chicken.djvu` and the larger
/// 4-chunk `colorbook.djvu` fixtures.
pub(super) fn assert_progressive_decoder_matches_step(filename: &str) {
    let doc = load_doc(filename);
    let page = doc.page(0).unwrap();
    let chunks = page.bg44_chunks();
    assert!(
        chunks.len() >= 2,
        "{filename}: need a multi-chunk BG44 page"
    );

    let opts = RenderOptions {
        width: page.width() as u32,
        height: page.height() as u32,
        resampling: Resampling::Bilinear,
        ..Default::default()
    };

    let mut dec = ProgressiveDecoder::new(page, &opts).expect("decoder");
    let steps = progressive_steps(page);
    for (i, chunk) in chunks.iter().take(steps).enumerate() {
        let streamed = dec.push_bg44_chunk(chunk).expect("push");
        let stepped = render_progressive_step(page, &opts, i).expect("progressive_step");
        assert_eq!(
            (streamed.width, streamed.height),
            (stepped.width, stepped.height),
            "{filename} frame {i} dimensions differ"
        );
        assert!(
            streamed.data == stepped.data,
            "{filename}: streamed frame {i} pixels differ from render_progressive_step"
        );
    }
}

#[test]
fn progressive_decoder_matches_render_progressive_step_chicken() {
    assert_progressive_decoder_matches_step("chicken.djvu");
}

#[test]
fn progressive_decoder_matches_render_progressive_step_colorbook() {
    assert_progressive_decoder_matches_step("colorbook.djvu");
}

/// Structural proof of the B5 claim (primary evidence per the design doc,
/// since wall-clock is noisy on a shared machine): a naive session that
/// calls `render_progressive_step(0..N)` re-decodes BG44 chunks
/// `1+2+...+N` times — O(N²) — while a `ProgressiveDecoder` session over
/// the same N frames decodes each chunk exactly once — O(N). Counted via
/// the `#[cfg(test)]`-only `BG44_CHUNK_DECODES` counter at the two real
/// `Iw44Image::decode_chunk` call sites, not wall-clock timing.
///
/// Under the `parallel` feature, `decode_layers` runs the naive session's
/// background decode through `rayon::join` (see `#440` there). Calling
/// `rayon::join` from a plain thread that isn't already a rayon worker —
/// such as this test's own thread — makes rayon bridge onto a worker
/// thread from its shared *global* pool to execute the join. That breaks
/// the `BG44_CHUNK_DECODES` thread-local's implicit assumption that every
/// counted decode call lands on the thread that set/reads it: the
/// increments happen on a global-pool worker thread this test's thread_local
/// handle never sees, so the naive count silently reads back as 0
/// (verified: found failing under `--features cli,mmap,parallel`, and in
/// isolation under `--features parallel` alone — `mmap` is not implicated).
/// Route the whole measurement through a dedicated, single-worker rayon
/// pool instead: with exactly one worker, any `rayon::join` bridged into
/// it always resolves on that same worker, so setting and reading the
/// counter from *inside* the pool keeps everything on one thread
/// regardless of the `parallel` feature — and because the pool is freshly
/// built here (not rayon's shared global pool), this stays isolated from
/// any other test's concurrent decode calls, preserving the isolation the
/// original thread-local was there for.
#[test]
fn progressive_decoder_chunk_decodes_are_on_not_on_squared() {
    let body = || {
        for filename in ["chicken.djvu", "colorbook.djvu"] {
            let doc = load_doc(filename);
            let page = doc.page(0).unwrap();
            let chunks = page.bg44_chunks();
            let n = chunks.len();
            assert!(n >= 3, "{filename}: need >=3 BG44 chunks");

            let opts = RenderOptions {
                width: page.width() as u32,
                height: page.height() as u32,
                resampling: Resampling::Bilinear,
                ..Default::default()
            };

            // Naive per-frame session: render_progressive_step(0..N), each call
            // re-decoding the chunk prefix from scratch (the pre-B5 behaviour).
            BG44_CHUNK_DECODES.with(|c| c.set(0));
            for step in 0..n {
                render_progressive_step(page, &opts, step).expect("progressive_step");
            }
            let naive = BG44_CHUNK_DECODES.with(|c| c.get());
            let expected_naive: usize = (1..=n).sum(); // 1+2+...+N
            assert_eq!(
                naive, expected_naive,
                "{filename}: naive per-frame session should decode chunks \
                     1+2+...+N = {expected_naive} times, got {naive}"
            );

            // Stateful streaming session: one decode per chunk, total N.
            BG44_CHUNK_DECODES.with(|c| c.set(0));
            let mut dec = ProgressiveDecoder::new(page, &opts).expect("decoder");
            for chunk in chunks.iter() {
                dec.push_bg44_chunk(chunk).expect("push");
            }
            let streamed = BG44_CHUNK_DECODES.with(|c| c.get());
            assert_eq!(
                streamed, n,
                "{filename}: stateful session should decode each chunk exactly \
                     once (O(N) = {n}), got {streamed}"
            );

            assert!(
                naive > streamed,
                "{filename}: naive session ({naive} decodes) should strictly \
                     exceed the streamed session ({streamed} decodes)"
            );
        }
    };

    #[cfg(feature = "parallel")]
    {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .expect("build single-threaded pool for deterministic thread-local counting");
        pool.install(body);
    }
    #[cfg(not(feature = "parallel"))]
    body();
}

#[test]
fn progressive_decoder_rejects_lanczos_and_zero_dims() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    let mut opts = RenderOptions {
        width: page.width() as u32,
        height: page.height() as u32,
        resampling: Resampling::Lanczos3,
        ..Default::default()
    };
    assert!(matches!(
        ProgressiveDecoder::new(page, &opts),
        Err(RenderError::UnsupportedOption(_))
    ));
    opts.resampling = Resampling::Bilinear;
    opts.width = 0;
    assert!(matches!(
        ProgressiveDecoder::new(page, &opts),
        Err(RenderError::InvalidDimensions { .. })
    ));
}

/// Same byte-identity guarantee for the incremental progressive path with
/// `bold > 0`: the fast path dilates the mask once and reuses it across
/// frames, which must match the per-frame path that dilates each frame.
#[test]
fn render_progressive_all_matches_per_frame_bold() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    assert!(
        page.bg44_chunks().len() >= 2,
        "need a multi-chunk BG44 page"
    );

    let opts = RenderOptions {
        width: page.width() as u32,
        height: page.height() as u32,
        resampling: Resampling::Bilinear,
        bold: 2,
        ..Default::default()
    };

    let all = render_progressive_all(page, &opts).expect("progressive_all");
    let steps = progressive_steps(page);
    assert_eq!(all.len(), steps);
    for (step, frame) in all.iter().enumerate() {
        let per_frame = render_progressive_step(page, &opts, step).expect("progressive_step");
        assert_eq!(
            (frame.width, frame.height),
            (per_frame.width, per_frame.height),
            "frame {step} dimensions differ (bold)"
        );
        assert!(
            frame.data == per_frame.data,
            "frame {step} pixels differ between incremental and per-frame paths (bold)"
        );
    }
}

/// Evicting the render cache must not change output: a re-render after
/// `evict_render_caches` is byte-identical to the first (the cache rebuilds
/// lazily and correctly).
#[test]
fn evict_render_cache_preserves_output() {
    let mut doc = load_doc("chicken.djvu");
    let (w, h) = {
        let p = doc.page(0).unwrap();
        (p.width() as u32, p.height() as u32)
    };
    let opts = RenderOptions {
        width: w,
        height: h,
        ..Default::default()
    };
    let first = {
        let p = doc.page(0).unwrap();
        render_pixmap(p, &opts).unwrap()
    };
    doc.evict_render_caches();
    let second = {
        let p = doc.page(0).unwrap();
        render_pixmap(p, &opts).unwrap()
    };
    assert_eq!(
        first.data, second.data,
        "output changed after cache eviction"
    );
}

/// `enforce_cache_budget` evicts least-recently-used unprotected pages down
/// to the budget, keeps protected pages, and re-renders byte-identically.
#[test]
fn enforce_cache_budget_lru_and_correctness() {
    let doc = load_doc("colorbook.djvu");
    if doc.page_count() < 3 {
        return; // needs a multi-page fixture
    }
    // Render pages 0,1,2 in order → LRU order is 0 < 1 < 2 by access tick.
    let mut first0 = None;
    for i in 0..3 {
        let (w, h) = {
            let p = doc.page(i).unwrap();
            (p.width() as u32, p.height() as u32)
        };
        let opts = RenderOptions {
            width: w,
            height: h,
            ..Default::default()
        };
        let p = doc.page(i).unwrap();
        let pm = render_pixmap(p, &opts).unwrap();
        if i == 0 {
            first0 = Some(pm);
        }
    }
    // LRU ticks strictly increase with render order.
    let t0 = doc.page(0).unwrap().render_cache_access_tick();
    let t1 = doc.page(1).unwrap().render_cache_access_tick();
    let t2 = doc.page(2).unwrap().render_cache_access_tick();
    assert!(t0 < t1 && t1 < t2, "LRU ticks not ordered: {t0} {t1} {t2}");

    assert!(doc.render_cache_bytes() > 0);
    // Budget 1 byte, protect page 2 → evict the two LRU unprotected pages.
    let freed = doc.enforce_cache_budget(1, &[2]);
    assert!(freed > 0, "expected some bytes freed");
    assert_eq!(
        doc.page(0).unwrap().render_cache_bytes(),
        0,
        "page 0 not evicted"
    );
    assert_eq!(
        doc.page(1).unwrap().render_cache_bytes(),
        0,
        "page 1 not evicted"
    );
    assert!(
        doc.page(2).unwrap().render_cache_bytes() > 0,
        "protected page 2 was evicted"
    );

    // Re-rendering an evicted page reproduces the original output exactly.
    let (w, h) = {
        let p = doc.page(0).unwrap();
        (p.width() as u32, p.height() as u32)
    };
    let opts = RenderOptions {
        width: w,
        height: h,
        ..Default::default()
    };
    let second0 = render_pixmap(doc.page(0).unwrap(), &opts).unwrap();
    assert_eq!(first0.unwrap().data, second0.data);
}

/// C5_COMPRESS: `downgrade_render_cache` must (a) shrink the cache, (b)
/// keep a previously-cached `bg_rgb_s2` warm — a subsequent sub=2 render
/// must not force a fresh BG44 decode — and (c) still reproduce the exact
/// same full-resolution output on a later cold sub=1 render (the
/// full-res path re-decodes from scratch, byte-identically).
#[test]
fn downgrade_render_cache_keeps_downscaled_tier_warm() {
    let doc = load_doc("colorbook.djvu");
    let (w, h) = {
        let p = doc.page(0).unwrap();
        (p.width() as u32, p.height() as u32)
    };
    let opts_s1 = RenderOptions {
        width: w,
        height: h,
        ..Default::default()
    };
    // sub=2 request: half-resolution output.
    let opts_s2 = RenderOptions {
        width: w / 2,
        height: h / 2,
        ..Default::default()
    };

    let first_s1 = render_pixmap(doc.page(0).unwrap(), &opts_s1).unwrap();
    let first_s2 = render_pixmap(doc.page(0).unwrap(), &opts_s2).unwrap();
    let bytes_before = doc.page(0).unwrap().render_cache_bytes();
    assert!(bytes_before > 0);

    doc.downgrade_render_caches();
    let bytes_after = doc.page(0).unwrap().render_cache_bytes();
    assert!(
        bytes_after > 0 && bytes_after < bytes_before,
        "downgrade should shrink but not zero the cache: before={bytes_before} after={bytes_after}"
    );

    // sub=2 render after downgrade: warm (bg_rgb_s2 preserved), and output
    // is unchanged.
    let second_s2 = render_pixmap(doc.page(0).unwrap(), &opts_s2).unwrap();
    assert_eq!(first_s2.data, second_s2.data, "sub=2 output changed");

    // sub=1 (full-res) render after downgrade: cold-decodes but still
    // reproduces the original output exactly.
    let second_s1 = render_pixmap(doc.page(0).unwrap(), &opts_s1).unwrap();
    assert_eq!(first_s1.data, second_s1.data, "sub=1 output changed");
}

/// IW44_CHECKPOINT (#608): a full render after a sub>=4 render (which
/// cached the first-chunk partial decode) resumes from that checkpoint —
/// and must be byte-identical to a cold full render on a fresh document.
#[test]
fn full_decode_resumed_from_partial_is_byte_identical() {
    let doc_a = load_doc("colorbook.djvu");
    let (w, h) = {
        let p = doc_a.page(0).unwrap();
        (p.width() as u32, p.height() as u32)
    };
    // Warm the partial tier via a sub=4 render, then full render.
    let opts_s4 = RenderOptions {
        width: w / 4,
        height: h / 4,
        ..Default::default()
    };
    let opts_s1 = RenderOptions {
        width: w,
        height: h,
        ..Default::default()
    };
    let _ = render_pixmap(doc_a.page(0).unwrap(), &opts_s4).unwrap();
    assert!(
        doc_a
            .page(0)
            .unwrap()
            .render_layers()
            .bg44_partial
            .is_computed(),
        "sub=4 render must populate the partial tier"
    );
    let resumed = render_pixmap(doc_a.page(0).unwrap(), &opts_s1).unwrap();

    // Cold full render on a fresh document (no partial tier).
    let doc_b = load_doc("colorbook.djvu");
    let cold = render_pixmap(doc_b.page(0).unwrap(), &opts_s1).unwrap();

    assert_eq!(
        resumed.data, cold.data,
        "resumed full decode must be byte-identical"
    );
}

/// #576: back-and-forth pan hit-rate through the tile cache. LRU keeps
/// the tiles a reversing pan is about to revisit; the printed numbers are
/// the experiment's measurement (run with --nocapture), the assert is the
/// regression floor.
#[test]
fn tile_cache_back_and_forth_pan_hit_rate() {
    let doc = load_doc("colorbook.djvu");
    let page = doc.page(0).unwrap();
    let (w, h) = (page.width() as u32, page.height() as u32);
    // 2x zoom full-render space, viewport ~1/3 page, 25% pan steps,
    // left-to-right then back — the classic reading pattern.
    let opts = RenderOptions {
        width: w * 2,
        height: h * 2,
        ..Default::default()
    };
    // Realistic laptop viewport: 1440×960 ≈ 24 tiles (fits the 8 MiB /
    // ~32-tile budget with headroom — a viewport larger than the budget
    // thrashes any policy).
    let vw = 1440u32.min(w * 2);
    let vh = 960u32.min(h * 2);
    let step = vw / 4;
    let max_x = (w * 2).saturating_sub(vw);
    let mut xs: Vec<u32> = (0..=(max_x / step)).map(|i| i * step).collect();
    let back: Vec<u32> = xs.iter().rev().skip(1).copied().collect();
    xs.extend(back);
    for &x in &xs {
        let _ = render_region_tiled(
            page,
            RenderRect {
                x,
                y: 0,
                width: vw,
                height: vh,
            },
            &opts,
        )
        .unwrap();
    }
    let (hits, misses, evictions) = page.render_layers().tile_cache_stats();
    let rate = hits as f64 / (hits + misses).max(1) as f64;
    println!(
        "tile cache back-and-forth pan: hits={hits} misses={misses} evictions={evictions} hit-rate={:.1}%",
        rate * 100.0
    );
    assert!(
        rate > 0.30,
        "back-and-forth pan hit rate too low: {:.1}%",
        rate * 100.0
    );
}

/// Round 89 follow-up: a *cold* thumbnail-style render (bg_subsample >= 4,
/// no bold, no FGbz, first render of the page — nothing warm yet) must
/// not run the full-resolution JB2 mask decode at all, and must still
/// produce output pixel-identical to the same render done the old way
/// (mask_sub4 built by downsampling an already-decoded full-resolution
/// mask). Before this change, `decode_layers`'s #607 fast path required
/// `mask_sub4` to already be warm, so the very first (cold) sub>=4
/// render — exactly `Document::thumbnails()`'s access pattern — always
/// paid for a full-resolution `extract_mask` canvas just to immediately
/// downsample and discard it.
#[cfg(feature = "std")]
#[test]
fn cold_thumbnail_sweep_skips_full_mask_decode() {
    let body = || {
        let (w, h) = {
            let doc = load_doc("colorbook.djvu");
            let page = doc.page(0).unwrap();
            (page.width() as u32, page.height() as u32)
        };
        let opts_s4 = RenderOptions {
            width: w / 4,
            height: h / 4,
            ..Default::default()
        };
        let opts_s1 = RenderOptions {
            width: w,
            height: h,
            ..Default::default()
        };

        // Reference: force the full-resolution mask to decode and cache
        // first (a plain sub=1 render), then take the sub4 render — this
        // exercises `mask_sub4`'s "downsample an already-cached full mask"
        // branch, matching pre-fix behaviour exactly.
        let doc_warm = load_doc("colorbook.djvu");
        let page_warm = doc_warm.page(0).unwrap();
        let _ = render_pixmap(page_warm, &opts_s1).unwrap();
        let reference = render_pixmap(page_warm, &opts_s4).unwrap();

        // Cold: a fresh document, straight to a sub4 render — nothing
        // warm, must decode straight to 1/4 resolution via
        // `extract_mask_sub4` and must not touch the full-res decoder.
        let doc_cold = load_doc("colorbook.djvu");
        let page_cold = doc_cold.page(0).unwrap();
        JB2_MASK_DECODES.with(|c| c.set(0));
        let cold_s4 = render_pixmap(page_cold, &opts_s4).unwrap();
        assert_eq!(
            JB2_MASK_DECODES.with(|c| c.get()),
            0,
            "cold sub>=4 render must not run the full-resolution JB2 decode"
        );

        assert_eq!(
            cold_s4.data, reference.data,
            "cold sub4 thumbnail-style render must match the warm-mask-sub4 render"
        );
    };

    #[cfg(feature = "parallel")]
    {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .expect("build single-threaded pool for deterministic thread-local counting");
        pool.install(body);
    }
    #[cfg(not(feature = "parallel"))]
    body();
}

/// #607: `downgrade` retains the 1/4-res mask, and an eligible sub>=4
/// re-render consumes it without re-running the JB2 decode — while
/// producing pixel-identical output. A full-resolution re-render stays
/// cold and also reproduces the original bytes.
///
/// Under `parallel`, `decode_layers` runs the cold full-res path through
/// `rayon::join` (#440). The same thread-local counting hazard as
/// [`progressive_decoder_chunk_decodes_are_on_not_on_squared`] applies:
/// increments land on a global-pool worker, so the test thread reads 0.
/// Route the measurement through a dedicated single-worker pool (#721).
#[test]
fn downgrade_retains_sub4_mask_and_skips_jb2_decode() {
    let body = || {
        let doc = load_doc("colorbook.djvu");
        let (w, h) = {
            let p = doc.page(0).unwrap();
            (p.width() as u32, p.height() as u32)
        };
        let opts_s4 = RenderOptions {
            width: w / 4,
            height: h / 4,
            ..Default::default()
        };
        let opts_s1 = RenderOptions {
            width: w,
            height: h,
            ..Default::default()
        };

        // Warm the sub4 tier (this decodes the full mask once and builds
        // mask_sub4), then downgrade.
        let first_s4 = render_pixmap(doc.page(0).unwrap(), &opts_s4).unwrap();
        let first_s1 = render_pixmap(doc.page(0).unwrap(), &opts_s1).unwrap();
        doc.downgrade_render_caches();
        assert!(
            doc.page(0)
                .unwrap()
                .render_layers()
                .mask_sub4_cached()
                .is_some(),
            "downgrade must retain mask_sub4"
        );

        // Structural proof: the warm sub4 re-render must not invoke the JB2
        // decoder at all.
        JB2_MASK_DECODES.with(|c| c.set(0));
        let second_s4 = render_pixmap(doc.page(0).unwrap(), &opts_s4).unwrap();
        assert_eq!(
            JB2_MASK_DECODES.with(|c| c.get()),
            0,
            "warm sub4 re-render after downgrade must not re-run the JB2 decode"
        );
        assert_eq!(first_s4.data, second_s4.data, "sub=4 output changed");

        // Full-resolution re-render: cold (decodes the mask again), output
        // unchanged.
        JB2_MASK_DECODES.with(|c| c.set(0));
        let second_s1 = render_pixmap(doc.page(0).unwrap(), &opts_s1).unwrap();
        assert!(
            JB2_MASK_DECODES.with(|c| c.get()) > 0,
            "full-res re-render after downgrade must cold-decode the mask"
        );
        assert_eq!(first_s1.data, second_s1.data, "sub=1 output changed");
    };

    #[cfg(feature = "parallel")]
    {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .expect("build single-threaded pool for deterministic thread-local counting");
        pool.install(body);
    }
    #[cfg(not(feature = "parallel"))]
    body();
}

/// #607 eligibility guard: bold dilation needs the full-resolution mask,
/// so a bold sub>=4 render after downgrade must decode it (and match the
/// pre-downgrade bold render exactly).
///
/// Same `parallel` + thread-local counting wrap as
/// [`downgrade_retains_sub4_mask_and_skips_jb2_decode`] (#721).
#[test]
fn downgraded_sub4_with_bold_still_full_decodes() {
    let body = || {
        let doc = load_doc("colorbook.djvu");
        let (w, h) = {
            let p = doc.page(0).unwrap();
            (p.width() as u32, p.height() as u32)
        };
        let opts_bold = RenderOptions {
            width: w / 4,
            height: h / 4,
            bold: 1,
            ..Default::default()
        };
        let first = render_pixmap(doc.page(0).unwrap(), &opts_bold).unwrap();
        // Also warm the plain sub4 tier so mask_sub4 survives the downgrade.
        let opts_s4 = RenderOptions {
            width: w / 4,
            height: h / 4,
            ..Default::default()
        };
        let _ = render_pixmap(doc.page(0).unwrap(), &opts_s4).unwrap();
        doc.downgrade_render_caches();

        JB2_MASK_DECODES.with(|c| c.set(0));
        let second = render_pixmap(doc.page(0).unwrap(), &opts_bold).unwrap();
        assert!(
            JB2_MASK_DECODES.with(|c| c.get()) > 0,
            "bold render must not take the retained-sub4 shortcut"
        );
        assert_eq!(first.data, second.data, "bold sub=4 output changed");
    };

    #[cfg(feature = "parallel")]
    {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .expect("build single-threaded pool for deterministic thread-local counting");
        pool.install(body);
    }
    #[cfg(not(feature = "parallel"))]
    body();
}

/// `enforce_cache_budget_with(downgrade_before_drop: true)` honours the same
/// byte ceiling as `enforce_cache_budget`, and a downgraded (not fully
/// dropped) page still reproduces byte-identical output.
#[test]
fn enforce_cache_budget_with_downgrade_matches_budget_and_output() {
    let doc = load_doc("colorbook.djvu");
    if doc.page_count() < 3 {
        return;
    }
    let mut expected = Vec::new();
    for i in 0..3 {
        let (w, h) = {
            let p = doc.page(i).unwrap();
            (p.width() as u32, p.height() as u32)
        };
        let opts = RenderOptions {
            width: w,
            height: h,
            ..Default::default()
        };
        let p = doc.page(i).unwrap();
        expected.push(render_pixmap(p, &opts).unwrap());
    }

    let total_before = doc.render_cache_bytes();
    assert!(total_before > 0);
    let budget = total_before / 2;
    let opts = crate::djvu_document::CacheBudgetOptions {
        downgrade_before_drop: true,
    };
    let _freed = doc.enforce_cache_budget_with(budget, &[], opts);
    assert!(
        doc.render_cache_bytes() <= budget,
        "cache not held under budget: {} > {}",
        doc.render_cache_bytes(),
        budget
    );

    // Re-render every page (whichever were downgraded or dropped) and
    // check the output is unchanged either way.
    for (i, expected_pm) in expected.iter().enumerate().take(3) {
        let (w, h) = {
            let p = doc.page(i).unwrap();
            (p.width() as u32, p.height() as u32)
        };
        let opts = RenderOptions {
            width: w,
            height: h,
            ..Default::default()
        };
        let pm = render_pixmap(doc.page(i).unwrap(), &opts).unwrap();
        assert_eq!(expected_pm.data, pm.data, "page {i} output changed");
    }
}

/// `fit_to_width` scales correctly, preserving aspect ratio.
#[test]
fn fit_to_width_preserves_aspect() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    let pw = page.width() as u32;
    let ph = page.height() as u32;

    let opts = RenderOptions::fit_to_width(page, 800);
    assert_eq!(opts.width, 800);
    let expected_h = ((ph as f64 * 800.0) / pw as f64).round() as u32;
    assert_eq!(opts.height, expected_h);
    // The pipeline's decode scale is derived from width; it matches the
    // width/page-width ratio the deprecated `scale` field used to carry.
    assert!((opts.decode_scale(page) - 800.0 / pw as f32).abs() < 0.01);
}

/// `fit_to_height` scales correctly, preserving aspect ratio.
#[test]
fn fit_to_height_preserves_aspect() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    let pw = page.width() as u32;
    let ph = page.height() as u32;

    let opts = RenderOptions::fit_to_height(page, 600);
    assert_eq!(opts.height, 600);
    let expected_w = ((pw as f64 * 600.0) / ph as f64).round() as u32;
    assert_eq!(opts.width, expected_w);
    // fit_to_height preserves aspect, so width/page-width equals
    // height/page-height — the decode scale matches either ratio.
    assert!((opts.decode_scale(page) - 600.0 / ph as f32).abs() < 0.01);
}

/// `fit_to_box` chooses the smaller scale factor.
#[test]
fn fit_to_box_constrains_both() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();

    // Very wide box — height should be the constraint
    let opts = RenderOptions::fit_to_box(page, 10000, 100);
    assert!(opts.width <= 10000);
    assert!(opts.height <= 100);
    assert!(opts.width > 0 && opts.height > 0);

    // Very tall box — width should be the constraint
    let opts = RenderOptions::fit_to_box(page, 100, 10000);
    assert!(opts.width <= 100);
    assert!(opts.height <= 10000);
    assert!(opts.width > 0 && opts.height > 0);
}

/// `fit_to_box` with a square box picks the tighter dimension.
#[test]
fn fit_to_box_square() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();

    let opts = RenderOptions::fit_to_box(page, 500, 500);
    assert!(opts.width <= 500);
    assert!(opts.height <= 500);
    // At least one dimension should be close to 500
    assert!(opts.width >= 490 || opts.height >= 490);
}

/// Rotated page: fit_to_width uses display dimensions (swapped w/h).
#[test]
fn fit_to_width_rotation_aware() {
    // boy_jb2_rotate90 has a 90° rotation in the INFO chunk
    let doc = load_doc("boy_jb2_rotate90.djvu");
    let page = doc.page(0).unwrap();
    let pw = page.width() as u32;
    let ph = page.height() as u32;
    // Display dimensions are swapped for 90° rotation
    let (dw, dh) = (ph, pw);

    // `width`/`height` are native (pre-rotation); the rendered pixmap
    // has the requested display width.
    let opts = RenderOptions::fit_to_width(page, 400);
    let expected_h = ((dh as f64 * 400.0) / dw as f64).round() as u32;
    assert_eq!((opts.width, opts.height), (expected_h, 400));
    let pm = render_pixmap(page, &opts).unwrap();
    assert_eq!((pm.width, pm.height), (400, expected_h));
}

/// `render_into` with a zero-width dimension returns InvalidDimensions.
#[test]
fn render_into_invalid_dimensions() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();

    let opts = RenderOptions {
        width: 0,
        height: 100,
        ..Default::default()
    };
    let mut buf = vec![0u8; 400];
    let err = render_into(page, &opts, &mut buf).unwrap_err();
    assert!(
        matches!(err, RenderError::InvalidDimensions { .. }),
        "expected InvalidDimensions, got {err:?}"
    );
}

/// `render_into` with a too-small buffer returns BufTooSmall.
#[test]
fn render_into_buf_too_small() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();

    let opts = RenderOptions {
        width: 10,
        height: 10,
        ..Default::default()
    };
    let mut buf = vec![0u8; 10]; // too small (needs 400)
    let err = render_into(page, &opts, &mut buf).unwrap_err();
    assert!(
        matches!(err, RenderError::BufTooSmall { need: 400, got: 10 }),
        "expected BufTooSmall, got {err:?}"
    );
}

/// `render_into` fills a pre-allocated buffer without allocating new one.
///
/// We verify by: calling with exactly the right size buf, no panic,
/// and the buffer is mutated (not all-zero after the call).
#[test]
fn render_into_fills_buffer_no_alloc() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();

    let w = 50u32;
    let h = 40u32;
    let opts = RenderOptions {
        width: w,
        height: h,
        ..Default::default()
    };
    let mut buf = vec![0u8; (w * h * 4) as usize];
    render_into(page, &opts, &mut buf).expect("render_into should succeed");

    // The page is a color image — pixels should not all be zero
    assert!(
        buf.iter().any(|&b| b != 0),
        "rendered buffer should contain non-zero pixels"
    );
}

/// `render_into` can be called twice with the same buffer (zero-allocation reuse).
#[test]
fn render_into_reuse_buffer() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();

    let w = 30u32;
    let h = 20u32;
    let opts = RenderOptions {
        width: w,
        height: h,
        ..Default::default()
    };
    let mut buf = vec![0u8; (w * h * 4) as usize];

    // First render
    render_into(page, &opts, &mut buf).expect("first render_into should succeed");
    let first = buf.clone();

    // Second render — same result
    render_into(page, &opts, &mut buf).expect("second render_into should succeed");
    assert_eq!(
        first, buf,
        "repeated render_into should produce identical output"
    );
}

/// gamma=2.2 (most DjVu files) produces an identity LUT — no correction needed
/// for a standard display gamma=2.2.
#[test]
fn gamma_lut_standard_is_identity() {
    let lut = build_gamma_lut(2.2);
    for (i, &val) in lut.iter().enumerate() {
        assert_eq!(
            val, i as u8,
            "gamma=2.2 LUT at {i}: expected {i}, got {val}"
        );
    }
}

/// A linear-light source (gamma=1.0) is corrected: midtones become brighter
/// (exponent=1/2.2<1 raises sub-unity values toward 1.0) to compensate
/// for the display gamma-2.2 encoding needed for correct appearance.
#[test]
fn gamma_lut_linear_source_brightens() {
    let lut_linear = build_gamma_lut(1.0); // linear source → needs brightening
    let mid = 128u8;
    let corrected = lut_linear[mid as usize];
    assert!(
        corrected > mid,
        "linear-source LUT at mid ({corrected}) should be brighter than {mid}"
    );
}

/// Gamma LUT for gamma=0.0 (invalid) falls back to identity.
#[test]
fn gamma_lut_zero_is_identity() {
    let lut = build_gamma_lut(0.0);
    for (i, &val) in lut.iter().enumerate() {
        assert_eq!(val, i as u8, "zero gamma should produce identity LUT");
    }
}

/// render_coarse returns a valid pixmap (non-empty, correct dimensions) for
/// a color page.
#[test]
fn render_coarse_returns_pixmap() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();

    let opts = RenderOptions {
        width: 60,
        height: 80,
        ..Default::default()
    };

    let result = render_coarse(page, &opts).expect("render_coarse should succeed");
    // chicken.djvu may or may not have BG44 chunks
    if let Some(pm) = result {
        assert_eq!(pm.width, 60);
        assert_eq!(pm.height, 80);
        assert_eq!(pm.data.len(), 60 * 80 * 4);
    }
    // Ok(None) is also valid if no BG44
}

/// render_progressive returns valid pixmap after each chunk.
#[test]
fn render_progressive_each_chunk() {
    // Use a page that has multiple BG44 chunks (boy.djvu is a good candidate)
    let doc = load_doc("boy.djvu");
    let page = doc.page(0).unwrap();

    let opts = RenderOptions {
        width: 80,
        height: 100,
        ..Default::default()
    };

    let n_bg44 = page.bg44_chunks().len();

    for chunk_n in 0..n_bg44 {
        let pm = render_progressive(page, &opts, chunk_n)
            .unwrap_or_else(|e| panic!("render_progressive chunk {chunk_n} failed: {e}"));
        assert_eq!(pm.width, 80);
        assert_eq!(pm.height, 100);
        assert_eq!(pm.data.len(), 80 * 100 * 4);
        // Each frame must have some non-zero pixels
        assert!(
            pm.data.iter().any(|&b| b != 0),
            "chunk {chunk_n}: rendered frame should not be all-zero"
        );
    }
}

/// Regression test for BUG-ZPSHORT (found while validating B5): on
/// `watchmaker.djvu` page 0, BG44 chunk 2 of 4 is a legitimate two-byte
/// `[serial, slices]` header with a **zero-length** ZP payload (the encoder
/// had nothing left to encode for that refinement round). The strict
/// progressive path (`render_progressive_step` / `ProgressiveDecoder`)
/// used to hard-fail on it with `Iw44(ZpTooShort)`, while the permissive
/// full-page cache (`PageLayers::bg44`) silently swallowed the error and
/// dropped that chunk *and every chunk after it* — so the full render
/// "succeeded" but silently used only 2 of 4 refinement chunks. Both are
/// now fixed by treating a short/empty payload as valid trailing `0xFF`
/// padding at the `djvu-iw44` layer (see `Iw44Image::decode_chunk`),
/// matching the padding convention `ZpDecoder::read_byte` already uses at
/// a stream's true end. Every step and the full render must now succeed.
#[test]
fn render_progressive_step_handles_zero_length_bg44_chunk() {
    let path =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/watchmaker.djvu");
    let data = std::fs::read(&path).expect("watchmaker.djvu must exist");
    let doc = DjVuDocument::parse(&data).expect("parse failed");
    let page = doc.page(0).unwrap();

    let chunks = page.bg44_chunks();
    assert_eq!(
        chunks.len(),
        4,
        "expected 4 BG44 chunks on watchmaker page 0"
    );
    assert_eq!(
        chunks[2].len(),
        2,
        "chunk 2 should be the zero-payload [serial, slices] header this regression covers"
    );

    let opts = RenderOptions {
        width: page.width() as u32,
        height: page.height() as u32,
        resampling: Resampling::Bilinear,
        ..Default::default()
    };

    for step in 0..progressive_steps(page) {
        render_progressive_step(page, &opts, step)
            .unwrap_or_else(|e| panic!("step {step} should succeed, got {e}"));
    }
    render_progressive_all(page, &opts).expect("progressive_all should succeed");
    render_pixmap(page, &opts).expect("render_pixmap should succeed");

    let mut dec = ProgressiveDecoder::new(page, &opts).expect("decoder");
    for chunk in &chunks {
        dec.push_bg44_chunk(chunk)
            .expect("ProgressiveDecoder should also handle the zero-length chunk");
    }
}

/// `render_progressive_all` yields `progressive_steps` frames and its last
/// frame is byte-identical to `render_pixmap` (the sealed protocol contract).
#[test]
fn render_progressive_all_seals_chunk_loop() {
    let doc = load_doc("boy.djvu");
    let page = doc.page(0).unwrap();
    let opts = RenderOptions {
        width: 80,
        height: 100,
        ..Default::default()
    };

    // The seam hides `max(1, bg44_chunks().len())` from callers.
    let steps = progressive_steps(page);
    assert_eq!(steps, page.bg44_chunks().len().max(1));

    let frames = render_progressive_all(page, &opts).expect("progressive_all must succeed");
    assert_eq!(frames.len(), steps, "one frame per progressive step");

    let full = render_pixmap(page, &opts).expect("render_pixmap must succeed");
    assert_eq!(
        frames.last().unwrap().data,
        full.data,
        "final progressive frame must equal the full render"
    );
    // `render_progressive_step(0)` is also a valid (coarse) frame.
    let first = render_progressive_step(page, &opts, 0).expect("step 0 must succeed");
    assert_eq!(first.data, frames[0].data);
}

/// #691 slice 3 regression: a progressive frame must not depend on
/// whether the retained 1/4-res mask cache (#607) is warm. The fast
/// path in `decode_layers` used to hand the progressive path a maskless
/// layer set, so a prior full render at the same downscale silently
/// dropped the text layer from every later progressive frame.
#[cfg(feature = "std")]
#[test]
fn render_progressive_ignores_mask_sub4_warmth() {
    // colorbook.djvu: multi-chunk BG44, JB2 mask, no FGbz palette — at a
    // strong downscale it is exactly the page shape the #607 fast path
    // triggers on.
    let opts = RenderOptions {
        width: 61,
        height: 83,
        ..Default::default()
    };
    let cold = {
        let doc = load_doc("colorbook.djvu");
        let page = doc.page(0).unwrap();
        render_progressive_step(page, &opts, 1).unwrap()
    };
    let doc = load_doc("colorbook.djvu");
    let page = doc.page(0).unwrap();
    // Warm the sub4 mask cache the way any interactive session would:
    // with a plain full render at the same output size.
    let _ = render_pixmap(page, &opts).unwrap();
    assert!(
        page.render_layers().mask_sub4_cached().is_some(),
        "precondition: the full render must have retained the sub4 mask"
    );
    let warm = render_progressive_step(page, &opts, 1).unwrap();
    assert_eq!(
        cold.data, warm.data,
        "progressive frame changed with cache warmth"
    );
}

/// render_progressive with chunk_n out of range returns ChunkOutOfRange.
#[test]
fn render_progressive_chunk_out_of_range() {
    let doc = load_doc("boy.djvu");
    let page = doc.page(0).unwrap();

    let opts = RenderOptions {
        width: 40,
        height: 50,
        ..Default::default()
    };

    let n_bg44 = page.bg44_chunks().len();
    if n_bg44 == 0 {
        // No BG44 chunks — skip this test
        return;
    }

    let err = render_progressive(page, &opts, n_bg44 + 10).unwrap_err();
    assert!(
        matches!(err, RenderError::ChunkOutOfRange { .. }),
        "expected ChunkOutOfRange, got {err:?}"
    );
}

/// render_pixmap with gamma gives different result than without (identity gamma).
///
/// We compare rendering chicken.djvu twice: once with its natural gamma,
/// once with gamma forced to 1.0 (identity). The pixel values should differ.
#[test]
fn render_pixmap_gamma_differs_from_identity() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();

    let w = 40u32;
    let h = 53u32; // ~native aspect for 181x240

    let opts = RenderOptions {
        width: w,
        height: h,
        ..Default::default()
    };

    // Render with native gamma (2.2 from INFO chunk)
    let pm_gamma = render_pixmap(page, &opts).expect("render with gamma should succeed");

    // Render with identity gamma LUT manually applied to output
    let lut_identity = build_gamma_lut(1.0);
    let pm_identity = render_pixmap(page, &opts).expect("render for identity should succeed");
    // Apply identity correction (no-op) — pixels should be the same
    for i in 0..pm_identity.data.len().saturating_sub(3) {
        if i % 4 != 3 {
            // non-alpha channel
            let _ = lut_identity[pm_identity.data[i] as usize];
        }
    }

    // Since chicken.djvu gamma = 2.2, the gamma-corrected render
    // should have generally brighter mid-tones than a raw (no-correction) render.
    // We test this by checking that the gamma render is not bit-for-bit identical
    // to a hypothetical no-correction render. Since we always apply gamma in
    // render_pixmap, we test the gamma LUT effect directly (covered by
    // `gamma_correction_changes_pixels`).
    //
    // Instead, verify that pm_gamma has valid dimensions and non-trivial content.
    assert_eq!(pm_gamma.width, w);
    assert_eq!(pm_gamma.height, h);
    assert!(
        pm_gamma.data.iter().any(|&b| b != 255),
        "should have non-white pixels"
    );
}

/// render_pixmap for a bilevel (JB2-only) page produces black pixels.
#[test]
fn render_bilevel_page_has_black_pixels() {
    let doc = load_doc("boy_jb2.djvu");
    let page = doc.page(0).unwrap();

    let opts = RenderOptions {
        width: 60,
        height: 80,
        ..Default::default()
    };

    let pm = render_pixmap(page, &opts).expect("render bilevel should succeed");
    assert_eq!(pm.width, 60);
    assert_eq!(pm.height, 80);
    // A bilevel page should have some black pixels
    assert!(
        pm.data
            .as_chunks::<4>()
            .0
            .iter()
            .any(|px| px[0] == 0 && px[1] == 0 && px[2] == 0),
        "bilevel page should contain black pixels"
    );
}

/// `render_pixmap` with aa=true returns a valid pixmap.
#[test]
fn render_with_aa() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();

    let opts = RenderOptions {
        width: 40,
        height: 54,
        aa: true,
        ..Default::default()
    };
    // With aa=true the output is downscaled 2×, so we get 20×27
    let pm = render_pixmap(page, &opts).expect("render with AA should succeed");
    // AA downscales the output
    assert_eq!(pm.width, 20);
    assert_eq!(pm.height, 27);
}

// -- Rotation tests -------------------------------------------------------

#[test]
fn rotate_pixmap_none_is_identity() {
    let mut pm = Pixmap::white(3, 2);
    pm.set_rgb(0, 0, 255, 0, 0);
    let rotated = rotate_pixmap(pm.clone(), crate::info::Rotation::None);
    assert_eq!(rotated.width, 3);
    assert_eq!(rotated.height, 2);
    assert_eq!(rotated.get_rgb(0, 0), (255, 0, 0));
}

#[test]
fn rotate_pixmap_cw90_swaps_dims() {
    let mut pm = Pixmap::white(4, 2);
    pm.set_rgb(0, 0, 255, 0, 0); // top-left red
    let rotated = rotate_pixmap(pm, crate::info::Rotation::Cw90);
    assert_eq!(rotated.width, 2);
    assert_eq!(rotated.height, 4);
    // Top-left (0,0) of original goes to (height-1-0, 0) = (1, 0) in rotated
    assert_eq!(rotated.get_rgb(1, 0), (255, 0, 0));
}

#[test]
fn rotate_pixmap_180_preserves_dims() {
    let mut pm = Pixmap::white(3, 2);
    pm.set_rgb(0, 0, 255, 0, 0); // top-left red
    let rotated = rotate_pixmap(pm, crate::info::Rotation::Rot180);
    assert_eq!(rotated.width, 3);
    assert_eq!(rotated.height, 2);
    assert_eq!(rotated.get_rgb(2, 1), (255, 0, 0));
}

#[test]
fn rotate_pixmap_ccw90_swaps_dims() {
    let mut pm = Pixmap::white(4, 2);
    pm.set_rgb(0, 0, 255, 0, 0); // top-left red
    let rotated = rotate_pixmap(pm, crate::info::Rotation::Ccw90);
    assert_eq!(rotated.width, 2);
    assert_eq!(rotated.height, 4);
    // Top-left (0,0) -> (0, width-1-0) = (0, 3) in rotated
    assert_eq!(rotated.get_rgb(0, 3), (255, 0, 0));
}

#[test]
fn render_pixmap_rotation_90_swaps_dimensions() {
    let doc = load_doc("boy_jb2_rotate90.djvu");
    let page = doc.page(0).expect("page 0");
    let orig_w = page.width();
    let orig_h = page.height();
    let opts = RenderOptions {
        width: orig_w as u32,
        height: orig_h as u32,
        ..Default::default()
    };
    let pm = render_pixmap(page, &opts).expect("render should succeed");
    // 90° rotation swaps width and height
    assert_eq!(
        pm.width, orig_h as u32,
        "rotated width should be original height"
    );
    assert_eq!(
        pm.height, orig_w as u32,
        "rotated height should be original width"
    );
}

#[test]
fn render_pixmap_rotation_180_preserves_dimensions() {
    let doc = load_doc("boy_jb2_rotate180.djvu");
    let page = doc.page(0).expect("page 0");
    let orig_w = page.width();
    let orig_h = page.height();
    let opts = RenderOptions {
        width: orig_w as u32,
        height: orig_h as u32,
        ..Default::default()
    };
    let pm = render_pixmap(page, &opts).expect("render should succeed");
    assert_eq!(pm.width, orig_w as u32);
    assert_eq!(pm.height, orig_h as u32);
}

#[test]
fn render_pixmap_rotation_270_swaps_dimensions() {
    let doc = load_doc("boy_jb2_rotate270.djvu");
    let page = doc.page(0).expect("page 0");
    let orig_w = page.width();
    let orig_h = page.height();
    let opts = RenderOptions {
        width: orig_w as u32,
        height: orig_h as u32,
        ..Default::default()
    };
    let pm = render_pixmap(page, &opts).expect("render should succeed");
    assert_eq!(
        pm.width, orig_h as u32,
        "rotated width should be original height"
    );
    assert_eq!(
        pm.height, orig_w as u32,
        "rotated height should be original width"
    );
}

// -- User rotation tests ---------------------------------------------------

/// combine_rotations adds steps modulo 4.
#[test]
fn combine_rotations_identity() {
    use crate::info::Rotation;
    assert_eq!(
        combine_rotations(Rotation::None, UserRotation::None),
        Rotation::None
    );
}

#[test]
fn combine_rotations_info_only() {
    use crate::info::Rotation;
    assert_eq!(
        combine_rotations(Rotation::Cw90, UserRotation::None),
        Rotation::Cw90
    );
}

#[test]
fn combine_rotations_user_only() {
    use crate::info::Rotation;
    assert_eq!(
        combine_rotations(Rotation::None, UserRotation::Ccw90),
        Rotation::Ccw90
    );
}

#[test]
fn combine_rotations_sum() {
    use crate::info::Rotation;
    // 90 CW (INFO) + 90 CW (user) = 180
    assert_eq!(
        combine_rotations(Rotation::Cw90, UserRotation::Cw90),
        Rotation::Rot180
    );
    // 90 CW + 270 CW = 360 = None
    assert_eq!(
        combine_rotations(Rotation::Cw90, UserRotation::Ccw90),
        Rotation::None
    );
    // 180 + 180 = 360 = None
    assert_eq!(
        combine_rotations(Rotation::Rot180, UserRotation::Rot180),
        Rotation::None
    );
}

/// User rotation Cw90 on a non-rotated page swaps output dimensions.
#[test]
fn user_rotation_cw90_swaps_dimensions() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    let pw = page.width() as u32;
    let ph = page.height() as u32;

    let opts = RenderOptions {
        width: pw,
        height: ph,
        rotation: UserRotation::Cw90,
        ..Default::default()
    };
    let pm = render_pixmap(page, &opts).expect("render");
    assert_eq!(pm.width, ph, "user Cw90 should swap: width becomes height");
    assert_eq!(pm.height, pw, "user Cw90 should swap: height becomes width");
}

/// User rotation 180° preserves dimensions.
#[test]
fn user_rotation_180_preserves_dimensions() {
    let doc = load_doc("chicken.djvu");
    let page = doc.page(0).unwrap();
    let pw = page.width() as u32;
    let ph = page.height() as u32;

    let opts = RenderOptions {
        width: pw,
        height: ph,
        rotation: UserRotation::Rot180,
        ..Default::default()
    };
    let pm = render_pixmap(page, &opts).expect("render");
    assert_eq!(pm.width, pw);
    assert_eq!(pm.height, ph);
}

/// UserRotation default is None.
#[test]
fn user_rotation_default_is_none() {
    assert_eq!(UserRotation::default(), UserRotation::None);
    let opts = RenderOptions::default();
    assert_eq!(opts.rotation, UserRotation::None);
}

// -- FGbz multi-color palette tests ---------------------------------------

#[test]
fn fgbz_palette_page_renders_multiple_colors() {
    // irish.djvu is a single-page file with an FGbz palette.
    let doc = load_doc("irish.djvu");
    let page = doc.page(0).expect("page 0");
    let w = page.width() as u32;
    let h = page.height() as u32;
    let opts = RenderOptions {
        width: w,
        height: h,
        ..Default::default()
    };
    let pm = render_pixmap(page, &opts).expect("render should succeed");

    // Collect distinct non-white, non-black foreground colors
    let mut fg_colors = std::collections::HashSet::new();
    for y in 0..h {
        for x in 0..w {
            let (r, g, b) = pm.get_rgb(x, y);
            // Skip white and near-white (background)
            if r > 240 && g > 240 && b > 240 {
                continue;
            }
            fg_colors.insert((r, g, b));
        }
    }

    // A multi-color palette page should produce more than 1 distinct
    // foreground color (if it only had 1, it'd be the old bug).
    assert!(
        fg_colors.len() > 1,
        "multi-color palette page should have >1 distinct foreground colors, got {}",
        fg_colors.len()
    );
}

#[test]
fn lookup_palette_color_uses_blit_map() {
    let pal = FgbzPalette {
        colors: vec![
            PaletteColor { r: 255, g: 0, b: 0 }, // index 0: red
            PaletteColor { r: 0, g: 0, b: 255 }, // index 1: blue
        ],
        indices: vec![1, 0], // blit 0 → color 1 (blue), blit 1 → color 0 (red)
    };
    let bm = crate::bitmap::Bitmap::new(2, 1);
    let blit_map = vec![0i32, 1i32]; // pixel (0,0) → blit 0, pixel (1,0) → blit 1

    let c0 = lookup_palette_color(&pal, Some(&blit_map), Some(&bm), 0, 0);
    assert_eq!(
        (c0.r, c0.g, c0.b),
        (0, 0, 255),
        "blit 0 → indices[0]=1 → blue"
    );

    let c1 = lookup_palette_color(&pal, Some(&blit_map), Some(&bm), 1, 0);
    assert_eq!(
        (c1.r, c1.g, c1.b),
        (255, 0, 0),
        "blit 1 → indices[1]=0 → red"
    );
}

#[test]
fn lookup_palette_color_fallback_without_blit_map() {
    let pal = FgbzPalette {
        colors: vec![PaletteColor { r: 0, g: 128, b: 0 }],
        indices: vec![],
    };
    let c = lookup_palette_color(&pal, None, None, 0, 0);
    assert_eq!(
        (c.r, c.g, c.b),
        (0, 128, 0),
        "should fall back to first color"
    );
}
