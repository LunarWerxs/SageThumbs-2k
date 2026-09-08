//! `BZZ_ENC_DIAG` diagnostic (see `PERF_EXPERIMENTS.md`): how big is the BZZ
//! encoder's share of a real per-page document encode, and how does
//! `bzz_encode`'s suffix-sort scale on realistic (non-synthetic) OCR text?
//!
//! Run with:
//! ```sh
//! cargo run --release --example bzz_encode_diag
//! ```
//!
//! Three measurements:
//! 1. Corpus census — how big are real `TXTz` plaintext payloads in practice?
//! 2. Scaling of `bzz_encode` on real (non-tiled) concatenated OCR text from
//!    10 KB up to ~2.25 MB — checks the suffix-sort's complexity class against
//!    real content (tiling a single block to reach size creates artificial
//!    periodicity that is not representative and was avoided).
//! 3. Same-session share of `bzz_encode` (text layer) vs a full per-page
//!    `PageEncoder` encode, for both a colour `Quality` page and a bilevel
//!    `Lossless` page (full native-resolution JB2 mask).

use std::time::Instant;

fn find_chunk_data(chunks: &[djvu_rs::iff::Chunk], target: &[u8; 4], out: &mut Vec<Vec<u8>>) {
    for chunk in chunks {
        match chunk {
            djvu_rs::iff::Chunk::Leaf { id, data } if id == target => out.push(data.clone()),
            djvu_rs::iff::Chunk::Form { children, .. } => find_chunk_data(children, target, out),
            _ => {}
        }
    }
}

/// Decode all `TXTz` chunks in a document, returning (compressed_total, plaintext).
fn collect_plain_text(path: &str) -> (usize, Vec<u8>) {
    let Ok(data) = std::fs::read(path) else {
        return (0, vec![]);
    };
    let Ok(file) = djvu_rs::iff::parse(&data) else {
        return (0, vec![]);
    };
    let mut txtz = vec![];
    find_chunk_data(file.root.children(), b"TXTz", &mut txtz);
    let mut total_comp = 0usize;
    let mut plain_all = vec![];
    for chunk in &txtz {
        total_comp += chunk.len();
        if let Ok(plain) = djvu_rs::bzz::bzz_decode(chunk) {
            plain_all.extend_from_slice(&plain);
        }
    }
    (total_comp, plain_all)
}

fn section1_corpus_census() -> Vec<u8> {
    println!("=== 1. Real TXTz payload census (7 corpus docs) ===");
    let docs = [
        "references/djvujs/library/assets/malliavin.djvu",
        "references/djvujs/library/assets/czech.djvu",
        "references/djvujs/library/assets/DjVu3Spec_bundled.djvu",
        "references/djvujs/library/assets/colorbook.djvu",
        "tests/corpus/watchmaker.djvu",
        "tests/corpus/conquete_paix.djvu",
        "tests/corpus/cable_1973_100133.djvu",
    ];
    let mut combined = vec![];
    for path in docs {
        let (comp, mut text) = collect_plain_text(path);
        println!(
            "  {path}: TXTz compressed={comp}B plaintext={}B",
            text.len()
        );
        combined.append(&mut text);
    }
    println!("  total real OCR plaintext gathered: {}B\n", combined.len());
    combined
}

fn section2_scaling(combined: &[u8]) {
    println!("=== 2. bzz_encode scaling on real (non-tiled) OCR text prefixes ===");
    let sizes = [10_000usize, 100_000, 500_000, combined.len()];
    let mut prev_time: Option<f64> = None;
    let mut prev_n: Option<usize> = None;
    for &target in &sizes {
        if target > combined.len() {
            continue;
        }
        let buf = &combined[..target];
        let iters = if target <= 100_000 { 5 } else { 2 };
        let start = Instant::now();
        let mut out_len = 0;
        for _ in 0..iters {
            out_len = djvu_rs::bzz_encode::bzz_encode(std::hint::black_box(buf)).len();
        }
        let elapsed = start.elapsed().as_secs_f64() / iters as f64;
        let ratio_time = prev_time.map(|p| elapsed / p);
        let ratio_n = prev_n.map(|p| target as f64 / p as f64);
        println!(
            "  n={:>8}B -> compressed={:>8}B ({:.1}%) time={:.4}s  ratio_n={:.2?} ratio_time={:.2?}",
            target,
            out_len,
            out_len as f64 / target as f64 * 100.0,
            elapsed,
            ratio_n,
            ratio_time,
        );
        prev_time = Some(elapsed);
        prev_n = Some(target);
    }
    println!();
}

fn section3_share() {
    println!("=== 3. Same-session share: bzz_encode(text) vs full page encode ===");

    // Colour `Quality` page (JB2-dict mask + IW44 BG + FGbz).
    {
        let path = "references/djvujs/library/assets/colorbook.djvu";
        let data = std::fs::read(path).expect("read colorbook.djvu");
        let doc = djvu_rs::DjVuDocument::parse(&data).expect("parse");
        let page = doc.page(0).expect("page 0");
        let chunks: Vec<Vec<u8>> = page.bg44_chunks().iter().map(|s| s.to_vec()).collect();
        let mut img = djvu_rs::iw44::Iw44Image::new();
        for chunk in &chunks {
            img.decode_chunk(chunk).expect("decode bg44");
        }
        let pm = img.to_rgb().expect("to_rgb");

        let iters = 20;
        let start = Instant::now();
        for _ in 0..iters {
            let _ = std::hint::black_box(
                djvu_rs::djvu_encode::PageEncoder::from_pixmap(std::hint::black_box(&pm))
                    .with_quality(djvu_rs::djvu_encode::EncodeQuality::Quality)
                    .encode(),
            );
        }
        let full_page_time = start.elapsed().as_secs_f64() / iters as f64;

        let (_, plain_all) = collect_plain_text("references/djvujs/library/assets/malliavin.djvu");
        let avg_len = plain_all.len() / 114; // malliavin has 114 TXTz chunks
        let buf = &plain_all[..avg_len.min(plain_all.len())];
        let iters2 = 500;
        let start2 = Instant::now();
        for _ in 0..iters2 {
            let _ =
                std::hint::black_box(djvu_rs::bzz_encode::bzz_encode(std::hint::black_box(buf)));
        }
        let bzz_time = start2.elapsed().as_secs_f64() / iters2 as f64;

        println!(
            "  colorbook.djvu p0 ({}x{}) Quality encode: {:.3} ms/page; avg TXTz bzz_encode ({}B): {:.4} ms -> share {:.2}%",
            pm.width,
            pm.height,
            full_page_time * 1000.0,
            buf.len(),
            bzz_time * 1000.0,
            bzz_time / full_page_time * 100.0
        );
    }

    // Bilevel `Lossless` page (JB2 mask only, native resolution).
    {
        let path = "tests/corpus/cable_1973_100133.djvu";
        let data = std::fs::read(path).expect("read");
        let file = djvu_rs::iff::parse(&data).unwrap();
        let mut sjbz = vec![];
        find_chunk_data(file.root.children(), b"Sjbz", &mut sjbz);
        let bitmap = djvu_rs::jb2::decode(&sjbz[0], None).expect("decode Sjbz");

        let iters = 20;
        let start = Instant::now();
        for _ in 0..iters {
            let _ = std::hint::black_box(
                djvu_rs::djvu_encode::PageEncoder::from_bitmap(std::hint::black_box(&bitmap))
                    .with_quality(djvu_rs::djvu_encode::EncodeQuality::Lossless)
                    .encode(),
            );
        }
        let full_page_time = start.elapsed().as_secs_f64() / iters as f64;

        let mut txtz = vec![];
        find_chunk_data(file.root.children(), b"TXTz", &mut txtz);
        let mut plain_all = vec![];
        for chunk in &txtz {
            if let Ok(p) = djvu_rs::bzz::bzz_decode(chunk) {
                plain_all.extend_from_slice(&p);
            }
        }
        let avg_len = plain_all.len() / txtz.len().max(1);
        let buf = &plain_all[..avg_len.min(plain_all.len())];

        let iters2 = 500;
        let start2 = Instant::now();
        for _ in 0..iters2 {
            let _ =
                std::hint::black_box(djvu_rs::bzz_encode::bzz_encode(std::hint::black_box(buf)));
        }
        let bzz_time = start2.elapsed().as_secs_f64() / iters2 as f64;

        println!(
            "  cable_1973_100133.djvu p0 ({}x{}) Lossless encode: {:.3} ms/page; own TXTz bzz_encode ({}B): {:.4} ms -> share {:.2}%",
            bitmap.width,
            bitmap.height,
            full_page_time * 1000.0,
            buf.len(),
            bzz_time * 1000.0,
            bzz_time / full_page_time * 100.0
        );
    }
}

fn main() {
    let combined = section1_corpus_census();
    section2_scaling(&combined);
    section3_share();
}
