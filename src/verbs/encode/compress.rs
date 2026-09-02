//! `compress` - shrink an image until it fits a target BYTE size.
//!
//! A binary search over JPEG quality, then a scale-down if quality alone cannot get
//! there. Separate from the plain convert path because its loop is driven by the encoded
//! SIZE rather than by any pixel property.

use super::*;

/// JPEG quality search bounds for [`compress_to_size`].
pub(super) const COMPRESS_Q_MIN: u8 = 20;
pub(super) const COMPRESS_Q_MAX: u8 = 95;

/// Encode `img` to in-memory JPEG bytes at `quality` — the probe the size search uses.
pub(super) fn jpeg_bytes(img: &DynamicImage, quality: u8) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    img.write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(
        &mut buf, quality,
    ))
    .map_err(|e| Error::new(E_FAIL, format!("jpeg encode at q{quality}: {e}")))?;
    Ok(buf)
}

/// Highest-quality JPEG of `img` at or under `target` bytes (binary search on quality),
/// or `None` if even [`COMPRESS_Q_MIN`] overshoots — then the caller downscales + retries.
pub(super) fn jpeg_under(img: &DynamicImage, target: u64) -> Result<Option<Vec<u8>>> {
    let floor = jpeg_bytes(img, COMPRESS_Q_MIN)?;
    if floor.len() as u64 > target {
        return Ok(None);
    }
    let (mut lo, mut hi) = (COMPRESS_Q_MIN, COMPRESS_Q_MAX);
    let mut best = floor;
    while lo <= hi {
        let mid = lo + (hi - lo) / 2;
        let b = jpeg_bytes(img, mid)?;
        if b.len() as u64 <= target {
            best = b;
            lo = mid + 1; // fits — try higher quality
        } else if mid == 0 {
            break;
        } else {
            hi = mid - 1;
        }
    }
    Ok(Some(best))
}

/// Compress `path` into a JPEG at or under `target_bytes`, by binary-searching JPEG
/// quality and — if even the lowest quality overshoots — progressively downscaling (20%
/// a step, down to a ~32px floor). The "(compressed)" sibling never upscales and never
/// overwrites the original. With an unreasonably tiny target it ships the smallest it can
/// make (which may slightly exceed it). Reusable by the CLI and a future menu/dialog.
pub fn compress_to_size(path: &str, target_bytes: u64) -> Result<PathBuf> {
    let bytes = read_full_fidelity_capped(path)?;
    // JPEG has no alpha → flatten transparency onto white, like shrink-for-email.
    // `decode_full_for_path` (not `decode_full`): the path-aware decode is what every
    // sibling verb uses, so a camera RAW gets the same full-resolution re-read here.
    let mut img = flatten_onto_white(&decode::decode_full_for_path(&bytes, path)?);
    let target = target_bytes.max(1);

    let mut chosen = None;
    for _ in 0..8 {
        if let Some(b) = jpeg_under(&img, target)? {
            chosen = Some(b);
            break;
        }
        let (w, h) = (img.width(), img.height());
        if w.min(h) <= 32 {
            break; // already tiny — stop shrinking
        }
        img = img.resize(
            (w * 4 / 5).max(1),
            (h * 4 / 5).max(1),
            image::imageops::FilterType::Lanczos3,
        );
    }
    let data = match chosen {
        Some(b) => b,
        None => jpeg_bytes(&img, COMPRESS_Q_MIN)?, // best-effort floor
    };

    let src = Path::new(path);
    let slot = reserve_unique_suffix(src, "compressed", "jpg");
    write_atomic(slot.path(), |tmp| {
        std::fs::write(tmp, &data)
            .map_err(|e| Error::new(E_FAIL, format!("write {}: {e}", tmp.display())))
    })?;
    preserve_src_time(src, slot.path());
    Ok(slot.path().to_path_buf())
}
