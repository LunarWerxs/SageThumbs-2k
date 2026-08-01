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
/// stdin is unused; stdout/stderr are dropped — the routed verbs communicate only
/// through the file they write + the exit code. A spawn error is logged once here
/// (it's a routing-level problem, not a per-file one) and surfaced as
/// [`RunOutcome::SpawnFailed`] so the caller can fall back to in-process.
fn run_st2k(exe: &Path, args: &[&str]) -> RunOutcome {
    match Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status()
    {
        Ok(s) if s.success() => RunOutcome::Ok,
        Ok(_) => RunOutcome::Failed,
        Err(e) => {
            crate::safety::log(&format!(
                "st2k helper FAILED TO SPAWN ({e}) — routing this verb in-process instead"
            ));
            RunOutcome::SpawnFailed
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
            match run_st2k(exe, &args) {
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
            // (same `transform_file`). Predict that name BEFORE the run (while it's
            // still free) so it matches what st2k picks — recomputing afterwards
            // would see the new file and pick `(edited 2)` instead.
            let src = Path::new(p);
            let ext = routed_edit_output_ext(src);
            // PREDICT (read-only) the name `st2k rotate` will auto-pick — do NOT
            // reserve it, or st2k's own picker would see our placeholder and bump to
            // `(edited 2)`. Rotate names derive from the distinct source stem, so
            // parallel rotates of a selection don't collide on the prediction.
            let predicted = predict_unique_suffix(src, "edited", &ext);
            match run_st2k(exe, &["rotate", p, "--by", by]) {
                RunOutcome::Ok => Some(predicted),
                RunOutcome::Failed => {
                    crate::safety::log(&format!("Transform (st2k) failed for {p}"));
                    None
                }
                RunOutcome::SpawnFailed => transform_one(None, p, t),
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
            // EMAIL_JPEG_QUALITY (82) is private to encode.rs; pass the same literal.
            match run_st2k(
                exe,
                &["convert", p, out_s, "--quality", "82", "--resize", &resize],
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
        Some(exe) => match run_st2k(exe, &["strip", p]) {
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
