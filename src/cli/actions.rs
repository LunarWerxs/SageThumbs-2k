//! The single-purpose child verbs: clipboard pixels, wallpaper prep, folder icon,
//! metadata strip, OCR, and the upload-hosts config + upload front door.

use super::*;

/// Decode `input` and print `w h` as two little-endian u32 followed by top-down RGBA8
/// bytes — the wire format the routed Clipboard verb
/// (`verbs::actions::helper::clipboard_one`) reads back off this process's stdout, so
/// the parent never runs an image parser itself: the only work it does on the routed
/// path is a bounded memcpy. Binary on success, so it can't return through the normal
/// `Result<String, String>` → `println!` verb machinery — `main` calls this directly
/// and writes the bytes to stdout itself. Hidden from `st2k --help` alongside the
/// other stdout-is-a-wire-format child verbs is NOT the shape here: this one IS
/// listed, since it's a documented part of the Clipboard routing contract.
pub fn clip_pixels(input: &str) -> Result<Vec<u8>, String> {
    let bytes = verbs::read_full_fidelity_capped(input).map_err(|e| e.to_string())?;
    let img = decode::decode_full_for_output(&bytes)
        .map_err(|e| format!("decode {input}: {e}"))?
        .to_rgba8();
    let (w, h) = (img.width(), img.height());
    let mut out = Vec::with_capacity(8 + img.as_raw().len());
    out.extend_from_slice(&w.to_le_bytes());
    out.extend_from_slice(&h.to_le_bytes());
    out.extend_from_slice(img.as_raw());
    Ok(out)
}

/// Decode + resize-to-screen `input` into `out_dir`, printing the produced PNG's path.
/// Powers the routed Wallpaper verb: the parent supplies its own
/// `%APPDATA%\SageThumbs2K` as `out_dir` and applies the result (registry write +
/// `SystemParametersInfoW`) without decoding the source itself.
pub fn wallpaper_prepare(input: &str, out_dir: &str) -> Result<String, String> {
    verbs::prepare_wallpaper_in(Path::new(out_dir), input)
        .map(|p| p.display().to_string())
        .map_err(|e| format!("wallpaper-prepare failed: {input}: {e}"))
}

/// Set `input` as its containing folder's icon (writes the hidden `.ico` +
/// `desktop.ini`). Powers the routed SetFolderIcon verb: the whole verb runs in this
/// disposable child; the parent only collects the exit status.
pub fn folder_icon(input: &str) -> Result<String, String> {
    verbs::set_folder_icon(input).map_err(|e| format!("folder-icon failed: {input}: {e}"))?;
    Ok(format!("set folder icon from {input}"))
}

/// Strip EXIF/IPTC/XMP/C2PA metadata in place (JPEG/PNG/WebP, lossless).
pub fn strip_meta(input: &str) -> Result<String, String> {
    strip::strip_metadata(input)
        .map_err(|e| format!("strip failed (JPEG/PNG/WebP only): {input}: {e}"))?;
    Ok(format!("stripped {input}"))
}

/// OCR an image to plain text on stdout.
pub fn ocr(input: &str) -> Result<String, String> {
    // Same shared input cap as `thumbnail`. (The buffer is MOVED onto the OCR worker
    // thread, so it isn't held twice.)
    let bytes = decode::read_capped(input).map_err(|e| e.to_string())?;
    // Propagate the REAL error — "no text", "no language pack", and "decode failed" are
    // three different, actionable situations (especially for an MCP/AI caller parsing this).
    ocr::recognize_bytes(bytes).map_err(|e| {
        // "Too large for the recognizer" is a different, actionable answer from "no text /
        // no language pack" — an MCP or AI caller parsing this should be told to downscale,
        // not to go install something.
        if e.code() == ocr::OCR_IMAGE_TOO_LARGE {
            format!("OCR failed: {e} (the image is larger than the recognizer's maximum dimension)")
        } else {
            format!("OCR failed: {e} (no text found, or no OCR language pack installed)")
        }
    })
}

/// `st2k upload-hosts [--open]` — show (or open) the user-editable upload-hosts config
/// file. The right-click "Upload" verb and the screenshot Upload button read this file
/// to decide which keyless host(s) to POST to; editing it lets you reorder / add hosts
/// or point at your own server. The documented template is created on first use. Path +
/// template are shared with the app via [`crate::upload_config`].
pub fn upload_hosts(open: bool) -> Result<String, String> {
    let path = crate::upload_config::ensure_config()
        .ok_or_else(|| "couldn't resolve %APPDATA% for the upload-hosts config path".to_string())?;
    let p = path.display().to_string();
    if open {
        // Open in the default editor (same "ShellExecute open" the Settings button uses).
        unsafe {
            use windows::core::{w, PCWSTR};
            use windows::Win32::UI::Shell::ShellExecuteW;
            use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
            let file = crate::wide(&p);
            ShellExecuteW(
                None,
                w!("open"),
                PCWSTR(file.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            );
        }
        Ok(format!(
            "Opening upload-hosts config in your default editor:\n{p}"
        ))
    } else {
        Ok(format!(
            "Upload-hosts config file:\n{p}\n\n\
             Edit it to choose / reorder / add upload hosts \u{2014} one host per line:\n  \
             <https-url> | <field> | text|json | extra=value ...\n\
             While every line is commented out, SageThumbs 2K uses its built-in defaults.\n\
             Run `st2k upload-hosts --open` to open it in your editor."
        ))
    }
}

/// `st2k upload <file> [--copy]` — upload a file through the same keyless-host chain the
/// screenshot editor uses, print the resulting URL, and (with `copy`) also put it on the
/// clipboard. `file` is a user's own file and is never modified or deleted (unlike the
/// screenshot editor's throwaway `--upload` capture, which is).
///
/// The POST itself is WinINet code that lives in the windows-subsystem app EXE
/// (`bin/app/screenshot/upload.rs`) — a console tool can't call it directly without either
/// linking WinINet into the DLL's rlib or forking the host-fallback logic, neither of which
/// is worth it for one verb. So this spawns `SageThumbs2K.exe --upload-keep <listfile>
/// --url-to <urlfile>` — the exact path the right-click "Upload" verb already uses for a
/// user's own files — and waits for it. Same hosts, same fallback chain, same "no silent
/// partial state" the editor has: see `run_upload_keep`'s doc comment for the exact
/// success/failure contract this relies on (stderr + a non-zero exit on failure, the URL
/// written to `--url-to` on success, nothing on the clipboard either way).
pub fn upload(path: &str, copy: bool) -> Result<String, String> {
    if !Path::new(path).exists() {
        return Err(format!("{path}: no such file"));
    }
    let exe = std::env::current_exe().map_err(|e| format!("could not locate this exe: {e}"))?;
    let app_exe = exe
        .parent()
        .ok_or("this exe has no parent directory")?
        .join(crate::APP_EXE);
    if !app_exe.exists() {
        return Err(format!(
            "{} not found beside st2k.exe — `upload` needs both installed together.",
            app_exe.display()
        ));
    }

    let pid = std::process::id();
    let dir = std::env::temp_dir();
    let list_path = dir.join(format!("st2k-upload-{pid}.txt"));
    let url_path = dir.join(format!("st2k-upload-{pid}.url"));
    let _ = std::fs::remove_file(&url_path); // stale leftover from a killed previous run
    std::fs::write(&list_path, path).map_err(|e| format!("couldn't write a temp file: {e}"))?;

    let output = match std::process::Command::new(&app_exe)
        .args(upload_child_args(&list_path, &url_path))
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            let _ = std::fs::remove_file(&list_path);
            return Err(format!("couldn't run {}: {e}", app_exe.display()));
        }
    };

    if !output.status.success() {
        let _ = std::fs::remove_file(&url_path);
        let reason = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if reason.is_empty() {
            format!("upload failed ({})", output.status)
        } else {
            reason
        });
    }

    let contents = std::fs::read_to_string(&url_path)
        .map_err(|e| format!("upload reported success but its result file was unreadable: {e}"))?;
    let _ = std::fs::remove_file(&url_path);
    let url = parse_upload_result(&contents)
        .ok_or_else(|| "upload reported success but returned no URL".to_string())?;

    if copy {
        // SAFETY: a plain clipboard write from this process's own (console) thread, the
        // same call `run_upload_keep` itself would have made — just done here instead so a
        // `--copy`-less run touches the clipboard not at all, as the CLI contract promises.
        unsafe {
            crate::clipboard::set_clipboard(
                crate::clipboard::CF_UNICODETEXT,
                &crate::clipboard::utf16_nul_bytes(&url),
            );
        }
    }
    Ok(url)
}

/// The argv `upload` passes to `SageThumbs2K.exe` — split out so the exact flags/order are
/// unit-testable without spawning a process.
fn upload_child_args(list_path: &Path, url_path: &Path) -> Vec<std::ffi::OsString> {
    vec![
        "--upload-keep".into(),
        list_path.as_os_str().to_owned(),
        "--url-to".into(),
        url_path.as_os_str().to_owned(),
    ]
}

/// The first non-empty line of `run_upload_keep`'s LF-joined result file. `upload` only ever
/// asks it to upload one file, so exactly one URL is expected — reading "the first line"
/// rather than "the whole file trimmed" just keeps this tolerant of the same format a future
/// multi-file `st2k upload` could reuse without changing the app-side contract.
fn parse_upload_result(contents: &str) -> Option<String> {
    contents
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `upload` refuses a missing file up front, before it ever spawns the app EXE — so a
    /// typo'd path fails fast with a plain message instead of a spawn error.
    #[test]
    fn upload_rejects_a_missing_file_without_spawning_anything() {
        let err = upload("this_file_does_not_exist_at_all.png", false).unwrap_err();
        assert!(err.contains("no such file"), "{err}");
    }

    /// The exact argv `upload` hands `SageThumbs2K.exe` — pinned so the flag names/order
    /// (which `run_upload_keep` on the app side parses positionally) can't drift silently.
    #[test]
    fn upload_child_args_are_upload_keep_then_url_to() {
        let list = Path::new(r"C:\temp\st2k-upload-123.txt");
        let url = Path::new(r"C:\temp\st2k-upload-123.url");
        let args: Vec<String> = upload_child_args(list, url)
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec![
                "--upload-keep",
                r"C:\temp\st2k-upload-123.txt",
                "--url-to",
                r"C:\temp\st2k-upload-123.url",
            ]
        );
    }

    /// The success path: a lone URL with no trailing newline.
    #[test]
    fn parse_upload_result_reads_a_single_url() {
        assert_eq!(
            parse_upload_result("https://x0.at/abc.png"),
            Some("https://x0.at/abc.png".to_string())
        );
    }

    /// `run_upload_keep` LF-joins and this is the one-file case, but the parser tolerates a
    /// trailing newline / blank lines rather than assuming an exact single-line file.
    #[test]
    fn parse_upload_result_trims_and_skips_blank_lines() {
        assert_eq!(
            parse_upload_result("\nhttps://x0.at/abc.png  \n\n"),
            Some("https://x0.at/abc.png".to_string())
        );
    }

    /// An empty result file (should never happen given the app-side contract, but a parser
    /// that panics on it would turn "success but no URL" into a crash instead of an error).
    #[test]
    fn parse_upload_result_of_empty_file_is_none() {
        assert_eq!(parse_upload_result(""), None);
        assert_eq!(parse_upload_result("\n\n"), None);
    }
}
