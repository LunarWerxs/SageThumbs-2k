//! The Set-as-wallpaper verb: decode any supported format, cap it to the screen,
//! write it as a PNG under %APPDATA% and hand it to the desktop.

use super::*;

/// %APPDATA%\SageThumbs2K (created on demand) — where the wallpaper image lives.
fn appdata_dir() -> Result<PathBuf> {
    let base = std::env::var("APPDATA")
        .map_err(|e| Error::new(E_FAIL, format!("%APPDATA% not set: {e}")))?;
    let dir = Path::new(&base).join("SageThumbs2K");
    std::fs::create_dir_all(&dir)
        .map_err(|e| Error::new(E_FAIL, format!("create {}: {e}", dir.display())))?;
    Ok(dir)
}

/// Decode `path` (any supported format, incl. ones Windows can't read directly)
/// and write it as a PNG `dir` can hold. Returns the written image path. Split
/// out from [`prepare_wallpaper`] so tests can target a temp dir instead of the
/// real `%APPDATA%` (writing the production wallpaper.png from a test would
/// pollute the live desktop state).
pub fn prepare_wallpaper_in(dir: &Path, path: &str) -> Result<PathBuf> {
    let bytes = read_full_fidelity_capped(path)?;
    // A wallpaper never needs more than screen resolution; downscale large
    // sources so we don't re-encode (and block the shell thread on) a giant PNG.
    let img = cap_to_screen(decode::decode_full(&bytes)?);
    let out = dir.join("wallpaper.png");
    // Atomic write (temp + rename) so a failed/interrupted encode can never
    // leave the live, OS-referenced wallpaper file half-written (the desktop
    // re-reads this exact path at logon). Mirrors `convert_file`. A per-call
    // unique staging name (not a bare `<out>.st2ktmp`): `out` is always the SAME
    // fixed path, so two quick Set-as-wallpaper clicks would otherwise write
    // through separate handles to the identical temp file.
    let tmp = unique_tmp(&out);
    img.save_with_format(&tmp, ImageFormat::Png).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        Error::new(E_FAIL, format!("encode wallpaper PNG: {e}"))
    })?;
    // Retry past a transient Explorer/thumbnail-cache lock on `out` (Windows os error
    // 5/32) instead of failing outright — the same short backoff every other writer in
    // this codebase uses (see `fsutil::rename_retrying`).
    crate::fsutil::rename_retrying(&tmp, &out).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        Error::new(E_FAIL, format!("rename wallpaper into place: {e}"))
    })?;
    Ok(out)
}

/// Downscale `img` to fit within the virtual-screen bounds, **never upscaling**.
/// The desktop can't display more than screen resolution, and PNG-re-encoding a
/// full-size camera image on the shell thread is pure waste. Falls back to an 8K
/// cap if the metrics are unavailable (e.g. a headless/service context).
fn cap_to_screen(img: DynamicImage) -> DynamicImage {
    let (mut cap_w, mut cap_h) = unsafe {
        (
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    if cap_w <= 0 || cap_h <= 0 {
        cap_w = 7680;
        cap_h = 4320;
    }
    let (cap_w, cap_h) = (cap_w as u32, cap_h as u32);
    if img.width() > cap_w || img.height() > cap_h {
        // resize() preserves aspect and fits within the box.
        img.resize(cap_w, cap_h, image::imageops::FilterType::Lanczos3)
    } else {
        img
    }
}

/// Decode `path` (any supported format, incl. ones Windows can't read directly)
/// and write it as a PNG the desktop can use. Returns the wallpaper image path.
pub fn prepare_wallpaper(path: &str) -> Result<PathBuf> {
    prepare_wallpaper_in(&appdata_dir()?, path)
}

/// Set the selected image as the desktop wallpaper with the given placement.
pub fn set_wallpaper(path: &str, mode: WallpaperMode) -> Result<()> {
    let wp = prepare_wallpaper(path)?;

    // Placement: HKCU\Control Panel\Desktop {WallpaperStyle, TileWallpaper}.
    let (style, tile) = match mode {
        WallpaperMode::Stretch => ("2", "0"),
        WallpaperMode::Tile => ("0", "1"),
        WallpaperMode::Center => ("0", "0"),
    };
    if let Ok(k) = windows_registry::CURRENT_USER.create("Control Panel\\Desktop") {
        let _ = k.set_string("WallpaperStyle", style);
        let _ = k.set_string("TileWallpaper", tile);
    }

    // Apply it (and persist + broadcast the change).
    let wide: Vec<u16> = wp.as_os_str().encode_wide().chain(once(0)).collect();
    unsafe {
        SystemParametersInfoW(
            SPI_SETDESKWALLPAPER,
            0,
            Some(wide.as_ptr() as *mut c_void),
            SPIF_UPDATEINIFILE | SPIF_SENDCHANGE,
        )
        .map_err(|e| Error::new(E_FAIL, format!("SPI_SETDESKWALLPAPER failed: {e}")))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::fs::OpenOptionsExt;

    /// A transient Explorer/AV lock on the destination (Windows os error 5/32) must not
    /// fail the wallpaper write outright — `prepare_wallpaper_in`'s final rename has to
    /// retry past it (`fsutil::rename_retrying`), not fail on a bare `std::fs::rename`.
    #[test]
    fn prepare_wallpaper_in_survives_a_transient_lock_on_the_destination() {
        let dir = std::env::temp_dir().join(format!(
            "st2k_wallpaper_lock_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let src_path = dir.join("src.png");
        let img = image::RgbImage::from_pixel(1, 1, image::Rgb([200, 100, 50]));
        image::DynamicImage::ImageRgb8(img)
            .save_with_format(&src_path, ImageFormat::Png)
            .unwrap();

        // Pre-create the destination and hold it open with no sharing for a while, the
        // way a real Explorer/thumbnail-cache lock briefly does.
        let dest = dir.join("wallpaper.png");
        std::fs::write(&dest, b"placeholder").unwrap();
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&dest)
            .unwrap();
        // Release ONE backoff interval after the encode stages its temp file, because the
        // rename is the very next statement. A flat 140 ms against a ~200 ms retry budget is
        // a 1.4x margin, and the identical pattern in foldericon.rs lost that race during a
        // release and blocked it. Anchoring to the staged file makes it deterministic without
        // making it vacuous - shortening the hold instead would let the lock expire during
        // setup, so the rename would never meet a locked destination at all.
        //
        // The staged name can't be predicted exactly any more (`unique_tmp` stamps a
        // per-process counter shared with every other test that stages a write), so poll
        // the directory for the SHAPE instead of a fixed path: any file starting with
        // `wallpaper.png.` and ending `.st2ktmp`.
        let watch_dir = dir.clone();
        let lock_thread = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                let staged = std::fs::read_dir(&watch_dir).ok().is_some_and(|rd| {
                    rd.filter_map(|e| e.ok()).any(|e| {
                        e.file_name().to_str().is_some_and(|n| {
                            n.starts_with("wallpaper.png.") && n.ends_with(".st2ktmp")
                        })
                    })
                });
                if staged || std::time::Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            std::thread::sleep(crate::fsutil::RENAME_BACKOFF);
            drop(held);
        });

        let result = prepare_wallpaper_in(&dir, src_path.to_str().unwrap());
        lock_thread.join().unwrap();

        assert!(
            result.is_ok(),
            "rename must retry past the transient lock, not fail immediately: {result:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
