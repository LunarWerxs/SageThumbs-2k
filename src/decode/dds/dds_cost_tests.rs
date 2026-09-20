use super::*;
use std::time::Instant;

/// Where a 12 MP DDS thumbnail's time ACTUALLY goes. Not a gate - a measuring stick, so
/// the next person to "optimise DDS" aims at the part that costs something.
///
///     cargo test --release --lib decode::dds::dds_cost_tests -- --ignored --nocapture
///
/// It exists because a plausible optimisation bought nothing: replacing the per-block
/// `bcdec_rs` expansion with an endpoint+histogram mean (byte-identical, and still in the
/// tree) moved a 4000x3000 DXT1 by under a millisecond. The block loop was simply not
/// where the time was, and three runs of the speed gate could not tell me that.
#[test]
#[ignore = "timing measurement over a 12 MP DXT1, not a gate; run --release --nocapture"]
fn where_a_twelve_megapixel_dds_spends_its_time() {
    const W: u32 = 4000;
    const H: u32 = 3000;
    let (bw, bh) = (W.div_ceil(4) as usize, H.div_ceil(4) as usize);

    // A synthetic BC1 surface with real per-block variation, so nothing is degenerate.
    let mut src = vec![0u8; bw * bh * 8];
    let mut s = 0x9E37_79B9u32;
    for b in src.iter_mut() {
        s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        *b = (s >> 24) as u8;
    }

    let mut out = vec![0u8; bw * bh * 4];

    // MIN OF N, AND EVERY BUFFER TOUCHED FIRST. The first shape of this test timed each
    // variant ONCE, in order, and made the cheaper algorithm look 2.4x slower - the first
    // call reads all 6 MB of `src` cold while the second finds it in cache, and
    // `vec![0u8; _]` hands back lazily-mapped pages so the first writer pays every page
    // fault too. Warm, then take the best of several.
    fn best_of<F: FnMut()>(mut f: F) -> std::time::Duration {
        f();
        let mut best = std::time::Duration::MAX;
        for _ in 0..5 {
            let t = Instant::now();
            f();
            best = best.min(t.elapsed());
        }
        best
    }

    // (a) what ships: one pixel per block, straight from endpoints + an index histogram.
    let fast = best_of(|| blocks_rgba8(&src, W, H, Block::Bc1, true, &mut out));

    // (b) THE PATH IT REPLACED, and the only honest comparison: expand all sixteen texels
    // through `bcdec_rs`, then average them. Reproduced here rather than kept behind a
    // flag in the shipping function - comparing against the bare expansion instead was
    // what made the first attempt look like a regression when it was not being measured
    // against its own alternative at all.
    let mut avg_out = vec![0u8; bw * bh * 4];
    let slow = best_of(|| {
        let mut tile = [0u8; 4 * 4 * 4];
        for by in 0..bh {
            for bx in 0..bw {
                let off = (by * bw + bx) * 8;
                bcdec_rs::bc1(&src[off..off + 8], &mut tile, 16);
                write_block_average(&tile, (by * bw + bx) * 4, 4, 4, &mut avg_out);
            }
        }
    });
    assert_eq!(
        out, avg_out,
        "the two averaging paths must agree byte for byte"
    );

    // (c) for scale: expanding the whole 12 MP surface, which is what both of the above
    // exist to avoid.
    let mut full = vec![0u8; W as usize * H as usize * 4];
    let expand = best_of(|| blocks_rgba8(&src, W, H, Block::Bc1, false, &mut full));

    let img = image::RgbaImage::from_raw(bw as u32, bh as u32, out.clone())
        .map(DynamicImage::ImageRgba8)
        .expect("block-average buffer");
    let fit = best_of(|| {
        let _ = super::super::thumb::thumbnail_from_image(img.clone(), 256);
    });

    println!("  (a) block mean, endpoints+histogram : {:>8.2?}", fast);
    println!("  (b) block mean, expand then average  : {:>8.2?}", slow);
    println!("  (c) full 12 MP expansion, for scale  : {:>8.2?}", expand);
    println!("  (d) fit 1000x750 -> 256              : {:>8.2?}", fit);
}
