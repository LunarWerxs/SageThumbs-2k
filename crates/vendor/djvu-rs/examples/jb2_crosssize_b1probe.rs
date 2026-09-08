//! Branch B / B1 measurement — cross-size near-twin population (the matches
//! same-size lossy misses), via the existing analyze_jb2_cross_size_refinement.
//! Run: cargo run --release --features experimental --example jb2_crosssize_b1probe
use djvu_rs::jb2_encode::analyze_jb2_cross_size_refinement;

fn run(path: &str) {
    let Ok(data) = std::fs::read(path) else {
        println!("{path}: missing");
        return;
    };
    let Ok(doc) = djvu_rs::DjVuDocument::parse(&data) else {
        return;
    };
    let (mut fresh, mut cand, mut near) = (0usize, 0usize, 0usize);
    let mut near_px = 0u64;
    let mut hist: Vec<u32> = Vec::new();
    for i in 0..doc.page_count() {
        if let Ok(page) = doc.page(i) {
            if let Ok(Some(m)) = page.extract_mask() {
                // max_dim_delta=2 px, budget 5% resampled Hamming (cjb2-ish).
                let s = analyze_jb2_cross_size_refinement(&m, &[], 2, 0.05);
                fresh += s.fresh_ccs;
                cand += s.candidate_ccs;
                near += s.near_matches;
                near_px += s.near_match_pixels;
                hist.extend(s.best_hamming);
            }
        }
    }
    println!("== {} ==", path);
    println!(
        "  fresh={fresh}  cross-size candidates={cand}  near(<=5%)={near}  ({:.1}% of fresh)  near_px={near_px}",
        100.0 * near as f64 / fresh.max(1) as f64
    );
}
fn main() {
    for p in [
        "tests/corpus/watchmaker.djvu",
        "tests/corpus/pathogenic_bacteria_1896.djvu",
    ] {
        run(p);
    }
}
