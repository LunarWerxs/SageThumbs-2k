//! `PDF_DCT_PROBE` diagnostic (see `PERF_EXPERIMENTS.md`): how much does the
//! *already-shipped* DCTDecode (JPEG) default (`PdfOptions::default().jpeg_quality
//! == Some(80)`, landed in #59) actually win over the Deflate alternative on real
//! corpus colour pages, and is quality 85 a better operating point?
//!
//! Run with:
//! ```sh
//! cargo run --release --example pdf_dct_probe
//! ```
//!
//! For each page: renders native-resolution RGB, encodes it both as Deflate
//! (zlib level 6, matching `src/pdf.rs`'s private `deflate()`) and as JPEG at
//! quality 80/85 (matching `encode_rgb_to_jpeg`), round-trips the JPEG through
//! `zune-jpeg` (already a `std`-feature dependency), and scores the decoded
//! pixels against the original render with `src/quality::ssim`.
//!
//! Finding: the win is content-dependent. Photographic/gradient-rich scans win
//! big from JPEG (3.6x-11.2x smaller than Deflate); a near-flat colour-mode
//! scan of mostly white paper + text loses (JPEG ends up 3.1x LARGER than
//! Deflate would be, at no SSIM benefit). See PERF_EXPERIMENTS.md for the
//! recommended adaptive-per-page follow-up.

use std::time::Instant;

fn rgba_to_rgb(rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len() / 4 * 3);
    for px in rgba.chunks_exact(4) {
        out.extend_from_slice(&px[..3]);
    }
    out
}

fn rgb_to_pixmap(rgb: &[u8], w: u32, h: u32) -> djvu_rs::Pixmap {
    let mut data = Vec::with_capacity(rgb.len() / 3 * 4);
    for px in rgb.chunks_exact(3) {
        data.extend_from_slice(px);
        data.push(255);
    }
    djvu_rs::Pixmap {
        width: w,
        height: h,
        data,
    }
}

fn decode_jpeg_rgb(jpeg: &[u8]) -> (u32, u32, Vec<u8>) {
    use zune_jpeg::JpegDecoder;
    use zune_jpeg::zune_core::bytestream::ZCursor;
    let cursor = ZCursor::new(jpeg);
    let mut decoder = JpegDecoder::new(cursor);
    decoder.decode_headers().expect("decode_headers");
    let info = decoder.info().expect("info");
    let rgb = decoder.decode().expect("decode");
    (info.width as u32, info.height as u32, rgb)
}

fn probe_page(name: &str, path: &str) {
    let data = std::fs::read(path).unwrap_or_else(|_| panic!("read {path}"));
    let doc = djvu_rs::DjVuDocument::parse(&data).expect("parse");
    let page = doc.page(0).expect("page 0");
    let w = page.width() as u32;
    let h = page.height() as u32;
    let opts = djvu_rs::djvu_render::RenderOptions {
        width: w,
        height: h,
        ..Default::default()
    };
    let pixmap = djvu_rs::djvu_render::render_pixmap(page, &opts).expect("render");
    let rgb = rgba_to_rgb(&pixmap.data);

    // Current shipped path: Deflate (zlib level 6, matches src/pdf.rs `deflate()`).
    let t0 = Instant::now();
    let deflate_bytes = miniz_oxide::deflate::compress_to_vec_zlib(&rgb, 6);
    let deflate_time = t0.elapsed();

    // Current shipped DEFAULT: DCTDecode (JPEG) at quality 80 (PdfOptions::default()).
    let t1 = Instant::now();
    let mut jpeg80 = Vec::new();
    {
        let enc = jpeg_encoder::Encoder::new(&mut jpeg80, 80);
        enc.encode(&rgb, w as u16, h as u16, jpeg_encoder::ColorType::Rgb)
            .expect("jpeg80 encode");
    }
    let jpeg80_time = t1.elapsed();

    // Requested comparison point: quality 85.
    let mut jpeg85 = Vec::new();
    {
        let enc = jpeg_encoder::Encoder::new(&mut jpeg85, 85);
        enc.encode(&rgb, w as u16, h as u16, jpeg_encoder::ColorType::Rgb)
            .expect("jpeg85 encode");
    }

    let (dw, dh, dec_rgb) = decode_jpeg_rgb(&jpeg80);
    assert_eq!((dw, dh), (w, h));
    let dec_pixmap80 = rgb_to_pixmap(&dec_rgb, dw, dh);
    let ssim80 = djvu_rs::quality::ssim(&pixmap, &dec_pixmap80);

    let (_, _, dec_rgb85) = decode_jpeg_rgb(&jpeg85);
    let dec_pixmap85 = rgb_to_pixmap(&dec_rgb85, w, h);
    let ssim85 = djvu_rs::quality::ssim(&pixmap, &dec_pixmap85);

    println!("=== {name} ({w}x{h}) ===");
    println!("  raw RGB:        {:>10} B", rgb.len());
    println!(
        "  Deflate (zlib6): {:>10} B   ({:.2}x raw)   {:.2} ms",
        deflate_bytes.len(),
        rgb.len() as f64 / deflate_bytes.len() as f64,
        deflate_time.as_secs_f64() * 1000.0
    );
    println!(
        "  JPEG q80 (default): {:>7} B   ({:.2}x raw, {:.2}x smaller than Deflate)  SSIM={:.4}  {:.2} ms",
        jpeg80.len(),
        rgb.len() as f64 / jpeg80.len() as f64,
        deflate_bytes.len() as f64 / jpeg80.len() as f64,
        ssim80,
        jpeg80_time.as_secs_f64() * 1000.0
    );
    println!(
        "  JPEG q85:        {:>10} B   ({:.2}x raw, {:.2}x smaller than Deflate)  SSIM={:.4}",
        jpeg85.len(),
        rgb.len() as f64 / jpeg85.len() as f64,
        deflate_bytes.len() as f64 / jpeg85.len() as f64,
        ssim85
    );
    println!();
}

fn main() {
    probe_page(
        "colorbook page0",
        "references/djvujs/library/assets/colorbook.djvu",
    );
    probe_page("watchmaker page0", "tests/corpus/watchmaker.djvu");
    probe_page(
        "big-scanned-page page0",
        "references/djvujs/library/assets/big-scanned-page.djvu",
    );
}
