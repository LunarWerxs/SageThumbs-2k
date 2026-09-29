//! `st2k avif-frame <edge>`: an AVIF on STDIN, its picture fitted within `edge` px as a PNG on
//! STDOUT. The decode is `st2k_codecs::decode::decode_avif_here` (our container reader, `rav1d`,
//! our colour conversion); it runs here and not in the shell extension because `rav1d` panics
//! on some malformed input (see `crates/codecs/src/decode/avif.rs`).

use super::encode_png;

/// Decode threads: this process answers one thumbnail and then exits.
const THREADS: u32 = 4;

pub(super) fn frame_png(input: &[u8]) -> Result<Vec<u8>, String> {
    let max = st2k_codecs::decode::AVIF_CHILD_MAX_EDGE;
    let edge = std::env::args()
        .nth(2)
        .and_then(|a| a.parse::<u32>().ok())
        .unwrap_or(max)
        .clamp(1, max);
    let img = st2k_codecs::decode::decode_avif_here(input, THREADS, Some(edge))
        .map_err(|e| e.to_string())?;
    // The decode already block-averaged it down toward the edge; an area average finishes it.
    let img = if img.width().max(img.height()) > edge {
        img.thumbnail(edge, edge)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    encode_png(rgba.width(), rgba.height(), rgba.into_raw())
}
