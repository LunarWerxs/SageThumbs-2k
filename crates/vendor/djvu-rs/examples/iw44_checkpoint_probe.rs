//! IW44_CHECKPOINT probe (#608): is resuming a full decode from a cloned
//! post-chunk-0 `Iw44Image` state worth its clone cost and retained bytes?
//!
//! Per multi-chunk colour page: cold full decode, checkpoint construction
//! (chunk 0 + clone), resume decode (clone + chunks 1.. + to_rgb), and
//! byte-identity of the resulting RGB pixmaps.
//!
//! Run with:
//! ```sh
//! cargo run --release --example iw44_checkpoint_probe
//! ```

use std::time::Instant;

use djvu_rs::djvu_document::DjVuDocument;
use djvu_rs::iw44::Iw44Image;

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn integrated(path: &str) {
    // Integrated path: sub>=4 render warms bg44_partial, then measure the
    // full-resolution render (which now resumes from the checkpoint) vs a
    // cold full render on a fresh document.
    let data = std::fs::read(path).unwrap();
    let trials = 7;
    let mut warm = Vec::new();
    let mut cold = Vec::new();
    for _ in 0..trials {
        let doc = DjVuDocument::parse(&data).unwrap();
        let page = doc.page(0).unwrap();
        let (w, h) = (page.width() as u32, page.height() as u32);
        let s4 = djvu_rs::djvu_render::RenderOptions {
            width: w / 4,
            height: h / 4,
            ..Default::default()
        };
        let s1 = djvu_rs::djvu_render::RenderOptions {
            width: w,
            height: h,
            ..Default::default()
        };
        let _ = djvu_rs::djvu_render::render_pixmap(page, &s4).unwrap();
        let t0 = Instant::now();
        let _ = djvu_rs::djvu_render::render_pixmap(page, &s1).unwrap();
        warm.push(t0.elapsed().as_secs_f64() * 1000.0);

        let doc2 = DjVuDocument::parse(&data).unwrap();
        let page2 = doc2.page(0).unwrap();
        let t1 = Instant::now();
        let _ = djvu_rs::djvu_render::render_pixmap(page2, &s1).unwrap();
        cold.push(t1.elapsed().as_secs_f64() * 1000.0);
    }
    println!(
        "{path}: full render after sub4 warm-up {:.2}ms vs cold {:.2}ms ({:+.1}%)",
        median(warm.clone()),
        median(cold.clone()),
        100.0 * (median(warm) - median(cold.clone())) / median(cold)
    );
}

fn main() {
    for path in [
        "tests/corpus/watchmaker.djvu",
        "tests/fixtures/colorbook.djvu",
        "tests/corpus/conquete_paix.djvu",
        "tests/fixtures/carte.djvu",
    ] {
        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let doc = DjVuDocument::parse(&data).unwrap();
        let page = doc.page(0).unwrap();
        let chunks = page.bg44_chunks();
        if chunks.len() < 2 {
            println!("{path}: only {} BG44 chunk(s) — skipping", chunks.len());
            continue;
        }

        let trials = 9;
        // Cold full decode.
        let mut cold = Vec::new();
        let mut cold_rgb = None;
        for _ in 0..trials {
            let t0 = Instant::now();
            let mut img = Iw44Image::new();
            for c in &chunks {
                img.decode_chunk(c).unwrap();
            }
            let rgb = img.to_rgb().unwrap();
            cold.push(t0.elapsed().as_secs_f64() * 1000.0);
            cold_rgb = Some(rgb);
        }

        // Checkpoint: chunk 0 decode + clone cost.
        let mut ckpt = Iw44Image::new();
        ckpt.decode_chunk(chunks[0]).unwrap();
        let mut clone_ms = Vec::new();
        for _ in 0..trials {
            let t0 = Instant::now();
            let c = ckpt.clone();
            clone_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
            std::hint::black_box(c);
        }

        // Resume: clone + chunks 1.. + to_rgb.
        let mut resume = Vec::new();
        let mut resume_rgb = None;
        for _ in 0..trials {
            let t0 = Instant::now();
            let mut img = ckpt.clone();
            for c in &chunks[1..] {
                img.decode_chunk(c).unwrap();
            }
            let rgb = img.to_rgb().unwrap();
            resume.push(t0.elapsed().as_secs_f64() * 1000.0);
            resume_rgb = Some(rgb);
        }

        let identical = cold_rgb.as_ref().unwrap().data == resume_rgb.as_ref().unwrap().data;
        // Retained-bytes estimate: coefficient planes are full-size i16 after
        // chunk 0 (PlaneDecoder allocates up front) — measure via the image
        // dims: Y (w*h*2) + 2 chroma planes (full res per CARTE_CHROMA_HEADER).
        let (w, h) = (ckpt.width as usize, ckpt.height as usize);
        let ckpt_bytes = w * h * 2 * 3;
        println!(
            "{path}: chunks={} cold={:.2}ms resume={:.2}ms ({:+.1}%) clone={:.2}ms ckpt≈{:.1}MB identical={}",
            chunks.len(),
            median(cold.clone()),
            median(resume.clone()),
            100.0 * (median(resume) - median(cold.clone())) / median(cold),
            median(clone_ms),
            ckpt_bytes as f64 / 1048576.0,
            identical
        );
    }
    println!("-- integrated render path --");
    for path in [
        "tests/corpus/watchmaker.djvu",
        "tests/fixtures/colorbook.djvu",
        "tests/corpus/conquete_paix.djvu",
    ] {
        if std::path::Path::new(path).exists() {
            integrated(path);
        }
    }
}
