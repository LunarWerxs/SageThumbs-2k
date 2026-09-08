//! C5_COMPRESS measurement harness — mirrors the C5_RENDER_CACHE_EVICTION /
//! C5_LRU_BUDGET harness shape from PERF_EXPERIMENTS.md (round 12/13).
//!
//! Simulates a mixed viewer workflow on colorbook.djvu (62 colour pages):
//! every page gets a thumbnail-rail render (sub=2) as it scrolls past, plus a
//! full-resolution open (sub=1), with a cache-budget sweep after each page.
//! Modes:
//!   - `none`      : no eviction (peak-RSS upper bound)
//!   - `drop`      : `enforce_cache_budget(budget, &[i])` (baseline, C5_LRU_BUDGET)
//!   - `downgrade` : `enforce_cache_budget_with(budget, &[i], downgrade_before_drop: true)`
//!     (C5_COMPRESS's cheaper middle tier — keeps `bg_rgb_s2` alive across
//!     eviction instead of dropping everything)
//!
//! After the sweep, measures "warm-render-after-eviction latency" for page 0
//! over several repeated evict→render trials (median reported — single-shot
//! wall-clock is noisy under M1 Max thermal throttling per this repo's perf
//! log methodology) at sub=1 (full-res) and sub=2 (half-res, e.g.
//! thumbnail/contact-sheet/zoomed-out pan).
//!
//! Run under `/usr/bin/time -l` (macOS) for peak RSS ("peak memory
//! footprint"): `/usr/bin/time -l ./target/release/examples/c5_compress_bench drop 62914560`
use std::time::Instant;

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn evict(doc: &mut djvu_rs::DjVuDocument, mode: &str, budget: usize, protect: &[usize]) {
    match mode {
        "drop" => {
            doc.enforce_cache_budget(budget, protect);
        }
        "downgrade" => {
            let cbo = djvu_rs::djvu_document::CacheBudgetOptions {
                downgrade_before_drop: true,
            };
            doc.enforce_cache_budget_with(budget, protect, cbo);
        }
        _ => {}
    }
}

fn main() {
    let mode = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "drop".to_string());
    let budget: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(60 * 1024 * 1024);

    let path = "tests/fixtures/colorbook.djvu";
    let data = std::fs::read(path).expect("colorbook.djvu fixture");
    let mut doc = djvu_rs::DjVuDocument::parse(&data).expect("parse");
    let n = doc.page_count();
    eprintln!("mode={mode} budget={budget} pages={n}");

    // Realistic mixed viewer workflow: every page gets a thumbnail-rail
    // render (sub=2) as the user scrolls past it, and every page is also
    // opened at full resolution (sub=1) — this is what actually populates
    // `bg_rgb_s2` before a page falls out of the protected window, which is
    // the precondition for the downgrade tier to do anything (a page that was
    // only ever rendered at sub=1 has nothing cheap left to keep).
    for i in 0..n {
        let (w, h) = {
            let p = doc.page(i).unwrap();
            (p.width() as u32, p.height() as u32)
        };
        let opts_full = djvu_rs::djvu_render::RenderOptions {
            width: w,
            height: h,
            ..Default::default()
        };
        let opts_thumb = djvu_rs::djvu_render::RenderOptions {
            width: w / 2,
            height: h / 2,
            ..Default::default()
        };
        let _ = djvu_rs::djvu_render::render_pixmap(doc.page(i).unwrap(), &opts_thumb).unwrap();
        let _ = djvu_rs::djvu_render::render_pixmap(doc.page(i).unwrap(), &opts_full).unwrap();
        evict(&mut doc, &mode, budget, &[i]);
    }

    let final_cache = doc.render_cache_bytes();
    eprintln!("final render_cache_bytes = {final_cache}");

    // Structural breakdown: how many pages are fully cached, "downgraded"
    // (nonzero but shrunk), or fully dropped (zero) — the primary signal per
    // the experiment brief (wall-clock is provisional / thermal-noisy).
    let mut zero = 0;
    let mut small = 0; // < 4 MB: consistent with only bg_rgb_s2/s4 surviving
    let mut large = 0;
    for i in 0..n {
        let b = doc.page(i).unwrap().render_cache_bytes();
        if b == 0 {
            zero += 1;
        } else if b < 4 * 1024 * 1024 {
            small += 1;
        } else {
            large += 1;
        }
    }
    eprintln!(
        "page cache-state histogram: zero={zero} downgraded(<4MB)={small} full(>=4MB)={large}"
    );

    let (w0, h0) = {
        let p = doc.page(0).unwrap();
        (p.width() as u32, p.height() as u32)
    };
    let opts_s1 = djvu_rs::djvu_render::RenderOptions {
        width: w0,
        height: h0,
        ..Default::default()
    };
    let opts_s2 = djvu_rs::djvu_render::RenderOptions {
        width: w0 / 2,
        height: h0 / 2,
        ..Default::default()
    };
    // sub=4: the #607 tier — thumbnail-rail / zoomed-out renders that consume
    // the retained 1/4-res mask instead of re-running the JB2 decode.
    let opts_s4 = djvu_rs::djvu_render::RenderOptions {
        width: w0 / 4,
        height: h0 / 4,
        ..Default::default()
    };

    // Direct full-evict/downgrade convenience calls (not budget-mediated):
    // `downgrade_render_caches` deliberately preserves any already-cached
    // `bg_rgb_s2`, so across repeated trials the sub=2 measurement converges
    // to a *warm* hit for "downgrade" while "drop" stays cold every time —
    // exactly the tiers under test. First trial is a warm-up (populates
    // bg_rgb_s2 for the first time under "downgrade"); only trials after that
    // are included in the median.
    let force_state = |doc: &mut djvu_rs::DjVuDocument| match mode.as_str() {
        "drop" => doc.evict_render_caches(),
        "downgrade" => doc.downgrade_render_caches(),
        _ => {}
    };

    let trials = 12;
    let mut s1_times = Vec::with_capacity(trials);
    let mut s2_times = Vec::with_capacity(trials);
    let mut s4_times = Vec::with_capacity(trials);

    for trial in 0..trials {
        force_state(&mut doc);
        let t0 = Instant::now();
        let _ = djvu_rs::djvu_render::render_pixmap(doc.page(0).unwrap(), &opts_s1).unwrap();
        let dt0 = t0.elapsed().as_secs_f64() * 1000.0;

        force_state(&mut doc);
        let t1 = Instant::now();
        let _ = djvu_rs::djvu_render::render_pixmap(doc.page(0).unwrap(), &opts_s2).unwrap();
        let dt1 = t1.elapsed().as_secs_f64() * 1000.0;

        force_state(&mut doc);
        let t2 = Instant::now();
        let _ = djvu_rs::djvu_render::render_pixmap(doc.page(0).unwrap(), &opts_s4).unwrap();
        let dt2 = t2.elapsed().as_secs_f64() * 1000.0;

        if trial > 0 {
            s1_times.push(dt0);
            s2_times.push(dt1);
            s4_times.push(dt2);
        }
    }

    println!("sub1_render_after_evict_median_ms={:.3}", median(s1_times));
    println!("sub2_render_after_evict_median_ms={:.3}", median(s2_times));
    println!("sub4_render_after_evict_median_ms={:.3}", median(s4_times));
    println!("final_render_cache_bytes={final_cache}");
}
