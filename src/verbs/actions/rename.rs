//! Batch-rename from metadata: EXIF capture date / camera for images, and the
//! artist-title / track-title patterns for audio tags.

use super::*;

/// Batch-rename the selected images from their EXIF capture metadata. Files
/// without the needed EXIF (e.g. screenshots) are left untouched. Best-effort —
/// one failure never aborts the rest. Returns counts so the caller can surface a
/// result: only a real rename ERROR (Err) is a failure; a deliberate skip
/// (Ok(false): missing metadata / name clash) is expected and not counted as
/// failed, so `attempted` is renamed + errored (NOT the skips).
pub(super) fn rename_by_exif(paths: &[String], pattern: RenamePattern) -> ActionReport {
    let mut renamed = 0usize;
    let mut skipped = 0usize;
    let mut errored = 0usize;
    for p in paths.iter().filter(|p| is_image(p.as_str())) {
        match rename_one(p, pattern) {
            Ok(true) => renamed += 1,
            Ok(false) => skipped += 1,
            Err(_) => errored += 1,
        }
    }
    if skipped > 0 || errored > 0 {
        crate::safety::log(&format!(
            "Rename by EXIF: {renamed} renamed, {skipped} skipped (no capture date / name clash), \
             {errored} errored"
        ));
    }
    // Count only true attempts (rename or error) — a skip means the file
    // intentionally has nothing to do, so it shouldn't read as "failed".
    let mut r = ActionReport::applied(renamed + errored, renamed);
    if errored > 0 {
        r.note = Some(format!(
            "{errored} couldn't be renamed (locked or name clash)"
        ));
    }
    r
}

/// Rename one file per `pattern`. Returns Ok(true) if renamed, Ok(false) if it
/// was skipped (the source metadata is absent — no EXIF date / no audio tag — or
/// it's already correctly named).
pub(crate) fn rename_one(path: &str, pattern: RenamePattern) -> Result<bool> {
    let Some(base) = rename_base(path, pattern) else {
        return Ok(false); // missing the metadata this pattern needs → leave it alone
    };
    let base = sanitize_component(&base);

    let src = Path::new(path);
    let dir = src.parent().unwrap_or_else(|| Path::new("."));

    // Reserve a free target atomically (see `reserve_dest` — the same race-prone
    // `while target.exists()` picker `fileops::move_into`/`copy_into` used to have,
    // where an external writer landing a file in the gap between the check and the
    // rename could collide). `None` = the source is already correctly named.
    let Some(slot) = reserve_dest(src, dir, &base)? else {
        return Ok(false);
    };

    // Retry briefly: a freshly-selected file can hold a transient Explorer lock.
    crate::fsutil::rename_retrying(src, slot.path())
        .map_err(|e| Error::new(E_FAIL, format!("rename to {}: {e}", slot.path().display())))?;
    slot.release();
    Ok(true)
}

/// The new base name (no extension) for `path` under `pattern`, or None when the
/// source lacks the metadata that pattern needs (EXIF date / audio title).
fn rename_base(path: &str, pattern: RenamePattern) -> Option<String> {
    match pattern {
        RenamePattern::DateTaken | RenamePattern::CameraDate => {
            let meta = crate::strip::read_capture(path);
            let time = meta.time?;
            Some(match pattern {
                RenamePattern::CameraDate => match meta.camera {
                    Some(cam) => format!("{} {time}", sanitize_component(&cam)),
                    None => time,
                },
                _ => time,
            })
        }
        RenamePattern::ArtistTitle | RenamePattern::TrackTitle => {
            tag_base(pattern, &crate::strip::read_audio_tags(path))
        }
    }
}

/// Format an audio-tag rename base. A title is required (it's the anchor); the
/// artist / track prefix is added when present. Pure, so it's unit-testable
/// without a real tagged file.
pub(crate) fn tag_base(pattern: RenamePattern, t: &crate::strip::AudioTags) -> Option<String> {
    let title = t.title.clone()?;
    Some(match pattern {
        RenamePattern::ArtistTitle => match &t.artist {
            Some(a) => format!("{a} - {title}"),
            None => title,
        },
        RenamePattern::TrackTitle => match t.track {
            Some(n) => format!("{n:02} - {title}"),
            None => title,
        },
        _ => return None,
    })
}
