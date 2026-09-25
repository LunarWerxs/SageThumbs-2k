//! FGBZ_PDF_STENCIL probe (#559): measure the colour fidelity of PDF export
//! against our own renderer on pages whose foreground text is coloured via an
//! FGbz palette.
//!
//! For each fixture: export to PDF (lossless background so the stencil is the
//! only colour-fidelity variable), rasterize with `pdftoppm` at the same DPI,
//! and compare each page against `render_pixmap` at identical dimensions using
//! `quality::compare_color` (ΔE + per-channel SSIM).
//!
//! Requires `pdftoppm` (poppler) on PATH.
//!
//! Run with:
//! ```sh
//! cargo run --release --example pdf_fg_color_probe [fixture.djvu ...]
//! ```

use std::process::Command;

use djvu_rs::Pixmap;
use djvu_rs::djvu_document::DjVuDocument;
use djvu_rs::djvu_render::{RenderOptions, render_pixmap};
use djvu_rs::pdf::{PdfOptions, djvu_to_pdf_with_options};
use djvu_rs::quality::compare_color;

const DPI: u32 = 300;

/// Minimal binary-PPM (P6, maxval 255) parser → RGBA Pixmap.
fn parse_ppm(data: &[u8]) -> Option<Pixmap> {
    let mut fields = Vec::new();
    let mut pos = 0usize;
    // Header: magic, width, height, maxval — whitespace-separated, # comments.
    while fields.len() < 4 && pos < data.len() {
        while pos < data.len() && data[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if pos < data.len() && data[pos] == b'#' {
            while pos < data.len() && data[pos] != b'\n' {
                pos += 1;
            }
            continue;
        }
        let start = pos;
        while pos < data.len() && !data[pos].is_ascii_whitespace() {
            pos += 1;
        }
        fields.push(&data[start..pos]);
    }
    if fields.len() < 4 || fields[0] != b"P6" {
        return None;
    }
    pos += 1; // single whitespace after maxval
    let w: u32 = std::str::from_utf8(fields[1]).ok()?.parse().ok()?;
    let h: u32 = std::str::from_utf8(fields[2]).ok()?.parse().ok()?;
    let rgb = data.get(pos..pos + w as usize * h as usize * 3)?;
    let mut out = Vec::with_capacity(w as usize * h as usize * 4);
    for px in rgb.as_chunks::<3>().0 {
        out.extend_from_slice(&[px[0], px[1], px[2], 255]);
    }
    Some(Pixmap {
        width: w,
        height: h,
        data: out,
    })
}

fn probe(path: &str, tmp_dir: &std::path::Path) {
    let data = std::fs::read(path).unwrap_or_else(|_| panic!("read {path}"));
    let doc = DjVuDocument::parse(&data).expect("parse");

    let opts = PdfOptions {
        jpeg_quality: None, // lossless background: stencil is the only variable
        output_dpi: DPI,
        ..PdfOptions::default()
    };
    let pdf = djvu_to_pdf_with_options(&doc, &opts).expect("pdf export");
    let stem = std::path::Path::new(path)
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let pdf_path = tmp_dir.join(format!("{stem}.pdf"));
    std::fs::write(&pdf_path, &pdf).expect("write pdf");

    // Rasterize at the page's native pixel dimensions (capped) rather than a
    // fixed -r: fixtures with bogus stored DPI (irish.djvu claims 1 dpi) have
    // enormous MediaBoxes that blow up a resolution-based rasterization.
    let page0 = doc.page(0).expect("page 0");
    let (nw, nh) = (page0.width() as u32, page0.height() as u32);
    let (tw, th) = if nw > 2600 {
        (2600, (nh as u64 * 2600 / nw as u64) as u32)
    } else {
        (nw, nh)
    };
    let ppm_root = tmp_dir.join(&stem);
    let status = Command::new("pdftoppm")
        .arg("-scale-to-x")
        .arg(tw.to_string())
        .arg("-scale-to-y")
        .arg(th.to_string())
        .arg(&pdf_path)
        .arg(&ppm_root)
        .status()
        .expect("run pdftoppm");
    assert!(status.success(), "pdftoppm failed for {path}");

    println!("== {stem} (pdf {} bytes) ==", pdf.len());
    for i in 0..doc.page_count() {
        let page = match doc.page(i) {
            Ok(p) => p,
            Err(_) => continue,
        };
        // pdftoppm numbers pages 1-based, zero-padded to the page-count width.
        let digits = doc.page_count().to_string().len();
        let ppm_path = tmp_dir.join(format!("{stem}-{:0width$}.ppm", i + 1, width = digits));
        let ppm_data = match std::fs::read(&ppm_path) {
            Ok(d) => d,
            Err(_) => {
                // Single-page docs get no suffix from some poppler versions.
                match std::fs::read(tmp_dir.join(format!("{stem}.ppm"))) {
                    Ok(d) => d,
                    Err(e) => {
                        println!("  page {i}: no ppm output ({e})");
                        continue;
                    }
                }
            }
        };
        let theirs = match parse_ppm(&ppm_data) {
            Some(p) => p,
            None => {
                println!("  page {i}: ppm parse failed");
                continue;
            }
        };
        let ours = match render_pixmap(
            page,
            &RenderOptions {
                width: theirs.width,
                height: theirs.height,
                ..RenderOptions::default()
            },
        ) {
            Ok(p) => p,
            Err(e) => {
                println!("  page {i}: render failed ({e:?})");
                continue;
            }
        };
        let has_fgbz = page.find_chunk(b"FGbz").is_some();
        let r = compare_color(&ours, &theirs);
        println!(
            "  page {i} ({}x{}, FGbz={}): dE_mean={:.3} dE_max={:.1} ssim_y={:.4} ssim_cb={:.4} ssim_cr={:.4}",
            theirs.width,
            theirs.height,
            has_fgbz,
            r.delta_e_mean,
            r.delta_e_max,
            r.ssim_y,
            r.ssim_cb,
            r.ssim_cr
        );
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let fixtures = if args.is_empty() {
        vec![
            "tests/fixtures/irish.djvu".to_string(),
            "tests/fixtures/navm_fgbz.djvu".to_string(),
        ]
    } else {
        args
    };
    let tmp_dir = std::env::temp_dir().join("pdf_fg_color_probe");
    std::fs::create_dir_all(&tmp_dir).expect("mkdir");
    for f in &fixtures {
        probe(f, &tmp_dir);
    }
    println!("(rasterized output in {})", tmp_dir.display());
}
