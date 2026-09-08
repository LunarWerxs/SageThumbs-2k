//! Profiling harness for IW44 reconstruct (scatter + wavelet + YCbCr).
//!
//! Decodes colorbook.djvu page 0 in a tight loop so that samply / Instruments
//! can collect meaningful samples and show the scatter-vs-wavelet split.
//!
//! Usage:
//!   cargo build --release --example profile_iw44
//!   samply record ./target/release/examples/profile_iw44

fn main() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("references/djvujs/library/assets/colorbook.djvu");

    let data = std::fs::read(&path).unwrap_or_else(|_| {
        eprintln!("ERROR: {} not found", path.display());
        std::process::exit(1);
    });

    let doc = djvu_rs::DjVuDocument::parse(&data).expect("parse failed");
    let page = doc.page(0).expect("page 0 not found");

    // Pre-decode all BG44 chunks (ZP decode) — benchmark only reconstruct.
    let chunks: Vec<Vec<u8>> = page
        .bg44_chunks()
        .iter()
        .map(|s: &&[u8]| s.to_vec())
        .collect();
    if chunks.is_empty() {
        eprintln!("ERROR: no BG44 chunks found");
        std::process::exit(1);
    }

    let mut img = djvu_rs::iw44::Iw44Image::new();
    for chunk in &chunks {
        img.decode_chunk(chunk).expect("decode_chunk failed");
    }

    // Warm up
    for _ in 0..3 {
        let _ = std::hint::black_box(img.to_rgb());
    }

    // Hot loop — enough iterations for stable samply samples (~8 s)
    let iters = 500;
    let t0 = std::time::Instant::now();
    for _ in 0..iters {
        let _ = std::hint::black_box(img.to_rgb());
    }
    let elapsed = t0.elapsed();
    eprintln!(
        "{iters} iters in {:.1}ms ({:.2}ms/iter)",
        elapsed.as_secs_f64() * 1000.0,
        elapsed.as_secs_f64() * 1000.0 / iters as f64,
    );
}
