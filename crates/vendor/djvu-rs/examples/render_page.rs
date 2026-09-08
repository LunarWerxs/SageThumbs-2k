//! Render the first page of a DjVu file to a PNG.
//!
//! Usage: cargo run --example render_page -- input.djvu output.png

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let input = args
        .next()
        .expect("usage: render_page <input.djvu> <output.png>");
    let output = args
        .next()
        .expect("usage: render_page <input.djvu> <output.png>");

    let doc = djvu_rs::Document::open(&input)?;
    let page = doc.page(0)?;
    let pixmap = page.render()?;

    let file = std::fs::File::create(&output)?;
    let mut w = std::io::BufWriter::new(file);
    let mut enc = png::Encoder::new(&mut w, pixmap.width, pixmap.height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(&pixmap.data)?;

    println!("Rendered {}x{} → {output}", pixmap.width, pixmap.height);
    Ok(())
}
