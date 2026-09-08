//! Reproducible memory probe for the TIFF row-streaming export path.
//!
//! Build once, then run under the platform's time/RSS tool, for example:
//!
//! ```text
//! cargo build --release --features tiff --example probe_tiff_streaming_memory
//! /usr/bin/time -l target/release/examples/probe_tiff_streaming_memory \
//!   tests/fixtures/problem_page.djvu /tmp/problem_page_streamed.tiff 1.0
//! ```
//!
//! On Linux, use `/usr/bin/time -v` instead of `-l`.

// Names the deprecated, now-ignored `RenderOptions::scale` field (#377); allowed
// so this probe keeps building while the back-compat field exists.
#![allow(deprecated)]

use std::{env, error::Error, fs::File, path::PathBuf};

use djvu_rs::{
    djvu_document::DjVuDocument,
    tiff_export::{TiffOptions, djvu_to_tiff_writer},
};

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = env::args_os().skip(1);
    let input = args.next().map(PathBuf::from).ok_or_else(usage)?;
    let output = args.next().map(PathBuf::from).ok_or_else(usage)?;
    let scale = args
        .next()
        .map(|s| s.to_string_lossy().parse::<f32>())
        .transpose()?
        .unwrap_or(1.0);

    let data = std::fs::read(&input)?;
    let doc = DjVuDocument::parse(&data)?;

    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }

    let mut total_pixels = 0u64;
    for i in 0..doc.page_count() {
        let page = doc.page(i)?;
        let w = ((page.width() as f32 * scale).round() as u32).max(1);
        let h = ((page.height() as f32 * scale).round() as u32).max(1);
        total_pixels += u64::from(w) * u64::from(h);
        println!(
            "page {}: {}x{} px at scale {:.3} (source dpi {})",
            i + 1,
            w,
            h,
            scale,
            page.dpi()
        );
    }

    let file = File::create(&output)?;
    djvu_to_tiff_writer(
        &doc,
        &TiffOptions {
            scale,
            ..TiffOptions::default()
        },
        file,
    )?;

    let out_bytes = std::fs::metadata(&output)?.len();
    println!("pages: {}", doc.page_count());
    println!("output_tiff_bytes: {out_bytes}");
    println!("full_rgba_pixmap_bytes_avoided: {}", total_pixels * 4);
    println!("full_rgb_staging_bytes_avoided: {}", total_pixels * 3);
    println!(
        "measure peak RSS with /usr/bin/time (-l on macOS, -v on Linux); \
         the exporter writes to File and does not retain the TIFF byte buffer"
    );

    Ok(())
}

fn usage() -> String {
    "usage: probe_tiff_streaming_memory <input.djvu> <output.tiff> [scale]".to_string()
}
