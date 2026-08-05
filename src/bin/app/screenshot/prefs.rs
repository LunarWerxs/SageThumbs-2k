//! Tiny persisted prefs for the screenshot tool. Stored in the same root the rest of the
//! app uses — see `settings.rs`, which is also what decides whether that root is HKCU or the
//! portable ini. Currently just the user's recent custom annotation colours, kept as one
//! small `RRGGBB,RRGGBB,…` string so the palette flyout can offer them across captures.

use windows::Win32::Foundation::COLORREF;

use sagethumbs2k_core::settings;

const VAL: &str = "ScreenshotCustomColors";
const MAX: usize = 4;

/// COLORREF (0x00BBGGRR) → `"RRGGBB"`.
fn to_hex(c: COLORREF) -> String {
    let v = c.0;
    format!(
        "{:02X}{:02X}{:02X}",
        v & 0xFF,
        (v >> 8) & 0xFF,
        (v >> 16) & 0xFF
    )
}

/// `"RRGGBB"` → COLORREF.
fn from_hex(s: &str) -> Option<COLORREF> {
    let v = u32::from_str_radix(s.trim(), 16).ok()?;
    let (r, g, b) = ((v >> 16) & 0xFF, (v >> 8) & 0xFF, v & 0xFF);
    Some(COLORREF(r | (g << 8) | (b << 16)))
}

/// The remembered custom colours (newest first, up to 4).
pub(super) fn load_custom_colors() -> Vec<COLORREF> {
    let Some(s) = settings::get_string_opt(VAL) else {
        return Vec::new();
    };
    s.split(',').filter_map(from_hex).take(MAX).collect()
}

/// Remember `c` as the most-recent custom colour (move-to-front, dedup, cap 4).
/// Best-effort — a write failure just means it isn't remembered.
pub(super) fn remember_custom_color(c: COLORREF) {
    let mut list = load_custom_colors();
    list.retain(|x| x.0 != c.0);
    list.insert(0, c);
    list.truncate(MAX);
    let joined = list
        .iter()
        .map(|&c| to_hex(c))
        .collect::<Vec<_>>()
        .join(",");
    let _ = settings::set_string(VAL, &joined);
}
