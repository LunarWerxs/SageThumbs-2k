//! CHEAP size-first probe for #568 (JBIG2 PDF masks).
//!
//! This does not encode JBIG2. It compares each DjVu page's existing `Sjbz`
//! payload bytes against the PDF ImageMask stream body that `collect_mask_stream`
//! emits today when `PdfOptions::ccitt_g4` is enabled: Deflate and G4 are both
//! encoded, then the smaller stream body is kept.
//!
//! Run with:
//! ```sh
//! cargo run --release --example jbig2_mask_size_probe
//! cargo run --release --example jbig2_mask_size_probe -- path/to/file.djvu ...
//! ```

use djvu_rs::{Bitmap, DjVuDocument};

const DECISION_THRESHOLD: f64 = 0.5;

struct Totals {
    pages_with_sjbz: usize,
    sjbz_bytes: usize,
    mask_stream_bytes: usize,
    g4_wins: usize,
    deflate_wins: usize,
}

impl Totals {
    fn new() -> Self {
        Self {
            pages_with_sjbz: 0,
            sjbz_bytes: 0,
            mask_stream_bytes: 0,
            g4_wins: 0,
            deflate_wins: 0,
        }
    }

    fn add(&mut self, sjbz_bytes: usize, mask_stream: MaskStream) {
        self.pages_with_sjbz += 1;
        self.sjbz_bytes += sjbz_bytes;
        self.mask_stream_bytes += mask_stream.bytes;
        match mask_stream.filter {
            MaskFilter::G4 => self.g4_wins += 1,
            MaskFilter::Deflate => self.deflate_wins += 1,
        }
    }

    fn ratio(&self) -> f64 {
        self.sjbz_bytes as f64 / self.mask_stream_bytes.max(1) as f64
    }

    fn recommendation(&self) -> &'static str {
        if self.ratio() <= DECISION_THRESHOLD {
            "continue-to-encoder"
        } else {
            "reject-early"
        }
    }
}

#[derive(Clone, Copy)]
enum MaskFilter {
    Deflate,
    G4,
}

struct MaskStream {
    bytes: usize,
    filter: MaskFilter,
}

fn make_stream(dict_extra: &str, data: &[u8]) -> Vec<u8> {
    let len = data.len();
    let mut body = format!("<< /Length {len}{dict_extra} >>\nstream\n").into_bytes();
    body.extend_from_slice(data);
    body.extend_from_slice(b"\nendstream");
    body
}

fn make_deflate_stream(dict_extra: &str, data: &[u8]) -> Vec<u8> {
    let compressed = miniz_oxide::deflate::compress_to_vec_zlib(data, 6);
    let extra = format!(" /Filter /FlateDecode{dict_extra}");
    make_stream(&extra, &compressed)
}

fn make_ccitt_stream(dict_extra: &str, ncols: u32, nrows: u32, bitstream: &[u8]) -> Vec<u8> {
    let extra = format!(
        " /Filter /CCITTFaxDecode /DecodeParms\
         << /K -1 /Columns {ncols} /Rows {nrows} /BlackIs1 true >>{dict_extra}"
    );
    make_stream(&extra, bitstream)
}

fn mask_stream_from_bitmap(bitmap: &Bitmap) -> MaskStream {
    let width = bitmap.width;
    let height = bitmap.height;
    let dict_extra = format!(
        " /Type /XObject /Subtype /Image /Width {width} /Height {height}\
         /ImageMask true /BitsPerComponent 1 /Decode [1 0]"
    );
    let deflate_body = make_deflate_stream(&dict_extra, &bitmap.data);
    let g4_bits = djvu_rs::smmr::encode_g4(bitmap);
    let g4_body = make_ccitt_stream(&dict_extra, width, height, &g4_bits);

    if g4_body.len() < deflate_body.len() {
        MaskStream {
            bytes: g4_body.len(),
            filter: MaskFilter::G4,
        }
    } else {
        MaskStream {
            bytes: deflate_body.len(),
            filter: MaskFilter::Deflate,
        }
    }
}

fn probe(label: &str, path: &str) -> Option<Totals> {
    let Ok(data) = std::fs::read(path) else {
        println!("{label:<18} {path:<48} missing");
        return None;
    };
    let Ok(document) = DjVuDocument::parse(&data) else {
        println!("{label:<18} {path:<48} parse-failed");
        return None;
    };

    let mut totals = Totals::new();
    for page_index in 0..document.page_count() {
        let Ok(page) = document.page(page_index) else {
            continue;
        };
        let Some(sjbz) = page.raw_chunk(b"Sjbz") else {
            continue;
        };
        let Ok(Some(bitmap)) = page.extract_mask() else {
            continue;
        };
        totals.add(sjbz.len(), mask_stream_from_bitmap(&bitmap));
    }

    Some(totals)
}

fn print_row(label: &str, path: &str, totals: &Totals) {
    println!(
        "{label:<18} {pages:>5} {sjbz:>12} {mask:>12} {ratio:>7.3}x {g4:>5} {deflate:>7} {recommendation}",
        pages = totals.pages_with_sjbz,
        sjbz = totals.sjbz_bytes,
        mask = totals.mask_stream_bytes,
        ratio = totals.ratio(),
        g4 = totals.g4_wins,
        deflate = totals.deflate_wins,
        recommendation = totals.recommendation(),
    );
    if totals.ratio() > DECISION_THRESHOLD {
        println!(
            "  note: {path}: Sjbz is not <= {DECISION_THRESHOLD:.1}x the current G4/Deflate mask stream"
        );
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let owned_inputs;
    let inputs: Vec<(&str, &str)> = if args.is_empty() {
        vec![
            ("watchmaker", "tests/corpus/watchmaker.djvu"),
            ("cable", "tests/corpus/cable_1973_100133.djvu"),
            ("irish", "tests/fixtures/irish.djvu"),
            ("pathogenic", "tests/corpus/pathogenic_bacteria_1896.djvu"),
        ]
    } else {
        owned_inputs = args
            .iter()
            .map(|path| (path.as_str(), path.as_str()))
            .collect::<Vec<_>>();
        owned_inputs
    };

    println!(
        "{:<18} {:>5} {:>12} {:>12} {:>8} {:>5} {:>7} decision",
        "document", "pages", "Sjbz-bytes", "PDF-mask", "Sjbz/PDF", "G4", "Deflate"
    );
    println!("{}", "-".repeat(91));

    let mut combined = Totals::new();
    let mut measured = 0usize;
    for (label, path) in inputs {
        let Some(totals) = probe(label, path) else {
            continue;
        };
        if totals.pages_with_sjbz == 0 {
            println!("{label:<18} {path:<48} no-sjbz-pages");
            continue;
        }
        print_row(label, path, &totals);
        combined.pages_with_sjbz += totals.pages_with_sjbz;
        combined.sjbz_bytes += totals.sjbz_bytes;
        combined.mask_stream_bytes += totals.mask_stream_bytes;
        combined.g4_wins += totals.g4_wins;
        combined.deflate_wins += totals.deflate_wins;
        measured += 1;
    }

    if measured > 1 {
        println!("{}", "-".repeat(91));
        print_row("TOTAL", "combined inputs", &combined);
    }
}
