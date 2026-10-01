use super::*;

/// The box `stamp` actually painted on a transparent `w` x `h` tile, as (x0, y0, x1, y1)
/// inclusive, or `None` when it drew nothing.
fn drawn_box(
    w: u32,
    h: u32,
    label: &str,
    style: BadgeStyle,
    size: BadgeSize,
    frame: Frame,
) -> Option<(u32, u32, u32, u32)> {
    let mut px = vec![0u8; (w * h * 4) as usize];
    stamp(&mut px, w, h, label, style, size, frame);
    let mut b: Option<(u32, u32, u32, u32)> = None;
    for y in 0..h {
        for x in 0..w {
            if px[((y * w + x) * 4 + 3) as usize] == 0 {
                continue;
            }
            b = Some(match b {
                None => (x, y, x, y),
                Some((a, c, d, e)) => (a.min(x), c.min(y), d.max(x), e.max(y)),
            });
        }
    }
    b
}

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
    let (x0, y0, x1, y1) = drawn_box(w, h, "PSD", BadgeStyle::Text, BadgeSize::Small, Frame::NONE)
        .expect("a 256px tile must badge");
    assert!(
        x0 > w / 2 && y0 > h / 2,
        "badge must stay in the bottom-right quarter"
    );
    assert!(x1 < w && y1 < h);
}

#[test]
fn tiny_thumbnails_are_left_alone() {
    let (w, h) = (48u32, 48u32);
    // The size choice does not buy a way past that floor: asking for Large on a tile below
    // MIN_BADGED_EDGE still draws nothing.
    for size in [BadgeSize::Small, BadgeSize::Large] {
        let mut px = vec![7u8; (w * h * 4) as usize];
        stamp(&mut px, w, h, "PSD", BadgeStyle::Icon, size, Frame::NONE);
        assert!(
            px.iter().all(|&v| v == 7),
            "a 48px tile is too small to badge legibly and must be untouched ({size:?})"
        );
    }
}

/// A long label on a small tile would cover the picture; draw nothing instead.
#[test]
fn refuses_when_the_chip_would_dominate() {
    let (w, h) = (64u32, 64u32);
    if let Some((x0, _, _, _)) = drawn_box(
        w,
        h,
        "WEBP2",
        BadgeStyle::Icon,
        BadgeSize::Small,
        Frame::NONE,
    ) {
        assert!(x0 >= 8, "chip must not span the whole tile");
    }
}

#[test]
fn does_not_panic_on_a_truncated_buffer() {
    let mut px = vec![0u8; 10];
    // buffer far too small for the claimed size
    stamp(
        &mut px,
        256,
        256,
        "PSD",
        BadgeStyle::Icon,
        BadgeSize::Large,
        Frame::NONE,
    );
}

/// The whole point of the setting: a bigger step must produce a visibly bigger chip at
/// the size Explorer's large-icons view uses. Measured off the geometry rather than the
/// pixels, so the assertion says what it means.
#[test]
fn bigger_steps_produce_a_wider_chip_at_256px() {
    let geom = |size| {
        badge_geometry(256, 256, "PSD", BadgeStyle::Icon, size, Frame::NONE)
            .expect("256px tile must badge")
    };
    let (s, m, l) = (
        geom(BadgeSize::Small),
        geom(BadgeSize::Medium),
        geom(BadgeSize::Large),
    );
    assert!(
        m.chip_w > s.chip_w,
        "Medium ({}) must be wider than Small ({})",
        m.chip_w,
        s.chip_w
    );
    assert!(
        l.chip_w > m.chip_w,
        "Large ({}) must be wider than Medium ({})",
        l.chip_w,
        m.chip_w
    );
    // Still an annotation, not a takeover: even Large stays well under half the tile.
    assert!(
        l.chip_w * 2 < 256,
        "Large chip {} dominates the tile",
        l.chip_w
    );
}

/// Explorer's smallest badged tile, at the largest step: either the chip fits (and stays
/// inside the tile) or `stamp` draws nothing - never a chip clipped by the edge.
#[test]
fn large_on_a_96px_tile_either_fits_or_draws_nothing() {
    let (w, h) = (96u32, 96u32);
    if let Some(g) = badge_geometry(w, h, "PSD", BadgeStyle::Icon, BadgeSize::Large, Frame::NONE) {
        assert!(
            g.x0 + g.chip_w <= w && g.y0 + g.chip_h <= h,
            "chip {}x{} at ({}, {}) runs off a {w}x{h} tile",
            g.chip_w,
            g.chip_h,
            g.x0,
            g.y0
        );
    }
}

/// Issue #50: one Explorer request (256 px) is a square archive cover, a wide picture, a
/// portrait book cover and a phone video, and Explorer shows all of them with their long side
/// across the tile. The badge has to come out the same size on each; it used to follow the
/// short side, so the square got twice the badge of the others. (A label too wide for a
/// narrow tile still shrinks to fit: `a_badge_too_wide_for_a_narrow_tile_shrinks_to_fit`.)
#[test]
fn one_request_gives_one_badge_size_whatever_the_picture_shape() {
    for size in [BadgeSize::Small, BadgeSize::Medium, BadgeSize::Large] {
        for label in ["ZIP", "MP4", "EPUB", "WEBP"] {
            let dims = |w, h| {
                let (x0, y0, x1, y1) = drawn_box(w, h, label, BadgeStyle::Icon, size, Frame::NONE)
                    .unwrap_or_else(|| panic!("{w}x{h} must badge {label} ({size:?})"));
                (x1 - x0, y1 - y0)
            };
            let square = dims(256, 256);
            for (w, h) in [(256, 134), (180, 256)] {
                let other = dims(w, h);
                assert!(
                    other.0.abs_diff(square.0) <= 1 && other.1.abs_diff(square.1) <= 1,
                    "{w}x{h} {label} badge {other:?} differs from the square's {square:?} ({size:?})"
                );
            }
        }
    }
}

/// The other half of the fit rule: a long label at the largest step cannot sit at full size
/// on a 9:16 tile, so it shrinks to fit inside it rather than run off the edge or vanish.
#[test]
fn a_badge_too_wide_for_a_narrow_tile_shrinks_to_fit() {
    let (x0, _, x1, _) = drawn_box(
        144,
        256,
        "WEBP",
        BadgeStyle::Icon,
        BadgeSize::Large,
        Frame::NONE,
    )
    .expect("a 144x256 tile still badges");
    assert!(
        x0 > 0 && x1 < 144,
        "chip spans x {x0}..{x1} of a 144 px tile"
    );
}

/// Issue #51: Explorer lays a film strip over the sides and a rim over the bottom of a
/// landscape video tile. The badge must sit clear of both, or the strip hides its last
/// letters ("MP" of "MP4" was all that showed).
#[test]
fn a_landscape_video_badge_stays_clear_of_the_film_strip() {
    let (w, h) = (256u32, 144u32);
    let frame = Frame::explorer_draws_over("mp4", w, h);
    assert_ne!(frame, Frame::NONE, "a landscape video tile gets the strip");
    for size in [BadgeSize::Small, BadgeSize::Large] {
        let (_, _, x1, y1) = drawn_box(w, h, "MP4", BadgeStyle::Icon, size, frame)
            .unwrap_or_else(|| panic!("a 256x144 video tile must badge ({size:?})"));
        // Measured strip: 12.4 % of the width, rim 5 % of the height.
        assert!(
            (x1 as f32) < w as f32 * (1.0 - 0.124),
            "badge reaches x={x1} under the strip"
        );
        assert!(
            (y1 as f32) < h as f32 * (1.0 - 0.05),
            "badge reaches y={y1} under the rim"
        );
    }
    // Explorer draws no strip on a portrait video or a picture, so those keep the corner.
    assert_eq!(Frame::explorer_draws_over("mp4", 144, 256), Frame::NONE);
    assert_eq!(Frame::explorer_draws_over("png", 256, 144), Frame::NONE);
}
