//! Phase A0 of docs/jb2-size-gap-plan.md — measure the same-size refinement
//! candidate population (experiment-only, no encoder change).
//! Run: cargo run --release --features experimental --example jb2_same_size_a0
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
    let (mut tot, mut fresh, mut elig, mut cand) = (0usize, 0usize, 0usize, 0usize);
    let (mut n2, mut n5, mut n10) = (0usize, 0usize, 0usize);
    let (mut px5, mut hb5) = (0u64, 0u64);
    let mut hist: Vec<u32> = Vec::new();
    for i in 0..n {
        let Ok(page) = doc.page(i) else { continue };
        let Ok(Some(mask)) = page.extract_mask() else {
            continue;
        };
        // Per-page independent baseline (shared=[]), i.e. encode_jb2_dict.
        let s = analyze_jb2_same_size_refinement(&mask, &[]);
        tot += s.total_ccs;
        fresh += s.fresh_ccs;
        elig += s.eligible_fresh_ccs;
        cand += s.candidate_ccs;
        n2 += s.near_le_2pct;
        n5 += s.near_le_5pct;
        n10 += s.near_le_10pct;
        px5 += s.near_le_5pct_pixels;
        hb5 += s.near_le_5pct_hamming_bytes;
        hist.extend(s.best_hamming_permille);
    }
    hist.sort_unstable();
    let median = hist.get(hist.len() / 2).copied().unwrap_or(0);
    println!("== {} ({} pages) ==", path, n);
    println!("  total_ccs={tot} fresh={fresh} eligible={elig} candidates={cand}");
    println!("  near-twins:  <=2% : {n2}   <=5% : {n5}   <=10% : {n10}");
    println!(
        "  fresh with same-size candidate = {:.1}% of fresh; <=5% near-twins = {:.1}% of fresh",
        100.0 * cand as f64 / fresh.max(1) as f64,
        100.0 * n5 as f64 / fresh.max(1) as f64
    );
    println!("  <=5% near-twin pixels={px5} hamming_bytes(1bit/px floor)={hb5}");
    println!(
        "  median best-Hamming among candidates = {:.1}% ",
        median as f64 / 10.0
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
