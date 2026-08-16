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

    let bytes = read_capped(image_path)?;
    let icon = make_icon_square(&decode::decode_full(&bytes)?, 256);

    // Encode the ICO into memory, then write it atomically (a half-written icon
    // would make the folder show a broken glyph).
    let mut ico_bytes = Vec::new();
    icon.write_to(&mut std::io::Cursor::new(&mut ico_bytes), ImageFormat::Ico)
        .map_err(|e| Error::new(E_FAIL, format!("encode folder .ico: {e}")))?;
    let ico_name = "SageThumbsFolder.ico";
    let ico_path = dir.join(ico_name);
    let tmp = with_tmp_suffix(&ico_path);
    std::fs::write(&tmp, &ico_bytes).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        Error::new(E_FAIL, format!("write {}: {e}", tmp.display()))
    })?;
    // Retry past a transient Explorer/shell lock on the target (Windows os error 5/32)
    // instead of failing outright — see `fsutil::rename_retrying`.
    crate::fsutil::rename_retrying(&tmp, &ico_path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
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
    let ini_tmp = with_tmp_suffix(&ini_path);
    std::fs::write(&ini_tmp, &bytes).map_err(|e| {
        let _ = std::fs::remove_file(&ini_tmp);
        Error::new(E_FAIL, format!("write desktop.ini: {e}"))
    })?;
    // desktop.ini is normally Hidden+System, and a rename onto a hidden file fails on Windows
    // unless the destination's attributes allow it — clear them first, then re-apply below.
    clear_attrs(&ini_path, FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM);
    // Retry past a transient Explorer/shell lock on the target (Windows os error 5/32)
    // instead of failing outright — see `fsutil::rename_retrying`.
    crate::fsutil::rename_retrying(&ini_tmp, &ini_path).map_err(|e| {
        let _ = std::fs::remove_file(&ini_tmp);
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

/// OR `add` into a path's existing file attributes (best-effort; a permission
/// failure just leaves the file as-is).
fn add_attrs(path: &Path, add: FILE_FLAGS_AND_ATTRIBUTES) {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(once(0)).collect();
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

/// Clear `drop` from a path's existing file attributes (best-effort). Needed before renaming
/// onto an existing Hidden+System file, which Windows otherwise refuses.
fn clear_attrs(path: &Path, drop: FILE_FLAGS_AND_ATTRIBUTES) {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(once(0)).collect();
    unsafe {
        let cur = GetFileAttributesW(PCWSTR(wide.as_ptr()));
        if cur == u32::MAX {
            return; // doesn't exist (or unreadable) — nothing to clear
        }
        let _ = SetFileAttributesW(
            PCWSTR(wide.as_ptr()),
            FILE_FLAGS_AND_ATTRIBUTES(cur & !drop.0),
        );
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
/// silently deleted all of it. Only `IconResource` / `IconFile` / `IconIndex` are ours to touch.
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
        let lock_thread = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(140));
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
        let lock_thread = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(140));
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
}
