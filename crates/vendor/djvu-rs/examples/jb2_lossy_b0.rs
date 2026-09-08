//! Branch B / Phase B0 of docs/jb2-size-gap-plan.md — measure the EXISTING
//! same-size lossy rec-7 substitution (#224 `lossy_threshold`): size vs quality
//! (PSNR/SSIM via the D1 harness) at several thresholds. Baseline for Branch B.
//! Run: cargo run --release --example jb2_lossy_b0
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

fn run(path: &str, thresholds: &[f32]) {
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
    // Lossless baseline size.
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
    for &t in thresholds {
        // `..default()` is a no-op under default features (only `lossy_threshold`
        // exists) but sets the experimental fields when they're compiled in.
        #[allow(clippy::needless_update)]
        let opts = Jb2EncodeOptions {
            lossy_threshold: t,
            ..Jb2EncodeOptions::default()
        };
        let mut total = 0usize;
        let (mut ssim_sum, mut psnr_sum, mut n) = (0.0f64, 0.0f64, 0usize);
        let (mut flipped, mut tot_px) = (0u64, 0u64);
        for m in &masks {
            let enc = encode_jb2_dict_with_options(m, &[], &opts);
            total += enc.len();
            if let Ok(dec) = djvu_rs::jb2::decode(&enc, None)
                && dec.width == m.width
                && dec.height == m.height
            {
                for y in 0..m.height {
                    for x in 0..m.width {
                        if m.get(x, y) != dec.get(x, y) {
                            flipped += 1;
                        }
                    }
                }
                tot_px += (m.width as u64) * (m.height as u64);
                let q = djvu_rs::quality::compare_gray(&mask_to_gray(m), &mask_to_gray(&dec));
                if q.psnr_db.is_finite() {
                    psnr_sum += q.psnr_db;
                }
                ssim_sum += q.ssim;
                n += 1;
            }
        }
        let delta = total as i64 - base_total as i64;
        println!(
            "  thr={:.0}%: Sjbz={} B  ({:+.2}%)  SSIM={:.5}  PSNR={:.1}dB  flipped={:.4}% of mask px",
            t * 100.0,
            total,
            100.0 * delta as f64 / base_total as f64,
            ssim_sum / n.max(1) as f64,
            psnr_sum / n.max(1) as f64,
            100.0 * flipped as f64 / tot_px.max(1) as f64
        );
    }
}

fn main() {
    let th = [0.02f32, 0.05, 0.08, 0.10];
    run("tests/corpus/watchmaker.djvu", &th);
    run("tests/corpus/pathogenic_bacteria_1896.djvu", &th);
}
