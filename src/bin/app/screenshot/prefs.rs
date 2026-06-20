//! Tiny persisted prefs for the screenshot tool (HKCU `Software\SageThumbs2K`,
//! the same root the rest of the app uses — see `settings.rs`). Currently just the
//! user's recent custom annotation colours, stored as one small `RRGGBB,RRGGBB,…`
//! string so the palette flyout can offer them across captures. No new file, no bloat.

use windows::Win32::Foundation::COLORREF;

const KEY: &str = sagethumbs2k_core::settings::ROOT;
const VAL: &str = "ScreenshotCustomColors";
const MAX: usize = 4;

/// COLORREF (0x00BBGGRR) → `"RRGGBB"`.
fn to_hex(c: COLORREF) -> String {
    let v = c.0;
    format!("{:02X}{:02X}{:02X}", v & 0xFF, (v >> 8) & 0xFF, (v >> 16) & 0xFF)
}

/// `"RRGGBB"` → COLORREF.
fn from_hex(s: &str) -> Option<COLORREF> {
    let v = u32::from_str_radix(s.trim(), 16).ok()?;
    let (r, g, b) = ((v >> 16) & 0xFF, (v >> 8) & 0xFF, v & 0xFF);
    Some(COLORREF(r | (g << 8) | (b << 16)))
}

/// The remembered custom colours (newest first, up to 4).
pub(super) fn load_custom_colors() -> Vec<COLORREF> {
    let Ok(k) = windows_registry::CURRENT_USER.open(KEY) else {
        return Vec::new();
    };
    let Ok(s) = k.get_string(VAL) else {
        return Vec::new();
    };
    s.split(',').filter_map(from_hex).take(MAX).collect()
}

/// Remember `c` as the most-recent custom colour (move-to-front, dedup, cap 4).
/// Best-effort — a registry failure just means it isn't remembered.
pub(super) fn remember_custom_color(c: COLORREF) {
    let mut list = load_custom_colors();
    list.retain(|x| x.0 != c.0);
    list.insert(0, c);
    list.truncate(MAX);
    let joined = list.iter().map(|&c| to_hex(c)).collect::<Vec<_>>().join(",");
    if let Ok(k) = windows_registry::CURRENT_USER.create(KEY) {
        let _ = k.set_string(VAL, &joined);
    }
}
