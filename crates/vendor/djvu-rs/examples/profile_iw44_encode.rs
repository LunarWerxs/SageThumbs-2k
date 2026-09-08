//! Profiling harness for IW44 encode.
//!
//! Renders boy.djvu page 0 to a Pixmap, then encodes it in a tight loop
//! so that samply / Instruments can collect meaningful samples.
//!
//! Usage:
//!   cargo build --release --example profile_iw44_encode
//!   samply record ./target/release/examples/profile_iw44_encode

fn main() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("references/djvujs/library/assets/boy.djvu");

    let data = std::fs::read(&path).unwrap_or_else(|_| {
        eprintln!("ERROR: {} not found", path.display());
        std::process::exit(1);
    });

    let doc = djvu_rs::DjVuDocument::parse(&data).expect("parse failed");
    let page = doc.page(0).expect("page 0 not found");

    let opts = djvu_rs::djvu_render::RenderOptions {
        width: page.width() as u32,
        height: page.height() as u32,
        ..Default::default()
    };
    let pixmap = djvu_rs::djvu_render::render_pixmap(page, &opts).expect("render failed");

    eprintln!("Encoding {}×{} pixmap", pixmap.width, pixmap.height);

    let enc_opts = djvu_rs::iw44_encode::Iw44EncodeOptions::default();

    // Warm up
    for _ in 0..3 {
        let _ = std::hint::black_box(djvu_rs::iw44_encode::encode_iw44_color(&pixmap, &enc_opts));
    }

    // Hot loop — enough iterations for stable samply samples (~5-8 s)
    let iters = 200;
    let t0 = std::time::Instant::now();
    for _ in 0..iters {
        let _ = std::hint::black_box(djvu_rs::iw44_encode::encode_iw44_color(&pixmap, &enc_opts));
    }
    let elapsed = t0.elapsed();
    eprintln!(
        "{iters} iters in {:.1}ms ({:.2}ms/iter)",
        elapsed.as_secs_f64() * 1000.0,
        elapsed.as_secs_f64() * 1000.0 / iters as f64,
    );
}
