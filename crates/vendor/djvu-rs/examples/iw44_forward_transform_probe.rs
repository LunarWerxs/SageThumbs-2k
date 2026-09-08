//! IW44_FWD_TRANSFORM (#578) — compare djvu-rs forward DWT with DjVuLibre's
//! scalar forward filter schedule on the colorbook pages that previously showed
//! the largest band-0/DC size gap vs `c44`.
//!
//! Usage:
//!   cargo run --release --features="iw44-probe,cli" --example iw44_forward_transform_probe
//!   cargo run --release --features="iw44-probe,cli" --example iw44_forward_transform_probe \
//!       -- tests/fixtures/colorbook.djvu 61 2 59 1 60
#![cfg(feature = "iw44-probe")]

use std::path::Path;
use std::process::ExitCode;

use djvu_rs::{
    DjVuDocument, Pixmap,
    iw44::Iw44Image,
    iw44_encode::{Iw44EncodeOptions, encode_iw44_color, probe},
};

const DEFAULT_COLORBOOK: &str = "tests/fixtures/colorbook.djvu";
const WORST_COLORBOOK_PAGES: [usize; 5] = [61, 2, 59, 1, 60];

fn decode_bg44(chunks: &[&[u8]]) -> Option<Pixmap> {
    let mut image = Iw44Image::new();
    for chunk in chunks {
        image.decode_chunk(chunk).ok()?;
    }
    image.to_rgb().ok()
}

fn write_ppm(path: &Path, pixmap: &Pixmap) -> std::io::Result<()> {
    let mut buffer = Vec::with_capacity(pixmap.data.len());
    buffer.extend_from_slice(format!("P6\n{} {}\n255\n", pixmap.width, pixmap.height).as_bytes());
    for pixel in 0..(pixmap.width as usize * pixmap.height as usize) {
        buffer.push(pixmap.data[pixel * 4]);
        buffer.push(pixmap.data[pixel * 4 + 1]);
        buffer.push(pixmap.data[pixel * 4 + 2]);
    }
    std::fs::write(path, buffer)
}

fn c44_size(pixmap: &Pixmap, tmp_tag: &str) -> Option<usize> {
    let tmp = std::env::temp_dir();
    let ppm = tmp.join(format!("iw44_fwd_transform_{tmp_tag}.ppm"));
    let out = tmp.join(format!("iw44_fwd_transform_{tmp_tag}.djvu"));
    write_ppm(&ppm, pixmap).ok()?;
    let status = std::process::Command::new("c44")
        .args([
            ppm.to_string_lossy().as_ref(),
            out.to_string_lossy().as_ref(),
        ])
        .output()
        .ok()?;
    let _ = std::fs::remove_file(&ppm);
    if !status.status.success() {
        let _ = std::fs::remove_file(&out);
        return None;
    }
    let size = std::fs::metadata(&out).ok()?.len() as usize;
    let _ = std::fs::remove_file(&out);
    Some(size)
}

fn print_plane_diff(diff: &probe::ForwardPlaneDiff) {
    println!(
        "  {}: samples={} changed={} sum_abs_delta={} max_abs_delta={} band0_changed={} band0_sum_abs_delta={} band0_max_abs_delta={}",
        diff.plane,
        diff.samples,
        diff.changed,
        diff.sum_abs_delta,
        diff.max_abs_delta,
        diff.bands[0].changed,
        diff.bands[0].sum_abs_delta,
        diff.bands[0].max_abs_delta
    );
}

fn process_page(doc: &DjVuDocument, file_label: &str, page_idx: usize) -> Option<(usize, usize)> {
    let page = doc.page(page_idx).ok()?;
    let bg44_chunks = page.bg44_chunks();
    if bg44_chunks.is_empty() {
        eprintln!("skip {file_label} page {page_idx}: no BG44 chunks");
        return None;
    }
    let pixmap = decode_bg44(&bg44_chunks)?;

    let opts = Iw44EncodeOptions::default();
    let rs_chunks = encode_iw44_color(&pixmap, &opts);
    let rs_total: usize = rs_chunks.iter().map(|chunk| chunk.len()).sum();
    let c44 = c44_size(&pixmap, &format!("{file_label}_{page_idx}"));
    let diffs = probe::forward_transform_diff_color(&pixmap);
    let changed_coefficients: u64 = diffs.iter().map(|diff| diff.changed).sum();

    println!(
        "\n=== {file_label} page {page_idx} ({}x{}) ===",
        pixmap.width, pixmap.height
    );
    match c44 {
        Some(c44_total) => {
            let gap = rs_total.saturating_sub(c44_total);
            let explained = if changed_coefficients == 0 {
                0
            } else {
                usize::MAX
            };
            if explained == 0 {
                println!(
                    "sizes: ours={rs_total} B c44={c44_total} B gap={gap} B transform_explained=0 B (0.0% of gap)"
                );
            } else {
                println!(
                    "sizes: ours={rs_total} B c44={c44_total} B gap={gap} B transform_explained=coefficients differ; feed-through probe required"
                );
            }
        }
        None => {
            println!("sizes: ours={rs_total} B c44=n/a transform_explained=0 B if changed=0");
        }
    }
    println!("forward DWT coefficient deltas (production - DjVuLibre reference):");
    for diff in &diffs {
        print_plane_diff(diff);
    }
    Some((rs_total, c44.unwrap_or(0)))
}

fn parse_args() -> (String, Vec<usize>) {
    let mut args = std::env::args().skip(1);
    let path = args.next().unwrap_or_else(|| DEFAULT_COLORBOOK.to_string());
    let pages: Vec<usize> = args
        .map(|arg| {
            arg.parse::<usize>()
                .unwrap_or_else(|_| panic!("page index must be numeric: {arg}"))
        })
        .collect();
    let pages = if pages.is_empty() {
        WORST_COLORBOOK_PAGES.to_vec()
    } else {
        pages
    };
    (path, pages)
}

fn main() -> ExitCode {
    let (path, pages) = parse_args();
    let data = match std::fs::read(&path) {
        Ok(data) => data,
        Err(error) => {
            eprintln!("failed to read {path}: {error}");
            return ExitCode::from(2);
        }
    };
    let document = match DjVuDocument::parse(&data) {
        Ok(document) => document,
        Err(error) => {
            eprintln!("failed to parse {path}: {error}");
            return ExitCode::from(2);
        }
    };
    let file_label = Path::new(&path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("document");

    let mut processed = 0usize;
    for page_idx in pages {
        if page_idx >= document.page_count() {
            eprintln!("skip {file_label} page {page_idx}: out of range");
            continue;
        }
        if process_page(&document, file_label, page_idx).is_some() {
            processed += 1;
        }
    }

    if processed == 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}
