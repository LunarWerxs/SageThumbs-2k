//! Phase A2 of docs/jb2-size-gap-plan.md — measure REAL emitted bytes + round-trip
//! for same-size record-6 refinement. Run:
//!   cargo run --release --features experimental --example jb2_same_size_a2
use djvu_rs::jb2_encode::{Jb2EncodeOptions, encode_jb2_dict_with_options};

fn run(path: &str, thresholds: &[f32]) {
    let Ok(data) = std::fs::read(path) else {
        println!("{path}: missing");
        return;
    };
    let Ok(doc) = djvu_rs::DjVuDocument::parse(&data) else {
        println!("{path}: parse fail");
        return;
    };
    let n = doc.page_count();
    let mut masks = Vec::new();
    for i in 0..n {
        if let Ok(page) = doc.page(i) {
            if let Ok(Some(m)) = page.extract_mask() {
                masks.push(m);
            }
        }
    }
    // Baseline: default lossless encode.
    let base_opts = Jb2EncodeOptions::default();
    let mut base_total = 0usize;
    for m in &masks {
        base_total += encode_jb2_dict_with_options(m, &[], &base_opts).len();
    }
    println!(
        "== {} ({} masks) ==  baseline Sjbz = {} B",
        path,
        masks.len(),
        base_total
    );
    for &frac in thresholds {
        let mut opts = Jb2EncodeOptions::default();
        opts.same_size_rec6 = Some(frac);
        let mut total = 0usize;
        let (mut rt_ok, mut rt_fail) = (0usize, 0usize);
        for m in &masks {
            let enc = encode_jb2_dict_with_options(m, &[], &opts);
            total += enc.len();
            match djvu_rs::jb2::decode(&enc, None) {
                Ok(dec) if dec.width == m.width && dec.height == m.height && dec.data == m.data => {
                    rt_ok += 1
                }
                _ => rt_fail += 1,
            }
        }
        let delta = total as i64 - base_total as i64;
        println!(
            "  frac={:.0}%: Sjbz={} B  delta={:+} B ({:+.2}%)  round-trip: {} exact / {} FAIL",
            frac * 100.0,
            total,
            delta,
            100.0 * delta as f64 / base_total as f64,
            rt_ok,
            rt_fail
        );
    }
}

fn main() {
    let th = [0.02f32, 0.05, 0.08];
    run("tests/corpus/watchmaker.djvu", &th);
    run("tests/corpus/pathogenic_bacteria_1896.djvu", &th);
}
