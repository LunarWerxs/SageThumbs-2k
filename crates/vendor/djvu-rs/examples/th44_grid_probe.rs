//! TH44 thumbnail probe (#590): per-page TH44 cost and thumbnail-grid decode
//! speed on a bundle encoded with vs without `--thumbnails`.
//!
//! ```sh
//! cargo run --release --example th44_grid_probe -- without.djvu with.djvu
//! ```

use std::time::Instant;

use djvu_rs::Document;

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn grid_ms(data: &[u8]) -> (f64, usize) {
    let mut times = Vec::new();
    let mut n = 0;
    for _ in 0..9 {
        let doc = Document::from_bytes(data.to_vec()).unwrap();
        let t0 = Instant::now();
        let thumbs = doc.thumbnails(128, 128);
        times.push(t0.elapsed().as_secs_f64() * 1000.0);
        n = thumbs.len();
        assert!(thumbs.iter().all(|t| t.is_ok()));
    }
    (median(times), n)
}

fn th44_bytes(data: &[u8]) -> (usize, usize) {
    // Sum TH44 chunk payloads by scanning the raw IFF (id at even offsets).
    let mut total = 0usize;
    let mut count = 0usize;
    let mut i = 0usize;
    while i + 8 <= data.len() {
        if &data[i..i + 4] == b"TH44" {
            let len =
                u32::from_be_bytes([data[i + 4], data[i + 5], data[i + 6], data[i + 7]]) as usize;
            total += len + 8;
            count += 1;
            i += 8 + len + (len & 1);
        } else {
            i += 2;
        }
    }
    (total, count)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (without, with) = (&args[0], &args[1]);
    let a = std::fs::read(without).unwrap();
    let b = std::fs::read(with).unwrap();
    let (ta, na) = grid_ms(&a);
    let (tb, nb) = grid_ms(&b);
    let (th_bytes, th_count) = th44_bytes(&b);
    println!(
        "without TH44: {} bytes, grid({na}) {ta:.2} ms\nwith TH44:    {} bytes (+{:.1}%), grid({nb}) {tb:.2} ms ({:.1}x faster)\nTH44 payload: {th_bytes} bytes in {th_count} chunks ({:.0} B/page, {:.2}% of bundle)",
        a.len(),
        b.len(),
        100.0 * (b.len() as f64 - a.len() as f64) / a.len() as f64,
        ta / tb,
        th_bytes as f64 / th_count.max(1) as f64,
        100.0 * th_bytes as f64 / b.len() as f64,
    );
}
