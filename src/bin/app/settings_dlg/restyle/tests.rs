use super::*;
use windows::Win32::Foundation::COLORREF;

/// The whole point of caching: repeated calls with the SAME key must return the
/// SAME handle (no repeated Gdip* allocation on every owner-draw pass — see the
/// `glyph_cache` doc comment), while a different color or width must not collide
/// with (and silently reuse) an unrelated one.
#[test]
fn glyph_cache_reuses_by_key_and_separates_distinct_keys() {
    unsafe {
        let token = gdip::startup();

        let blue1 = glyph_cache::brush(COLORREF(0x00FF0000));
        let blue2 = glyph_cache::brush(COLORREF(0x00FF0000));
        let green = glyph_cache::brush(COLORREF(0x0000FF00));
        assert_eq!(blue1, blue2, "same color must reuse the cached brush");
        assert_ne!(blue1, green, "different colors must not share a brush");

        let p1 = glyph_cache::pen(COLORREF(0x00FF0000), 2);
        let p2 = glyph_cache::pen(COLORREF(0x00FF0000), 2);
        let p3 = glyph_cache::pen(COLORREF(0x00FF0000), 3);
        assert_eq!(p1, p2, "same (color, width) must reuse the cached pen");
        assert_ne!(
            p1, p3,
            "a different width must not reuse another width's pen"
        );

        // Square-capped and round-capped pens of the identical (color, width) must
        // stay in separate caches — otherwise whichever flavor is requested first
        // would silently hand its caps to every later request of the other flavor.
        let square = glyph_cache::pen(COLORREF(0x000000FF), 2);
        let round = glyph_cache::pen_round(COLORREF(0x000000FF), 2);
        assert_ne!(
            square, round,
            "round-capped and square-capped pens must not collide"
        );

        gdip::shutdown(token);
    }
}
