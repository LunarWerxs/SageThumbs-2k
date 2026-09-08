//! Export default (Deflate) and `ccitt_g4: true` PDFs for a given corpus file,
//! for size comparison and external rendering validation (pdftoppm/compare).
//!
//! ```sh
//! cargo run --release --example pdf_g4_export -- tests/corpus/watchmaker.djvu /tmp/out
//! ```
//! Writes `<out>_default.pdf`, `<out>_g4.pdf`, `<out>_adaptive.pdf`,
//! `<out>_both.pdf` (adaptive_raster + ccitt_g4 composed).

use djvu_rs::djvu_document::DjVuDocument;
use djvu_rs::pdf::{PdfOptions, djvu_to_pdf_with_options};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let input = args
        .get(1)
        .expect("usage: pdf_g4_export <input.djvu> <out_prefix>");
    let out_prefix = args
        .get(2)
        .expect("usage: pdf_g4_export <input.djvu> <out_prefix>");

    let data = std::fs::read(input).unwrap_or_else(|_| panic!("read {input}"));
    let doc = DjVuDocument::parse(&data).expect("parse");

    let variants: &[(&str, PdfOptions)] = &[
        ("default", PdfOptions::default()),
        (
            "g4",
            PdfOptions {
                ccitt_g4: true,
                ..PdfOptions::default()
            },
        ),
        (
            "adaptive",
            PdfOptions {
                adaptive_raster: true,
                ..PdfOptions::default()
            },
        ),
        (
            "both",
            PdfOptions {
                adaptive_raster: true,
                ccitt_g4: true,
                ..PdfOptions::default()
            },
        ),
    ];

    for (name, opts) in variants {
        let pdf = djvu_to_pdf_with_options(&doc, opts).expect("pdf export must succeed");
        let path = format!("{out_prefix}_{name}.pdf");
        std::fs::write(&path, &pdf).expect("write pdf");
        println!("{name:>10}: {:>10} bytes -> {path}", pdf.len());
    }
}
