//! PDF_G4 probe: how much does `smmr::encode_g4`'s full T.6 (pass/vertical/
//! horizontal) G4/MMR bitstream win over the current Deflate-of-1-bit-raster
//! mask encoding used by `src/pdf.rs::collect_mask_stream`?
//!
//! For each page with an `Sjbz` chunk: decode the JB2 mask to a `Bitmap`, then
//! compare:
//! - raw packed size (`bitmap.data.len()`)
//! - Deflate (zlib level 6, matching pdf.rs) of the raw packed bytes
//! - G4 bitstream via `smmr::encode_g4` (PDF `CCITTFaxDecode`-ready payload,
//!   no in-band header)
//!
//! Run with:
//! ```sh
//! cargo run --release --example pdf_g4_probe
//! ```

use djvu_rs::djvu_document::DjVuDocument;

fn deflate(data: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec_zlib(data, 6)
}

fn probe(name: &str, path: &str) {
    let data = std::fs::read(path).unwrap_or_else(|_| panic!("read {path}"));
    let doc = DjVuDocument::parse(&data).expect("parse");
    let count = doc.page_count();

    let mut total_raw = 0usize;
    let mut total_deflate = 0usize;
    let mut total_g4 = 0usize;
    let mut total_adaptive = 0usize;
    let mut pages_with_mask = 0usize;
    let mut pages_g4_won = 0usize;

    for i in 0..count {
        let page = match doc.page(i) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let sjbz = match page.find_chunk(b"Sjbz") {
            Some(s) => s,
            None => continue,
        };
        let dict = page
            .find_chunk(b"Djbz")
            .and_then(|d| djvu_rs::jb2::decode_dict(d, None).ok());
        let bitmap = match djvu_rs::jb2::decode(sjbz, dict.as_ref()) {
            Ok(b) => b,
            Err(_) => continue,
        };

        let raw = bitmap.data.len();
        let defl = deflate(&bitmap.data).len();
        let g4 = djvu_rs::smmr::encode_g4(&bitmap).len();

        total_raw += raw;
        total_deflate += defl;
        total_g4 += g4;
        total_adaptive += defl.min(g4);
        pages_with_mask += 1;
        if g4 < defl {
            pages_g4_won += 1;
        }
    }

    println!(
        "{name:>28}: pages={pages_with_mask:>4} (g4 won {pages_g4_won:>4})  raw={total_raw:>9}  deflate={total_deflate:>9}  g4={total_g4:>9}  adaptive_min={total_adaptive:>9}  deflate/adaptive={:.2}x",
        total_deflate as f64 / total_adaptive.max(1) as f64
    );
}

fn main() {
    probe("cable_1973_100133", "tests/corpus/cable_1973_100133.djvu");
    probe(
        "pathogenic_bacteria_1896",
        "tests/corpus/pathogenic_bacteria_1896.djvu",
    );
    probe("watchmaker", "tests/corpus/watchmaker.djvu");
    probe("conquete_paix", "tests/corpus/conquete_paix.djvu");
}
