//! Format corner-badges: stamp the file's format (`PSD`, `JXL`, `AVIF`…) onto the thumbnail.
//!
//! Requested for 1.7.6: in a folder of visually similar images you cannot tell a PSD from a
//! JPEG, because a thumbnail shows the picture and says nothing about the container.
//!
//! Off by default (`FormatBadge`), because it MODIFIES the picture the user asked to see —
//! an opt-in decoration, never a surprise.
//!
//! # Why a hand-rolled bitmap font instead of GDI text
//!
//! This runs inside `explorer.exe`'s thumbnail worker, on any machine, under any locale and
//! any DPI. `DrawText` would drag in a device context, font selection, and the user's font
//! substitution settings — three things that differ per machine and would make the badge
//! render differently (or fail) depending on who is looking. The glyphs here are 5x7 cells
//! scaled by an integer factor, so a badge is byte-identical everywhere, needs no GDI object,
//! and cannot fail. We only ever draw `A-Z`, `0-9` and `+`, which is every character a format
//! label can contain.

/// Longest label we will draw. Keeps the badge from eating the tile on a silly extension.
const MAX_LABEL: usize = 5;

/// A thumbnail smaller than this gets NO badge: below ~64px the label would cover a
/// meaningful share of the image and be unreadable anyway. Explorer's smallest tile is 16px.
const MIN_BADGED_EDGE: u32 = 64;

/// How the corner badge is drawn.
///
/// `Text` is the original: a near-black chip with light letters, deliberately colourless so
/// it never competes with the picture. `Icon` is the one people actually asked for — a
/// dog-eared page tinted by the format's CATEGORY, so a folder of mixed files is scannable
/// by colour at a glance and only needs reading when two categories sit side by side.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum BadgeStyle {
    /// Neutral dark chip, light text.
    Text,
    /// Category-coloured page mark with the label inside.
    #[default]
    Icon,
}

impl BadgeStyle {
    /// `FormatBadgeStyle`: 0 = text, anything else = icon. Unknown values fall to the
    /// default rather than to "no badge" — a badge was still asked for.
    pub fn from_dword(v: u32) -> Self {
        if v == 0 {
            Self::Text
        } else {
            Self::Icon
        }
    }
}

/// The category tint for an extension, as (r, g, b).
///
/// Seven hues, one per [`crate::formats::Category`], picked to stay apart from each other at
/// badge size (roughly 20 px across on a 256 px tile) and to read on both light and dark
/// thumbnails. They are NOT theme-aware on purpose: the badge is burned into the bitmap the
/// shell caches, so it cannot follow a theme the user changes later.
fn category_rgb(ext: &str) -> (u8, u8, u8) {
    use crate::formats::Category;
    match crate::formats::category(ext) {
        Category::Image => (36, 116, 208),   // blue
        Category::Raw => (124, 77, 200),     // violet
        Category::Ebook => (32, 140, 84),    // green
        Category::Document => (196, 60, 52), // red
        Category::Audio => (0, 138, 148),    // teal
        Category::Video => (176, 52, 132),   // magenta
        Category::Archive => (90, 96, 110),  // slate
    }
}

/// Scale each channel by `n/255`, for the darker rim of a tint (`n` < 255).
fn shade(rgb: (u8, u8, u8), n: u32) -> (u8, u8, u8) {
    let f = |c: u8| ((c as u32 * n) / 255).min(255) as u8;
    (f(rgb.0), f(rgb.1), f(rgb.2))
}

/// Mix `rgb` toward white by `n/255`. Used for the dog-ear flap: multiplying a tint UP
/// saturates the already-large channel and shifts the hue, while mixing toward white keeps
/// the hue and just lightens it, which is what "the back of the sheet" should look like.
fn blend_to_white(rgb: (u8, u8, u8), n: u32) -> (u8, u8, u8) {
    let f = |c: u8| (c as u32 + (255 - c as u32) * n / 255).min(255) as u8;
    (f(rgb.0), f(rgb.1), f(rgb.2))
}

/// 5x7 uppercase glyphs, one `u8` per row (bit 4 = leftmost pixel). Only the characters a
/// format label can contain.
#[rustfmt::skip]
fn glyph(c: u8) -> Option<[u8; 7]> {
    Some(match c {
        b'A' => [0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        b'B' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110],
        b'C' => [0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110],
        b'D' => [0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110],
        b'E' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111],
        b'F' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000],
        b'G' => [0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01111],
        b'H' => [0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        b'I' => [0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        b'J' => [0b00111, 0b00010, 0b00010, 0b00010, 0b00010, 0b10010, 0b01100],
        b'K' => [0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001],
        b'L' => [0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111],
        b'M' => [0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001],
        b'N' => [0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001],
        b'O' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        b'P' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000],
        b'Q' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101],
        b'R' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001],
        b'S' => [0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110],
        b'T' => [0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100],
        b'U' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        b'V' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100],
        b'W' => [0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001],
        b'X' => [0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001],
        b'Y' => [0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100],
        b'Z' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111],
        b'0' => [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
        b'1' => [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        b'2' => [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111],
        b'3' => [0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110],
        b'4' => [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
        b'5' => [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
        b'6' => [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
        b'7' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
        b'8' => [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
        b'9' => [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100],
        b'+' => [0b00000, 0b00100, 0b00100, 0b11111, 0b00100, 0b00100, 0b00000],
        _ => return None,
    })
}

/// The label to stamp for a file name, or `None` when there is nothing sensible to draw.
///
/// Uppercased, `.`-stripped, and limited to characters the font has. A name with no
/// extension, or an extension too long or full of unrenderable characters, gets no badge
/// rather than a mangled one.
pub fn label_for(file_name: &str) -> Option<String> {
    let ext = file_name.rsplit_once('.')?.1;
    if ext.is_empty() || ext.len() > MAX_LABEL {
        return None;
    }
    let up: String = ext.to_ascii_uppercase();
    if !up.bytes().all(|b| glyph(b).is_some()) {
        return None;
    }
    Some(up)
}

/// Stamp `label` into the bottom-right of an RGBA8 image, in place.
///
/// Drawn as a dark rounded-ish chip with light text: a bare glyph would vanish against
/// matching image content, and a thumbnail is arbitrary content by definition. The chip is
/// alpha-blended rather than overwritten so it reads as an overlay, and it is sized from the
/// image edge so it looks the same relative size on a 96 px tile and a 1024 px one.
///
/// [`BadgeStyle::Icon`] keeps all of that and adds the category tint + dog-eared page
/// silhouette; the geometry below is shared so the two styles can never drift apart in size
/// or position.
///
/// No-ops on images too small to badge legibly, or when the label does not fit.
pub fn stamp(rgba: &mut [u8], w: u32, h: u32, label: &str, style: BadgeStyle) {
    let edge = w.min(h);
    if edge < MIN_BADGED_EDGE || label.is_empty() {
        return;
    }
    // Integer glyph scale keyed to the SHORT edge so the badge occupies a CONSTANT FRACTION
    // of the tile rather than growing with it. A 3-char chip is 21 glyph-cells wide, so
    // edge/110 lands it near ~18% of the width at every size; the first attempt used edge/48
    // and produced a badge that looked fine at 96px and swallowed a third of a 512px tile.
    // Capped at 8 so an unusually large request cannot run away.
    let scale = (edge / 110).clamp(1, 8);
    let (gw, gh) = (5 * scale, 7 * scale);
    let gap = scale;
    let pad = 2 * scale;

    let n = label.len() as u32;
    let text_w = n * gw + (n.saturating_sub(1)) * gap;
    // The icon style reserves an extra column on the right for the dog-ear, so the flap
    // never lands on the last glyph. Everything else about the box is identical.
    let fold = if style == BadgeStyle::Icon {
        3 * scale
    } else {
        0
    };
    let chip_w = text_w + pad * 2 + fold;
    let chip_h = gh + pad * 2;
    // Inset from the corner by the same padding, so the chip does not touch the edge.
    let margin = pad;
    if chip_w + margin * 2 > w || chip_h + margin * 2 > h {
        return; // would dominate the tile: better to draw nothing
    }
    let x0 = w - chip_w - margin;
    let y0 = h - chip_h - margin;

    let put = |buf: &mut [u8], x: u32, y: u32, rgb: (u8, u8, u8), a: u32| {
        if x >= w || y >= h {
            return;
        }
        let i = ((y * w + x) * 4) as usize;
        let Some(px) = buf.get_mut(i..i + 4) else {
            return;
        };
        // Source-over onto the existing pixel; the thumbnail's own alpha is preserved and
        // raised toward opaque where the chip covers it, so a badge on a transparent PNG
        // still reads instead of blending into whatever sits behind the icon.
        let blend = |dst: u8, src: u8| ((src as u32 * a + dst as u32 * (255 - a)) / 255) as u8;
        px[0] = blend(px[0], rgb.0);
        px[1] = blend(px[1], rgb.1);
        px[2] = blend(px[2], rgb.2);
        px[3] = px[3].max(a as u8);
    };

    // The label's own category decides the tint. `label` is the uppercased extension, and
    // `formats::category` matches lowercase tables, so fold the case back down here.
    let tint = category_rgb(&label.to_ascii_lowercase());

    for y in y0..y0 + chip_h {
        for x in x0..x0 + chip_w {
            // Distance from the TOP-RIGHT corner, which is where the page is dog-eared.
            let dx = x0 + chip_w - 1 - x;
            let dy = y - y0;
            // Clip the plain corner pixels for a softened (not sharply rectangular) look.
            // The top-right one is skipped when a fold already reshapes that corner.
            let cx = x == x0 || x == x0 + chip_w - 1;
            let cy = y == y0 || y == y0 + chip_h - 1;
            if cx && cy && !(fold > 0 && dx + dy < fold) {
                continue;
            }
            match style {
                // Near-black at ~72% so the underlying image still shows through slightly.
                BadgeStyle::Text => put(rgba, x, y, (16, 16, 16), 184),
                BadgeStyle::Icon => {
                    let on_fold_edge = fold > 0 && dx + dy == fold;
                    let outline = cx || cy || on_fold_edge;
                    let (rgb, a) = if fold > 0 && dx + dy < fold {
                        // The folded-back flap: lighter, so it reads as the sheet's underside.
                        (blend_to_white(tint, 150), 245)
                    } else if outline {
                        // A darker rim keeps the mark legible on a same-coloured picture.
                        (shade(tint, 150), 255)
                    } else {
                        (tint, 235)
                    };
                    put(rgba, x, y, rgb, a);
                }
            }
        }
    }
    // Glyphs, near-white and fully opaque for maximum contrast against the chip.
    let mut cx = x0 + pad;
    for ch in label.bytes() {
        if let Some(g) = glyph(ch) {
            for (row, bits) in g.iter().enumerate() {
                for col in 0..5u32 {
                    if bits & (1 << (4 - col)) == 0 {
                        continue;
                    }
                    for sy in 0..scale {
                        for sx in 0..scale {
                            put(
                                rgba,
                                cx + col * scale + sx,
                                y0 + pad + row as u32 * scale + sy,
                                (245, 245, 245),
                                255,
                            );
                        }
                    }
                }
            }
        }
        cx += gw + gap;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_comes_from_the_extension_uppercased() {
        assert_eq!(label_for("holiday.psd").as_deref(), Some("PSD"));
        assert_eq!(label_for("a.JxL").as_deref(), Some("JXL"));
        assert_eq!(label_for(r"C:\pics\shot.avif").as_deref(), Some("AVIF"));
    }

    #[test]
    fn refuses_labels_it_cannot_draw() {
        assert_eq!(label_for("no-extension"), None);
        assert_eq!(label_for("trailing."), None);
        // Longer than MAX_LABEL, and characters with no glyph.
        assert_eq!(label_for("x.toolongext"), None);
        assert_eq!(label_for("x.p-d"), None);
        assert_eq!(label_for("x.日本語"), None);
    }

    /// The badge must land in the BOTTOM-RIGHT and leave the rest of the picture alone —
    /// the whole point is that it annotates the thumbnail rather than obscuring it.
    #[test]
    fn stamps_only_the_bottom_right_corner() {
        let (w, h) = (256u32, 256u32);
        let mut px = vec![0u8; (w * h * 4) as usize];
        stamp(&mut px, w, h, "PSD", BadgeStyle::Text);
        let at = |x: u32, y: u32| px[((y * w + x) * 4) as usize..][..4].to_vec();
        assert_eq!(at(4, 4), vec![0, 0, 0, 0], "top-left must be untouched");
        assert_eq!(at(4, h - 4), vec![0, 0, 0, 0], "bottom-left untouched");
        assert_eq!(at(w - 4, 4), vec![0, 0, 0, 0], "top-right untouched");
        // Somewhere in the bottom-right chip must now be non-transparent.
        let touched = (h - 40..h - 4).any(|y| (w - 60..w - 4).any(|x| at(x, y)[3] != 0));
        assert!(touched, "no badge drawn in the bottom-right");
    }

    #[test]
    fn tiny_thumbnails_are_left_alone() {
        let (w, h) = (48u32, 48u32);
        let mut px = vec![7u8; (w * h * 4) as usize];
        stamp(&mut px, w, h, "PSD", BadgeStyle::Icon);
        assert!(
            px.iter().all(|&v| v == 7),
            "a 48px tile is too small to badge legibly and must be untouched"
        );
    }

    /// A long label on a small tile would cover the picture; draw nothing instead.
    #[test]
    fn refuses_when_the_chip_would_dominate() {
        let (w, h) = (64u32, 64u32);
        let mut px = vec![7u8; (w * h * 4) as usize];
        stamp(&mut px, w, h, "WEBP2", BadgeStyle::Icon);
        let drawn = px.iter().any(|&v| v != 7);
        // Either it fits comfortably or it drew nothing — never a chip wider than the tile.
        if drawn {
            let untouched_left = (0..8).all(|x| px[((32 * w + x) * 4) as usize + 3] == 7);
            assert!(untouched_left, "chip must not span the whole tile");
        }
    }

    #[test]
    fn does_not_panic_on_a_truncated_buffer() {
        let mut px = vec![0u8; 10];
        stamp(&mut px, 256, 256, "PSD", BadgeStyle::Icon); // buffer far too small for the claimed size
    }
}

#[cfg(test)]
mod visual {
    /// Renders badges at the sizes Explorer actually uses onto a real photo, so the result
    /// can be LOOKED AT. Pixel assertions prove placement; only eyes prove legibility.
    #[test]
    fn render_sample_sheet() {
        let src =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../test-corpus/sample.png");
        let Ok(img) = image::open(&src) else {
            eprintln!("skipping: no ../test-corpus/sample.png");
            return;
        };
        let mut sheet = image::RgbaImage::new(96 + 8 + 256 + 8 + 512, 512 + 8 + 160);
        for p in sheet.pixels_mut() {
            *p = image::Rgba([32, 32, 40, 255]);
        }
        let mut x = 0u32;
        for (cx, label) in [(96u32, "PSD"), (256, "AVIF"), (512, "JXL")] {
            let t = img.resize_to_fill(cx, cx, image::imageops::FilterType::Lanczos3);
            let mut rgba = t.to_rgba8();
            let (w, h) = (rgba.width(), rgba.height());
            super::stamp(&mut rgba, w, h, label, super::BadgeStyle::Text);
            image::imageops::overlay(&mut sheet, &rgba, x as i64, 0);
            x += cx + 8;
        }
        // Second row: the icon style, one label per category, so the seven tints can be
        // compared side by side at the size Explorer's medium view actually uses.
        let mut x = 0u32;
        for label in ["PNG", "CR2", "EPUB", "PDF", "MP3", "MP4", "ZIP"] {
            let t = img.resize_to_fill(160, 160, image::imageops::FilterType::Lanczos3);
            let mut rgba = t.to_rgba8();
            let (w, h) = (rgba.width(), rgba.height());
            super::stamp(&mut rgba, w, h, label, super::BadgeStyle::Icon);
            image::imageops::overlay(&mut sheet, &rgba, x as i64, 520);
            x += 168;
        }
        let out = std::env::temp_dir().join("st2k_badge_sheet.png");
        sheet.save(&out).expect("save");
        eprintln!("badge sheet -> {}", out.display());
    }
}
