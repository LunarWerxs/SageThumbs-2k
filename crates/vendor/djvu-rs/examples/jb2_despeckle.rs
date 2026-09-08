//! JB2_DESPECKLE experiment: speck-removal pre-pass for lossy JB2 on noisy
//! scans. Measures Sjbz size + mask SSIM (via the D1 harness, same as round
//! 19's `jb2_lossy_b0`) across a grid of `despeckle` (max speck pixel-area)
//! x `lossy_threshold` (same-size near-twin substitution) settings.
//!
//! Run: cargo run --release --example jb2_despeckle
use djvu_rs::jb2_encode::{Jb2EncodeOptions, encode_jb2_dict_with_options};
use djvu_rs::{Bitmap, GrayPixmap};

fn mask_to_gray(bm: &Bitmap) -> GrayPixmap {
    let mut data = vec![0u8; (bm.width * bm.height) as usize];
    for y in 0..bm.height {
        for x in 0..bm.width {
            data[(y * bm.width + x) as usize] = if bm.get(x, y) { 0 } else { 255 };
        }
    }
    GrayPixmap {
        width: bm.width,
        height: bm.height,
        data,
    }
}

fn run(path: &str, despeckle_levels: &[Option<u32>], lossy_levels: &[f32]) {
    let Ok(data) = std::fs::read(path) else {
        println!("{path}: missing");
        return;
    };
    let Ok(doc) = djvu_rs::DjVuDocument::parse(&data) else {
        println!("{path}: parse fail");
        return;
    };
    let mut masks = Vec::new();
    for i in 0..doc.page_count() {
        if let Ok(page) = doc.page(i)
            && let Ok(Some(m)) = page.extract_mask()
        {
            masks.push(m);
        }
    }
    let base_opts = Jb2EncodeOptions::default();
    let base_total: usize = masks
        .iter()
        .map(|m| encode_jb2_dict_with_options(m, &[], &base_opts).len())
        .sum();
    println!(
        "== {} ({} masks) ==  lossless Sjbz = {} B",
        path,
        masks.len(),
        base_total
    );

    for &despeckle in despeckle_levels {
        for &lossy in lossy_levels {
            // `..default()` is a no-op under default features (only
            // `lossy_threshold`/`despeckle` exist) but sets the experimental
            // fields when they're compiled in.
            #[allow(clippy::needless_update)]
            let opts = Jb2EncodeOptions {
                lossy_threshold: lossy,
                despeckle,
                ..Jb2EncodeOptions::default()
            };
            let mut total = 0usize;
            let (mut ssim_sum, mut psnr_sum, mut n) = (0.0f64, 0.0f64, 0usize);
            let (mut flipped, mut tot_px) = (0u64, 0u64);
            let mut all_decoded = true;
            for m in &masks {
                let enc = encode_jb2_dict_with_options(m, &[], &opts);
                total += enc.len();
                match djvu_rs::jb2::decode(&enc, None) {
                    Ok(dec) if dec.width == m.width && dec.height == m.height => {
                        for y in 0..m.height {
                            for x in 0..m.width {
                                if m.get(x, y) != dec.get(x, y) {
                                    flipped += 1;
                                }
                            }
                        }
                        tot_px += (m.width as u64) * (m.height as u64);
                        let q =
                            djvu_rs::quality::compare_gray(&mask_to_gray(m), &mask_to_gray(&dec));
                        if q.psnr_db.is_finite() {
                            psnr_sum += q.psnr_db;
                        }
                        ssim_sum += q.ssim;
                        n += 1;
                    }
                    _ => {
                        all_decoded = false;
                    }
                }
            }
            let delta = total as i64 - base_total as i64;
            println!(
                "  despeckle={:>4}  lossy={:>4.0}%: Sjbz={:>10} B  ({:+6.2}%)  SSIM={:.5}  PSNR={:.1}dB  flipped={:.4}% of mask px  decode_ok={}",
                despeckle
                    .map(|d| d.to_string())
                    .unwrap_or_else(|| "off".to_string()),
                lossy * 100.0,
                total,
                100.0 * delta as f64 / base_total as f64,
                ssim_sum / n.max(1) as f64,
                psnr_sum / n.max(1) as f64,
                100.0 * flipped as f64 / tot_px.max(1) as f64,
                all_decoded
            );
        }
    }
}

fn main() {
    let despeckle_levels = [None, Some(2), Some(4), Some(8)];
    let lossy_levels = [0.0f32, 0.02];
    run(
        "tests/corpus/watchmaker.djvu",
        &despeckle_levels,
        &lossy_levels,
    );
    run(
        "tests/corpus/pathogenic_bacteria_1896.djvu",
        &despeckle_levels,
        &lossy_levels,
    );
}
