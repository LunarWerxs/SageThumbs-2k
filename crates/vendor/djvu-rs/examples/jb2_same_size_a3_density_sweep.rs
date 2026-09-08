//! Scratch sweep for JB2_AUTO_REC6: measure the same-size near-twin density
//! (A0 metric) across all four corpus files to calibrate the auto-policy
//! threshold with an intermediate data point beyond the two round-17
//! calibration corpora (watchmaker / pathogenic). Not part of the shipped
//! test suite — a one-off measurement helper.
//! Run: cargo run --release --features experimental --example jb2_same_size_auto_sweep
use djvu_rs::jb2_encode::analyze_jb2_same_size_refinement;

fn run(path: &str) {
    let Ok(data) = std::fs::read(path) else {
        println!("{path}: missing");
        return;
    };
    let Ok(doc) = djvu_rs::DjVuDocument::parse(&data) else {
        println!("{path}: parse fail");
        return;
    };
    let n = doc.page_count();
    let (mut fresh, mut n5) = (0usize, 0usize);
    for i in 0..n {
        let Ok(page) = doc.page(i) else { continue };
        let Ok(Some(mask)) = page.extract_mask() else {
            continue;
        };
        let s = analyze_jb2_same_size_refinement(&mask, &[]);
        fresh += s.fresh_ccs;
        n5 += s.near_le_5pct;
    }
    println!(
        "{path}: fresh={fresh} near_le_5pct={n5} density={:.2}%",
        100.0 * n5 as f64 / fresh.max(1) as f64
    );
}

fn main() {
    for p in [
        "tests/corpus/watchmaker.djvu",
        "tests/corpus/cable_1973_100133.djvu",
        "tests/corpus/conquete_paix.djvu",
        "tests/corpus/pathogenic_bacteria_1896.djvu",
    ] {
        run(p);
    }
}
