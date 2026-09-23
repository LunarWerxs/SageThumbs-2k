//! The batch runner: `prebuild` (Explorer thumbnail cache warmup, with its Ctrl+C
//! cancel wiring), the input-expansion walk, and `batch`'s fan-out over
//! thumbnail/convert/info with collision-free output reservation.

use super::*;

/// Ctrl+C -> graceful cancel for [`prebuild`], the one CLI verb long enough to need it
/// (`prebuild.rs`'s module doc promises "v1 offers cancel (Ctrl+C) instead" of pause/resume).
///
/// A raw kernel32 import rather than pulling in the `windows` crate's `Win32_System_Console`
/// feature for one call — the same tradeoff `decode.rs`'s `magick_gate` makes for its
/// semaphore calls: kernel32 is always linked, so declaring the one function here avoids
/// growing the feature list (and the generated bindings) for a single call site.
mod ctrlc_cancel {
    use std::sync::atomic::{AtomicBool, Ordering};

    static CANCEL: AtomicBool = AtomicBool::new(false);
    static INSTALLED: AtomicBool = AtomicBool::new(false);

    #[link(name = "kernel32")]
    extern "system" {
        fn SetConsoleCtrlHandler(handler: Option<extern "system" fn(u32) -> i32>, add: i32) -> i32;
    }

    const CTRL_C_EVENT: u32 = 0;

    /// Runs on a dedicated OS thread Windows creates for it, NOT the main thread — so this
    /// must stay to a single atomic store and nothing that could block or panic (panic=abort
    /// would take the whole process down from a thread `run`'s cancel-check loop never sees).
    /// Returning TRUE (handled) for Ctrl+C stops Windows from ALSO running its own terminate
    /// action, which is what makes the graceful partial-report path in `run` reachable at all;
    /// every other event (Break/Close/Logoff/Shutdown) returns FALSE so it keeps behaving like
    /// there is no handler installed.
    extern "system" fn on_ctrl(ctrl_type: u32) -> i32 {
        if ctrl_type == CTRL_C_EVENT {
            CANCEL.store(true, Ordering::SeqCst);
            1
        } else {
            0
        }
    }

    /// Install the handler (once per process — a second `SetConsoleCtrlHandler(Some(_), TRUE)`
    /// would just chain a duplicate) and reset the flag, so a second `prebuild` call in the
    /// same process (tests; a future long-lived host) starts from "not cancelled" rather than
    /// inheriting a stale Ctrl+C from a previous run.
    pub(super) fn install() {
        CANCEL.store(false, Ordering::SeqCst);
        if !INSTALLED.swap(true, Ordering::SeqCst) {
            // SAFETY: `on_ctrl` matches `HandlerRoutine`'s `extern "system" fn(u32) -> BOOL`
            // signature exactly, and the handle/pointer types involved are `Option<fn>` and
            // `i32`, not raw pointers this call could misuse.
            unsafe {
                SetConsoleCtrlHandler(Some(on_ctrl), 1);
            }
        }
    }

    /// The flag [`super::prebuild`] hands to `prebuild::run`'s `cancel` parameter.
    pub(super) fn flag() -> &'static AtomicBool {
        &CANCEL
    }
}

/// Cap on `expand_inputs`'s recursive descent — a junction/symlink cycle would otherwise
/// spin forever. Matches `prebuild::Options::default()`'s own `max_depth`, so `st2k batch
/// --recurse` and `st2k prebuild --recurse` behave the same on a pathological tree.
const MAX_RECURSE_DEPTH: u32 = 64;

/// `FILE_ATTRIBUTE_REPARSE_POINT` — junctions and symlinks, which `expand_inputs`'s
/// recursive walk does not follow (mirrors `prebuild.rs`'s private `walk`, which cannot be
/// called from here — see [`expand_inputs`]'s doc comment).
const REPARSE_ATTR: u32 = 0x0000_0400;

fn expand_inputs_is_supported(p: &Path) -> bool {
    // `is_known` is ASCII-case-insensitive — no lowercase allocation needed.
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(formats::is_known)
}

/// One directory entry from [`expand_inputs_walk`]'s `read_dir` loop, classified and
/// (if it's a wanted file) pushed into `out`/`skipped_offline`. Split out so the loop
/// body itself is a single call and the branching lives in one place.
fn expand_inputs_visit(
    p: &Path,
    recurse: bool,
    depth: u32,
    out: &mut Vec<String>,
    skipped_offline: &mut usize,
) {
    let a = st2k_base::fsutil::file_attributes(p);
    if a & REPARSE_ATTR != 0 {
        return;
    }
    if p.is_dir() {
        if recurse {
            expand_inputs_walk(p, recurse, depth + 1, out, skipped_offline);
        }
    } else if p.is_file() && expand_inputs_is_supported(p) {
        if a & crate::prebuild::OFFLINE_ATTRS != 0 {
            *skipped_offline += 1;
        } else {
            out.push(p.to_string_lossy().into_owned());
        }
    }
}

fn expand_inputs_walk(
    dir: &Path,
    recurse: bool,
    depth: u32,
    out: &mut Vec<String>,
    skipped_offline: &mut usize,
) {
    if depth > MAX_RECURSE_DEPTH {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        expand_inputs_visit(&e.path(), recurse, depth, out, skipped_offline);
    }
}

/// Expand `inputs` (files and/or directories) into a flat list of SUPPORTED image files;
/// unsupported extensions dropped; cloud placeholders dropped too — see
/// [`crate::prebuild::is_cloud_placeholder`]. Second element is how many were skipped as
/// placeholders, so callers can tell the user rather than silently hydrating them.
///
/// `recurse = false` scans each directory ONE level deep (the historical, still-default
/// behaviour — an agent pointed at a photo tree with subfolders used to get a partial
/// result and a clean "N/N succeeded" with no way to ask for more). `recurse = true` walks
/// the whole tree, never following a reparse point (junction/symlink) and capped at
/// [`MAX_RECURSE_DEPTH`] so a cycle can't spin forever — the same two guards
/// `prebuild::walk` applies, duplicated rather than shared because that function is
/// private to `prebuild.rs` and unreachable from here.
///
/// Third element: the EXPLICIT arguments that resolved to nothing - a path that does not
/// exist, or a named file of a type we do not read - each with why. A directory scan drops
/// incidental unsupported files quietly by design; an input the caller NAMED is different, and
/// until 2026-09-19 it simply vanished from the totals (`requested=1 succeeded=1 status=ok` for
/// two inputs, audit F20), so nothing downstream could report or retry it.
#[allow(clippy::type_complexity)]
fn expand_inputs(inputs: &[String], recurse: bool) -> (Vec<String>, usize, Vec<(String, String)>) {
    let mut out = Vec::new();
    let mut skipped_offline = 0usize;
    let mut unresolved = Vec::new();
    for i in inputs {
        expand_single_input(i, recurse, &mut out, &mut skipped_offline, &mut unresolved);
    }
    (out, skipped_offline, unresolved)
}

/// Classify a single explicit input argument as a directory, supported file, or unresolved.
fn expand_single_input(
    i: &str,
    recurse: bool,
    out: &mut Vec<String>,
    skipped_offline: &mut usize,
    unresolved: &mut Vec<(String, String)>,
) {
    let p = Path::new(i);
    if p.is_dir() {
        expand_inputs_walk(p, recurse, 0, out, skipped_offline);
    } else if p.is_file() && expand_inputs_is_supported(p) {
        if crate::prebuild::is_cloud_placeholder(p) {
            *skipped_offline += 1;
        } else {
            out.push(i.to_string());
        }
    } else if p.is_file() {
        unresolved.push((i.to_string(), "not a supported image type".to_string()));
    } else {
        unresolved.push((i.to_string(), "input not found".to_string()));
    }
}

#[allow(clippy::too_many_arguments)]
/// Pre-build Explorer's thumbnails for whole folders, so browsing them later is instant.
///
/// Refuses to run elevated on purpose: the thumbnail cache is per-user, so an admin prompt
/// would faithfully build every thumbnail into the ADMINISTRATOR's cache and the user would
/// see no change at all — a total success that accomplishes nothing.
pub fn prebuild(
    inputs: &[String],
    recurse: bool,
    sizes: Vec<u32>,
    rebuild_all: bool,
    jobs: usize,
) -> Result<String, String> {
    use crate::prebuild as pb;

    if pb::is_elevated() {
        return Err(
            "prebuild must NOT run as administrator: Windows keeps the thumbnail \
                    cache per user, so an elevated run fills the administrator's cache and \
                    nothing changes for you. Run it from a normal prompt."
                .to_string(),
        );
    }

    let opts = pb::Options {
        recurse,
        sizes,
        rebuild_all,
        jobs,
        ..Default::default()
    };

    // Wire up the graceful cancel `run`'s own doc promises ("v1 offers cancel (Ctrl+C)
    // instead"): without a handler installed, Windows' default action on Ctrl+C is to kill the
    // process outright, so a long prebuild had no way to stop early with a partial report — only
    // `taskkill`, which loses the report entirely. `ctrlc_cancel::install` sets `CANCEL` and
    // returns TRUE so the default terminate never runs; `run` checks the flag between files.
    ctrlc_cancel::install();
    let cancel = ctrlc_cancel::flag();

    // A drive walk can take a while before the first thumbnail; say what is happening rather
    // than looking hung.
    eprintln!("Scanning...");
    let last = std::sync::atomic::AtomicUsize::new(0);
    let rep = pb::run(inputs, &opts, Some(cancel), |done, total| {
        // One line per percent, not per file: a 200k-file run would otherwise spend its time
        // writing to the console.
        let pct = done * 100 / total.max(1);
        if pct != last.swap(pct, std::sync::atomic::Ordering::Relaxed) {
            eprint!("\r  {done}/{total} ({pct}%)   ");
        }
    });
    eprintln!();

    let mut out = format!(
        "{} supported file(s) found\n  built    {}\n  cached   {}\n  failed   {}",
        rep.found, rep.built, rep.already, rep.failed
    );
    // `partial` belongs here for the same reason it exists at all: without it, built+cached+
    // failed silently fails to add up to `found`, and the unexplained remainder is exactly the
    // files that will re-extract on first browse. The GUI summary already shows it, so leaving
    // the CLI out would reintroduce "the run says it finished" on the other surface.
    if rep.partial > 0 {
        out.push_str(&format!(
            "\n  partial  {} — cached at some sizes but not all; those views still rebuild on first browse",
            rep.partial
        ));
    }
    if rep.cancelled {
        out.push_str("\n  stopped early: Ctrl+C — the counts above are a partial report");
    }
    if rep.skipped_offline > 0 {
        out.push_str(&format!(
            "\n  skipped  {} cloud placeholder(s) — extracting these would download them",
            rep.skipped_offline
        ));
    }
    if rep.unreadable_dirs > 0 {
        out.push_str(&format!(
            "\n  {} folder(s) could not be read",
            rep.unreadable_dirs
        ));
    }
    let px = rep
        .sizes
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    out.push_str(&format!(
        "\n\nBuilt at {px} px — the buckets Explorer's Medium, Large and Extra-large views \
         read. The largest is rendered once and the smaller views are derived from it, so the \
         run costs one render per file rather than one per size. Windows caps the cache and \
         evicts the oldest entries, so very large runs can lose their earliest work — prefer \
         folders over whole drives."
    ));
    Ok(out)
}

/// Atomically claim the first available `<stem>[ (n)].<ext>` path under `dir` by
/// creating it with `create_new` — no separate "does it exist" check followed by a
/// later write, so nothing (not the parallel pass below, not an external writer
/// like a concurrent `st2k` invocation, Explorer, or a right-click verb) can land
/// on the same name in between. `verbs::encode::slots::reserve` documents this
/// exact TOCTOU race and fixes it the same way, but that module is private to
/// `verbs` and unreachable from here, hence the local copy of the technique
/// rather than a plain `exists()` loop (the bug this replaces).
///
/// The returned path is a real, empty, already-created file — the caller fills it
/// in (a plain encoder save overwrites the empty placeholder).
///
/// `Err` carries the OS reason the name could not be claimed (2026-09-05 audit, F11).
/// This used to hand the candidate path back anyway and let the encode pass surface the
/// error, which cost the caller the ONE fact it needed to classify the failure: an
/// unwritable destination then arrived indistinguishable from a corrupt input. The file
/// count is unchanged either way, since a name this call cannot create is a name the
/// encoder's own temp-write-then-rename cannot land on either.
fn reserve_batch_output(dir: &Path, stem: &str, ext: &str) -> std::result::Result<PathBuf, String> {
    let mut n = 0u32;
    loop {
        let cand = if n == 0 {
            dir.join(format!("{stem}.{ext}"))
        } else {
            dir.join(format!("{stem} ({n}).{ext}"))
        };
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&cand)
        {
            Ok(_) => return Ok(cand),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
            // Couldn't create for another reason: permission, a missing directory, or a
            // FOLDER already sitting on that exact name (Windows answers that with access
            // denied, not already-exists, so bumping the suffix would not help).
            Err(e) => return Err(format!("cannot write {}: {e}", cand.display())),
        }
    }
}

/// BULK process many inputs (files and/or folders) in ONE process, fanned out across all
/// cores via the shared batch pool — the fast path for the regression harness and AI
/// agents (no more one `st2k` spawn per file). `op` is `thumbnail` (→ PNG at `size`px),
/// `convert` (→ `to_ext`, honoring `quality`/`resize`), or `info` (dimensions/EXIF/audio
/// tags → one JSON array, see [`batch_info`]; `out_dir`/`size`/`to_ext`/`quality`/`resize`
/// are ignored for that op). Outputs (for `thumbnail`/`convert`) go to `out_dir` (created if
/// needed) or next to each source. `recurse = false` (the default) scans each input
/// directory ONE level deep; `true` walks the whole tree — see [`expand_inputs`]. Returns a
/// `done/total` summary for `thumbnail`/`convert`, or the JSON array for `info`.
///
/// `json` (the CLI's `--json`, always on over MCP, exactly as `pdf`/`cbz` and `info` do it)
/// returns the whole [`verbs::BatchReport`] instead: input, output, status, cause and
/// elapsed time per file, so a script knows precisely which files to retry and why
/// (2026-09-05 audit, F11). The human text is unchanged for a clean run and grows one
/// `failed` line per failure otherwise, the same shape `pdf`/`cbz` print for an omitted
/// input.
#[allow(clippy::too_many_arguments)]
pub fn batch(
    op: &str,
    inputs: &[String],
    recurse: bool,
    out_dir: Option<&str>,
    size: u32,
    to_ext: Option<&str>,
    quality: u8,
    resize: verbs::Resize,
    json: bool,
) -> Result<String, String> {
    // Same clamp `convert` applies — one place both front ends agree on.
    let quality = quality.clamp(1, 100);
    if op == "info" {
        return batch_info(inputs, recurse);
    }
    let is_convert = match op {
        "thumbnail" | "thumb" => false,
        "convert" => true,
        other => {
            return Err(format!(
                "unknown batch op '{other}' (thumbnail|convert|info)"
            ))
        }
    };
    let ext = if is_convert {
        to_ext
            .ok_or("batch convert needs --to <ext>")?
            .trim_start_matches('.')
            .to_ascii_lowercase()
    } else {
        "png".to_string()
    };

    let (files, skipped_offline, unresolved) = expand_inputs(inputs, recurse);
    if files.is_empty() {
        return Err("no supported image files found in the inputs".to_string());
    }
    if let Some(d) = out_dir {
        std::fs::create_dir_all(d).map_err(|e| format!("cannot create output dir {d}: {e}"))?;
    }

    let pairs = reserve_batch_outputs(&files, out_dir, &ext);
    let job = BatchJob {
        is_convert,
        size,
        quality,
        ext: &ext,
        resize,
    };
    // Fan out: each (input, pre-reserved output) is independent → no naming race.
    let mut outcomes = st2k_base::parallel::map(&pairs, |_, (input, slot)| job.run(input, slot));
    // AFTER the real outcomes, so `clean_failed_placeholders`' pair-by-pair zip still lines up.
    outcomes.extend(unresolved.iter().map(|(input, why)| {
        verbs::FileOutcome::failed(input, Some(verbs::OmitCause::Unreadable), why)
    }));
    let report = verbs::BatchReport {
        files: outcomes,
        skipped_offline,
    };
    clean_failed_placeholders(&pairs, &report);
    batch_report(&report, json)
}

/// Reserve one collision-free output path per input, SERIALLY and ATOMICALLY, so neither
/// the parallel pass nor an EXTERNAL writer (a concurrent `st2k` invocation, Explorer, a
/// right-click verb) can land on the same name. See [`reserve_batch_output`] for why (and
/// why not a plain `used`/`exists()` check). A reservation that fails is carried as the
/// `Err` it was, not dropped: it is that file's whole result.
fn reserve_batch_outputs(
    files: &[String],
    out_dir: Option<&str>,
    ext: &str,
) -> Vec<(String, std::result::Result<PathBuf, String>)> {
    let mut pairs = Vec::with_capacity(files.len());
    for f in files {
        let src = Path::new(f);
        let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("image");
        let dir = match out_dir {
            Some(d) => PathBuf::from(d),
            None => src
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| PathBuf::from(".")),
        };
        pairs.push((f.clone(), reserve_batch_output(&dir, stem, ext)));
    }
    pairs
}

/// The one op a `batch` run repeats over every input. A struct rather than seven arguments
/// threaded through the worker: the settings are fixed for the whole run, only the file
/// changes.
struct BatchJob<'a> {
    is_convert: bool,
    size: u32,
    quality: u8,
    ext: &'a str,
    resize: verbs::Resize,
}

impl BatchJob<'_> {
    /// One input, start to finish, as the per-file record both front ends report through
    /// (2026-09-05 audit, F11). Before this the whole thing was `.is_ok()`, so the reason
    /// died here.
    fn run(&self, input: &str, slot: &std::result::Result<PathBuf, String>) -> verbs::FileOutcome {
        let started = std::time::Instant::now();
        let out = match slot {
            Ok(p) => p,
            Err(e) => {
                return verbs::FileOutcome::failed(input, Some(verbs::OmitCause::Unwritable), e)
                    .timed(started.elapsed())
            }
        };
        let attempt = if self.is_convert {
            // `quality` is the only quality knob `batch` exposes; before this fix it
            // was dropped for WebP specifically (`None` = lossless, unconditionally),
            // so `batch convert --to webp --quality N` silently ignored N and always
            // wrote a large lossless file. Reuse it as the WebP quality too.
            let webp_quality = (self.ext == "webp").then_some(self.quality);
            verbs::convert_to_reporting(input, out, self.quality, webp_quality, self.resize)
                .map_err(|(cause, e)| (cause, e.message()))
        } else {
            thumbnail_reporting(input, &out.to_string_lossy(), self.size).map(|_| ())
        };
        match attempt {
            Ok(()) => verbs::FileOutcome::ok(input, out.clone()),
            Err((cause, detail)) => verbs::FileOutcome::failed(input, Some(cause), detail),
        }
        .timed(started.elapsed())
    }
}

/// A failed encode never got past the reserved placeholder, so clean up any that are still
/// zero bytes (mirrors OutSlot's own drop behavior), so a failed batch item leaves nothing
/// behind, same as before the reservation happened ahead of time. Entries whose name was
/// never claimed at all have nothing to clean.
fn clean_failed_placeholders(
    pairs: &[(String, std::result::Result<PathBuf, String>)],
    report: &verbs::BatchReport,
) {
    for ((_, slot), outcome) in pairs.iter().zip(report.files.iter()) {
        let Ok(out) = slot else { continue };
        if outcome.is_ok() {
            continue;
        }
        let empty = std::fs::metadata(out)
            .map(|m| m.len() == 0)
            .unwrap_or(false);
        if empty {
            let _ = std::fs::remove_file(out);
        }
    }
}

/// Render a finished run. The clean-run text is exactly what it has always been; a run with
/// failures adds one tab-separated `failed` line each, matching the `omitted` lines
/// `pdf`/`cbz` print, so one parser reads every verb here.
///
/// Total failure stays an `Err` in BOTH forms: it must FAIL the command (nonzero exit for
/// scripts/CI/MCP callers), and an `Err` carrying the JSON body would be printed to stderr
/// behind the tool's own `st2k:` prefix, i.e. not parseable as JSON anyway. The failure
/// lines go with it, so even the refusal names every file and cause.
fn batch_report(report: &verbs::BatchReport, json: bool) -> Result<String, String> {
    let (done, total) = (report.succeeded(), report.requested());
    if done == 0 {
        return Err(format!("0/{total} succeeded{}", report.failure_lines()));
    }
    if json {
        return Ok(report.to_json().to_string());
    }
    let offline_note = if report.skipped_offline > 0 {
        format!(
            "\n  skipped  {} cloud placeholder(s), opening these would download them",
            report.skipped_offline
        )
    } else {
        String::new()
    };
    if done < total {
        return Ok(format!(
            "{done}/{total} succeeded ({} failed){offline_note}{}",
            total - done,
            report.failure_lines()
        ));
    }
    Ok(format!("{done}/{total} succeeded{offline_note}"))
}

/// `batch`'s `"info"` op: fan [`info`] (JSON form, so the audio branch is included) across
/// every expanded input via the same `parallel::map` `thumbnail`/`convert` already use, and
/// return one JSON array — a folder of RAW photos or music files becomes ONE call instead
/// of one `info` round-trip per file. A per-file failure becomes an `"error"` field in that
/// file's element rather than failing the whole batch (a single unreadable file must not
/// hide the other 999 results).
fn batch_info(inputs: &[String], recurse: bool) -> Result<String, String> {
    // Cloud placeholders are simply absent from the result array, same as `thumbnail`/`convert`.
    let (files, _skipped_offline, _unresolved) = expand_inputs(inputs, recurse);
    if files.is_empty() {
        return Err("no supported image files found in the inputs".to_string());
    }
    let results = st2k_base::parallel::map(&files, |_, f: &String| -> serde_json::Value {
        match info(f, true) {
            Ok(text) => {
                // `info`'s JSON already excludes the path (it's the CALLER's argument in
                // every other use); splice it in here so each array element is
                // self-describing once results are no longer positionally paired with
                // the request.
                let mut v: serde_json::Value =
                    serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
                if let serde_json::Value::Object(ref mut m) = v {
                    m.insert("input".to_string(), serde_json::Value::String(f.clone()));
                }
                v
            }
            Err(e) => serde_json::json!({ "input": f, "error": e }),
        }
    });
    Ok(serde_json::Value::Array(results).to_string())
}

#[cfg(test)]
mod tests;
