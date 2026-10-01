/// Renders badges at the sizes Explorer actually uses onto a real photo, so the result
/// can be LOOKED AT. Pixel assertions prove placement; only eyes prove legibility.
#[test]
fn render_sample_sheet() {
    use super::{BadgeSize, BadgeStyle, Frame};
    let src = st2k_base::testcorpus::dir().join("sample.png");
    let Ok(img) = image::open(&src) else {
        eprintln!("skipping: no ../test-corpus/sample.png");
        return;
    };
    // One ROW per size step (Small / Medium / Large), each row the sizes Explorer
    // actually uses - so "is Medium actually readable at 96px" is a thing eyes can
    // answer instead of a number in a test.
    let steps = [BadgeSize::Small, BadgeSize::Medium, BadgeSize::Large];
    let row_h = 512 + 8;
    let mut sheet = image::RgbaImage::new(
        96 + 8 + 256 + 8 + 512,
        row_h * steps.len() as u32 + 8 + 2 * 168,
    );
    for p in sheet.pixels_mut() {
        *p = image::Rgba([32, 32, 40, 255]);
    }
    let mut put = |w: u32, h: u32, label: &str, style, size, x: u32, y: u32| {
        let t = img.resize_to_fill(w, h, image::imageops::FilterType::Lanczos3);
        let mut rgba = t.to_rgba8();
        let frame = Frame::explorer_draws_over(label, w, h);
        super::stamp(&mut rgba, w, h, label, style, size, frame);
        image::imageops::overlay(&mut sheet, &rgba, x as i64, y as i64);
    };
    for (row, size) in steps.iter().enumerate() {
        let mut x = 0u32;
        for (cx, label) in [(96u32, "PSD"), (256, "AVIF"), (512, "JXL")] {
            put(
                cx,
                cx,
                label,
                BadgeStyle::Text,
                *size,
                x,
                row as u32 * row_h,
            );
            x += cx + 8;
        }
    }
    // The icon style, one label per category, at the size Explorer's medium view uses, then
    // the shapes issue #50 compared (square, wide, tall) and a landscape video (issue #51).
    let base = row_h * steps.len() as u32 + 8;
    let mut x = 0u32;
    for label in ["PNG", "CR2", "EPUB", "PDF", "MP3", "MP4", "ZIP"] {
        put(
            160,
            160,
            label,
            BadgeStyle::Icon,
            BadgeSize::Medium,
            x,
            base,
        );
        x += 168;
    }
    let mut x = 0u32;
    for (w, h, label) in [
        (160, 160, "ZIP"),
        (160, 84, "WEBP"),
        (90, 160, "MP4"),
        (160, 90, "MP4"),
    ] {
        put(
            w,
            h,
            label,
            BadgeStyle::Icon,
            BadgeSize::Large,
            x,
            base + 168,
        );
        x += w + 8;
    }
    let out = std::env::temp_dir().join("st2k_badge_sheet.png");
    sheet.save(&out).expect("save");
    eprintln!("badge sheet -> {}", out.display());
}
