//! The picked colour as text: hex, CSS `rgb()`, `hsl()` and `hsv()`. Pure maths, so the
//! clipboard string and the loupe's value row are tested without a screen.

pub(super) fn hex_of((r, g, b): (u8, u8, u8)) -> String {
    format!("#{r:02X}{g:02X}{b:02X}")
}

/// RGB → (hue 0..360, saturation 0..1, lightness 0..1). Textbook; kept exact enough that
/// round numbers come out round (pure red is `hsl(0, 100%, 50%)`, not 99.6%).
pub(super) fn rgb_to_hsl((r, g, b): (u8, u8, u8)) -> (f64, f64, f64) {
    let (r, g, b) = (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if max == min {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if max == r {
        ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    } * 60.0;
    (h, s, l)
}

/// RGB → (hue 0..360, saturation 0..1, value 0..1).
pub(super) fn rgb_to_hsv((r, g, b): (u8, u8, u8)) -> (f64, f64, f64) {
    let (h, _, _) = rgb_to_hsl((r, g, b));
    let (rf, gf, bf) = (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
    let max = rf.max(gf).max(bf);
    let min = rf.min(gf).min(bf);
    let s = if max == 0.0 { 0.0 } else { (max - min) / max };
    (h, s, max)
}

/// The colour formatted for the ACTIVE format: 0 hex (the historical behaviour and the
/// default), 1 CSS `rgb()`, 2 `hsl()`, 3 `hsv()`. Everything that reaches the clipboard —
/// single pick, stash list, history recall — and the loupe's value row go through here, so
/// what you read is always what you get.
pub(super) fn fmt_color(fmt: i32, c: (u8, u8, u8)) -> String {
    match fmt {
        1 => format!("rgb({}, {}, {})", c.0, c.1, c.2),
        2 => {
            let (h, s, l) = rgb_to_hsl(c);
            format!(
                "hsl({}, {}%, {}%)",
                h.round() as i32 % 360,
                (s * 100.0).round() as i32,
                (l * 100.0).round() as i32
            )
        }
        3 => {
            let (h, s, v) = rgb_to_hsv(c);
            format!(
                "hsv({}, {}%, {}%)",
                h.round() as i32 % 360,
                (s * 100.0).round() as i32,
                (v * 100.0).round() as i32
            )
        }
        _ => hex_of(c),
    }
}
