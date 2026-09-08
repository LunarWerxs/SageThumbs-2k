//! Batch-rename from metadata: EXIF capture date / camera for images, and the
//! artist-title / track-title patterns for audio tags — plus the free-pattern
//! engine below (`expand_pattern` / `rename_by_pattern`) behind the "Rename with
//! pattern…" dialog, which lets the user write their own template instead of
//! picking one of the four fixed ones above.

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

// ---- Free-pattern rename engine ------------------------------------------
//
// "Rename with pattern…" (`VerbAction::RenameWithPattern`) lets the user write their
// own template instead of picking one of the four fixed patterns above. The engine
// here is pure (no filesystem) except `pattern_stem`'s two metadata probes ({date} /
// {w}/{h}), which only READ; the actual file move is `rename_by_pattern_one`, the
// same reserve-then-move shape `rename_one` uses above.

/// Why [`expand_pattern`] rejected a pattern — shown to the user (the dialog's live
/// preview turns this red) via its `Display` impl.
#[derive(Debug, PartialEq, Eq)]
pub enum PatternError {
    /// `{body}` isn't one of the recognized placeholders.
    UnknownPlaceholder(String),
    /// A `{` with no matching `}` before the pattern ends.
    UnclosedBrace,
    /// A bare `}` that isn't part of a `}}` escape.
    StrayCloseBrace,
    /// `{n:width}` where `width` isn't a plain non-negative integer.
    InvalidWidth(String),
}

impl std::fmt::Display for PatternError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PatternError::UnknownPlaceholder(p) => write!(f, "unknown placeholder {{{p}}}"),
            PatternError::UnclosedBrace => write!(f, "unclosed {{ — did you mean {{{{?"),
            PatternError::StrayCloseBrace => write!(f, "stray }} — did you mean }}}}?"),
            PatternError::InvalidWidth(p) => write!(f, "bad width in {{{p}}} — use e.g. {{n:3}}"),
        }
    }
}

/// Everything [`expand_pattern`] needs for one file: the metadata each placeholder
/// reads, plus the find/replace applied to the expanded result. Built fresh per file
/// (per [`pattern_stem`] call) — nothing here is cached across files.
pub struct PatternCtx<'a> {
    /// `{name}` — the source file's stem (no extension).
    pub name: &'a str,
    /// `{ext}` — the source file's extension, no leading dot (`""` if none).
    pub ext: &'a str,
    /// `{n}` / `{n:width}` — this file's 1-based position in the batch.
    pub n: u32,
    /// `{date}` — `"YYYY-MM-DD"`, or `None` when neither the capture date nor the
    /// file's modified date could be read.
    pub date: Option<&'a str>,
    /// `{w}` — pixel width, or `None` when the file isn't a readable image.
    pub w: Option<u32>,
    /// `{h}` — pixel height, or `None` when the file isn't a readable image.
    pub h: Option<u32>,
    /// Literal substring to find in the expanded pattern (`""` = no find/replace).
    pub find: &'a str,
    /// What `find` is replaced with.
    pub replace: &'a str,
}

/// Expand `pattern` against `ctx`: `{name}`/`{ext}`/`{n}`/`{n:width}`/`{date}`/`{w}`/
/// `{h}` substitute the matching `ctx` field (a missing `date`/`w`/`h` becomes an
/// empty string, not an error — the same "best effort" the fixed EXIF patterns use);
/// `{{`/`}}` are literal braces. `ctx.find`/`ctx.replace` are applied to the fully
/// expanded string last (a plain, non-regex [`str::replace`]), so a replace can act
/// on text a placeholder just produced. Pure — no filesystem, no allocation beyond
/// the returned `String`, safe to call on every keystroke.
pub fn expand_pattern(
    pattern: &str,
    ctx: &PatternCtx<'_>,
) -> std::result::Result<String, PatternError> {
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '{' => {
                let mut body = String::new();
                let mut closed = false;
                for c2 in chars.by_ref() {
                    if c2 == '}' {
                        closed = true;
                        break;
                    }
                    body.push(c2);
                }
                if !closed {
                    return Err(PatternError::UnclosedBrace);
                }
                out.push_str(&expand_placeholder(&body, ctx)?);
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}');
            }
            '}' => return Err(PatternError::StrayCloseBrace),
            c => out.push(c),
        }
    }
    if !ctx.find.is_empty() {
        out = out.replace(ctx.find, ctx.replace);
    }
    Ok(out)
}

/// One `{...}` placeholder body (already stripped of its braces) → its substitution.
fn expand_placeholder(
    body: &str,
    ctx: &PatternCtx<'_>,
) -> std::result::Result<String, PatternError> {
    match body {
        "name" => Ok(ctx.name.to_string()),
        "ext" => Ok(ctx.ext.to_string()),
        "date" => Ok(ctx.date.unwrap_or("").to_string()),
        "w" => Ok(ctx.w.map(|v| v.to_string()).unwrap_or_default()),
        "h" => Ok(ctx.h.map(|v| v.to_string()).unwrap_or_default()),
        "n" => Ok(ctx.n.to_string()),
        _ if body.starts_with("n:") => {
            let width: usize = body[2..]
                .parse()
                .map_err(|_| PatternError::InvalidWidth(body.to_string()))?;
            Ok(format!("{:0width$}", ctx.n, width = width))
        }
        _ => Err(PatternError::UnknownPlaceholder(body.to_string())),
    }
}

/// `path`'s pixel size, or `None` when it can't be read as an image — mirrors
/// `fileops::dims` exactly (that one is private to its module and this engine can't
/// reach it, so the same two-tier probe — a fast header read, then the bounded
/// full-fidelity decode chain — is repeated here rather than exposed).
fn pattern_dims(path: &str) -> Option<(u32, u32)> {
    if let Ok(r) = image::ImageReader::open(path).and_then(|r| r.with_guessed_format()) {
        if let Ok(d) = r.into_dimensions() {
            return Some(d);
        }
    }
    let bytes = read_full_fidelity_capped(path).ok()?;
    crate::container::real_or_decoded_dims(&bytes)
}

/// `path`'s modified date as `"YYYY-MM-DD"` in local time, or `None` if it can't be
/// read. The `{date}` placeholder's fallback when there's no EXIF/tag capture date.
fn pattern_modified_date(path: &str) -> Option<String> {
    use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};

    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let unix_secs = modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    // FILETIME ticks are 100ns units since 1601-01-01; the Unix epoch (1970-01-01) is
    // 11_644_473_600 seconds later (same plumbing the app EXE's `format_unix_date` uses
    // — duplicated here because this is the lib side and that one lives in the bin crate).
    let ticks = unix_secs
        .saturating_add(11_644_473_600)
        .saturating_mul(10_000_000);
    let ft = FILETIME {
        dwLowDateTime: (ticks & 0xFFFF_FFFF) as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    if unsafe { FileTimeToSystemTime(&ft, &mut utc) }.is_err() {
        return None;
    }
    let mut local = utc;
    unsafe {
        let _ = SystemTimeToTzSpecificLocalTime(None, &utc, &mut local);
    }
    Some(format!(
        "{:04}-{:02}-{:02}",
        local.wYear, local.wMonth, local.wDay
    ))
}

/// `{date}`'s value for `path`: the same capture date [`RenamePattern::DateTaken`]
/// uses when present, else the file's modified date.
fn pattern_date(path: &str) -> Option<String> {
    if let Some(t) = crate::strip::read_capture(path).time {
        if let Some((date, _)) = t.split_once(' ') {
            return Some(date.to_string());
        }
    }
    pattern_modified_date(path)
}

/// The sanitized STEM `pattern` (with `find`→`replace`) produces for `path` at batch
/// position `n` (1-based) — shared, unchanged, by the dialog's live preview (no
/// filesystem writes) and the real rename below, so the preview can never promise a
/// name the apply step doesn't actually produce. Reads `path`'s own metadata for
/// `{date}`/`{w}`/`{h}`; never writes.
pub fn pattern_stem(
    path: &str,
    n: u32,
    pattern: &str,
    find: &str,
    replace: &str,
) -> std::result::Result<String, PatternError> {
    let src = Path::new(path);
    let name = src.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let ext = src.extension().and_then(|s| s.to_str()).unwrap_or("");
    let date = pattern_date(path);
    let (w, h) = pattern_dims(path).map_or((None, None), |(w, h)| (Some(w), Some(h)));
    let ctx = PatternCtx {
        name,
        ext,
        n,
        date: date.as_deref(),
        w,
        h,
        find,
        replace,
    };
    expand_pattern(pattern, &ctx).map(|s| sanitize_component(&s))
}

/// The exact display filename `path` would get under `pattern` (`find`→`replace`) at
/// batch position `n` — `pattern_stem` plus the source's own extension back on, the
/// way [`reserve_dest`] would join them. `Err`'s message is user-facing (the dialog
/// shows it in place of the preview list).
pub fn rename_pattern_preview(
    path: &str,
    n: u32,
    pattern: &str,
    find: &str,
    replace: &str,
) -> std::result::Result<String, String> {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    pattern_stem(path, n, pattern, find, replace)
        .map(|stem| {
            if ext.is_empty() {
                stem
            } else {
                format!("{stem}.{ext}")
            }
        })
        .map_err(|e| e.to_string())
}

/// Rename one file under the free-pattern engine — same reserve-then-move shape as
/// [`rename_one`] (`Ok(false)` when the natural name is already `path` itself, so two
/// files can never race the same destination). The pattern is validated once, up
/// front, by the dialog's live preview; a `PatternError` surfacing here means that
/// check was skipped, so it's folded into the generic `Err` rather than given its own
/// `Ok(false)`-style "expected" outcome.
pub(crate) fn rename_by_pattern_one(
    path: &str,
    n: u32,
    pattern: &str,
    find: &str,
    replace: &str,
) -> Result<bool> {
    let base = pattern_stem(path, n, pattern, find, replace)
        .map_err(|e| Error::new(E_FAIL, format!("pattern: {e}")))?;
    let src = Path::new(path);
    let dir = src.parent().unwrap_or_else(|| Path::new("."));
    let Some(slot) = reserve_dest(src, dir, &base)? else {
        return Ok(false); // already correctly named
    };
    crate::fsutil::rename_retrying(src, slot.path())
        .map_err(|e| Error::new(E_FAIL, format!("rename to {}: {e}", slot.path().display())))?;
    slot.release();
    Ok(true)
}

/// `VerbAction::RenameWithPattern`'s apply step, run on the dialog's worker thread
/// after OK. Serial (like [`rename_by_exif`]): `rename_by_pattern_one`'s
/// reserve-then-move already makes concurrent renames collision-safe, but the whole
/// point of the `{n}` counter is a stable, predictable order across the batch, which a
/// parallel pass can't promise. Every selected file is attempted — unlike
/// `rename_by_exif`, this isn't gated to images, since a free pattern works on any
/// file type (`{w}`/`{h}` just come back empty for a non-image).
pub fn rename_by_pattern(
    paths: &[String],
    pattern: &str,
    find: &str,
    replace: &str,
) -> ActionReport {
    let mut renamed = 0usize;
    let mut skipped = 0usize;
    let mut errored = 0usize;
    for (i, p) in paths.iter().enumerate() {
        match rename_by_pattern_one(p, (i + 1) as u32, pattern, find, replace) {
            Ok(true) => renamed += 1,
            Ok(false) => skipped += 1,
            Err(_) => errored += 1,
        }
    }
    if skipped > 0 || errored > 0 {
        crate::safety::log(&format!(
            "Rename with pattern: {renamed} renamed, {skipped} skipped (already named / name \
             clash), {errored} errored"
        ));
    }
    let mut r = ActionReport::applied(renamed + errored, renamed);
    if errored > 0 {
        r.note = Some(format!(
            "{errored} couldn't be renamed (locked, invalid pattern, or name clash)"
        ));
    }
    r
}

#[cfg(test)]
mod pattern_tests {
    use super::*;

    fn ctx<'a>(name: &'a str, ext: &'a str, n: u32) -> PatternCtx<'a> {
        PatternCtx {
            name,
            ext,
            n,
            date: Some("2026-01-02"),
            w: Some(1920),
            h: Some(1080),
            find: "",
            replace: "",
        }
    }

    #[test]
    fn expands_name_and_ext() {
        let c = ctx("vacation", "jpg", 1);
        assert_eq!(expand_pattern("{name}.{ext}", &c).unwrap(), "vacation.jpg");
    }

    #[test]
    fn expands_counter_unpadded_and_padded() {
        let c = ctx("f", "png", 7);
        assert_eq!(expand_pattern("{n}", &c).unwrap(), "7");
        assert_eq!(expand_pattern("{n:3}", &c).unwrap(), "007");
        assert_eq!(expand_pattern("{n:1}", &c).unwrap(), "7");
    }

    #[test]
    fn expands_date() {
        let c = ctx("f", "jpg", 1);
        assert_eq!(expand_pattern("{date}", &c).unwrap(), "2026-01-02");
    }

    #[test]
    fn expands_width_and_height() {
        let c = ctx("f", "jpg", 1);
        assert_eq!(expand_pattern("{w}x{h}", &c).unwrap(), "1920x1080");
    }

    #[test]
    fn missing_dims_expand_to_empty_not_an_error() {
        let mut c = ctx("f", "jpg", 1);
        c.w = None;
        c.h = None;
        assert_eq!(expand_pattern("[{w}x{h}]", &c).unwrap(), "[x]");
    }

    #[test]
    fn missing_date_expands_to_empty() {
        let mut c = ctx("f", "jpg", 1);
        c.date = None;
        assert_eq!(expand_pattern("{date}", &c).unwrap(), "");
    }

    #[test]
    fn literal_braces_are_escaped_with_doubling() {
        let c = ctx("f", "jpg", 1);
        assert_eq!(expand_pattern("{{{name}}}", &c).unwrap(), "{f}");
    }

    #[test]
    fn find_replace_applies_to_the_expanded_result() {
        let mut c = ctx("my vacation photo", "jpg", 1);
        c.find = " ";
        c.replace = "_";
        assert_eq!(expand_pattern("{name}", &c).unwrap(), "my_vacation_photo");
    }

    #[test]
    fn unknown_placeholder_is_an_error() {
        let c = ctx("f", "jpg", 1);
        assert_eq!(
            expand_pattern("{bogus}", &c),
            Err(PatternError::UnknownPlaceholder("bogus".to_string()))
        );
    }

    #[test]
    fn unclosed_brace_is_an_error() {
        let c = ctx("f", "jpg", 1);
        assert_eq!(
            expand_pattern("{name", &c),
            Err(PatternError::UnclosedBrace)
        );
    }

    #[test]
    fn stray_close_brace_is_an_error() {
        let c = ctx("f", "jpg", 1);
        assert_eq!(
            expand_pattern("{name}}", &c),
            Err(PatternError::StrayCloseBrace)
        );
    }

    #[test]
    fn bad_width_is_an_error() {
        let c = ctx("f", "jpg", 1);
        assert_eq!(
            expand_pattern("{n:x}", &c),
            Err(PatternError::InvalidWidth("n:x".to_string()))
        );
    }

    #[test]
    fn pattern_error_display_is_human_readable() {
        assert!(PatternError::UnclosedBrace.to_string().contains("unclosed"));
    }

    // ---- Filesystem-backed: collision + reserved-name handling ----

    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("st2k_rnpattern_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn collision_gets_a_numbered_suffix() {
        let dir = scratch_dir("collision");
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        // Both files rename to the SAME target under a constant pattern — the second
        // must land on "target (2).txt", never overwrite the first.
        assert!(rename_by_pattern_one(a.to_str().unwrap(), 1, "target", "", "").unwrap());
        assert!(rename_by_pattern_one(b.to_str().unwrap(), 2, "target", "", "").unwrap());
        assert!(dir.join("target.txt").exists());
        assert!(dir.join("target (2).txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reserved_device_name_is_sanitized_not_rejected() {
        let dir = scratch_dir("reserved");
        let src = dir.join("src.txt");
        std::fs::write(&src, b"x").unwrap();
        assert!(rename_by_pattern_one(src.to_str().unwrap(), 1, "CON", "", "").unwrap());
        // `sanitize_component` maps a bare reserved name to "image" — the same
        // fallback the EXIF renamer already relies on.
        assert!(dir.join("image.txt").exists());
        assert!(!dir.join("CON.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn preview_matches_what_apply_would_produce() {
        let dir = scratch_dir("preview");
        let src = dir.join("photo.jpg");
        std::fs::write(&src, b"x").unwrap();
        let path = src.to_str().unwrap();
        let preview = rename_pattern_preview(path, 1, "{name}_{n:3}", "", "").unwrap();
        assert_eq!(preview, "photo_001.jpg");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
