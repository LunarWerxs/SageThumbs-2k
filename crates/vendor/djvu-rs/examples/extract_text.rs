//! Extract the text layer from all pages of a DjVu file.
//!
//! Usage: cargo run --example extract_text -- path/to/file.djvu

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .expect("usage: extract_text <file.djvu>");
    let doc = djvu_rs::Document::open(&path)?;

    for i in 0..doc.page_count() {
        let page = doc.page(i)?;
        if let Some(text) = page.text()? {
            println!("=== Page {} ===", i + 1);
            println!("{text}");
        }
    }
    Ok(())
}
