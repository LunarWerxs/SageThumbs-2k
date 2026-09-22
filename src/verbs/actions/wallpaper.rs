//! The Set-as-wallpaper verb: decode any supported format, cap it to the screen,
//! write it as a PNG under %APPDATA% and hand it to the desktop.

use super::*;

/// %APPDATA%\SageThumbs2K (created on demand) — where the wallpaper image lives.
/// `pub(super)` so [`super::helper::wallpaper_one`] can pass this SAME directory as
/// the routed `st2k wallpaper-prepare <file> <out-dir>` child's `<out-dir>` — the
/// routed and in-process arms have to agree on where the wallpaper PNG lands.
pub(super) fn appdata_dir() -> Result<PathBuf> {
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
    prepare_desktop_image_in(dir, path, "wallpaper.png")
}

/// [`prepare_wallpaper_in`] for Set as lock screen, into its OWN file. Both verbs used to write
/// `wallpaper.png`, so setting a lock screen replaced the bytes the DESKTOP wallpaper re-reads
/// at the next sign-in (2026-09-19 audit F21): two features, one persistent asset.
pub fn prepare_lock_screen_in(dir: &Path, path: &str) -> Result<PathBuf> {
    prepare_desktop_image_in(dir, path, "lockscreen.png")
}

fn prepare_desktop_image_in(dir: &Path, path: &str, file_name: &str) -> Result<PathBuf> {
    let bytes = read_full_fidelity_capped(path)?;
    // A wallpaper never needs more than screen resolution; downscale large
    // sources so we don't re-encode (and block the shell thread on) a giant PNG.
    let img = cap_to_screen(decode::decode_full_for_output(&bytes)?);
    let out = dir.join(file_name);
    // Atomic write (temp + rename) so a failed/interrupted encode can never
    // leave the live, OS-referenced wallpaper file half-written (the desktop
    // re-reads this exact path at logon). Mirrors `convert_file`. A per-call
    // unique staging name (not a bare `<out>.st2ktmp`): `out` is always the SAME
    // fixed path, so two quick Set-as-wallpaper clicks would otherwise write
    // through separate handles to the identical temp file.
    crate::verbs::write_atomic(&out, |tmp| {
        img.save_with_format(tmp, ImageFormat::Png)
            .map_err(|e| Error::new(E_FAIL, format!("encode wallpaper PNG: {e}")))
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

/// The lock-screen twin of [`prepare_wallpaper`]: same directory, its own file.
pub fn prepare_lock_screen(path: &str) -> Result<PathBuf> {
    prepare_lock_screen_in(&appdata_dir()?, path)
}

/// The `HKCU\Control Panel\Desktop` `WallpaperStyle`/`TileWallpaper` pair for a
/// placement mode. Pure (no registry access) so the mapping is unit-testable without
/// touching the live desktop settings - split out 2026-09-07 when Fill/Fit/Span were
/// added, so the enumeration test below can check every mode against the real Windows
/// values without mocking `windows_registry`.
fn style_and_tile(mode: WallpaperMode) -> (&'static str, &'static str) {
    match mode {
        WallpaperMode::Stretch => ("2", "0"),
        WallpaperMode::Tile => ("0", "1"),
        WallpaperMode::Center => ("0", "0"),
        WallpaperMode::Fill => ("10", "0"),
        WallpaperMode::Fit => ("6", "0"),
        WallpaperMode::Span => ("22", "0"),
    }
}

/// Apply an already-prepared wallpaper PNG (e.g. what [`prepare_wallpaper_in`] just
/// wrote) with the given placement: HKCU\Control Panel\Desktop
/// {WallpaperStyle,TileWallpaper} + `SystemParametersInfoW`. No decode - this is the
/// half [`super::helper::wallpaper_one`] calls after the routed `st2k
/// wallpaper-prepare` child (or the in-process [`prepare_wallpaper`] fallback) has
/// already produced `wp`, so this process never runs an image parser on this path.
pub(super) fn apply_wallpaper(wp: &Path, mode: WallpaperMode) -> Result<()> {
    // Placement: HKCU\Control Panel\Desktop {WallpaperStyle, TileWallpaper}.
    let (style, tile) = style_and_tile(mode);
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

/// Apply an already-prepared image (e.g. what [`prepare_wallpaper_in`] just wrote, since
/// Set-as-lock-screen shares the SAME decode/resize/encode half as Set-as-wallpaper) as the
/// Windows lock screen background, via `Windows.System.UserProfile.LockScreen`. No decode -
/// this is the half [`super::helper::lock_screen_one`] calls after the routed `st2k
/// wallpaper-prepare` child (or the in-process [`prepare_wallpaper`] fallback) has already
/// produced `path`, so this process never runs an image parser on this path. Verified on this
/// machine: `LockScreen` activates fine from an unpackaged desktop process.
pub(super) fn apply_lock_screen(path: &Path) -> Result<()> {
    use windows::core::HSTRING;
    use windows::Storage::StorageFile;
    use windows::System::UserProfile::LockScreen;

    let hpath = HSTRING::from(path.to_string_lossy().as_ref());
    let file: StorageFile = crate::pdf::block_op(&StorageFile::GetFileFromPathAsync(&hpath)?)?;
    crate::pdf::block_action(&LockScreen::SetImageFileAsync(&file)?)
}

/// Prepare and apply in one call, in-process. Production goes through the routed
/// `prepare_wallpaper_routed` + [`apply_wallpaper`] pair instead (the decode half runs in
/// the `st2k` helper); this composition remains for the end-to-end test only.
#[cfg(test)]
pub fn set_wallpaper(path: &str, mode: WallpaperMode) -> Result<()> {
    let wp = prepare_wallpaper(path)?;
    apply_wallpaper(&wp, mode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    /// Every `WallpaperMode` maps to the exact `WallpaperStyle`/`TileWallpaper` pair
    /// Windows itself uses for that placement — pins all six modes at once so a future
    /// addition can't land with the wrong registry values (or forget this test).
    #[test]
    fn style_and_tile_matches_every_mode() {
        assert_eq!(style_and_tile(WallpaperMode::Stretch), ("2", "0"));
        assert_eq!(style_and_tile(WallpaperMode::Tile), ("0", "1"));
        assert_eq!(style_and_tile(WallpaperMode::Center), ("0", "0"));
        assert_eq!(style_and_tile(WallpaperMode::Fill), ("10", "0"));
        assert_eq!(style_and_tile(WallpaperMode::Fit), ("6", "0"));
        assert_eq!(style_and_tile(WallpaperMode::Span), ("22", "0"));
    }

    /// 2026-09-19 audit F21: Set as lock screen used to rewrite `wallpaper.png`, the desktop
    /// wallpaper's persistent asset (the desktop re-reads that exact path at logon), so the
    /// lock screen replaced the wallpaper's backing file. Each has its own file now: after a
    /// lock-screen prepare the wallpaper bytes are identical, and the other order holds too.
    #[test]
    fn lock_screen_and_wallpaper_keep_separate_assets() {
        let dir = std::env::temp_dir().join(format!(
            "st2k_wallpaper_vs_lockscreen_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let png = |name: &str, rgb: [u8; 3]| {
            let p = dir.join(name);
            let img = image::RgbImage::from_pixel(2, 2, image::Rgb(rgb));
            image::DynamicImage::ImageRgb8(img)
                .save_with_format(&p, ImageFormat::Png)
                .unwrap();
            p.to_string_lossy().into_owned()
        };
        let red = png("red.png", [200, 0, 0]);
        let blue = png("blue.png", [0, 0, 200]);

        let wall = prepare_wallpaper_in(&dir, &red).unwrap();
        assert_eq!(wall, dir.join("wallpaper.png"));
        let wall_bytes = std::fs::read(&wall).unwrap();

        let lock = prepare_lock_screen_in(&dir, &blue).unwrap();
        assert_eq!(
            lock,
            dir.join("lockscreen.png"),
            "the lock screen has its own file"
        );
        assert_ne!(lock, wall);
        assert_eq!(
            std::fs::read(&wall).unwrap(),
            wall_bytes,
            "Set as lock screen must not touch the wallpaper's bytes"
        );
        let lock_bytes = std::fs::read(&lock).unwrap();
        assert_ne!(
            lock_bytes, wall_bytes,
            "two different pictures, two different files"
        );

        // And the other order: a new wallpaper leaves the lock screen alone.
        let wall2 = prepare_wallpaper_in(&dir, &blue).unwrap();
        assert_eq!(wall2, wall);
        assert_eq!(
            std::fs::read(&lock).unwrap(),
            lock_bytes,
            "the lock screen is untouched"
        );
        assert!(
            crate::fsutil::staging_leftovers(&wall).is_empty()
                && crate::fsutil::staging_leftovers(&lock).is_empty()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

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
        // The lock is released by the retry loop's own hook, so it is provably held for the
        // first attempt and provably gone before the second. The watcher thread this used to
        // spawn released after a hard-coded interval measured from when it HAPPENED TO NOTICE
        // the staged temp file, which under a loaded suite can land after the whole retry
        // budget is spent - see `fsutil::on_transient_failure`.
        let failures = crate::fsutil::lock_until_first_retry(&dest);

        let result = prepare_wallpaper_in(&dir, src_path.to_str().unwrap());
        crate::fsutil::clear_transient_failure_hook();

        assert!(
            result.is_ok(),
            "rename must retry past the transient lock, not fail immediately: {result:?}"
        );
        assert!(
            failures.load(Ordering::SeqCst) >= 1,
            "the rename never met the lock, so nothing here exercised retrying"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
