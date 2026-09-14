//! TRUE_MRC probe (#563): whole-file size + rasterized colour fidelity of the
//! opt-in `PdfOptions::mrc` path vs the default composited export.
//!
//! For each fixture: export both ways (default JPEG-80 raster policy),
//! rasterize with `pdftoppm` at native pixel dims (capped), compare each page
//! against `render_pixmap` via `quality::compare_color`.
//!
//! Run with:
//! ```sh
//! cargo run --release --features pdf --example pdf_mrc_probe [fixture.djvu ...]
//! ```

use std::process::Command;

use djvu_rs::Pixmap;
use djvu_rs::djvu_document::DjVuDocument;
use djvu_rs::djvu_render::{RenderOptions, render_pixmap};
use djvu_rs::pdf::{PdfOptions, djvu_to_pdf_with_options};
use djvu_rs::quality::compare_color;

/// Minimal binary-PPM (P6, maxval 255) parser → RGBA Pixmap.
fn parse_ppm(data: &[u8]) -> Option<Pixmap> {
    let mut fields = Vec::new();
    let mut pos = 0usize;
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
    pos += 1;
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

fn rasterize_and_compare(doc: &DjVuDocument, pdf: &[u8], tag: &str, tmp: &std::path::Path) {
    let pdf_path = tmp.join(format!("{tag}.pdf"));
    std::fs::write(&pdf_path, pdf).unwrap();
    let page0 = doc.page(0).unwrap();
    let (nw, nh) = (page0.width() as u32, page0.height() as u32);
    let (tw, th) = if nw > 2600 {
        (2600, (nh as u64 * 2600 / nw as u64) as u32)
    } else {
        (nw, nh)
    };
    let root = tmp.join(tag);
    let ok = Command::new("pdftoppm")
        .arg("-scale-to-x")
        .arg(tw.to_string())
        .arg("-scale-to-y")
        .arg(th.to_string())
        .arg(&pdf_path)
        .arg(&root)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        println!("  {tag}: pdftoppm failed");
        return;
    }
    let digits = doc.page_count().to_string().len();
    let mut des = Vec::new();
    let mut ys = Vec::new();
    for i in 0..doc.page_count() {
        let ppm = tmp.join(format!("{tag}-{:0width$}.ppm", i + 1, width = digits));
        let Ok(data) = std::fs::read(&ppm) else {
            continue;
        };
        let Some(theirs) = parse_ppm(&data) else {
            continue;
        };
        let Ok(page) = doc.page(i) else { continue };
        let Ok(ours) = render_pixmap(
            page,
            &RenderOptions {
                width: theirs.width,
                height: theirs.height,
                ..RenderOptions::default()
            },
        ) else {
            continue;
        };
        let r = compare_color(&ours, &theirs);
        des.push(r.delta_e_mean);
        ys.push(r.ssim_y);
    }
    let avg = |v: &[f64]| v.iter().sum::<f64>() / v.len().max(1) as f64;
    println!(
        "  {tag}: {} bytes, pages={} dE_mean(avg)={:.3} ssim_y(avg)={:.4}",
        pdf.len(),
        des.len(),
        avg(&des),
        avg(&ys)
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let fixtures = if args.is_empty() {
        vec![
            "tests/corpus/watchmaker.djvu".to_string(),
            "tests/fixtures/colorbook.djvu".to_string(),
            "tests/fixtures/irish.djvu".to_string(),
            "tests/fixtures/navm_fgbz.djvu".to_string(),
        ]
    } else {
        args
    };
    let tmp = std::env::temp_dir().join("pdf_mrc_probe");
    std::fs::create_dir_all(&tmp).unwrap();
    for f in &fixtures {
        let data = std::fs::read(f).unwrap_or_else(|_| panic!("read {f}"));
        let doc = DjVuDocument::parse(&data).expect("parse");
        println!("== {f} ==");
        let default_pdf = djvu_to_pdf_with_options(&doc, &PdfOptions::default()).unwrap();
        rasterize_and_compare(&doc, &default_pdf, "default", &tmp);
        let mrc_pdf = djvu_to_pdf_with_options(
            &doc,
            &PdfOptions {
                mrc: true,
                ..PdfOptions::default()
            },
        )
        .unwrap();
        rasterize_and_compare(&doc, &mrc_pdf, "mrc", &tmp);
        let delta =
            100.0 * (mrc_pdf.len() as f64 - default_pdf.len() as f64) / default_pdf.len() as f64;
        println!("  size delta: {delta:+.1}%");
    }
}
