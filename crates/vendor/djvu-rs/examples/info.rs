//! Print basic info about a DjVu file.
//!
//! Usage: cargo run --example info -- path/to/file.djvu

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).expect("usage: info <file.djvu>");
    let doc = djvu_rs::Document::open(&path)?;

    println!("Pages: {}", doc.page_count());
    for i in 0..doc.page_count() {
        let page = doc.page(i)?;
        println!(
            "  {:>4}: {} x {} px  {} dpi",
            i + 1,
            page.width(),
            page.height(),
            page.dpi()
        );
    }
    Ok(())
}
