//! The Set-as-folder-icon verb: write a hidden square .ico plus the `desktop.ini`
//! that points at it, the way Explorer's own Customize > Change Icon does.

use super::*;

/// Set the selected image as the icon of the folder that contains it: write a
/// hidden square `.ico`, a `desktop.ini` pointing at it, mark the folder
/// customized, and ask the shell to refresh. Mirrors how Explorer's own
/// "Customize ▸ Change Icon" persists a folder icon.
pub(crate) fn set_folder_icon(image_path: &str) -> Result<()> {
    let src = Path::new(image_path);
    let dir = src
        .parent()
        .ok_or_else(|| Error::new(E_FAIL, "image has no parent folder"))?;

    let bytes = read_full_fidelity_capped(image_path)?;
    let icon = make_icon_square(&decode::decode_full(&bytes)?, 256);

    // Encode the ICO into memory, then write it atomically (a half-written icon
    // would make the folder show a broken glyph).
    let mut ico_bytes = Vec::new();
    icon.write_to(&mut std::io::Cursor::new(&mut ico_bytes), ImageFormat::Ico)
        .map_err(|e| Error::new(E_FAIL, format!("encode folder .ico: {e}")))?;
    let ico_name = "SageThumbsFolder.ico";
    let ico_path = dir.join(ico_name);
    // A per-call unique staging name (not a bare `<out>.st2ktmp`): two quick
    // Set-as-folder-icon clicks in the same folder would otherwise write through
    // separate handles to the SAME temp file, exactly like the race
    // `launch_with_list`'s counter already guards against for listfile names.
    let tmp = unique_tmp(&ico_path);
    std::fs::write(&tmp, &ico_bytes).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        Error::new(E_FAIL, format!("write {}: {e}", tmp.display()))
    })?;
    // A re-run against a folder that already has an icon renames onto a target that
    // `add_attrs` (below) left Hidden last time — clear it first, like desktop.ini
    // below, and put the ORIGINAL attributes back if the rename doesn't take.
    let ico_prior_attrs = clear_attrs(&ico_path, FILE_ATTRIBUTE_HIDDEN);
    // Retry past a transient Explorer/shell lock on the target (Windows os error 5/32)
    // instead of failing outright — see `fsutil::rename_retrying`.
    crate::fsutil::rename_retrying(&tmp, &ico_path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        if let Some(prior) = ico_prior_attrs {
            restore_attrs(&ico_path, prior);
        }
        Error::new(E_FAIL, format!("rename .ico into place: {e}"))
    })?;

    // desktop.ini references the icon by a RELATIVE name (so it survives a move).
    // `IconResource` is the modern key; `IconFile`/`IconIndex` keep older Explorer
    // happy. CRLF + a trailing newline, matching what Explorer writes.
    //
    // MERGE, never clobber: a folder can already carry a desktop.ini that Explorer or another
    // tool wrote — a `[LocalizedFileNames]` block, `[ViewState]`, an InfoTip, a ConfirmFileOp
    // flag. Blindly overwriting it silently destroyed all of that. And write it atomically, like
    // the .ico above: a half-written desktop.ini makes the folder lose its identity entirely.
    let ini_path = dir.join("desktop.ini");
    let existing = std::fs::read(&ini_path).ok();
    let (prior, utf16) = match existing.as_deref() {
        // UTF-16 LE with BOM — what Explorer writes for a localized folder name.
        Some(b) if b.starts_with(&[0xFF, 0xFE]) => (decode_utf16le(&b[2..]), true),
        Some(b) => (String::from_utf8(b.to_vec()).ok(), false),
        None => (Some(String::new()), false),
    };
    // An encoding we can't read is an encoding we can't safely rewrite. Better to fail the verb
    // than to replace content we couldn't even see.
    let prior = prior.ok_or_else(|| {
        Error::new(
            E_FAIL,
            "desktop.ini is in an encoding we can't read — leaving it untouched",
        )
    })?;
    let ini = merge_shell_class_info(&prior, ico_name);
    let bytes: Vec<u8> = if utf16 {
        let mut v = vec![0xFF, 0xFE];
        v.extend(ini.encode_utf16().flat_map(u16::to_le_bytes));
        v
    } else {
        ini.into_bytes()
    };
    let ini_tmp = unique_tmp(&ini_path);
    std::fs::write(&ini_tmp, &bytes).map_err(|e| {
        let _ = std::fs::remove_file(&ini_tmp);
        Error::new(E_FAIL, format!("write desktop.ini: {e}"))
    })?;
    // desktop.ini is normally Hidden+System, and a rename onto a hidden file fails on Windows
    // unless the destination's attributes allow it — clear them first, then re-apply below. If
    // the rename doesn't take, put the ORIGINAL attributes straight back rather than leaving a
    // bare, unhidden desktop.ini behind — `map_err` below is the only path out of this function
    // once they're cleared, so it's the only place that can still restore them.
    let ini_prior_attrs = clear_attrs(&ini_path, FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM);
    // Retry past a transient Explorer/shell lock on the target (Windows os error 5/32)
    // instead of failing outright — see `fsutil::rename_retrying`.
    crate::fsutil::rename_retrying(&ini_tmp, &ini_path).map_err(|e| {
        let _ = std::fs::remove_file(&ini_tmp);
        if let Some(prior) = ini_prior_attrs {
            restore_attrs(&ini_path, prior);
        }
        Error::new(E_FAIL, format!("rename desktop.ini into place: {e}"))
    })?;

    // Hide the helper files; mark the folder System+ReadOnly so Explorer actually
    // reads desktop.ini (the documented requirement to honor a custom icon).
    add_attrs(&ico_path, FILE_ATTRIBUTE_HIDDEN);
    add_attrs(&ini_path, FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM);
    add_attrs(dir, FILE_ATTRIBUTE_READONLY | FILE_ATTRIBUTE_SYSTEM);

    // Nudge the shell to repaint the folder with its new icon.
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(once(0)).collect();
    unsafe {
        SHChangeNotify(
            SHCNE_UPDATEDIR,
            SHCNF_PATHW,
            Some(wide.as_ptr() as *const c_void),
            None,
        );
    }
    Ok(())
}

/// Fit `img` inside a transparent `size`×`size` RGBA canvas, centered — so a
/// non-square image becomes a clean square icon (Explorer scales/letterboxes
/// otherwise). Never upscales the source beyond the canvas.
fn make_icon_square(img: &DynamicImage, size: u32) -> DynamicImage {
    let fit = img
        .resize(size, size, image::imageops::FilterType::Lanczos3)
        .to_rgba8();
    let mut canvas = image::RgbaImage::from_pixel(size, size, image::Rgba([0, 0, 0, 0]));
    let ox = ((size - fit.width()) / 2) as i64;
    let oy = ((size - fit.height()) / 2) as i64;
    image::imageops::overlay(&mut canvas, &fit, ox, oy);
    DynamicImage::ImageRgba8(canvas)
}

/// Prefix `path` into the long-path-safe `\\?\` form `Get`/`SetFileAttributesW` need
/// to see a file past the legacy `MAX_PATH` (260-char) limit — a folder a few levels
/// deep plus this module's fixed `SageThumbsFolder.ico`/`desktop.ini` names reaches
/// that more easily than it looks. `\\?\` for a drive-absolute path, `\\?\UNC\` for a
/// UNC share (`\\server\share\…` becomes `\\?\UNC\server\share\…`); anything else
/// (relative, or already prefixed) is returned unchanged — the shell always hands
/// this verb an absolute path, so only those two shapes come up in practice.
fn to_verbatim(path: &Path) -> PathBuf {
    let s = path.as_os_str().to_string_lossy();
    if s.starts_with(r"\\?\") {
        return path.to_path_buf();
    }
    if let Some(share) = s.strip_prefix(r"\\") {
        return PathBuf::from(format!(r"\\?\UNC\{share}"));
    }
    if s.as_bytes().get(1) == Some(&b':') {
        return PathBuf::from(format!(r"\\?\{s}"));
    }
    path.to_path_buf()
}

/// OR `add` into a path's existing file attributes (best-effort; a permission
/// failure just leaves the file as-is). Goes through [`to_verbatim`] so this still
/// works past the legacy path-length limit.
fn add_attrs(path: &Path, add: FILE_FLAGS_AND_ATTRIBUTES) {
    let verbatim = to_verbatim(path);
    let wide: Vec<u16> = verbatim.as_os_str().encode_wide().chain(once(0)).collect();
    unsafe {
        let cur = GetFileAttributesW(PCWSTR(wide.as_ptr()));
        // GetFileAttributesW returns INVALID_FILE_ATTRIBUTES (u32::MAX) on error;
        // start from zero in that case rather than OR-ing the sentinel in.
        let base = if cur == u32::MAX {
            FILE_FLAGS_AND_ATTRIBUTES(0)
        } else {
            FILE_FLAGS_AND_ATTRIBUTES(cur)
        };
        let _ = SetFileAttributesW(PCWSTR(wide.as_ptr()), base | add);
    }
}

/// Clear `drop` from `path`'s current attributes (best-effort; goes through
/// [`to_verbatim`]). Returns the attributes `path` carried BEFORE clearing, so a
/// caller can put them back exactly via [`restore_attrs`] if a later step (the
/// rename this clear exists for) fails — `None` only when `path` genuinely doesn't
/// exist yet.
///
/// That "genuinely" is checked, not assumed: `GetFileAttributesW` returning
/// `INVALID_FILE_ATTRIBUTES` used to be read as "the file is absent" unconditionally,
/// which is also what came back for a file that WAS there but sat past the legacy
/// `MAX_PATH` limit — this clear silently no-op'd, and the rename onto the still-hidden
/// target then failed with no explanation. `to_verbatim` fixes the path-length case
/// directly; the `GetLastError` check below confirms absence instead of inferring it,
/// and logs the rare case where the failure means something else (permissions, a
/// transient I/O error) instead of pretending the file simply isn't there.
fn clear_attrs(path: &Path, drop: FILE_FLAGS_AND_ATTRIBUTES) -> Option<FILE_FLAGS_AND_ATTRIBUTES> {
    let verbatim = to_verbatim(path);
    let wide: Vec<u16> = verbatim.as_os_str().encode_wide().chain(once(0)).collect();
    unsafe {
        let cur = GetFileAttributesW(PCWSTR(wide.as_ptr()));
        if cur == u32::MAX {
            if !matches!(GetLastError(), ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) {
                crate::safety::log(&format!(
                    "clear_attrs: GetFileAttributesW({}) failed for a reason other than \
                     \"doesn't exist\" — leaving it untouched",
                    path.display()
                ));
            }
            return None;
        }
        let prior = FILE_FLAGS_AND_ATTRIBUTES(cur);
        let _ = SetFileAttributesW(
            PCWSTR(wide.as_ptr()),
            FILE_FLAGS_AND_ATTRIBUTES(cur & !drop.0),
        );
        Some(prior)
    }
}

/// Put `path`'s attributes back to exactly `prior` (best-effort; goes through
/// [`to_verbatim`]) — undoes a [`clear_attrs`] when the step it was staged for
/// (a rename) didn't happen after all.
fn restore_attrs(path: &Path, prior: FILE_FLAGS_AND_ATTRIBUTES) {
    let verbatim = to_verbatim(path);
    let wide: Vec<u16> = verbatim.as_os_str().encode_wide().chain(once(0)).collect();
    unsafe {
        let _ = SetFileAttributesW(PCWSTR(wide.as_ptr()), prior);
    }
}

/// UTF-16 LE bytes (BOM already stripped) → `String`, or `None` if they aren't valid UTF-16.
fn decode_utf16le(b: &[u8]) -> Option<String> {
    if !b.len().is_multiple_of(2) {
        return None;
    }
    let units: Vec<u16> = b
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16(&units).ok()
}

/// Rewrite the icon keys inside `prior`'s `[.ShellClassInfo]` section, leaving every other line —
/// and every other section — byte-for-byte alone.
///
/// A folder's desktop.ini is shared state: Explorer puts `[LocalizedFileNames]` there for
/// localized folder names, `InfoTip`/`ConfirmFileOp` live in `[.ShellClassInfo]` beside the icon
/// keys, and other tools add their own sections. Replacing the whole file (what this used to do)
/// silently deleted all of that. Only `IconResource` / `IconFile` / `IconIndex` are ours to touch.
///
/// Output is CRLF, matching what Explorer writes. Section matching is case-insensitive because
/// INI section names are.
pub(super) fn merge_shell_class_info(prior: &str, ico_name: &str) -> String {
    let icon_keys = [
        format!("IconResource={ico_name},0"),
        format!("IconFile={ico_name}"),
        "IconIndex=0".to_string(),
    ];
    let is_ours = |l: &str| {
        let k = l
            .split('=')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        matches!(k.as_str(), "iconresource" | "iconfile" | "iconindex")
    };

    let mut out: Vec<String> = Vec::new();
    let mut in_section = false;
    let mut wrote = false;
    for line in prior.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_section = t.eq_ignore_ascii_case("[.ShellClassInfo]");
            out.push(line.to_string());
            if in_section {
                out.extend(icon_keys.iter().cloned());
                wrote = true;
            }
            continue;
        }
        // Drop the icon keys we're replacing; keep everything else (InfoTip, ConfirmFileOp,
        // a stray comment, a blank line) exactly where it was.
        if in_section && is_ours(t) {
            continue;
        }
        out.push(line.to_string());
    }
    if !wrote {
        // No [.ShellClassInfo] yet — append one. A leading blank line only if there was content.
        if out.iter().any(|l| !l.trim().is_empty()) {
            out.push(String::new());
        } else {
            out.clear();
        }
        out.push("[.ShellClassInfo]".to_string());
        out.extend(icon_keys.iter().cloned());
    }
    let mut s = out.join("\r\n");
    s.push_str("\r\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::fs::OpenOptionsExt;

    /// Build a tiny source PNG in `dir` for `set_folder_icon` to read.
    fn make_src(dir: &Path) -> PathBuf {
        let src_path = dir.join("src.png");
        let img = image::RgbImage::from_pixel(4, 4, image::Rgb([10, 20, 30]));
        image::DynamicImage::ImageRgb8(img)
            .save_with_format(&src_path, ImageFormat::Png)
            .unwrap();
        src_path
    }

    /// Wait (bounded) for a staging file matching `<target_name>.<pid>_<n>.st2ktmp` to
    /// appear in `dir` — the counter `unique_tmp` stamps into the name means the exact
    /// filename can't be predicted (a shared per-process counter, bumped by every test
    /// that stages a write), so the tests poll the directory for the SHAPE instead of a
    /// fixed path.
    fn wait_for_staged(dir: &Path, target_name: &str) -> Option<PathBuf> {
        let prefix = format!("{target_name}.");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Ok(rd) = std::fs::read_dir(dir) {
                for entry in rd.filter_map(|e| e.ok()) {
                    let path = entry.path();
                    let matches = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".st2ktmp"));
                    if matches {
                        return Some(path);
                    }
                }
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// A transient Explorer/shell lock on the destination `.ico` (Windows os error 5/32)
    /// must not fail the folder-icon write outright — the rename has to retry past it
    /// (`fsutil::rename_retrying`), not fail on a bare `std::fs::rename`.
    #[test]
    fn set_folder_icon_survives_a_transient_lock_on_the_ico() {
        let dir = std::env::temp_dir().join(format!(
            "st2k_foldericon_lock_ico_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let src_path = make_src(&dir);

        // Pre-create the .ico target and hold it open with no sharing, mimicking a
        // transient Explorer/thumbnail-cache lock on the rename destination.
        let ico_path = dir.join("SageThumbsFolder.ico");
        std::fs::write(&ico_path, b"placeholder").unwrap();
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&ico_path)
            .unwrap();
        // Release the lock ONE backoff interval after the code under test stages its temp
        // file, because the rename is the very next statement. Anchoring to the CALL instead
        // is wrong in both directions and this test has been wrong in both: a flat 140 ms
        // against a ~200 ms retry budget is a 1.4x margin that loses whenever a release build
        // runs beside the suite (it blocked a release), and simply shortening the hold made
        // the test VACUOUS - the lock expired during setup, the rename never met a locked
        // destination, and the test passed with retrying disabled entirely.
        let dir_for_thread = dir.clone();
        let lock_thread = std::thread::spawn(move || {
            wait_for_staged(&dir_for_thread, "SageThumbsFolder.ico");
            std::thread::sleep(crate::fsutil::RENAME_BACKOFF);
            drop(held);
        });

        let result = set_folder_icon(src_path.to_str().unwrap());
        lock_thread.join().unwrap();

        assert!(
            result.is_ok(),
            "rename must retry past the transient lock, not fail immediately: {result:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Same as above, for the `desktop.ini` rename (the second of the two writer sites
    /// this file owns).
    #[test]
    fn set_folder_icon_survives_a_transient_lock_on_desktop_ini() {
        let dir = std::env::temp_dir().join(format!(
            "st2k_foldericon_lock_ini_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let src_path = make_src(&dir);

        let ini_path = dir.join("desktop.ini");
        std::fs::write(&ini_path, b"placeholder").unwrap();
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&ini_path)
            .unwrap();
        // Release the lock ONE backoff interval after the code under test stages its temp
        // file, because the rename is the very next statement. Anchoring to the CALL instead
        // is wrong in both directions and this test has been wrong in both: a flat 140 ms
        // against a ~200 ms retry budget is a 1.4x margin that loses whenever a release build
        // runs beside the suite (it blocked a release), and simply shortening the hold made
        // the test VACUOUS - the lock expired during setup, the rename never met a locked
        // destination, and the test passed with retrying disabled entirely.
        let dir_for_thread = dir.clone();
        let lock_thread = std::thread::spawn(move || {
            wait_for_staged(&dir_for_thread, "desktop.ini");
            std::thread::sleep(crate::fsutil::RENAME_BACKOFF);
            drop(held);
        });

        let result = set_folder_icon(src_path.to_str().unwrap());
        lock_thread.join().unwrap();

        assert!(
            result.is_ok(),
            "rename must retry past the transient lock, not fail immediately: {result:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A280/P41: re-running Set-as-folder-icon on a folder that already has one must not
    /// leave the ico Hidden across the rename — `clear_attrs` has to run before the .ico
    /// rename too, mirroring the desktop.ini handling that already existed.
    #[test]
    fn set_folder_icon_survives_a_re_run_against_an_already_hidden_ico() {
        let dir = std::env::temp_dir().join(format!(
            "st2k_foldericon_rerun_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let src_path = make_src(&dir);

        // First run: creates the .ico and (per the normal add_attrs pass) marks it Hidden.
        let first = set_folder_icon(src_path.to_str().unwrap());
        assert!(first.is_ok(), "first run must succeed: {first:?}");
        let ico_path = dir.join("SageThumbsFolder.ico");
        let attrs_after_first = unsafe {
            let wide: Vec<u16> = to_verbatim(&ico_path)
                .as_os_str()
                .encode_wide()
                .chain(once(0))
                .collect();
            GetFileAttributesW(PCWSTR(wide.as_ptr()))
        };
        assert!(
            attrs_after_first & FILE_ATTRIBUTE_HIDDEN.0 != 0,
            "the .ico must be Hidden after a successful run"
        );

        // Second run: must succeed even though the .ico it's about to rename over
        // already carries Hidden from the first run.
        let second = set_folder_icon(src_path.to_str().unwrap());
        assert!(
            second.is_ok(),
            "re-running against an already-Hidden .ico must not fail: {second:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn desktop_ini_merge_preserves_everything_else() {
        // Empty / missing file → just our section.
        let fresh = merge_shell_class_info("", "SageThumbsFolder.ico");
        assert_eq!(
            fresh,
            "[.ShellClassInfo]\r\nIconResource=SageThumbsFolder.ico,0\r\n\
             IconFile=SageThumbsFolder.ico\r\nIconIndex=0\r\n"
        );

        // Existing unrelated section survives, and our keys get their own section appended.
        let loc = "[LocalizedFileNames]\r\nreport.docx=@shell32.dll,-1\r\n";
        let merged = merge_shell_class_info(loc, "SageThumbsFolder.ico");
        assert!(merged.contains("[LocalizedFileNames]"), "{merged}");
        assert!(merged.contains("report.docx=@shell32.dll,-1"), "{merged}");
        assert!(merged.contains("[.ShellClassInfo]"), "{merged}");

        // An existing [.ShellClassInfo] keeps its NON-icon keys; the icon keys are replaced,
        // not duplicated.
        let prior = "[.ShellClassInfo]\r\nInfoTip=My photos\r\nIconResource=old.ico,3\r\n\
                     IconFile=old.ico\r\nIconIndex=3\r\nConfirmFileOp=0\r\n";
        let merged = merge_shell_class_info(prior, "SageThumbsFolder.ico");
        assert!(merged.contains("InfoTip=My photos"), "{merged}");
        assert!(merged.contains("ConfirmFileOp=0"), "{merged}");
        assert!(!merged.contains("old.ico"), "{merged}");
        assert_eq!(merged.matches("IconResource=").count(), 1, "{merged}");
        assert_eq!(merged.matches("[.ShellClassInfo]").count(), 1, "{merged}");

        // Section names are case-insensitive in INI files.
        let odd = "[.shellclassinfo]\r\nIconFile=old.ico\r\n";
        let merged = merge_shell_class_info(odd, "new.ico");
        assert_eq!(merged.matches("[.").count(), 1, "{merged}");
        assert!(merged.contains("IconFile=new.ico"), "{merged}");
        assert!(!merged.contains("old.ico"), "{merged}");

        // Re-running is idempotent — no key or section pile-up.
        let once = merge_shell_class_info("", "a.ico");
        assert_eq!(merge_shell_class_info(&once, "a.ico"), once);
    }

    /// `\\?\` / `\\?\UNC\` prefixing must be idempotent and must not touch a relative
    /// path (there's nothing correct to prefix it with).
    #[test]
    fn to_verbatim_prefixes_drive_and_unc_paths_only() {
        assert_eq!(
            to_verbatim(Path::new(r"C:\Users\me\Pictures")),
            PathBuf::from(r"\\?\C:\Users\me\Pictures")
        );
        assert_eq!(
            to_verbatim(Path::new(r"\\server\share\Pictures")),
            PathBuf::from(r"\\?\UNC\server\share\Pictures")
        );
        // Already prefixed → unchanged (not double-prefixed).
        assert_eq!(
            to_verbatim(Path::new(r"\\?\C:\Users\me\Pictures")),
            PathBuf::from(r"\\?\C:\Users\me\Pictures")
        );
        // Relative → unchanged (nothing to prefix it with).
        assert_eq!(
            to_verbatim(Path::new(r"Pictures\sub")),
            PathBuf::from(r"Pictures\sub")
        );
    }
}
