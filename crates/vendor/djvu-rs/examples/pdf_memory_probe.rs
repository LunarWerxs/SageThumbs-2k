//! PDF export timing and memory probe.
//!
//! Usage:
//!   cargo run --release --example pdf_memory_probe -- tests/corpus/watchmaker.djvu
//!   cargo run --release --features parallel --example pdf_memory_probe -- tests/corpus/watchmaker.djvu
//!
//! For peak RSS, wrap the command with the platform time tool, for example:
//!   /usr/bin/time -l cargo run --release --example pdf_memory_probe -- tests/corpus/watchmaker.djvu
//!   /usr/bin/time -v cargo run --release --example pdf_memory_probe -- tests/corpus/watchmaker.djvu

use std::path::PathBuf;
use std::time::Instant;

use djvu_rs::djvu_document::DjVuDocument;
use djvu_rs::djvu_render::{RenderOptions, render_pixmap};
use djvu_rs::pdf::djvu_to_pdf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let path = args.next().map(PathBuf::from).ok_or_else(|| {
        "usage: pdf_memory_probe <file.djvu> [page-index-for-breakdown]".to_string()
    })?;
    let page_index = args
        .next()
        .map(|v| v.to_string_lossy().parse::<usize>())
        .transpose()?
        .unwrap_or(0);

    let read_start = Instant::now();
    let data = std::fs::read(&path)?;
    let read_ms = read_start.elapsed().as_secs_f64() * 1000.0;

    let parse_start = Instant::now();
    let doc = DjVuDocument::parse(&data)?;
    let parse_ms = parse_start.elapsed().as_secs_f64() * 1000.0;

    let page_count = doc.page_count();
    let page = doc.page(page_index)?;
    let (rw, rh) = render_dims(
        u32::from(page.width()),
        u32::from(page.height()),
        u32::from(page.dpi().max(1)),
        150,
    );

    let (render_ms, rgba_bytes, rgb_ms, rgb_bytes, jpeg_ms, jpeg_len) = {
        let render_start = Instant::now();
        let pixmap = render_pixmap(
            page,
            &RenderOptions {
                width: rw,
                height: rh,
                ..RenderOptions::default()
            },
        )?;
        let render_ms = render_start.elapsed().as_secs_f64() * 1000.0;
        let rgba_bytes = pixmap.data.len();

        let rgb_start = Instant::now();
        let rgb = pixmap.to_rgb();
        let rgb_ms = rgb_start.elapsed().as_secs_f64() * 1000.0;
        let rgb_bytes = rgb.len();

        let jpeg_start = Instant::now();
        let jpeg_len = encode_rgb_to_jpeg_len(&rgb, rw, rh, 80);
        let jpeg_ms = jpeg_start.elapsed().as_secs_f64() * 1000.0;

        (render_ms, rgba_bytes, rgb_ms, rgb_bytes, jpeg_ms, jpeg_len)
    };

    let pdf_start = Instant::now();
    let pdf = djvu_to_pdf(&doc)?;
    let pdf_ms = pdf_start.elapsed().as_secs_f64() * 1000.0;

    println!("file={}", path.display());
    println!("input_bytes={}", data.len());
    println!("pages={page_count}");
    println!("read_ms={read_ms:.3}");
    println!("parse_ms={parse_ms:.3}");
    println!("breakdown_page={page_index}");
    println!("render_dims={}x{}", rw, rh);
    println!("render_pixmap_ms={render_ms:.3}");
    println!("rgba_bytes={rgba_bytes}");
    println!("rgb_stage_ms={rgb_ms:.3}");
    println!("rgb_bytes={rgb_bytes}");
    println!("jpeg_stage_ms={jpeg_ms:.3}");
    println!("jpeg_bytes={jpeg_len}");
    println!("pdf_export_ms={pdf_ms:.3}");
    println!("pdf_bytes={}", pdf.len());
    println!(
        "parallel_feature={}",
        if cfg!(feature = "parallel") {
            "enabled"
        } else {
            "disabled"
        }
    );

    Ok(())
}

fn render_dims(width: u32, height: u32, page_dpi: u32, output_dpi: u32) -> (u32, u32) {
    let target = if output_dpi == 0 {
        page_dpi
    } else {
        output_dpi
    };
    if target == page_dpi {
        return (width, height);
    }
    let scale = target as f64 / page_dpi as f64;
    (
        ((width as f64 * scale).round() as u32).max(1),
        ((height as f64 * scale).round() as u32).max(1),
    )
}

fn encode_rgb_to_jpeg_len(rgb: &[u8], width: u32, height: u32, quality: u8) -> usize {
    let mut jpeg = Vec::new();
    let encoder = jpeg_encoder::Encoder::new(&mut jpeg, quality);
    let _ = encoder.encode(
        rgb,
        width as u16,
        height as u16,
        jpeg_encoder::ColorType::Rgb,
    );
    jpeg.len()
}
