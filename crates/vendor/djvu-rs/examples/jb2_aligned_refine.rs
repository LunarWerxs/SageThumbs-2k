//! Center-aligned record-4/6 refinement probe: real emitted Sjbz bytes and a
//! pixel-exact round-trip against the default lossless encoder. Run:
//!   cargo run --release --features experimental --example jb2_aligned_refine [max_pages]
use djvu_rs::jb2_encode::{
    AlignedRefine, Jb2EncodeOptions, encode_jb2, encode_jb2_dict_with_options, encode_jb2_lossless,
};

fn masks(path: &str, max_pages: usize) -> Vec<djvu_rs::Bitmap> {
    let data = std::fs::read(path).expect("read corpus file");
    let doc = djvu_rs::DjVuDocument::parse(&data).expect("parse");
    (0..doc.page_count().min(max_pages))
        .filter_map(|i| doc.page(i).ok()?.extract_mask().ok()?)
        .collect()
}

fn total(masks: &[djvu_rs::Bitmap], opts: &Jb2EncodeOptions) -> (usize, usize) {
    let mut bytes = 0;
    let mut fail = 0;
    for m in masks {
        let enc = encode_jb2_dict_with_options(m, &[], opts);
        bytes += enc.len();
        match djvu_rs::jb2::decode(&enc, None) {
            Ok(d) if d.width == m.width && d.height == m.height && d.data == m.data => {}
            _ => fail += 1,
        }
    }
    (bytes, fail)
}

fn main() {
    let max_pages: usize = std::env::args().nth(1).map_or(8, |a| a.parse().unwrap());
    let files = [
        "tests/corpus/cable_1973_100133.djvu",
        "tests/corpus/chinese_cookbook_sample.djvu",
        "tests/corpus/watchmaker.djvu",
        "tests/corpus/conquete_paix.djvu",
        "tests/corpus/pathogenic_bacteria_1896.djvu",
        "tests/corpus/war_1812.djvu",
    ];
    for path in files {
        let ms = masks(path, max_pages);
        let t = std::time::Instant::now();
        let (base, _) = total(&ms, &Jb2EncodeOptions::default());
        println!(
            "== {path} ({} masks) baseline {base} B  {:?}",
            ms.len(),
            t.elapsed()
        );
        let direct: usize = ms.iter().map(|m| encode_jb2(m).len()).sum();
        let lossless: usize = ms.iter().map(|m| encode_jb2_lossless(m).len()).sum();
        println!("  direct tiles {direct} B, encode_jb2_lossless {lossless} B");
        for add_to_dict in [true, false] {
            for max_dim_delta in [1u32, 2, 3] {
                for frac in [0.10f32, 0.20, 0.30] {
                    let opts = Jb2EncodeOptions {
                        aligned_refine: Some(AlignedRefine {
                            max_dim_delta,
                            max_hamming_fraction: frac,
                            add_to_dict,
                        }),
                        ..Jb2EncodeOptions::default()
                    };
                    let t = std::time::Instant::now();
                    let (b, fail) = total(&ms, &opts);
                    println!(
                        "  rec{} d={max_dim_delta} frac={:>2.0}%: {b:>8} B {:+6.2}%  rt_fail={fail}  {:?}",
                        if add_to_dict { 4 } else { 6 },
                        frac * 100.0,
                        100.0 * (b as f64 - base as f64) / base as f64,
                        t.elapsed()
                    );
                }
            }
        }
    }
}
