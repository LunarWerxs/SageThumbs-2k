//! The head-preview rescues: containers whose baked thumbnail lives in the first
//! bytes (Photoshop PSD/PSB image-resource 1036, Blender's `TEST` block, DWG's preview
//! records), so a bounded prefix read stands in for the whole document.
//!
//! Two entry points on purpose: the UNDER-cap fast path exists to save I/O on a folder
//! of big documents, and only commits when the prefix really does yield a preview; the
//! OVERSIZED one is a rescue for files the cascade would otherwise refuse outright.

use super::*;

/// The head-preview fast path (see the call site in [`stream_source`]): bounded-
/// prefix read + probe for an opaque PSD/PSB or plain `.blend`, any file size.
/// Returns the prefix only when it is strictly smaller than the file (no byte
/// savings otherwise), AND [`crate::container::extract_cover`] — the same extractor
/// the decode tier will run on it — actually finds a preview inside, AND that
/// preview is big enough to answer a `target_edge`-px request (issue #33 — see
/// [`crate::container::upgradable_head_preview_edge`], which is what keeps this
/// narrow enough not to punish `.blend`/`.dwg`). Any miss returns None and the
/// caller proceeds exactly as before this path existed. Rewinds via `stream_prefix`
/// on the hit path and explicitly on the miss paths.
pub(super) unsafe fn head_preview_fast(
    stream: &IStream,
    head: &StreamHead,
    target_edge: u32,
) -> Option<Vec<u8>> {
    let size = head.size?;
    let first = head.first(8);
    if first.len() < 8 {
        return None;
    }
    // G-code carries no magic bytes, so it is reachable only by extension — the
    // same Stat-recovered name the generic-archive probe uses. A stream with no
    // recoverable name (rare virtual sources) simply misses that one member.
    //
    // The extension comes from the head's `Stat`, NOT from `stream_path`: only the file
    // TYPE is wanted here, and a shell stream reports a bare leaf name. `stream_path`
    // refuses a name it cannot resolve to a real file (by design — a relative name would
    // resolve against OUR working directory), so routing this through it meant G-code
    // never matched for anything Explorer handed over.
    let wanted = crate::container::head_preview_len(
        first,
        head.ext.as_deref(),
        &mut IStreamReader {
            stream: stream.clone(),
        },
        decode::HEAD_PREVIEW_BYTES as u64,
    );
    // The length probe seeks the SHARED stream around; park it back at 0 before
    // any return. Every downstream consumer re-seeks anyway — this is insurance
    // for future ones that might not.
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let wanted = wanted?.min(decode::HEAD_PREVIEW_BYTES as u64);
    if wanted >= size {
        return None; // prefix would be the whole file — the normal read is equivalent
    }
    let prefix = stream_prefix(stream, Some(size), wanted as usize)?;
    crate::container::extract_cover(&prefix)?;
    // ISSUE #33. Committing to the prefix here is not just a choice of decoder, it decides
    // what BYTES exist downstream: once we hand back 29 KB of PSD head, the merged composite
    // is not merely slower to reach, it is gone. So a container that has a better picture
    // behind its baked preview gets the size question asked HERE, with the same predicate the
    // decode side uses, or the two would disagree and one of them would do wasted work.
    // Anything else (a `.blend`, a `.dwg`, an unmeasurable preview) answers None and keeps the
    // fast path unconditionally — reading their whole document would buy the same image.
    if let Some(edge) = crate::container::upgradable_head_preview_edge(&prefix) {
        if !decode::embedded_preview_serves(edge, target_edge) {
            return None;
        }
    }
    Some(prefix)
}

/// For an OVERSIZED file (past the in-memory cap): if its magic marks a container
/// whose baked preview lives in the head — Blender `.blend` (`TEST` block ~100 bytes
/// in) or Photoshop PSD/PSB (image resource 1036 just past the header) — read a
/// bounded [`decode::HEAD_PREVIEW_BYTES`] prefix and thumbnail from THAT, instead of
/// skipping to the default icon. Big Blender scenes and PSBs routinely exceed the
/// 100 MB default cap while their thumbnails sit in the first kilobytes (GitHub
/// issue #1). Every container extractor is bounds-checked, so a truncated tail just
/// means "no preview found" (default icon — same as before), never a mis-decode.
pub(super) unsafe fn head_preview_prefix(stream: &IStream, head: &StreamHead) -> Option<Vec<u8>> {
    let first = head.first(8);
    if first.len() < 8 || !crate::container::has_head_preview(first) {
        return None;
    }
    stream_prefix(stream, head.size, decode::HEAD_PREVIEW_BYTES)
}
