//! `previewshot <image> <out.png> [light|dark]` — render the right-click menu
//! preview to a PNG so the layout/colors can be eyeballed without a real menu.
fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 3 {
        eprintln!("usage: previewshot <image> <out.png> [light|dark]");
        std::process::exit(2);
    }
    let bg = match a.get(3).map(|s| s.as_str()) {
        Some("light") => Some(0x00F0_F0F0),
        Some("dark") => Some(0x002B_2B2B),
        _ => None,
    };
    println!("{}", if sagethumbs2k::render_preview_png(&a[1], &a[2], bg) { "OK" } else { "FAIL" });
}
