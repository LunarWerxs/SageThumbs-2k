//! Where a finished capture goes: the system clipboard (CF_DIB) and a timestamped
//! PNG. Both take the already-composited top-down BGRA pixels from `overlay.rs`, so
//! this file knows nothing about windows or annotations.

use windows::Win32::Graphics::Gdi::BITMAPINFOHEADER;

/// Put a packed CF_DIB (bottom-up BGRA) on the clipboard from top-down BGRA pixels.
/// Returns whether the clipboard actually took it — the editor-less instant capture
/// surfaces a failure (there's no other sign), the overlay's own flows already show
/// a "Copied" state and keep discarding it.
pub(super) unsafe fn copy_dib_to_clipboard(top_down_bgra: &[u8], w: i32, h: i32) -> bool {
    let header = core::mem::size_of::<BITMAPINFOHEADER>();
    let row = (w * 4) as usize;
    let total = header + row * h as usize;
    let mut dib = Vec::with_capacity(total);
    dib.extend_from_slice(&(header as u32).to_le_bytes()); // biSize
    dib.extend_from_slice(&w.to_le_bytes()); // biWidth
    dib.extend_from_slice(&h.to_le_bytes()); // biHeight (positive = bottom-up)
    dib.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    dib.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    dib.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
    dib.extend_from_slice(&0u32.to_le_bytes()); // biSizeImage
    dib.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter
    dib.extend_from_slice(&0i32.to_le_bytes()); // biYPelsPerMeter
    dib.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    dib.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant
                                                // Emit rows bottom-up from the top-down source.
    for y in (0..h as usize).rev() {
        dib.extend_from_slice(&top_down_bgra[y * row..y * row + row]);
    }

    // The unsafe HGLOBAL ownership dance lives once in the lib's `clipboard` module.
    sagethumbs2k_core::clipboard::set_clipboard(sagethumbs2k_core::clipboard::CF_DIB, &dib)
}

/// BGRA (top-down) -> an opaque RGBA image (GDI bitmaps carry no alpha).
fn to_rgba(top_down_bgra: &[u8], w: i32, h: i32) -> Option<image::RgbaImage> {
    let mut rgba = vec![0u8; top_down_bgra.len()];
    for (dst, src) in rgba.chunks_exact_mut(4).zip(top_down_bgra.chunks_exact(4)) {
        dst[0] = src[2];
        dst[1] = src[1];
        dst[2] = src[0];
        dst[3] = 255;
    }
    image::RgbaImage::from_raw(w as u32, h as u32, rgba)
}

/// A capture's default filename, e.g. `Screenshot 2026-06-18 09.41.07.png`.
pub(super) unsafe fn timestamped_name() -> String {
    let st = windows::Win32::System::SystemInformation::GetLocalTime();
    format!(
        "Screenshot {:04}-{:02}-{:02} {:02}.{:02}.{:02}.png",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
    )
}

/// Auto-save a timestamped PNG into `dir` (created if missing). Used by Ctrl+S / the
/// Save button when "use a fixed save folder" is on, and by the editor-less instant
/// capture. Returns whether the file was written.
pub(super) fn save_png_to_dir(dir: &std::path::Path, top_down_bgra: &[u8], w: i32, h: i32) -> bool {
    let Some(img) = to_rgba(top_down_bgra, w, h) else {
        return false;
    };
    let _ = std::fs::create_dir_all(dir);
    let name = unsafe { timestamped_name() };
    img.save(unique_name_in(dir, &name)).is_ok()
}

/// Pick a filename in `dir` that doesn't already exist, appending " (2)", " (3)", … before the
/// extension when `name` collides. `timestamped_name` only has 1-second resolution, so two
/// captures started within the same second would otherwise silently overwrite each other —
/// `img.save` has no "don't clobber" mode of its own.
fn unique_name_in(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let stem = std::path::Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name);
    let ext = std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("png");
    let mut n = 2u32;
    loop {
        let candidate = dir.join(format!("{stem} ({n}).{ext}"));
        // Give up disambiguating past a pathological run rather than looping forever; a rare
        // overwrite here beats a hang on the capture path.
        if !candidate.exists() || n >= 1000 {
            return candidate;
        }
        n += 1;
    }
}

/// Save the PNG to an exact path — the location the user chose in the Save-As dialog
/// (Ctrl+S / Save with the fixed-folder option off). Returns whether it was written.
pub(super) fn save_png_to_path(
    path: &std::path::Path,
    top_down_bgra: &[u8],
    w: i32,
    h: i32,
) -> bool {
    let Some(img) = to_rgba(top_down_bgra, w, h) else {
        return false;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    img.save(path).is_ok()
}

/// Save the capture to a unique temp PNG and return its path — the handoff to a helper
/// process (`--upload`, `--ocr`), which owns the file and deletes it once it has read it.
/// If the helper never starts, the caller (`overlay::compose_and_spawn`) deletes it instead.
pub(super) fn save_temp_png(top_down_bgra: &[u8], w: i32, h: i32) -> Option<String> {
    let img = to_rgba(top_down_bgra, w, h)?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir();
    sweep_stale_captures(&dir);
    let mut p = dir.clone();
    p.push(format!("st2k_shot_{}_{}.png", std::process::id(), nanos));
    img.save(&p).ok()?;
    p.to_str().map(|s| s.to_string())
}

/// Age past which a leftover `st2k_shot_*.png` is certainly orphaned. The handoff is
/// read-and-delete within a second or two, so anything from yesterday belongs to a run that died
/// between spawning the helper and the helper reading the file.
const CAPTURE_TTL: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Delete stale capture hand-off PNGs from `dir`. These are pictures of the user's screen, so
/// they shouldn't sit in `%TEMP%` indefinitely when the read-and-delete handshake is interrupted
/// (helper killed at startup, machine powered off mid-capture). Best-effort and cheap: it runs
/// only when a new capture is being written, and never touches a file younger than the TTL, so a
/// hand-off in flight — including a concurrent one from another instance — is always safe.
fn sweep_stale_captures(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten().take(4096) {
        let name = e.file_name();
        let Some(n) = name.to_str() else { continue };
        if !(n.starts_with("st2k_shot_") && n.ends_with(".png")) {
            continue;
        }
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > CAPTURE_TTL);
        if old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two captures landing on the same second must not collide — `timestamped_name` only has
    /// 1-second resolution, so without a disambiguator the second capture's `img.save` would
    /// silently destroy the first one with no error.
    #[test]
    fn unique_name_in_does_not_collide_with_an_existing_same_second_capture() {
        let dir =
            std::env::temp_dir().join(format!("st2k_unique_name_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        let name = "Screenshot 2026-01-01 00.00.00.png";
        std::fs::write(dir.join(name), b"first capture").expect("write first capture");

        let picked = unique_name_in(&dir, name);
        assert_ne!(
            picked,
            dir.join(name),
            "a second same-second capture must not be pointed at the first capture's path"
        );
        assert!(
            !picked.exists(),
            "the disambiguated path must not itself already be taken"
        );

        // A name with no existing collision is returned unchanged.
        let free = unique_name_in(&dir, "Screenshot 2026-01-01 00.00.01.png");
        assert_eq!(free, dir.join("Screenshot 2026-01-01 00.00.01.png"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The sweep must only ever take files that are BOTH ours and old. Getting this wrong deletes
    /// a capture out from under a hand-off that's still in flight — i.e. the user's screenshot
    /// vanishes — or eats an unrelated file in `%TEMP%`.
    #[test]
    fn capture_sweep_takes_only_stale_captures() {
        let dir = std::env::temp_dir().join(format!("st2k_sweep_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        use std::io::Write;
        let write = |name: &str, age: Option<std::time::Duration>| {
            let p = dir.join(name);
            let mut f = std::fs::File::create(&p).expect("create");
            f.write_all(b"x").expect("write");
            if let Some(a) = age {
                f.set_modified(std::time::SystemTime::now() - a)
                    .expect("mtime");
            }
            p
        };

        let fresh = write("st2k_shot_1234_5678.png", None);
        let stale = write("st2k_shot_4321_8765.png", Some(CAPTURE_TTL * 2));
        // Old, but not ours — and ours-looking, but not a .png.
        let other = write("someone_elses.png", Some(CAPTURE_TTL * 2));
        let notpng = write("st2k_shot_9_9.tmp", Some(CAPTURE_TTL * 2));

        sweep_stale_captures(&dir);

        assert!(
            fresh.exists(),
            "deleted a capture that could still be in flight"
        );
        assert!(
            !stale.exists(),
            "left a stale capture of the user's screen behind"
        );
        assert!(other.exists(), "deleted a file that isn't ours");
        assert!(notpng.exists(), "deleted a non-capture file");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
