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
}
