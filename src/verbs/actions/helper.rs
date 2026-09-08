//! Out-of-process dispatch of the decode/encode-heavy verbs to the sibling
//! `st2k.exe` helper, for crash isolation.
//!
//! Each `*_one` here runs ONE file through the helper when it is installed and
//! falls back to the identical in-process call when it is not; the produced file
//! is byte-identical either way. See the parent module's docs for the full
//! rationale, the routed/not-routed verb list, and the output-identity argument.

use super::*;

/// The `st2k.exe` CLI helper that ships next to our DLL, if it's actually there.
///
/// The installer drops `st2k.exe` in the same directory as `sagethumbs2k.dll`, so
/// we resolve it from the DLL's OWN path ([`crate::module_path`]) — **never**
/// `current_exe()`, which in the shell host is `explorer.exe`/`dllhost.exe`. Returns
/// `Some` only when the file exists; `None` (helper missing — tests, or a DLL-only
/// install) makes every routed verb fall back to its in-process path. See the
/// module docs for the rationale.
pub(super) fn st2k_exe() -> Option<PathBuf> {
    crate::sibling_of_dll(crate::CLI_EXE)
}

/// Outcome of a routed `st2k` helper run. The three cases are deliberately
/// distinct: a clean exit, a per-file failure (the child ran but reported an error
/// or crashed/aborted on this one file), and a SPAWN failure (the helper couldn't
/// even start — missing/corrupt/arch-mismatched exe). They must not be conflated:
/// a spawn failure breaks EVERY routed verb identically, so the caller degrades to
/// its in-process path instead of failing all files silently.
enum RunOutcome {
    /// Exited 0 — the file was produced exactly as the in-process call would have.
    Ok,
    /// The child ran but failed (non-zero exit / crash / abort) on this file.
    Failed,
    /// The child could not be spawned at all — the helper itself is broken.
    SpawnFailed,
}

/// Run the `st2k` CLI helper synchronously with the given args, no console window.
/// stdin is unused; stdout is dropped; stderr is captured so a non-zero exit can be
/// logged with the REAL decoder/IO error instead of a name-only "failed for {path}"
/// — `path` is the file this call is acting on, for that log line (not passed to the
/// child; it's already one of `args`). A spawn error is logged once here (it's a
/// routing-level problem, not a per-file one) and surfaced as
/// [`RunOutcome::SpawnFailed`] so the caller can fall back to in-process.
fn run_st2k(exe: &Path, path: &str, args: &[&str]) -> RunOutcome {
    match Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(out) if out.status.success() => RunOutcome::Ok,
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            crate::safety::log_error(&format!("st2k helper failed for {path}: {}", stderr.trim()));
            RunOutcome::Failed
        }
        Err(e) => {
            crate::safety::log_error(&format!(
                "st2k helper FAILED TO SPAWN ({e}) — routing this verb in-process instead"
            ));
            RunOutcome::SpawnFailed
        }
    }
}

/// Outcome of a routed `st2k` run whose stdout IS the answer (the produced file's
/// real path), not just success/failure.
enum CaptureOutcome {
    /// Exited 0 with a non-empty stdout line — the real path `st2k` wrote to.
    Ok(PathBuf),
    /// The child ran but failed, or printed nothing usable on a "successful" exit.
    Failed,
    /// The child could not be spawned at all.
    SpawnFailed,
}

/// Like [`run_st2k`], but reads the child's stdout back instead of discarding it —
/// for verbs (like `rotate`) whose one line of stdout on success IS the real output
/// path `main.rs`'s `println!("{out}")` prints, so the caller can read back what
/// `st2k` actually produced instead of predicting the name it will pick. `path` is
/// the file this call is acting on, for the failure log line only (see [`run_st2k`]).
fn run_st2k_capture(exe: &Path, path: &str, args: &[&str]) -> CaptureOutcome {
    match Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(out) if out.status.success() => {
            // `println!` adds the trailing newline; trim it (and any stray CR) off.
            let stdout_path = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if stdout_path.is_empty() {
                CaptureOutcome::Failed
            } else {
                CaptureOutcome::Ok(PathBuf::from(stdout_path))
            }
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            crate::safety::log_error(&format!("st2k helper failed for {path}: {}", stderr.trim()));
            CaptureOutcome::Failed
        }
        Err(e) => {
            crate::safety::log_error(&format!(
                "st2k helper FAILED TO SPAWN ({e}) — routing this verb in-process instead"
            ));
            CaptureOutcome::SpawnFailed
        }
    }
}

/// Render a [`Resize`] preset into the `--resize WxH|N%` syntax the CLI accepts
/// (the inverse of `cli::parse_resize`), or `None` when there's nothing to pass.
/// `Fit`/`FitUp` both serialize to `WxH`; the CLI's `convert` always fits-without-
/// upscale, which matches the resize/email presets (they never upscale either).
fn resize_arg(r: Resize) -> Option<String> {
    match r {
        Resize::None => None,
        Resize::Fit(w, h) | Resize::FitUp(w, h) => Some(format!("{w}x{h}")),
        Resize::Percent(p) => Some(format!("{p}%")),
        // Pad has no CLI spelling, so a padded convert stays in-process rather
        // than being routed to the helper with the padding silently dropped.
        Resize::Pad(..) => None,
    }
}

/// Convert one file. Routes to `st2k convert <in> <out> --quality Q
/// [--webp-quality Q]` when the helper is present (computing the SAME `<out>`
/// `convert_file` would pick, and passing `target.webp_quality` so a lossy-WebP
/// verb stays lossy out-of-process), else falls back to the in-process
/// `convert_file`. Returns whether the file was
/// produced — drop-in for the `convert_file(p, target).is_ok()` predicate. Logs
/// failures (with the error detail on the in-process path) like the originals did.
pub(super) fn convert_one(exe: Option<&Path>, p: &str, target: Target) -> Option<PathBuf> {
    match exe {
        Some(exe) => {
            // Reserve the SAME collision-free destination `convert_file` would pick
            // (atomic `create_new` placeholder, so parallel workers — across the
            // st2k processes too — never claim one name twice), then have the routed
            // CLI write exactly there. The slot is held across the run: on success
            // it keeps the (now non-empty) file, on failure its drop removes the
            // still-empty placeholder. (On the rare spawn-failure fallback the slot
            // still exists, so the in-process retry picks `(1)` — a cosmetic edge in
            // an almost-never path.)
            let slot = crate::verbs::encode::unique_output(Path::new(p), target.ext);
            let Some(out_s) = slot.path().to_str() else {
                return convert_one(None, p, target);
            };
            let q = crate::settings::jpeg_quality().to_string();
            let mut args = vec!["convert", p, out_s, "--quality", q.as_str()];
            // Lossy WebP (the quick WebP verb): pass the same quality the in-process
            // `convert_file` would use via `target.webp_quality`, so the routed file
            // matches. `wq` outlives `args` (borrowed as &str below).
            let wq;
            if let Some(w) = target.webp_quality {
                wq = w.to_string();
                args.push("--webp-quality");
                args.push(wq.as_str());
            }
            match run_st2k(exe, p, &args) {
                RunOutcome::Ok => Some(slot.path().to_path_buf()),
                RunOutcome::Failed => {
                    crate::safety::log(&format!("Convert (st2k) failed for {p}"));
                    None
                }
                RunOutcome::SpawnFailed => convert_one(None, p, target),
            }
        }
        None => match crate::verbs::encode::convert_file(p, target) {
            Ok(out) => Some(out),
            Err(e) => {
                crate::safety::log(&format!("Convert failed for {p}: {e:?}"));
                None
            }
        },
    }
}

/// Rotate/flip one file. Routes to `st2k rotate <in> --by …` (which auto-names the
/// `<stem> (edited).<ext>` sibling itself, via the same `transform_file`), else
/// falls back to in-process `transform_file`.
pub(super) fn transform_one(exe: Option<&Path>, p: &str, t: Transform) -> Option<PathBuf> {
    match exe {
        Some(exe) => {
            let by = match t {
                Transform::Right90 => "right",
                Transform::Left90 => "left",
                Transform::Rotate180 => "180",
                Transform::FlipH => "fliph",
                Transform::FlipV => "flipv",
            };
            // `st2k rotate` auto-names the `<stem> (edited).<ext>` sibling itself
            // (same `transform_file`) and PRINTS that real path on success
            // (`src/bin/cli.rs`'s `println!("{out}")`). Read that back instead of
            // guessing the name ourselves — a guess made BEFORE the subprocess runs
            // can be stolen by a concurrent rotate landing in the gap between our
            // prediction and st2k's own atomic reserve, which would report a name
            // st2k didn't actually produce.
            match run_st2k_capture(exe, p, &["rotate", p, "--by", by]) {
                CaptureOutcome::Ok(path) => {
                    // Cheap self-check against the OLD predict-then-hope approach: a
                    // mismatch here IS the exact race A277 was about (a concurrent
                    // rotate stealing the guessed name) — worth a log line, but `path`
                    // (read back from st2k's own stdout, not guessed) is always the
                    // ground truth now, so it's used regardless.
                    let src = Path::new(p);
                    let ext = routed_edit_output_ext(src);
                    let predicted = predict_unique_suffix(src, "edited", &ext);
                    if predicted != path {
                        crate::safety::log(&format!(
                            "Transform (st2k): predicted output {predicted:?} differs from \
                             the actual {path:?} for {p} — a concurrent edit likely won the \
                             naming race; using the actual path"
                        ));
                    }
                    Some(path)
                }
                CaptureOutcome::Failed => {
                    crate::safety::log(&format!("Transform (st2k) failed for {p}"));
                    None
                }
                CaptureOutcome::SpawnFailed => transform_one(None, p, t),
            }
        }
        None => transform_file(p, t).ok(),
    }
}

/// Resize one file. Routes to `st2k convert <in> <out> --resize …`, computing the
/// SAME `<stem> (resized).<ext>` sibling (and source format) that `resize_file`
/// writes, else falls back to in-process `resize_file`.
pub(super) fn resize_one(exe: Option<&Path>, p: &str, r: Resize) -> Option<PathBuf> {
    match exe {
        Some(exe) => {
            let src = Path::new(p);
            let ext = routed_edit_output_ext(src);
            let slot = reserve_unique_suffix(src, "resized", &ext);
            let (Some(out_s), Some(rs)) = (slot.path().to_str(), resize_arg(r)) else {
                return resize_one(None, p, r);
            };
            let q = crate::settings::jpeg_quality().to_string();
            match run_st2k(
                exe,
                p,
                &["convert", p, out_s, "--quality", &q, "--resize", &rs],
            ) {
                RunOutcome::Ok => Some(slot.path().to_path_buf()),
                RunOutcome::Failed => {
                    crate::safety::log(&format!("Resize (st2k) failed for {p}"));
                    None
                }
                RunOutcome::SpawnFailed => resize_one(None, p, r),
            }
        }
        None => match resize_file(p, r) {
            Ok(out) => Some(out),
            Err(e) => {
                crate::safety::log(&format!("Resize failed for {p}: {e:?}"));
                None
            }
        },
    }
}

pub(super) fn routed_edit_output_ext(src: &Path) -> String {
    let source_ext = src
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("png")
        .to_ascii_lowercase();
    edit_output_ext(&source_ext).to_string()
}

/// Shrink one file for email. Routes to `st2k convert <in> <out> --resize ExE`
/// onto the SAME `<stem> (email).jpg` sibling at the email JPEG quality, else falls
/// back to in-process `shrink_for_email`. (The CLI `convert` flattens onto white
/// for JPEG just like the in-process path, so the bytes match.)
pub(super) fn shrink_one(exe: Option<&Path>, p: &str, size: EmailSize) -> Option<PathBuf> {
    match exe {
        Some(exe) => {
            let src = Path::new(p);
            let slot = reserve_unique_suffix(src, "email", "jpg");
            let Some(out_s) = slot.path().to_str() else {
                return shrink_one(None, p, size);
            };
            let edge = size.max_edge();
            let resize = format!("{edge}x{edge}");
            // Same constant the in-process path uses (encode::EMAIL_JPEG_QUALITY is
            // pub(crate) exactly so this can't silently desync from it).
            let quality = crate::verbs::encode::EMAIL_JPEG_QUALITY.to_string();
            match run_st2k(
                exe,
                p,
                &[
                    "convert",
                    p,
                    out_s,
                    "--quality",
                    &quality,
                    "--resize",
                    &resize,
                ],
            ) {
                RunOutcome::Ok => Some(slot.path().to_path_buf()),
                RunOutcome::Failed => {
                    crate::safety::log(&format!("Shrink for email (st2k) failed for {p}"));
                    None
                }
                RunOutcome::SpawnFailed => shrink_one(None, p, size),
            }
        }
        None => match shrink_for_email(p, size) {
            Ok(out) => Some(out),
            Err(e) => {
                crate::safety::log(&format!("Shrink for email failed for {p}: {e:?}"));
                None
            }
        },
    }
}

/// Strip metadata from one file in place. Routes to `st2k strip <in>` (same
/// `strip::strip_metadata`), else falls back to the in-process call.
pub(super) fn strip_one(exe: Option<&Path>, p: &str) -> bool {
    match exe {
        Some(exe) => match run_st2k(exe, p, &["strip", p]) {
            RunOutcome::Ok => true,
            RunOutcome::Failed => {
                crate::safety::log(&format!("Strip metadata (st2k) failed for {p}"));
                false
            }
            RunOutcome::SpawnFailed => strip_one(None, p),
        },
        None => match crate::strip::strip_metadata(p) {
            Ok(()) => true,
            Err(e) => {
                crate::safety::log(&format!("Strip metadata failed for {p}: {e:?}"));
                false
            }
        },
    }
}

/// Compress one file to a target size. Routes to `st2k compress <in> --max-size
/// <bytes>` (same `compress_to_size` engine, same batch pool), reading the produced
/// path back off stdout like [`transform_one`]; a non-zero exit's stderr carries the
/// same "the smallest JPEG this can make is N bytes" text `compress_to_size` returns
/// in-process, parsed the same way `compress_one_to_size` already is. Falls back to
/// in-process `compress_one_to_size` otherwise.
pub(super) fn compress_one(
    exe: Option<&Path>,
    p: &str,
    target: u64,
) -> std::result::Result<PathBuf, u64> {
    match exe {
        Some(exe) => {
            let target_s = target.to_string();
            match run_st2k_capture_text(exe, p, &["compress", p, "--max-size", &target_s]) {
                TextOutcome::Ok(path) => Ok(path),
                TextOutcome::Failed(stderr) => {
                    crate::safety::log(&format!("Compress (st2k) failed for {p}"));
                    Err(parse_smallest_achievable(&stderr).unwrap_or(target))
                }
                TextOutcome::SpawnFailed => compress_one(None, p, target),
            }
        }
        None => compress_one_to_size(p, target),
    }
}

/// Put the first image's pixels on the clipboard. Routes to `st2k clip-pixels <file>`
/// (decode entirely inside the disposable child; this process runs no image parser on
/// the routed path, only a bounded memcpy after [`parse_clip_pixels`] validates the
/// stream), else falls back to in-process `copy_to_clipboard`.
pub(super) fn clipboard_one(exe: Option<&Path>, p: &str) -> Result<()> {
    match exe {
        Some(exe) => match run_st2k_capture_bytes(exe, p, &["clip-pixels", p]) {
            BytesOutcome::Ok(stdout) => match parse_clip_pixels(&stdout) {
                Some((w, h, rgba)) => copy_rgba_to_clipboard(w as i32, h as i32, rgba),
                None => {
                    crate::safety::log(&format!(
                        "Copy to clipboard (st2k) produced an unreadable pixel stream for {p}"
                    ));
                    Err(Error::new(
                        E_FAIL,
                        "clipboard helper produced an invalid pixel stream",
                    ))
                }
            },
            BytesOutcome::Failed => {
                crate::safety::log(&format!("Copy to clipboard (st2k) failed for {p}"));
                Err(Error::new(E_FAIL, "couldn't decode or copy the image"))
            }
            BytesOutcome::SpawnFailed => clipboard_one(None, p),
        },
        None => copy_to_clipboard(p),
    }
}

/// Parse and validate the wire format `st2k clip-pixels` writes to stdout: `w h` as
/// two little-endian u32 followed by exactly `w * h * 4` bytes of top-down RGBA8.
/// Rejects a stream shorter than the 8-byte header, a `w`/`h` of zero, a `w`/`h` over
/// [`decode::limits::MAX_DIM`] (the same bomb guard every decode path in this crate
/// enforces, applied here to a stream that never touches a real image parser), a
/// pixel count over [`decode::limits::MAX_ALLOC`], and a payload whose length doesn't
/// match `w * h * 4` exactly. Pure (no I/O), so it's testable without spawning
/// anything - a hostile/buggy child can't hand the parent anything it will trust.
pub(super) fn parse_clip_pixels(stdout: &[u8]) -> Option<(u32, u32, &[u8])> {
    if stdout.len() < 8 {
        return None;
    }
    let w = u32::from_le_bytes(stdout[0..4].try_into().ok()?);
    let h = u32::from_le_bytes(stdout[4..8].try_into().ok()?);
    if w == 0 || h == 0 || w > decode::limits::MAX_DIM || h > decode::limits::MAX_DIM {
        return None;
    }
    let want = u64::from(w) * u64::from(h) * 4;
    if want > decode::limits::MAX_ALLOC {
        return None;
    }
    let rgba = &stdout[8..];
    if rgba.len() as u64 != want {
        return None;
    }
    Some((w, h, rgba))
}

/// Prepare the wallpaper PNG for one file - the decode/resize/encode half of
/// Set-as-wallpaper. Routes to `st2k wallpaper-prepare <file> <out-dir>`, passing the
/// SAME `%APPDATA%\SageThumbs2K` directory the in-process [`prepare_wallpaper`] uses
/// (via [`wallpaper::appdata_dir`]) so the routed and in-process arms agree on where
/// the file lands, else falls back to in-process `prepare_wallpaper`. Split out from
/// [`wallpaper_one`] so this - the only decode-heavy half - is testable without
/// touching the live desktop; applying the result is a separate, decode-free step.
pub(super) fn prepare_wallpaper_routed(exe: Option<&Path>, p: &str) -> Result<PathBuf> {
    match exe {
        Some(exe) => {
            let dir = wallpaper::appdata_dir()?;
            let Some(dir_s) = dir.to_str() else {
                return prepare_wallpaper_routed(None, p);
            };
            match run_st2k_capture(exe, p, &["wallpaper-prepare", p, dir_s]) {
                CaptureOutcome::Ok(wp) => Ok(wp),
                CaptureOutcome::Failed => {
                    crate::safety::log(&format!("Set wallpaper (st2k) failed for {p}"));
                    Err(Error::new(E_FAIL, "couldn't set the wallpaper"))
                }
                CaptureOutcome::SpawnFailed => prepare_wallpaper_routed(None, p),
            }
        }
        None => prepare_wallpaper(p),
    }
}

/// `VerbAction::Wallpaper` - decode/resize/encode routed via
/// [`prepare_wallpaper_routed`], then applied in-process
/// (`wallpaper::apply_wallpaper` - registry write + `SystemParametersInfoW`, no
/// decode either way).
pub(super) fn wallpaper_one(exe: Option<&Path>, p: &str, mode: WallpaperMode) -> Result<()> {
    let wp = prepare_wallpaper_routed(exe, p)?;
    wallpaper::apply_wallpaper(&wp, mode)
}

/// `VerbAction::LockScreen` - decode/resize/encode routed via the SAME
/// [`prepare_wallpaper_routed`] (shares `prepare_wallpaper`/`wallpaper-prepare`'s output with
/// Set-as-wallpaper — no separate prepare path), then applied in-process
/// (`wallpaper::apply_lock_screen` - WinRT `LockScreen::SetImageFileAsync`, no decode either
/// way).
pub(super) fn lock_screen_one(exe: Option<&Path>, p: &str) -> Result<()> {
    let wp = prepare_wallpaper_routed(exe, p)?;
    wallpaper::apply_lock_screen(&wp)
}

/// Set the selected image as its folder's icon. Routes to `st2k folder-icon <file>`,
/// which runs the WHOLE verb (writes the .ico + desktop.ini) in the disposable child;
/// the parent only collects the exit status. Falls back to in-process
/// `set_folder_icon`.
pub(super) fn folder_icon_one(exe: Option<&Path>, p: &str) -> Result<()> {
    match exe {
        Some(exe) => match run_st2k(exe, p, &["folder-icon", p]) {
            RunOutcome::Ok => Ok(()),
            RunOutcome::Failed => {
                crate::safety::log(&format!("Set folder icon (st2k) failed for {p}"));
                Err(Error::new(E_FAIL, "couldn't set the folder icon"))
            }
            RunOutcome::SpawnFailed => folder_icon_one(None, p),
        },
        None => set_folder_icon(p),
    }
}

/// Like [`run_st2k_capture`], but returns the child's stderr text (trimmed) on a
/// non-zero exit instead of discarding it after logging - [`compress_one`] needs the
/// real error text to pull the "smallest reachable" byte count back out via
/// [`parse_smallest_achievable`], the same text `compress_to_size` returns in-process.
enum TextOutcome {
    /// Exited 0 with a non-empty stdout line - the real path `st2k` wrote to.
    Ok(PathBuf),
    /// The child ran but failed; carries its stderr (trimmed) for the caller to parse.
    Failed(String),
    /// The child could not be spawned at all.
    SpawnFailed,
}

fn run_st2k_capture_text(exe: &Path, path: &str, args: &[&str]) -> TextOutcome {
    match Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(out) if out.status.success() => {
            let stdout_path = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if stdout_path.is_empty() {
                TextOutcome::Failed(String::new())
            } else {
                TextOutcome::Ok(PathBuf::from(stdout_path))
            }
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            crate::safety::log_error(&format!("st2k helper failed for {path}: {stderr}"));
            TextOutcome::Failed(stderr)
        }
        Err(e) => {
            crate::safety::log_error(&format!(
                "st2k helper FAILED TO SPAWN ({e}) — routing this verb in-process instead"
            ));
            TextOutcome::SpawnFailed
        }
    }
}

/// Outcome of a routed `st2k` run whose stdout IS binary data (`clip-pixels`'s pixel
/// stream), not text - so it's read back as raw bytes rather than through
/// `String::from_utf8_lossy`, which would corrupt non-UTF8 pixel values.
enum BytesOutcome {
    /// Exited 0 - `stdout` is the raw bytes the child wrote.
    Ok(Vec<u8>),
    /// The child ran but failed (non-zero exit / crash / abort) on this file.
    Failed,
    /// The child could not be spawned at all.
    SpawnFailed,
}

fn run_st2k_capture_bytes(exe: &Path, path: &str, args: &[&str]) -> BytesOutcome {
    match Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(out) if out.status.success() => BytesOutcome::Ok(out.stdout),
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            crate::safety::log_error(&format!("st2k helper failed for {path}: {}", stderr.trim()));
            BytesOutcome::Failed
        }
        Err(e) => {
            crate::safety::log_error(&format!(
                "st2k helper FAILED TO SPAWN ({e}) — routing this verb in-process instead"
            ));
            BytesOutcome::SpawnFailed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Locks the fix for A121: shrink_one's routed `--quality` arg must be DERIVED from
    // encode::EMAIL_JPEG_QUALITY (now pub(crate)), not a hand-copied literal that can
    // silently drift from the in-process value. Referencing the constant here would not
    // even COMPILE without the pub(crate) visibility fix, and the value check catches a
    // future edit to one side without the other.
    #[test]
    fn routed_email_quality_matches_in_process_constant() {
        let routed_quality_arg = crate::verbs::encode::EMAIL_JPEG_QUALITY.to_string();
        assert_eq!(routed_quality_arg, "82");
        assert_eq!(crate::verbs::encode::EMAIL_JPEG_QUALITY, 82);
    }

    /// The module docs promise a missing helper "can never break a verb — it only
    /// forfeits the crash isolation". Nothing was proving that, and the reason is easy
    /// to miss: [`st2k_exe`] RESOLVES under test on any machine that has built the
    /// workspace, because cargo hardlinks `st2k.exe` into the very `deps\` directory
    /// the test binary runs from — and CI runs `cargo build` before `cargo test`, so it
    /// does too. Every routed verb therefore takes the ROUTED arm in BOTH places, and
    /// the in-process arm — the one a DLL-only install actually runs — was exercised
    /// nowhere. Don't "simplify" this by deleting the explicit `None`: that argument is
    /// the whole test, and letting `st2k_exe()` supply it would silently go back to
    /// testing the routed path twice.
    #[test]
    fn the_in_process_fallback_still_converts_when_no_helper_is_present() {
        let dir = std::env::temp_dir().join(format!(
            "st2k_helper_fallback_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("photo.png");
        DynamicImage::ImageRgb8(image::RgbImage::from_pixel(9, 7, image::Rgb([10, 90, 200])))
            .save(&src)
            .unwrap();

        // `None` is exactly what `run_action` passes when `st2k_exe()` finds nothing.
        let out = convert_one(
            None,
            src.to_str().unwrap(),
            Target {
                format: ImageFormat::Jpeg,
                ext: "jpg",
                webp_quality: None,
            },
        )
        .expect("a missing helper must forfeit isolation only — never the conversion");

        assert_eq!(
            out.extension().and_then(|e| e.to_str()),
            Some("jpg"),
            "the fallback must land on the same auto-named path the routed arm targets"
        );
        assert!(
            image::open(&out).is_ok(),
            "the fallback's output must be a real decodable image, not an empty slot"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // Locks the fix for A277: `transform_one` must return the path `st2k rotate`
    // actually reports on stdout, not a name predicted before the subprocess ran
    // (which a concurrent edit could steal). `run_st2k_capture` is the mechanism
    // that makes that possible — it must actually read the child's stdout instead
    // of discarding it like `run_st2k` does. Exercises a real subprocess (cmd.exe
    // standing in for st2k) rather than mocking it: a regression back to
    // `.status()` (stdout discarded) would make this fail every time, since
    // `CaptureOutcome::Ok` could never be reached.
    #[test]
    fn run_st2k_capture_reads_the_real_path_from_stdout() {
        let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
        let cmd_exe = Path::new(&system_root).join("System32").join("cmd.exe");
        // No spaces/parens in the echoed path — keeps this test independent of
        // Windows' argv quoting rules, which aren't what's under test here.
        let outcome = run_st2k_capture(
            &cmd_exe,
            "C:\\out\\file.jpg",
            &["/c", "echo", "C:\\out\\file_edited.jpg"],
        );
        match outcome {
            CaptureOutcome::Ok(path) => {
                assert_eq!(path, PathBuf::from("C:\\out\\file_edited.jpg"));
            }
            CaptureOutcome::Failed => panic!("cmd.exe echo should have exited 0 with output"),
            CaptureOutcome::SpawnFailed => panic!("cmd.exe should always be spawnable in CI"),
        }
    }

    /// `run_st2k` used to discard the child's stderr entirely (`Stdio::null()`),
    /// so a non-zero exit logged only a generic "failed for {path}" with no hint of
    /// WHY (access denied, disk full, an unsupported target — all indistinguishable).
    /// Exercises a real subprocess that writes a known marker to stderr and exits
    /// non-zero, and checks that marker actually reaches the diagnostics log.
    #[test]
    fn run_st2k_logs_the_real_stderr_on_a_non_zero_exit() {
        let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
        let cmd_exe = Path::new(&system_root).join("System32").join("cmd.exe");
        let marker = format!("st2k_helper_stderr_marker_{}", std::process::id());

        let outcome = run_st2k(
            &cmd_exe,
            "C:\\some\\path.jpg",
            &["/c", "echo", &marker, "1>&2", "&", "exit", "1"],
        );
        assert!(
            matches!(outcome, RunOutcome::Failed),
            "a non-zero exit must report Failed"
        );

        let log_path = crate::safety::log_file().expect("LOCALAPPDATA must be set to find the log");
        let contents = std::fs::read_to_string(&log_path).unwrap_or_default();
        assert!(
            contents.contains(&marker),
            "the child's real stderr must reach the diagnostics log"
        );
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "st2k_helper_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Pure validation, no subprocess: rejects a stream shorter than the header, a
    /// `w`/`h` over `MAX_DIM`, and a payload whose length doesn't match `w * h * 4`
    /// exactly — the checks that keep a hostile or buggy `clip-pixels` child from
    /// handing the parent anything it will trust with a bounded memcpy.
    #[test]
    fn parse_clip_pixels_rejects_short_oversized_and_mismatched_streams() {
        // Shorter than the 8-byte header.
        assert!(parse_clip_pixels(&[1, 2, 3]).is_none());

        // A well-formed 2x2 header with only 4 of the required 16 payload bytes.
        let mut short_payload = 2u32.to_le_bytes().to_vec();
        short_payload.extend_from_slice(&2u32.to_le_bytes());
        short_payload.extend_from_slice(&[0u8; 4]);
        assert!(parse_clip_pixels(&short_payload).is_none());

        // A width over MAX_DIM must be refused outright, whatever the rest of the
        // stream looks like — an attacker-controlled header claiming a huge canvas.
        let mut oversized = (decode::limits::MAX_DIM + 1).to_le_bytes().to_vec();
        oversized.extend_from_slice(&1u32.to_le_bytes());
        assert!(parse_clip_pixels(&oversized).is_none());

        // A correctly-sized, in-bounds stream parses to the exact dimensions + bytes.
        let mut good = 2u32.to_le_bytes().to_vec();
        good.extend_from_slice(&2u32.to_le_bytes());
        let rgba = [9u8; 16];
        good.extend_from_slice(&rgba);
        let (w, h, pixels) = parse_clip_pixels(&good).expect("a well-formed stream must parse");
        assert_eq!((w, h), (2, 2));
        assert_eq!(pixels, &rgba[..]);
    }

    /// `clipboard_one`'s ROUTED arm (`st2k clip-pixels` on the batch pool) must decode
    /// the exact same pixels the in-process arm would hand to `copy_rgba_to_clipboard`
    /// — checked without touching the real clipboard (shared, process-global state a
    /// test can't safely claim) by comparing the RGBA bytes each side produces.
    #[test]
    fn clip_pixels_routed_output_matches_the_in_process_decode() {
        let dir = scratch("clip_pixels");
        let src = dir.join("swatch.png");
        let img = image::RgbaImage::from_fn(3, 2, |x, y| {
            image::Rgba([(x * 40) as u8, (y * 60) as u8, 200, 255])
        });
        DynamicImage::ImageRgba8(img.clone()).save(&src).unwrap();
        let path = src.to_str().unwrap();
        let expected = img.into_raw();

        let Some(exe) = st2k_exe() else {
            panic!("st2k.exe must be resolvable under test — see the module docs' fallback note");
        };
        match run_st2k_capture_bytes(&exe, path, &["clip-pixels", path]) {
            BytesOutcome::Ok(stdout) => {
                let (w, h, rgba) = parse_clip_pixels(&stdout)
                    .expect("clip-pixels must print a well-formed header + payload");
                assert_eq!((w, h), (3, 2));
                assert_eq!(rgba, expected.as_slice());
            }
            BytesOutcome::Failed => panic!("st2k clip-pixels failed for {path}"),
            BytesOutcome::SpawnFailed => panic!("st2k.exe must be spawnable under test"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `clipboard_one(None, …)` must degrade straight to `copy_to_clipboard`, never
    /// trying to spawn anything — checked by comparing outcomes rather than reading
    /// the real clipboard back (no test in this file touches it directly, matching
    /// `clipboard.rs`'s own tests, which stop at the pure DIB-building functions).
    #[test]
    fn clipboard_one_with_no_helper_takes_the_in_process_fallback() {
        let dir = scratch("clip_fallback");
        let src = dir.join("swatch.png");
        DynamicImage::ImageRgb8(image::RgbImage::from_pixel(2, 2, image::Rgb([1, 2, 3])))
            .save(&src)
            .unwrap();
        let path = src.to_str().unwrap();

        assert_eq!(
            clipboard_one(None, path).is_ok(),
            copy_to_clipboard(path).is_ok(),
            "None must delegate straight to copy_to_clipboard, whatever this session's \
             clipboard access turns out to be"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `prepare_wallpaper_routed`'s ROUTED arm (`st2k wallpaper-prepare` on the batch
    /// pool) must decode/resize/encode the exact same PNG bytes the in-process
    /// `prepare_wallpaper` arm does, and the `None` arm must succeed standalone (the
    /// fallback). Both arms target the SAME persistent `%APPDATA%\SageThumbs2K`
    /// destination by design (the desktop needs a fixed path to keep reading from,
    /// not a scratch one) — this is the exact file every real Set-as-wallpaper click
    /// already overwrites, so reading it back here carries no extra risk.
    #[test]
    fn wallpaper_prepare_routed_matches_the_in_process_decode() {
        let dir = scratch("wallpaper_routed");
        let src = dir.join("swatch.png");
        DynamicImage::ImageRgb8(image::RgbImage::from_pixel(6, 4, image::Rgb([12, 200, 90])))
            .save(&src)
            .unwrap();
        let path = src.to_str().unwrap();

        let Some(exe) = st2k_exe() else {
            panic!("st2k.exe must be resolvable under test — see the module docs' fallback note");
        };
        let routed = prepare_wallpaper_routed(Some(&exe), path)
            .expect("the routed arm must succeed for a valid PNG");
        let routed_bytes = std::fs::read(&routed).unwrap();

        let in_process = prepare_wallpaper_routed(None, path)
            .expect("the in-process (fallback) arm must succeed for the same PNG");
        let in_process_bytes = std::fs::read(&in_process).unwrap();

        assert_eq!(
            routed, in_process,
            "both arms must target the same persistent path"
        );
        assert_eq!(
            routed_bytes, in_process_bytes,
            "the routed and in-process arms must encode identical PNG bytes"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `folder_icon_one`'s ROUTED arm (`st2k folder-icon` on the batch pool) must
    /// produce the exact same `.ico` bytes and `desktop.ini` content the in-process
    /// `set_folder_icon` arm does, each in a folder it owns; the `None` arm succeeding
    /// on its own is the fallback proof.
    #[test]
    fn folder_icon_routed_matches_the_in_process_output() {
        let base = scratch("foldericon_routed");
        let routed_dir = base.join("routed");
        let direct_dir = base.join("direct");
        std::fs::create_dir_all(&routed_dir).unwrap();
        std::fs::create_dir_all(&direct_dir).unwrap();

        let make_src = |dir: &Path| {
            let p = dir.join("src.png");
            image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
                5,
                5,
                image::Rgb([30, 60, 90]),
            ))
            .save(&p)
            .unwrap();
            p
        };
        let routed_src = make_src(&routed_dir);
        let direct_src = make_src(&direct_dir);

        let Some(exe) = st2k_exe() else {
            panic!("st2k.exe must be resolvable under test — see the module docs' fallback note");
        };
        folder_icon_one(Some(&exe), routed_src.to_str().unwrap())
            .expect("the routed arm must succeed for a valid PNG");
        folder_icon_one(None, direct_src.to_str().unwrap())
            .expect("the in-process (fallback) arm must succeed for the same PNG");

        let routed_ico = std::fs::read(routed_dir.join("SageThumbsFolder.ico")).unwrap();
        let direct_ico = std::fs::read(direct_dir.join("SageThumbsFolder.ico")).unwrap();
        assert_eq!(
            routed_ico, direct_ico,
            "both arms must encode an identical .ico"
        );

        let routed_ini = std::fs::read_to_string(routed_dir.join("desktop.ini")).unwrap();
        let direct_ini = std::fs::read_to_string(direct_dir.join("desktop.ini")).unwrap();
        assert_eq!(
            routed_ini, direct_ini,
            "both arms must write an identical desktop.ini"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// `compress_one`'s ROUTED arm (`st2k compress` on the batch pool) must reach the
    /// same meetable target the in-process `compress_one_to_size` arm does, with
    /// byte-identical output (same engine, same deterministic search); the `None` arm
    /// succeeding on its own is the fallback proof.
    #[test]
    fn compress_one_routed_matches_the_in_process_result_for_a_meetable_target() {
        let dir = scratch("compress_routed");
        // Per-pixel noise so the JPEG has real size to search over (a flat image would
        // compress to almost nothing regardless of target).
        let img = image::RgbImage::from_fn(96, 96, |x, y| {
            let h = (x.wrapping_mul(0x9E37_79B9) ^ y.wrapping_mul(0x85EB_CA6B)).rotate_left(7);
            image::Rgb([h as u8, (h >> 8) as u8, (h >> 16) as u8])
        });
        let src = dir.join("noise.png");
        DynamicImage::ImageRgb8(img).save(&src).unwrap();
        let path = src.to_str().unwrap();
        // Generous relative to the 96x96 noise source — easily meetable either way.
        let target = 40_000u64;

        let Some(exe) = st2k_exe() else {
            panic!("st2k.exe must be resolvable under test — see the module docs' fallback note");
        };
        let routed =
            compress_one(Some(&exe), path, target).expect("the routed arm must meet the target");
        let routed_bytes = std::fs::read(&routed).unwrap();
        assert!(
            routed_bytes.len() as u64 <= target,
            "routed output {} exceeds target {target}",
            routed_bytes.len()
        );

        let in_process = compress_one(None, path, target)
            .expect("the in-process (fallback) arm must meet the same target");
        let in_process_bytes = std::fs::read(&in_process).unwrap();
        assert!(
            in_process_bytes.len() as u64 <= target,
            "in-process output {} exceeds target {target}",
            in_process_bytes.len()
        );

        assert_eq!(
            routed_bytes, in_process_bytes,
            "the routed and in-process arms must produce byte-identical output"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
