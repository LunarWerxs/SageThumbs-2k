//! Where a finished capture goes: the system clipboard (CF_DIB) and a file named from the
//! user's template, in the format Settings picks. Both take the already-composited top-down
//! BGRA pixels from `overlay.rs`, so this file knows nothing about annotations; the one
//! window it looks at is the foreground one, for the `{app}` token.

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{CloseHandle, HWND, SYSTEMTIME};
use windows::Win32::Graphics::Gdi::BITMAPINFOHEADER;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetForegroundWindow, GetWindowThreadProcessId,
};

use st2k_appkit::win::window_shot::to_rgba;
use st2k_base::settings::{self, ShotFormat};

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

/// The program a capture is named after (`{app}`): the foreground window's EXE name without
/// its extension, or `Desktop` when the desktop itself (or nothing) is in front or the owning
/// process cannot be read. Read before the overlay exists, so it is the user's window.
pub(super) unsafe fn foreground_app() -> String {
    let fg = GetForegroundWindow();
    if fg.is_invalid() {
        return app_token(None, None);
    }
    let mut cls = [0u16; 64];
    let n = GetClassNameW(fg, &mut cls).max(0) as usize;
    let class = String::from_utf16_lossy(&cls[..n.min(cls.len())]);
    app_token(Some(&class), process_image(fg).as_deref())
}

/// The full image path of the process that owns `hwnd`, when it can be read.
unsafe fn process_image(hwnd: HWND) -> Option<String> {
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == 0 {
        return None;
    }
    let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    let ok = QueryFullProcessImageNameW(
        process,
        PROCESS_NAME_WIN32,
        windows::core::PWSTR(buf.as_mut_ptr()),
        &mut len,
    )
    .is_ok();
    let _ = CloseHandle(process);
    ok.then(|| String::from_utf16_lossy(&buf[..(len as usize).min(buf.len())]))
}

/// `{app}` for a foreground window of class `class` owned by the EXE at `image`. The desktop
/// windows (`Progman`, `WorkerW`) and anything unreadable are `Desktop`.
fn app_token(class: Option<&str>, image: Option<&str>) -> String {
    const DESKTOP: &str = "Desktop";
    if matches!(class, None | Some("Progman" | "WorkerW")) {
        return DESKTOP.to_string();
    }
    image
        .and_then(|p| Path::new(p).file_stem())
        .and_then(|s| s.to_str())
        .filter(|s| !s.trim().is_empty())
        .map_or_else(|| DESKTOP.to_string(), str::to_string)
}

/// The local time a capture's file name is built from.
pub(super) fn now() -> SYSTEMTIME {
    unsafe { windows::Win32::System::SystemInformation::GetLocalTime() }
}

/// The default template's PNG name for this moment, e.g. `Screenshot 2026-06-18 09.41.07.png`:
/// what a failed upload's recovery copy is saved as (its bytes are always the PNG that was
/// being uploaded, so neither the user's template nor the save format applies).
pub(super) fn timestamped_name() -> String {
    expand_file_name(settings::DEFAULT_SHOT_FILE_NAME, &now(), "", "png")
        .to_string_lossy()
        .into_owned()
}

/// Expand a file-name template into a path RELATIVE to the save folder: the tokens filled in
/// from `st` and `app`, `/` and `\` turned into subfolders, every component cleaned so Windows
/// will create it, and `.ext` appended. A template that cleans down to nothing (blank, all
/// separators, all forbidden characters) falls back to the default one, so a capture always
/// gets a name.
pub(super) fn expand_file_name(template: &str, st: &SYSTEMTIME, app: &str, ext: &str) -> PathBuf {
    expand_with(template, st, app, ext).unwrap_or_else(|| {
        expand_with(settings::DEFAULT_SHOT_FILE_NAME, st, app, ext)
            .unwrap_or_else(|| PathBuf::from(format!("Screenshot.{ext}")))
    })
}

/// [`expand_file_name`] for one template, `None` when no file name is left after cleaning.
fn expand_with(template: &str, st: &SYSTEMTIME, app: &str, ext: &str) -> Option<PathBuf> {
    // `{app}` is a single name: a separator inside it must not open a folder.
    let app = app.replace(['/', '\\'], "_");
    let expanded = without_image_ext(template)
        .replace("{yyyy}", &format!("{:04}", st.wYear))
        .replace("{MM}", &format!("{:02}", st.wMonth))
        .replace("{dd}", &format!("{:02}", st.wDay))
        .replace("{HH}", &format!("{:02}", st.wHour))
        .replace("{mm}", &format!("{:02}", st.wMinute))
        .replace("{ss}", &format!("{:02}", st.wSecond))
        .replace("{ms}", &format!("{:03}", st.wMilliseconds))
        .replace("{app}", &app);
    let mut parts: Vec<String> = expanded
        .split(['/', '\\'])
        .map(clean_component)
        .filter(|c| !c.is_empty())
        .map(not_reserved)
        .collect();
    let file = parts.pop()?;
    let mut path: PathBuf = parts.into_iter().collect();
    path.push(format!("{file}.{ext}"));
    Some(path)
}

/// `template` without a trailing image extension the user typed: the extension follows the
/// save format, so `shot.png` saved as JPEG must not become `shot.png.jpg`.
fn without_image_ext(template: &str) -> &str {
    let t = template.trim_end();
    for ext in [".png", ".jpg", ".jpeg", ".webp"] {
        let Some(cut) = t.len().checked_sub(ext.len()) else {
            continue;
        };
        if cut > 0 && t.is_char_boundary(cut) && t[cut..].eq_ignore_ascii_case(ext) {
            return &t[..cut];
        }
    }
    t
}

/// One path component made safe to create: the characters Windows forbids in a name
/// (`< > : " | ? *` and control characters) removed, leading spaces and trailing dots and
/// spaces trimmed (Windows silently drops the trailing ones, so `a.` and `a` would collide;
/// `.` and `..` clean to nothing, so a template cannot climb out of the save folder), and the
/// length capped well inside the 255-character limit to leave room for ` (2)` and the
/// extension.
fn clean_component(c: &str) -> String {
    const FORBIDDEN: &[char] = &['<', '>', ':', '"', '|', '?', '*'];
    let kept: String = c
        .chars()
        .filter(|ch| !ch.is_control() && !FORBIDDEN.contains(ch))
        .take(120)
        .collect();
    kept.trim_start().trim_end_matches(['.', ' ']).to_string()
}

/// A component moved off the DOS device names (`CON`, `NUL`, `COM1`, ...), which Windows will
/// not create as a file or folder whatever extension follows.
fn not_reserved(c: String) -> String {
    const RESERVED: &[&str] = &["CON", "PRN", "AUX", "NUL"];
    let stem = c.split('.').next().unwrap_or_default().trim_end();
    let upper = stem.to_ascii_uppercase();
    let numbered = |prefix: &str| {
        upper.len() == 4 && upper.starts_with(prefix) && matches!(upper.as_bytes()[3], b'1'..=b'9')
    };
    if RESERVED.contains(&upper.as_str()) || numbered("COM") || numbered("LPT") {
        format!("_{c}")
    } else {
        c
    }
}

/// Where Save puts a capture inside `dir`, from the current template, format and `app`:
/// the folder (the template's subfolders under `dir`) and the bare file name to reserve in it.
fn capture_target(dir: &Path, app: &str, format: ShotFormat) -> (PathBuf, String) {
    let rel = expand_file_name(&settings::shot_file_name(), &now(), app, format.ext());
    let name = rel
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let folder = rel
        .parent()
        .map_or_else(|| dir.to_path_buf(), |p| dir.join(p));
    (folder, name)
}

/// The image crate's format for a capture save format.
fn image_format(format: ShotFormat) -> image::ImageFormat {
    match format {
        ShotFormat::Png => image::ImageFormat::Png,
        ShotFormat::Jpeg => image::ImageFormat::Jpeg,
        ShotFormat::WebP => image::ImageFormat::WebP,
    }
}

/// Encode a capture into `path` in `format` through the Convert verbs' own encoders.
fn encode_capture(img: &image::DynamicImage, format: ShotFormat, path: &Path) -> bool {
    let quality = settings::shot_save_quality().min(100) as u8;
    st2k_actions::verbs::encode_image_file(img, image_format(format), quality, path).is_ok()
}

/// Auto-save a capture into `dir` under the file-name template, in the Settings format.
/// Used by Ctrl+S / the Save button when "use a fixed save folder" is on, and by the
/// editor-less instant capture. Returns whether the file was written.
pub(super) fn save_capture_to_dir(
    dir: &Path,
    top_down_bgra: &[u8],
    w: i32,
    h: i32,
    app: &str,
) -> bool {
    let format = settings::shot_save_format();
    let (folder, name) = capture_target(dir, app, format);
    save_capture_as(&folder, &name, top_down_bgra, w, h, format).is_some()
}

/// Encode a capture as `format` into a fresh reservation of `name` in `folder` (created if
/// missing); the written path on success.
fn save_capture_as(
    folder: &Path,
    name: &str,
    top_down_bgra: &[u8],
    w: i32,
    h: i32,
    format: ShotFormat,
) -> Option<PathBuf> {
    let img = image::DynamicImage::ImageRgba8(to_rgba(top_down_bgra, w, h)?);
    std::fs::create_dir_all(folder).ok()?;
    fill_reserved(folder, name, |_, path| {
        if encode_capture(&img, format, path) {
            Ok(())
        } else {
            Err(std::io::Error::other("encode failed"))
        }
    })
}

/// Save a capture to the exact `path` the Save dialog returned, in `format`: encoded into a
/// staging file beside it and renamed over it, so a failure leaves whatever was there.
pub(super) fn save_capture_to_path(
    path: &Path,
    top_down_bgra: &[u8],
    w: i32,
    h: i32,
    format: ShotFormat,
) -> bool {
    let Some(img) = to_rgba(top_down_bgra, w, h) else {
        return false;
    };
    let img = image::DynamicImage::ImageRgba8(img);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    st2k_actions::verbs::write_atomic(path, |tmp| {
        if encode_capture(&img, format, tmp) {
            Ok(())
        } else {
            Err(windows::core::Error::from(
                windows::Win32::Foundation::E_FAIL,
            ))
        }
    })
    .is_ok()
}

/// What the Save dialog opens on: the folder the template's subfolders name inside `dir`
/// when it already exists (a dialog the user may cancel creates nothing), else `dir`; and the
/// template's file name.
pub(super) fn dialog_seed(dir: &str, app: &str, format: ShotFormat) -> (String, String) {
    let (folder, name) = capture_target(Path::new(dir), app, format);
    let folder = if folder.is_dir() {
        folder.to_string_lossy().into_owned()
    } else {
        dir.to_string()
    };
    (folder, name)
}

/// Reserve a fresh name in `dir` and write `bytes` into it; the path on success. A write that
/// fails removes the reservation again, so a failed capture never leaves an empty file where
/// the next capture's disambiguator would count it as a taken name.
pub(super) fn write_reserved(dir: &Path, name: &str, bytes: &[u8]) -> Option<PathBuf> {
    use std::io::Write;
    fill_reserved(dir, name, |file, _| {
        file.write_all(bytes)?;
        file.flush()
    })
}

/// Reserve a fresh name in `dir` (see [`reserve_unique_in`]) and let `fill` produce the file,
/// through the open reservation or by its path; a `fill` that fails removes the reservation.
fn fill_reserved(
    dir: &Path,
    name: &str,
    fill: impl FnOnce(&mut std::fs::File, &Path) -> std::io::Result<()>,
) -> Option<PathBuf> {
    let (path, mut file) = reserve_unique_in(dir, name).ok()?;
    if fill(&mut file, &path).is_err() {
        drop(file);
        let _ = std::fs::remove_file(&path);
        return None;
    }
    Some(path)
}

/// Reserve a filename in `dir` that nothing else owns, appending " (2)", " (3)", ... before the
/// extension when `name` is taken, and return it OPEN: the entry is created with `create_new`,
/// so the reservation and the check are one operation. The default file name only has 1-second
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

    /// Two captures landing on the same second must not collide — the default file name only has
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

    /// 2026-06-18 09:41:07.123, the moment every naming test below is taken at.
    fn june_18() -> SYSTEMTIME {
        SYSTEMTIME {
            wYear: 2026,
            wMonth: 6,
            wDayOfWeek: 4,
            wDay: 18,
            wHour: 9,
            wMinute: 41,
            wSecond: 7,
            wMilliseconds: 123,
        }
    }

    fn name(template: &str, app: &str, ext: &str) -> String {
        expand_file_name(template, &june_18(), app, ext)
            .to_string_lossy()
            .into_owned()
    }

    /// The default template must reproduce, byte for byte, the name every capture got before
    /// templates existed, so nobody's sorting or scripts break on upgrade.
    #[test]
    fn the_default_template_is_the_old_capture_name() {
        assert_eq!(
            name(settings::DEFAULT_SHOT_FILE_NAME, "notepad", "png"),
            "Screenshot 2026-06-18 09.41.07.png"
        );
    }

    /// Every token expands, zero-padded, and the extension follows the format: a typed image
    /// extension is replaced, never doubled.
    #[test]
    fn every_token_expands() {
        assert_eq!(
            name("{app} {yyyy}{MM}{dd}-{HH}{mm}{ss}.{ms}", "notepad", "png"),
            "notepad 20260618-094107.123.png"
        );
        assert_eq!(name("shot {app}", "Desktop", "webp"), "shot Desktop.webp");
        assert_eq!(name("shot.PNG", "x", "jpg"), "shot.jpg");
        // `{app}` names a file, never a folder.
        assert_eq!(name("{app}", "a/b\\c", "png"), "a_b_c.png");
    }

    /// Whatever the template says, the result is a name Windows will create, inside the save
    /// folder: forbidden and control characters go, trailing dots and spaces go, device names
    /// are moved aside, `..` cannot climb out, and an empty result falls back to the default.
    #[test]
    fn bad_characters_and_names_are_cleaned() {
        let default = "Screenshot 2026-06-18 09.41.07.png";
        for (template, want) in [
            ("a<b>c:d\"e|f?g*h", "abcdefgh.png"),
            ("tab\there\u{7}bell", "tabherebell.png"),
            ("  name. . ", "name.png"),
            ("CON", "_CON.png"),
            ("nul.txt", "_nul.txt.png"),
            ("com1", "_com1.png"),
            ("LPT9", "_LPT9.png"),
            ("COM0", "COM0.png"),
            ("CONSOLE", "CONSOLE.png"),
            ("../../evil", "evil.png"),
            ("", default),
            ("???", default),
            ("/ \\ /", default),
        ] {
            assert_eq!(name(template, "app", "png"), want, "template {template:?}");
        }
        let long = name(&"x".repeat(400), "app", "png");
        assert!(
            long.len() <= 124,
            "a component must stay well under 255: {}",
            long.len()
        );
    }

    /// `/` and `\` in the template become subfolders, and saving creates them.
    #[test]
    fn subfolders_in_the_template_are_created() {
        let dir = std::env::temp_dir().join(format!("st2k_shot_subdirs_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let rel = expand_file_name("{app}\\{yyyy}/shot {HH}", &june_18(), "notepad", "png");
        assert_eq!(
            rel,
            PathBuf::from("notepad").join("2026").join("shot 09.png")
        );
        let folder = dir.join(rel.parent().expect("subfolders"));
        let file = rel.file_name().and_then(|n| n.to_str()).expect("file name");
        let bgra = vec![200u8; 8 * 6 * 4];
        let saved = save_capture_as(&folder, file, &bgra, 8, 6, ShotFormat::Png).expect("saved");
        assert_eq!(saved, dir.join(&rel));
        assert!(saved.is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Each save format writes a file of that format, under its own extension, that decodes
    /// back at the capture's size. JPEG has no alpha and still comes out right.
    #[test]
    fn every_save_format_decodes_back_at_the_capture_size() {
        let dir = std::env::temp_dir().join(format!("st2k_shot_formats_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (w, h) = (37, 23);
        let bgra: Vec<u8> = (0..w * h)
            .flat_map(|i| [(i * 7) as u8, (i * 3) as u8, 90, 255])
            .collect();
        for format in ShotFormat::ALL {
            let file = format!("capture.{}", format.ext());
            let saved =
                save_capture_as(&dir, &file, &bgra, w, h, format).expect("the capture saved");
            assert_eq!(
                saved.extension().and_then(|e| e.to_str()),
                Some(format.ext())
            );
            let bytes = std::fs::read(&saved).expect("read back");
            assert_eq!(
                image::guess_format(&bytes).ok(),
                Some(image_format(format)),
                "{file} is not {}",
                format.name()
            );
            let img = image::load_from_memory(&bytes).expect("decodes");
            assert_eq!((img.width(), img.height()), (w as u32, h as u32), "{file}");
        }
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
