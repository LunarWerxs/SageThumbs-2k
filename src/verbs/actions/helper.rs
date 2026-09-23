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
/// we resolve it from the DLL's OWN path ([`st2k_base::host::module_path`]) — **never**
/// `current_exe()`, which in the shell host is `explorer.exe`/`dllhost.exe`. Returns
/// `Some` only when the file exists; `None` (helper missing — tests, or a DLL-only
/// install) makes every routed verb fall back to its in-process path. See the
/// module docs for the rationale.
pub(super) fn st2k_exe() -> Option<PathBuf> {
    st2k_base::host::sibling_of_dll(st2k_base::host::CLI_EXE)
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
    // Worded differently from the thumbnail tiers' "spawned helper pid" on purpose: the
    // Explorer verify counts THAT phrase per thumbnail, and a verb the user happens to run
    // during it must not land in the count.
    st2k_base::safety::log_debugf!(
        "running verb helper {} for {path}",
        args.first().copied().unwrap_or("?")
    );
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
            st2k_base::safety::log_error(&format!(
                "st2k helper failed for {path}: {}",
                stderr.trim()
            ));
            RunOutcome::Failed
        }
        Err(e) => {
            st2k_base::safety::log_error(&format!(
                "st2k helper FAILED TO SPAWN ({e}) — routing this verb in-process instead"
            ));
            RunOutcome::SpawnFailed
        }
    }
}

/// Spawn `st2k` with `args` (no console window) and collect its output: stdin unused,
/// stdout and stderr piped. The shape every capturing caller needs.
fn spawn_st2k(exe: &Path, args: &[&str]) -> std::io::Result<std::process::Output> {
    Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
}

/// The stdout of a successful `st2k` run read back as the real output path it printed
/// (`println!`'s trailing newline and any stray CR trimmed off), or `None` when it
/// printed nothing usable.
fn stdout_path(out: &std::process::Output) -> Option<PathBuf> {
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
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
    match spawn_st2k(exe, args) {
        Ok(out) if out.status.success() => {
            // `println!` adds the trailing newline; trim it (and any stray CR) off.
            match stdout_path(&out) {
                Some(path) => CaptureOutcome::Ok(path),
                None => CaptureOutcome::Failed,
            }
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            st2k_base::safety::log_error(&format!(
                "st2k helper failed for {path}: {}",
                stderr.trim()
            ));
            CaptureOutcome::Failed
        }
        Err(e) => {
            st2k_base::safety::log_error(&format!(
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
            let q = st2k_base::settings::jpeg_quality().to_string();
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
                    st2k_base::safety::log(&format!("Convert (st2k) failed for {p}"));
                    None
                }
                RunOutcome::SpawnFailed => convert_one(None, p, target),
            }
        }
        None => match crate::verbs::encode::convert_file(p, target) {
            Ok(out) => Some(out),
            Err(e) => {
                st2k_base::safety::log(&format!("Convert failed for {p}: {e:?}"));
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
                        st2k_base::safety::log(&format!(
                            "Transform (st2k): predicted output {predicted:?} differs from \
                             the actual {path:?} for {p} — a concurrent edit likely won the \
                             naming race; using the actual path"
                        ));
                    }
                    Some(path)
                }
                CaptureOutcome::Failed => {
                    st2k_base::safety::log(&format!("Transform (st2k) failed for {p}"));
                    None
                }
                CaptureOutcome::SpawnFailed => transform_one(None, p, t),
            }
        }
        None => transform_file(p, t).ok(),
    }
}

/// The shared tail of a routed verb that writes to a caller-reserved path: `exe` runs
/// `st2k` with `args`; a clean exit yields `out` (the reserved path), a per-file failure
/// logs `fail_msg` and yields `None`, and a spawn failure yields the caller's in-process
/// `fallback` instead.
fn finish_st2k<F: FnOnce() -> Option<PathBuf>>(
    exe: &Path,
    p: &str,
    args: &[&str],
    out: PathBuf,
    fail_msg: &str,
    fallback: F,
) -> Option<PathBuf> {
    match run_st2k(exe, p, args) {
        RunOutcome::Ok => Some(out),
        RunOutcome::Failed => {
            st2k_base::safety::log(fail_msg);
            None
        }
        RunOutcome::SpawnFailed => fallback(),
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
            let q = st2k_base::settings::jpeg_quality().to_string();
            finish_st2k(
                exe,
                p,
                &["convert", p, out_s, "--quality", &q, "--resize", &rs],
                slot.path().to_path_buf(),
                &format!("Resize (st2k) failed for {p}"),
                || resize_one(None, p, r),
            )
        }
        None => match resize_file(p, r) {
            Ok(out) => Some(out),
            Err(e) => {
                st2k_base::safety::log(&format!("Resize failed for {p}: {e:?}"));
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
            finish_st2k(
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
                    // An email attachment stays clean even while "keep metadata" is on; the
                    // in-process `shrink_for_email` never carried any either.
                    "--strip-metadata",
                ],
                slot.path().to_path_buf(),
                &format!("Shrink for email (st2k) failed for {p}"),
                || shrink_one(None, p, size),
            )
        }
        None => match shrink_for_email(p, size) {
            Ok(out) => Some(out),
            Err(e) => {
                st2k_base::safety::log(&format!("Shrink for email failed for {p}: {e:?}"));
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
                st2k_base::safety::log(&format!("Strip metadata (st2k) failed for {p}"));
                false
            }
            RunOutcome::SpawnFailed => strip_one(None, p),
        },
        None => match st2k_codecs::strip::strip_metadata(p) {
            Ok(()) => true,
            Err(e) => {
                st2k_base::safety::log(&format!("Strip metadata failed for {p}: {e:?}"));
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
) -> std::result::Result<PathBuf, Option<u64>> {
    match exe {
        Some(exe) => {
            let target_s = target.to_string();
            match run_st2k_capture_text(exe, p, &["compress", p, "--max-size", &target_s]) {
                TextOutcome::Ok(path) => Ok(path),
                TextOutcome::Failed(stderr) => {
                    st2k_base::safety::log(&format!("Compress (st2k) failed for {p}"));
                    Err(parse_smallest_achievable(&stderr))
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
                    st2k_base::safety::log(&format!(
                        "Copy to clipboard (st2k) produced an unreadable pixel stream for {p}"
                    ));
                    Err(Error::new(
                        E_FAIL,
                        "clipboard helper produced an invalid pixel stream",
                    ))
                }
            },
            BytesOutcome::Failed => {
                st2k_base::safety::log(&format!("Copy to clipboard (st2k) failed for {p}"));
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
pub(super) fn prepare_wallpaper_routed(
    exe: Option<&Path>,
    p: &str,
    lock_screen: bool,
) -> Result<PathBuf> {
    match exe {
        Some(exe) => {
            let dir = wallpaper::appdata_dir()?;
            let Some(dir_s) = dir.to_str() else {
                return prepare_wallpaper_routed(None, p, lock_screen);
            };
            let mut args = vec!["wallpaper-prepare", p, dir_s];
            if lock_screen {
                args.push("--lockscreen");
            }
            match run_st2k_capture(exe, p, &args) {
                CaptureOutcome::Ok(wp) => Ok(wp),
                CaptureOutcome::Failed => {
                    st2k_base::safety::log(&format!("Set wallpaper (st2k) failed for {p}"));
                    Err(Error::new(E_FAIL, "couldn't set the wallpaper"))
                }
                CaptureOutcome::SpawnFailed => prepare_wallpaper_routed(None, p, lock_screen),
            }
        }
        None if lock_screen => wallpaper::prepare_lock_screen(p),
        None => prepare_wallpaper(p),
    }
}

/// `VerbAction::Wallpaper` - decode/resize/encode routed via
/// [`prepare_wallpaper_routed`], then applied in-process
/// (`wallpaper::apply_wallpaper` - registry write + `SystemParametersInfoW`, no
/// decode either way).
pub(super) fn wallpaper_one(exe: Option<&Path>, p: &str, mode: WallpaperMode) -> Result<()> {
    let wp = prepare_wallpaper_routed(exe, p, false)?;
    wallpaper::apply_wallpaper(&wp, mode)
}

/// `VerbAction::LockScreen` - decode/resize/encode routed via the SAME
/// [`prepare_wallpaper_routed`] (shares `prepare_wallpaper`/`wallpaper-prepare`'s output with
/// Set-as-wallpaper — no separate prepare path), then applied in-process
/// (`wallpaper::apply_lock_screen` - WinRT `LockScreen::SetImageFileAsync`, no decode either
/// way).
pub(super) fn lock_screen_one(exe: Option<&Path>, p: &str) -> Result<()> {
    let wp = prepare_wallpaper_routed(exe, p, true)?;
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
                st2k_base::safety::log(&format!("Set folder icon (st2k) failed for {p}"));
                Err(Error::new(E_FAIL, "couldn't set the folder icon"))
            }
            RunOutcome::SpawnFailed => folder_icon_one(None, p),
        },
        None => set_folder_icon(p),
    }
}

/// Save a video's frame as a standalone `<stem> (frame).png` sibling. Routes to
/// `st2k thumbnail <file> <out> --size 0` — `0` asks for the full-resolution frame,
/// not a thumbnail-sized one (see `cli::fit_for_cli`). Unlike every other verb in
/// this file, there is **no in-process fallback**: video decode (OS Media
/// Foundation) must never run inside `explorer.exe`/`dllhost.exe`, only in the
/// disposable `st2k.exe` child, so a missing/unspawnable helper fails the verb
/// outright instead of degrading.
pub(super) fn save_video_frame_one(exe: Option<&Path>, p: &str) -> Result<PathBuf> {
    let Some(exe) = exe else {
        return Err(Error::new(
            E_FAIL,
            "the st2k helper is required to save a video frame",
        ));
    };
    let src = Path::new(p);
    let slot = reserve_unique_suffix(src, "frame", "png");
    let Some(out_s) = slot.path().to_str() else {
        return Err(Error::new(E_FAIL, "the output path isn't valid UTF-8"));
    };
    match run_st2k(exe, p, &["thumbnail", p, out_s, "--size", "0"]) {
        RunOutcome::Ok => Ok(slot.path().to_path_buf()),
        RunOutcome::Failed => {
            st2k_base::safety::log(&format!("Save video frame (st2k) failed for {p}"));
            Err(Error::new(E_FAIL, "couldn't extract the video frame"))
        }
        RunOutcome::SpawnFailed => {
            st2k_base::safety::log(&format!(
                "Save video frame (st2k) couldn't spawn the helper for {p}"
            ));
            Err(Error::new(E_FAIL, "the st2k helper couldn't be started"))
        }
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
    match spawn_st2k(exe, args) {
        Ok(out) if out.status.success() => match stdout_path(&out) {
            Some(path) => TextOutcome::Ok(path),
            None => TextOutcome::Failed(String::new()),
        },
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            st2k_base::safety::log_error(&format!("st2k helper failed for {path}: {stderr}"));
            TextOutcome::Failed(stderr)
        }
        Err(e) => {
            st2k_base::safety::log_error(&format!(
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
    match spawn_st2k(exe, args) {
        Ok(out) if out.status.success() => BytesOutcome::Ok(out.stdout),
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            st2k_base::safety::log_error(&format!(
                "st2k helper failed for {path}: {}",
                stderr.trim()
            ));
            BytesOutcome::Failed
        }
        Err(e) => {
            st2k_base::safety::log_error(&format!(
                "st2k helper FAILED TO SPAWN ({e}) — routing this verb in-process instead"
            ));
            BytesOutcome::SpawnFailed
        }
    }
}

#[cfg(test)]
mod tests;
