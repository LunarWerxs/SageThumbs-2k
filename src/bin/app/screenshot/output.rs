//! Where a finished capture goes: the system clipboard (CF_DIB) and a timestamped
//! PNG. Both take the already-composited top-down BGRA pixels from `overlay.rs`, so
//! this file knows nothing about windows or annotations.

use windows::Win32::Graphics::Gdi::BITMAPINFOHEADER;

use crate::win::window_shot::{encode_png, to_rgba};

/// Put a packed CF_DIB (bottom-up BGRA) on the clipboard from top-down BGRA pixels.
/// Returns whether the clipboard actually took it — the editor-less instant capture
/// surfaces a failure (there's no other sign), the overlay's own flows already show
/// a "Copied" state and keep discarding it.
pub(super) unsafe fn copy_dib_to_clipboard(top_down_bgra: &[u8], w: i32, h: i32) -> bool {
    let header = core::mem::size_of::<BITMAPINFOHEADER>();
    let row = (w * 4) as usize;
    let total = header + row * h as usize;
    let mut dib = Vec::with_capacity(total);
    st2k_base::dib::push_cf_dib_header(&mut dib, w, h, header);

    // Emit rows bottom-up from the top-down source.
    for y in (0..h as usize).rev() {
        dib.extend_from_slice(&top_down_bgra[y * row..y * row + row]);
    }

    // The unsafe HGLOBAL ownership dance lives once in the lib's `clipboard` module.
    st2k_base::clipboard::set_clipboard(st2k_base::clipboard::CF_DIB, &dib)
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
    // Encode first: a reserved name is only worth keeping once there are bytes for it.
    let Some(png) =
        encode_png(|buf| img.write_to(&mut std::io::Cursor::new(buf), image::ImageFormat::Png))
    else {
        return false;
    };
    let name = unsafe { timestamped_name() };
    write_reserved(dir, &name, &png).is_some()
}

/// Reserve a fresh name in `dir` and write `bytes` into it; the path on success. A write that
/// fails removes the reservation again, so a failed capture never leaves an empty file where
/// the next capture's disambiguator would count it as a taken name.
pub(super) fn write_reserved(
    dir: &std::path::Path,
    name: &str,
    bytes: &[u8],
) -> Option<std::path::PathBuf> {
    use std::io::Write;
    let (path, mut file) = reserve_unique_in(dir, name).ok()?;
    if file.write_all(bytes).and_then(|()| file.flush()).is_err() {
        drop(file);
        let _ = std::fs::remove_file(&path);
        return None;
    }
    Some(path)
}

/// Reserve a filename in `dir` that nothing else owns, appending " (2)", " (3)", ... before the
/// extension when `name` is taken, and return it OPEN: the entry is created with `create_new`,
/// so the reservation and the check are one operation. `timestamped_name` only has 1-second
/// resolution, and until 2026-09-19 this picked a free name with `exists()` and handed it back
/// unreserved, so two captures in the same tick could both be told the same name and the
/// second silently overwrote the first (audit concern 6). `pub(super)` so `upload.rs`'s
/// failed-upload recovery copy routes through the same guard instead of writing
/// `dir.join(name)` directly.
pub(super) fn reserve_unique_in(
    dir: &std::path::Path,
    name: &str,
) -> std::io::Result<(std::path::PathBuf, std::fs::File)> {
    let stem = std::path::Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name);
    let ext = std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("png");
    // Give up disambiguating past a pathological run rather than looping forever; the last
    // attempt's error is the answer then, never an overwrite.
    let mut last = None;
    for n in 1u32..=1000 {
        let candidate = if n == 1 {
            dir.join(name)
        } else {
            dir.join(format!("{stem} ({n}).{ext}"))
        };
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("no free capture name")))
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
    /// 1-second resolution, so without a disambiguator the second capture's write would
    /// silently destroy the first one with no error.
    #[test]
    fn unique_name_in_does_not_collide_with_an_existing_same_second_capture() {
        let dir =
            std::env::temp_dir().join(format!("st2k_unique_name_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        let name = "Screenshot 2026-01-01 00.00.00.png";
        std::fs::write(dir.join(name), b"first capture").expect("write first capture");

        let (picked, _file) = reserve_unique_in(&dir, name).expect("reserve");
        assert_ne!(
            picked,
            dir.join(name),
            "a second same-second capture must not be pointed at the first capture's path"
        );
        assert_eq!(picked, dir.join("Screenshot 2026-01-01 00.00.00 (2).png"));
        assert_eq!(
            std::fs::read(dir.join(name)).unwrap(),
            b"first capture",
            "the first capture's bytes are untouched"
        );

        // A name with no existing collision is returned unchanged.
        let (free, _f) = reserve_unique_in(&dir, "Screenshot 2026-01-01 00.00.01.png").unwrap();
        assert_eq!(free, dir.join("Screenshot 2026-01-01 00.00.01.png"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Audit concern 6 (2026-09-19): the name is RESERVED when it is picked, not merely found
    /// free. Two picks with no write in between - two captures in one tick, or a recovery copy
    /// landing beside a Ctrl+S save - must come back with different names, and the reservation
    /// must be the entry the bytes then land in.
    #[test]
    fn two_reservations_in_one_tick_get_different_names() {
        let dir =
            std::env::temp_dir().join(format!("st2k_reserve_twice_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        let name = "Screenshot 2026-01-01 00.00.00.png";
        let (a, _fa) = reserve_unique_in(&dir, name).expect("first reservation");
        let (b, _fb) = reserve_unique_in(&dir, name).expect("second reservation");
        let (c, _fc) = reserve_unique_in(&dir, name).expect("third reservation");
        assert_eq!(a, dir.join(name));
        assert_eq!(b, dir.join("Screenshot 2026-01-01 00.00.00 (2).png"));
        assert_eq!(c, dir.join("Screenshot 2026-01-01 00.00.00 (3).png"));
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            3,
            "three reserved entries"
        );

        // Writing through the reservation lands in the reserved entry, and a same-named write
        // afterwards still steps past every reservation.
        let d = write_reserved(&dir, name, b"fourth").expect("write");
        assert_eq!(d, dir.join("Screenshot 2026-01-01 00.00.00 (4).png"));
        assert_eq!(std::fs::read(&d).unwrap(), b"fourth");

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
