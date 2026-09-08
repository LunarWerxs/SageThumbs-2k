//! Phase A3 follow-up of docs/jb2-size-gap-plan.md (JB2_AUTO_REC6) — validate
//! the adaptive same-size rec-6 **auto-policy**: probe once per document
//! (first page), reuse the resulting `Jb2EncodeOptions` for every page, and
//! confirm it reproduces the round-17/18 lossless win on text while staying
//! off (byte-identical to the default encoder) on noisy scans.
//! Run: cargo run --release --features experimental --example jb2_same_size_a3_auto
use djvu_rs::jb2_encode::{Jb2EncodeOptions, encode_jb2_dict_with_options};

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
    let mut masks = Vec::new();
    for i in 0..n {
        if let Ok(page) = doc.page(i)
            && let Ok(Some(m)) = page.extract_mask()
        {
            masks.push(m);
        }
    }
    let Some(first) = masks.first() else {
        println!("{path}: no bilevel pages");
        return;
    };

    // One probe decision per document, reused for every page.
    let opts = Jb2EncodeOptions::same_size_rec6_auto(first, &[]);
    println!(
        "== {} ({} masks) == auto decision: same_size_rec6 = {:?}",
        path,
        masks.len(),
        opts.same_size_rec6
    );

    let base_opts = Jb2EncodeOptions::default();
    let mut base_total = 0usize;
    let mut auto_total = 0usize;
    let (mut rt_ok, mut rt_fail) = (0usize, 0usize);
    let mut identical_to_default = true;
    for m in &masks {
        let base = encode_jb2_dict_with_options(m, &[], &base_opts);
        let auto = encode_jb2_dict_with_options(m, &[], &opts);
        if base != auto {
            identical_to_default = false;
        }
        base_total += base.len();
        auto_total += auto.len();
        match djvu_rs::jb2::decode(&auto, None) {
            Ok(dec) if dec.width == m.width && dec.height == m.height && dec.data == m.data => {
                rt_ok += 1
            }
            _ => rt_fail += 1,
        }
    }
    let delta = auto_total as i64 - base_total as i64;
    println!(
        "  baseline={base_total} B  auto={auto_total} B  delta={delta:+} B ({:+.2}%)  round-trip: {rt_ok} exact / {rt_fail} FAIL  byte-identical-to-default={identical_to_default}",
        100.0 * delta as f64 / base_total.max(1) as f64
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
