//! CHEAP word-duplication probe for OCR memoization (#602).
//!
//! This does not run OCR. It decodes JB2 masks with the indexed blit map, groups
//! blits into geometric words, hashes each word's glyph-shape sequence, and
//! reports the best-case OCR speedup bound (`total_words / unique_words`).
//!
//! ```sh
//! cargo run --release --example ocr_word_dup_probe
//! OCR_WORD_DUP_MAX_PAGES=40 cargo run --release --example ocr_word_dup_probe \
//!   tests/corpus/pathogenic_bacteria_1896.djvu tests/corpus/watchmaker.djvu
//! ```

use std::collections::HashSet;
use std::time::Instant;

use djvu_rs::Bitmap;
use djvu_rs::djvu_document::DjVuDocument;

const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;
const MIN_WORD_BLITS: usize = 1;
const PROMISING_SPEEDUP_BOUND: f64 = 5.0;

#[derive(Clone, Debug)]
struct BlitBox {
    index: i32,
    min_x: u32,
    min_y: u32,
    max_x: u32,
    max_y: u32,
    symbol_hash: u64,
}

impl BlitBox {
    fn new(index: i32, x: u32, y: u32) -> Self {
        Self {
            index,
            min_x: x,
            min_y: y,
            max_x: x,
            max_y: y,
            symbol_hash: 0,
        }
    }

    fn add_pixel(&mut self, x: u32, y: u32) {
        self.min_x = self.min_x.min(x);
        self.min_y = self.min_y.min(y);
        self.max_x = self.max_x.max(x);
        self.max_y = self.max_y.max(y);
    }

    fn width(&self) -> u32 {
        self.max_x - self.min_x + 1
    }

    fn height(&self) -> u32 {
        self.max_y - self.min_y + 1
    }

    fn center_y(&self) -> f64 {
        (self.min_y as f64 + self.max_y as f64) * 0.5
    }
}

#[derive(Default)]
struct DocStats {
    pages_seen: usize,
    pages_with_jb2: usize,
    blits: usize,
    words: usize,
    unique_words: usize,
    elapsed_ms: f64,
}

fn fnv_mix(mut hash: u64, value: u64) -> u64 {
    for byte in value.to_le_bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn stable_word_hash(symbols: &[u64]) -> u64 {
    let mut hash = fnv_mix(FNV_OFFSET, symbols.len() as u64);
    for &symbol in symbols {
        hash = fnv_mix(hash, symbol);
    }
    hash
}

fn median_u32(values: &mut [u32]) -> u32 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    values[values.len() / 2]
}

fn blit_shape_hash(mask: &Bitmap, blit_map: &[i32], blit: &BlitBox) -> u64 {
    let width = mask.width as usize;
    let mut hash = fnv_mix(FNV_OFFSET, blit.width() as u64);
    hash = fnv_mix(hash, blit.height() as u64);
    for y in blit.min_y..=blit.max_y {
        for x in blit.min_x..=blit.max_x {
            let map_idx = y as usize * width + x as usize;
            let bit = blit_map.get(map_idx).copied() == Some(blit.index) && mask.get(x, y);
            hash = fnv_mix(hash, u64::from(bit));
        }
    }
    hash
}

fn extract_blit_boxes(mask: &Bitmap, blit_map: &[i32]) -> Vec<BlitBox> {
    let width = mask.width as usize;
    let max_blit = blit_map.iter().copied().filter(|&index| index >= 0).max();
    let Some(max_blit) = max_blit else {
        return Vec::new();
    };

    let mut boxes: Vec<Option<BlitBox>> = vec![None; max_blit as usize + 1];
    for y in 0..mask.height {
        let row = y as usize * width;
        for x in 0..mask.width {
            let index = blit_map[row + x as usize];
            if index < 0 {
                continue;
            }
            match &mut boxes[index as usize] {
                Some(blit) => blit.add_pixel(x, y),
                slot @ None => *slot = Some(BlitBox::new(index, x, y)),
            }
        }
    }

    boxes
        .into_iter()
        .flatten()
        .map(|mut blit| {
            blit.symbol_hash = blit_shape_hash(mask, blit_map, &blit);
            blit
        })
        .collect()
}

fn group_words(mut blits: Vec<BlitBox>) -> Vec<Vec<u64>> {
    if blits.is_empty() {
        return Vec::new();
    }

    let mut heights: Vec<u32> = blits.iter().map(BlitBox::height).collect();
    let mut widths: Vec<u32> = blits.iter().map(BlitBox::width).collect();
    let median_height = median_u32(&mut heights).max(1) as f64;
    let median_width = median_u32(&mut widths).max(1) as f64;
    let y_band = (median_height * 0.75).max(3.0);

    blits.sort_by(|left, right| {
        left.center_y()
            .total_cmp(&right.center_y())
            .then(left.min_x.cmp(&right.min_x))
    });

    let mut lines: Vec<Vec<BlitBox>> = Vec::new();
    let mut current_line: Vec<BlitBox> = Vec::new();
    let mut current_center_y = blits[0].center_y();
    for blit in blits {
        if !current_line.is_empty() && (blit.center_y() - current_center_y).abs() > y_band {
            lines.push(std::mem::take(&mut current_line));
            current_center_y = blit.center_y();
        }
        let line_len = current_line.len() as f64;
        current_center_y = (current_center_y * line_len + blit.center_y()) / (line_len + 1.0);
        current_line.push(blit);
    }
    if !current_line.is_empty() {
        lines.push(current_line);
    }

    let mut words = Vec::new();
    for mut line in lines {
        line.sort_by_key(|blit| (blit.min_x, blit.min_y));
        let mut gaps: Vec<u32> = line
            .windows(2)
            .filter_map(|pair| pair[1].min_x.checked_sub(pair[0].max_x + 1))
            .collect();
        let median_gap = median_u32(&mut gaps) as f64;
        let word_gap = (median_gap * 2.5).max(median_width * 0.75).max(3.0);

        let mut current_word = Vec::new();
        let mut previous_max_x = None;
        for blit in line {
            if let Some(max_x) = previous_max_x {
                let gap = blit.min_x.saturating_sub(max_x + 1) as f64;
                if gap > word_gap && current_word.len() >= MIN_WORD_BLITS {
                    words.push(std::mem::take(&mut current_word));
                }
            }
            current_word.push(blit.symbol_hash);
            previous_max_x = Some(blit.max_x);
        }
        if current_word.len() >= MIN_WORD_BLITS {
            words.push(current_word);
        }
    }

    words
}

fn max_pages() -> Option<usize> {
    std::env::var("OCR_WORD_DUP_MAX_PAGES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|&value| value > 0)
}

fn probe(path: &str) -> Option<DocStats> {
    let started = Instant::now();
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(error) => {
            eprintln!("{path}: skip ({error})");
            return None;
        }
    };
    let doc = match DjVuDocument::parse(&data) {
        Ok(doc) => doc,
        Err(error) => {
            eprintln!("{path}: parse failed ({error})");
            return None;
        }
    };

    let mut stats = DocStats::default();
    let mut unique_word_hashes = HashSet::new();
    let page_limit = max_pages().unwrap_or_else(|| doc.page_count());
    for page_index in 0..doc.page_count().min(page_limit) {
        stats.pages_seen += 1;
        let Ok(page) = doc.page(page_index) else {
            continue;
        };
        if page.find_chunk(b"Sjbz").is_none() {
            continue;
        }
        let Ok(Some((mask, blit_map))) = page.extract_mask_indexed() else {
            continue;
        };

        stats.pages_with_jb2 += 1;
        let blits = extract_blit_boxes(&mask, &blit_map);
        stats.blits += blits.len();
        let words = group_words(blits);
        stats.words += words.len();
        for word in words {
            unique_word_hashes.insert(stable_word_hash(&word));
        }
    }

    stats.unique_words = unique_word_hashes.len();
    stats.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    Some(stats)
}

fn print_row(path: &str, stats: &DocStats) {
    let unique_ratio = if stats.words == 0 {
        0.0
    } else {
        stats.unique_words as f64 / stats.words as f64
    };
    let duplication_rate = 1.0 - unique_ratio;
    let speedup_bound = if stats.unique_words == 0 {
        0.0
    } else {
        stats.words as f64 / stats.unique_words as f64
    };
    let decision = if speedup_bound >= PROMISING_SPEEDUP_BOUND {
        "Promising"
    } else {
        "Reject"
    };

    println!(
        "{path:<48} {pages:>5} {jb2_pages:>5} {blits:>9} {words:>9} {unique:>9} {unique_ratio:>8.2}% {dup_rate:>8.2}% {bound:>8.2}x {decision:>10}",
        pages = stats.pages_seen,
        jb2_pages = stats.pages_with_jb2,
        blits = stats.blits,
        words = stats.words,
        unique = stats.unique_words,
        unique_ratio = unique_ratio * 100.0,
        dup_rate = duplication_rate * 100.0,
        bound = speedup_bound,
    );
}

fn main() {
    let paths: Vec<String> = std::env::args().skip(1).collect();
    let paths = if paths.is_empty() {
        vec![
            "tests/corpus/pathogenic_bacteria_1896.djvu".to_string(),
            "tests/corpus/watchmaker.djvu".to_string(),
        ]
    } else {
        paths
    };

    let mut has_rows = false;
    println!(
        "{:<48} {:>5} {:>5} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>10}",
        "document",
        "pages",
        "jb2",
        "blits",
        "words",
        "unique",
        "uniq%",
        "dup%",
        "bound",
        "decision"
    );
    for path in &paths {
        if let Some(stats) = probe(path) {
            has_rows = true;
            print_row(path, &stats);
            eprintln!("{path}: probe {:.0} ms", stats.elapsed_ms);
        }
    }
    if !has_rows {
        std::process::exit(1);
    }
}
