//! The conversion run: read the dialog into jobs, convert the batch off the UI thread, and report what happened.

use super::*;

/// Every `(resize, name tag)` this run should produce per source file.
///
/// One entry normally; three when "write every preset size" is ticked. The tag
/// goes into the output name so the results are self-describing instead of
/// `photo.jpg`, `photo (2).jpg`, `photo (3).jpg`.
pub(super) unsafe fn read_resize_jobs(hwnd: HWND) -> Vec<(Resize, Option<String>)> {
    if checked(hwnd, CID_RESIZE_CHK) && checked(hwnd, CID_RESIZE_ALL) {
        let pad = checked(hwnd, CID_RESIZE_PAD);
        return CV_ALL_SIZES
            .iter()
            .map(|&(w, h)| {
                let r = if pad {
                    Resize::Pad(w, h)
                } else {
                    Resize::Fit(w, h)
                };
                (r, Some(format!("{w}x{h}")))
            })
            .collect();
    }
    vec![(read_resize(hwnd), None)]
}

/// Mirrors `decode::limits::MAX_DIM` (16384): that constant is `pub(crate)` to the
/// core lib, so it isn't reachable from this EXE crate, but the ceiling it
/// enforces is the same one that matters here. Without a cap, a typed dimension
/// like 30000x30000 reaches `apply_resize`'s `FitUp` arm (which only floors with
/// `.max(1)`, no ceiling) and attempts a multi-GB allocation; release runs
/// panic="abort", so an allocation failure aborts the WHOLE process mid-batch.
pub(super) const MAX_TYPED_RESIZE_DIM: u32 = 16_384;

/// Parse one typed resize-dimension field, clamped to [`MAX_TYPED_RESIZE_DIM`].
/// Pulled out of `read_resize` as a plain function (no `HWND`) so the clamp is
/// unit-testable without a live dialog.
pub(super) fn parse_resize_dim(text: &str) -> u32 {
    text.trim()
        .parse::<u32>()
        .unwrap_or(0)
        .min(MAX_TYPED_RESIZE_DIM)
}

/// The verbs-crate `Resize` selected in the dialog (None when unchecked).
pub(super) unsafe fn read_resize(hwnd: HWND) -> Resize {
    if !checked(hwnd, CID_RESIZE_CHK) {
        return Resize::None;
    }
    // Padding turns any fit into an exact canvas; a percentage has no canvas to
    // pad to, so it is left alone.
    let pad = checked(hwnd, CID_RESIZE_PAD);
    let wrap = |w: u32, h: u32, fit: Resize| if pad { Resize::Pad(w, h) } else { fit };
    match CV_RESIZE.get(combo_sel(hwnd, CID_RESIZE)).map(|r| r.1) {
        Some(ResizeMode::Fit(w, h)) => wrap(w, h, Resize::Fit(w, h)),
        Some(ResizeMode::Pct(p)) => Resize::Percent(p),
        _ => {
            // Clamped BEFORE the w>0 && h>0 gate below, not after: a typed value
            // past MAX_TYPED_RESIZE_DIM is out-of-range input, not a request for
            // "as big as possible", so it's capped to the same ceiling decode::
            // uses rather than let through to become a multi-GB allocation
            // attempt (release runs panic="abort", so an alloc failure there kills
            // the whole batch, not just this one file).
            let w = parse_resize_dim(&get_edit_text(hwnd, CID_RESIZE_W));
            let h = parse_resize_dim(&get_edit_text(hwnd, CID_RESIZE_H));
            if w > 0 && h > 0 {
                // Explicitly typed dimensions scale UP too — "make it bigger"
                // must make it bigger. The presets above stay shrink-only.
                wrap(w, h, Resize::FitUp(w, h))
            } else {
                Resize::None
            }
        }
    }
}

/// The dialog's configured output directory, or `None` for "same folder as each
/// image" (the localized placeholder, or the legacy `(`-prefixed form, both mean
/// "unset").
pub(super) unsafe fn resolve_convert_outdir(hwnd: HWND) -> Option<PathBuf> {
    let outdir_text = get_edit_text(hwnd, CID_OUTDIR);
    let is_placeholder = outdir_text.is_empty()
        || outdir_text == t("cv_same_folder")
        || outdir_text.starts_with('(');
    (!is_placeholder).then(|| std::path::PathBuf::from(&outdir_text))
}

/// One (resize, tag) job's output for `f`, dispatched by target kind.
/// `pdf_already_written` suppresses duplicate PDF jobs: the PDF writer takes no
/// resize, so re-running it once per size would emit N identical PDFs under
/// confusing names, so only the first job in a file's list is honored.
///
/// The outer `None` is that suppressed job and ONLY that: an attempt that ran and failed
/// comes back as `Some(Err(reason))`, so the completion report can say why (2026-09-05
/// audit, F11). These calls each return one opaque error rather than a phase, which is why
/// the dialog's records carry a sentence and no machine cause.
/// `watermark`: the same optional image overlay `ConvertOpts::watermark` carries,
/// applied to every target this dispatches to except PDF (the PDF writer goes
/// through `topdf`, a separate pipeline this does not touch).
#[allow(clippy::too_many_arguments)]
pub(super) fn produce_convert_job(
    f: &str,
    tgt: CvTarget,
    dir: &std::path::Path,
    resize: Resize,
    tag: Option<&str>,
    quality: u8,
    png_level: u32,
    webp_quality: Option<u8>,
    pdf_already_written: bool,
    watermark: Option<&Watermark>,
) -> Option<Result<PathBuf, String>> {
    match tgt {
        CvTarget::Native(format, ext) => {
            let opts = ConvertOpts {
                // The dialog supplies WebP quality via `opts.webp_quality`
                // (from its per-format Settings), so the Target stays None.
                target: Target {
                    format,
                    ext,
                    webp_quality: None,
                },
                jpeg_quality: quality,
                png_level,
                webp_quality,
                resize,
                watermark: watermark.cloned(),
            };
            Some(
                sagethumbs2k_core::convert_file_opts_named(f, opts, dir, tag)
                    .map_err(|e| e.message()),
            )
        }
        // One image -> one single-page PDF (reserved name in `dir`). Page geometry
        // is a PDF page-layout setting (Settings > Saving), not a pixel resize.
        CvTarget::Pdf if pdf_already_written => None,
        CvTarget::Pdf => Some(
            sagethumbs2k_core::convert_image_to_pdf_in(f, dir, quality).map_err(|e| e.message()),
        ),
        // Exotic target written by the bundled ImageMagick (reserved name).
        CvTarget::Magick(ext) => {
            // AVIF/JXL honor the quality slider; the lossless exotic targets
            // (PSD/DDS/…) get magick's default (None).
            let q = matches!(ext, "avif" | "jxl")
                .then(|| MAGICK_QUALITY.load(Ordering::Relaxed).clamp(1, 100) as u8);
            Some(
                sagethumbs2k_core::convert_to_magick_in_named(
                    f, dir, ext, resize, q, tag, watermark,
                )
                .map_err(|e| e.message()),
            )
        }
    }
}

/// One source file's whole job list (normally one job; three when "write every
/// preset size" is on). Each source runs its whole size list here rather than the
/// list being flattened into the work items, so one file's outputs stay on one
/// worker and cannot interleave with another file's. Note the decode still
/// happens once per OUTPUT, not once per file - each `convert_file_opts_named`
/// reads and decodes the source itself. Sharing one decode across the sizes would
/// mean holding a full-resolution image while three encodes run, which is the
/// trade this deliberately does not make.
/// Reduces one file's per-job outputs (in job order) into the first produced output
/// (for the "open folder" reveal) and the first REASON a job did not produce one, or
/// `None` when every job succeeded (issue #28: a file used to count as fully converted
/// the moment job 0 succeeded, even when "write every preset size" left jobs 1/2
/// unwritten). `is_pdf` suppresses the duplicate-PDF-job case: `produce_convert_job`
/// intentionally returns `None` for every PDF job after the first (a PDF ignores resize,
/// so re-running it would only emit identical copies), and that intentional `None` must
/// not count as a failure.
///
/// The FIRST reason, not all of them: a file with three failed sizes usually failed them
/// for one reason, and the report gets one entry per file.
///
/// Pure and separately testable on purpose, same reasoning as `failure_report`
/// below: the surrounding `convert_one_file` does real file I/O per job, which no
/// unit test here can drive without a fixture image on disk.
pub(super) fn reduce_job_outputs(
    is_pdf: bool,
    produced: &[Option<Result<PathBuf, String>>],
) -> (Option<PathBuf>, Option<String>) {
    let mut first: Option<PathBuf> = None;
    let mut reason: Option<String> = None;
    for (i, job) in produced.iter().enumerate() {
        fold_job_output(is_pdf, i, job, &mut first, &mut reason);
    }
    // No output at all is a failure even when no single job reported one (an empty job
    // list, or a PDF whose only honored job was suppressed). Before this, `all_ok` was
    // ANDed with `first.is_some()` for exactly the same reason.
    if reason.is_none() && first.is_none() {
        reason = Some(String::new());
    }
    (first, reason)
}

/// Folds ONE job's output into the running first-produced-output / first-reason pair.
fn fold_job_output(
    is_pdf: bool,
    i: usize,
    job: &Option<Result<PathBuf, String>>,
    first: &mut Option<PathBuf>,
    reason: &mut Option<String>,
) {
    let pdf_duplicate = is_pdf && i > 0;
    match job {
        Some(Ok(p)) if first.is_none() => *first = Some(p.clone()),
        Some(Ok(_)) => {}
        Some(Err(e)) if reason.is_none() => *reason = Some(e.clone()),
        Some(Err(_)) => {}
        // Nothing ran. Only the suppressed duplicate PDF job reaches here; anything
        // else would be a job that silently vanished, which is a failure.
        None if !pdf_duplicate && reason.is_none() => *reason = Some(String::new()),
        None => {}
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn convert_one_file(
    f: &str,
    tgt: CvTarget,
    jobs: &[(Resize, Option<String>)],
    quality: u8,
    png_level: u32,
    webp_quality: Option<u8>,
    outdir: &Option<PathBuf>,
    watermark: &Option<Watermark>,
) -> (Option<PathBuf>, Option<String>) {
    // Cancelled mid-run: skip the rest cheaply so the batch winds down fast.
    if CONVERT_CANCEL.load(Ordering::Relaxed) {
        return (None, Some(String::new()));
    }
    let dir = match outdir
        .clone()
        .or_else(|| std::path::Path::new(f).parent().map(|p| p.to_path_buf()))
    {
        Some(d) => d,
        // No reason text for either of these two: a cancel is the user's own act and a
        // path with no parent folder has nothing to tell them. `failure_report` lists a
        // bare path for an empty reason, exactly as it did before there were reasons.
        None => return (None, Some(String::new())),
    };
    let is_pdf = matches!(tgt, CvTarget::Pdf);
    let mut produced_per_job: Vec<Option<Result<PathBuf, String>>> = Vec::with_capacity(jobs.len());
    for (i, (resize, tag)) in jobs.iter().enumerate() {
        let pdf_already_written = is_pdf && i > 0;
        let produced = produce_convert_job(
            f,
            tgt,
            &dir,
            *resize,
            tag.as_deref(),
            quality,
            png_level,
            webp_quality,
            pdf_already_written,
            watermark.as_ref(),
        );
        produced_per_job.push(produced);
    }
    reduce_job_outputs(is_pdf, &produced_per_job)
}

/// The Convert button: a FRESH run over every file the dialog was opened with. Forgets the
/// previous run's output and count so a later "open folder" reveals this run's file, not a
/// stale one, then launches.
pub(super) unsafe fn start_convert(hwnd: HWND) {
    let files = match CONVERT_FILES.get() {
        Some(f) => f.clone(),
        None => return,
    };
    if files.is_empty() {
        return;
    }
    *LAST_OUTPUT.lock().unwrap() = None;
    CONVERTED_SO_FAR.store(0, Ordering::Relaxed);
    launch_batch(hwnd, files);
}

/// Read the dialog options and run the batch conversion over `files` on a worker thread,
/// posting progress back to the window. The one launcher for a fresh run and for a retry
/// of the failures (2026-09-05 audit, E01): the retry reads the same controls this reads,
/// and they cannot have changed in between, because the report that offers the retry is
/// modal over this dialog and hands control straight back here. Capturing the settings
/// into a struct at the first run would say the same thing with a second copy to keep
/// true.
pub(super) unsafe fn launch_batch(hwnd: HWND, files: Vec<String>) {
    let tgt = resolve_cv_target(combo_sel(hwnd, CID_FORMAT));
    let quality = QUALITY.load(Ordering::Relaxed).clamp(1, 100) as u8;
    let png_level = PNG_LEVEL.load(Ordering::Relaxed).clamp(0, 9) as u32;
    let webp_quality = if matches!(tgt, CvTarget::Native(ImageFormat::WebP, _))
        && WEBP_LOSSLESS.load(Ordering::Relaxed) == 0
    {
        Some(WEBP_QUALITY.load(Ordering::Relaxed).clamp(1, 100) as u8)
    } else {
        None
    };
    // Normally one job per file; three when "write every preset size" is on.
    let jobs = read_resize_jobs(hwnd);
    let outdir = resolve_convert_outdir(hwnd);
    // Checked but no image chosen behaves as "no watermark" rather than an error -
    // there is nothing to read yet, so nothing has failed.
    let watermark = checked(hwnd, CID_CV_WATERMARK_CHK)
        .then(|| WATERMARK_PATH.lock().unwrap().clone())
        .filter(|p| !p.is_empty())
        .map(|path| Watermark {
            path,
            corner: CV_WM_CORNERS
                .get(combo_sel(hwnd, CID_CV_WATERMARK_CORNER))
                .map(|(_, c)| *c)
                .unwrap_or(Corner::BottomRight),
            scale_pct: WATERMARK_SCALE.load(Ordering::Relaxed).clamp(1, 100) as u8,
            opacity_pct: WATERMARK_OPACITY.load(Ordering::Relaxed).clamp(0, 100) as u8,
        });

    if let Ok(prog) = GetDlgItem(Some(hwnd), CID_PROGRESS) {
        let _ = ShowWindow(prog, SW_SHOW);
        SendMessageW(
            prog,
            PBM_SETRANGE32,
            Some(WPARAM(0)),
            Some(LPARAM(files.len() as isize)),
        );
        SendMessageW(prog, PBM_SETPOS, Some(WPARAM(0)), None);
    }
    if let Ok(btn) = GetDlgItem(Some(hwnd), IDOK) {
        let _ = EnableWindow(btn, false);
    }

    CONVERT_CANCEL.store(false, Ordering::Relaxed);
    CONVERT_RUNNING.store(true, Ordering::Relaxed);

    let raw = hwnd.0 as usize;
    std::thread::spawn(move || {
        let total = files.len();
        // Convert every file on the batch thread pool (the orchestrator thread blocks
        // here, keeping the UI thread free). Each target's lib fn reserves a
        // collision-free output name internally — race-safe across the parallel
        // workers — and the global magick cap bounds memory for the exotic targets.
        // Progress is posted as each file finishes (from worker threads;
        // `PostMessageW` is thread-safe), keeping the bar live.
        let done = std::sync::atomic::AtomicUsize::new(0);
        // Each entry is (first produced output, why the file is not fully converted;
        // `None` means every requested size/job for it was written, issue #28).
        let outs: Vec<(Option<PathBuf>, Option<String>)> = sagethumbs2k_core::parallel::map_indexed(
            &files,
            0, // auto worker count = available_parallelism
            |_, f| {
                convert_one_file(
                    f,
                    tgt,
                    &jobs,
                    quality,
                    png_level,
                    webp_quality,
                    &outdir,
                    &watermark,
                )
            },
            || {
                let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                let _ = PostMessageW(
                    Some(HWND(raw as *mut c_void)),
                    WM_CONVERT_PROGRESS,
                    WPARAM(n),
                    LPARAM(0),
                );
            },
        );
        let ok = outs.iter().filter(|(_, why)| why.is_none()).count();
        // Name the ones that did NOT fully convert, AND why (issue #34, #28; 2026-09-05
        // audit, F11 for the reason). `map_indexed` returns results in input order, so
        // entry i IS `files[i]`, no plumbing needed to find out which. A file whose first
        // job wrote output but a later preset size did not is listed here too, rather than
        // being silently folded into "N of N converted"; it keeps the output it did write,
        // which is why the record carries both.
        // Skipped when the user cancelled: everything queued behind the cancel failed with
        // no reason too, and listing those as failures would be a lie about the user's own
        // act.
        *FAILED_FILES.lock().unwrap() = if CONVERT_CANCEL.load(Ordering::Relaxed) {
            Vec::new()
        } else {
            files
                .iter()
                .zip(&outs)
                .filter_map(|(f, (out, why))| {
                    why.as_ref().map(|reason| {
                        // No cause token: the dialog's converters each return one opaque
                        // error, so a bucket here would be a guess. See `FileOutcome`.
                        FileOutcome::failed(f, None, reason).produced(out.clone())
                    })
                })
                .collect()
        };
        // Remember the first produced output (ordered results → lowest-index success,
        // matching the old first-in-iteration reveal) so completion can offer it.
        if let Some(first) = outs.into_iter().find_map(|(out, _)| out) {
            *LAST_OUTPUT.lock().unwrap() = Some(first);
        }
        let _ = PostMessageW(
            Some(HWND(raw as *mut c_void)),
            WM_CONVERT_DONE,
            WPARAM(ok),
            LPARAM(total as isize),
        );
    });
}

/// `WM_CONVERT_DONE`: report the summary, offer to open the output folder when at
/// least one file was written, then close.
/// The copyable report the failure window shows (issue #34 for the names, 2026-09-05 audit
/// F11 for the reasons): the same summary line the message box carries, then EVERY failure
/// with its full path and reason.
///
/// Nothing is elided. The message box this replaces listed six names and summarised the
/// rest, because a box cannot scroll and a sixty-line one is unreadable; a scrollable,
/// copyable window has no such limit, and a truncated failure list is the exact problem
/// F11 is about, since the files it hides are the ones nobody can retry.
///
/// Pure and separately testable on purpose: the surrounding function puts up a modal
/// window, which no test can drive. The report window's headless shot renders its canned
/// failures through this too, so the shot cannot drift from the real text.
pub(crate) fn failure_report(summary: &str, failed: &[FileOutcome]) -> String {
    let mut out = format!("{summary}\n\n{}", t("cv_failed_list"));
    for f in failed {
        out.push_str(&format!("\n{}", f.input));
        let reason = f.detail.lines().next().unwrap_or("").trim();
        if !reason.is_empty() {
            out.push_str(&format!("\n    {reason}"));
        }
    }
    out
}

pub(super) unsafe fn on_convert_done(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    CONVERT_RUNNING.store(false, Ordering::Relaxed);
    let ok = wparam.0;
    let counts = t("cv_done")
        .replace("{ok}", &ok.to_string())
        .replace("{total}", &lparam.0.to_string());
    let failed = FAILED_FILES.lock().unwrap().clone();
    // When at least one file was written, offer to open the output folder (Explorer with
    // the first produced file selected). Nothing written → no offer. Counted over the whole
    // retry chain, so a retry that converted nothing still offers the first run's output.
    let converted = CONVERTED_SO_FAR.fetch_add(ok, Ordering::Relaxed) + ok;
    let reveal = LAST_OUTPUT
        .lock()
        .unwrap()
        .clone()
        .filter(|_| converted > 0);
    let action = if failed.is_empty() {
        if report_clean_run(hwnd, &counts, reveal.is_some()) {
            ReportAction::OpenFolder
        } else {
            ReportAction::Close
        }
    } else {
        // Something failed: the report window instead of the box, because a list a user
        // cannot copy out is a list they have to reproduce by hand (2026-09-05 audit, F11).
        // It carries the same summary line plus every failure with its full path and reason,
        // and the failures themselves, for its Retry button (E01).
        crate::convert_report::show_convert_failures(
            hwnd,
            &failure_report(&counts, &failed),
            &failed,
            reveal.is_some(),
        )
    };
    match action {
        // The dialog stays up and runs the failed inputs through the same launcher the
        // Convert button used, reading the same controls; its report comes back through
        // this handler, so a second failure can be retried again.
        ReportAction::Retry(inputs) => {
            launch_batch(hwnd, inputs);
            return LRESULT(0);
        }
        ReportAction::OpenFolder => {
            if let Some(path) = reveal {
                reveal_in_explorer(&path);
            }
        }
        ReportAction::Close => {}
    }
    let _ = DestroyWindow(hwnd);
    LRESULT(0)
}

/// The completion message for a run where every file converted: one line, plus the
/// "Open output folder?" question when there is something to reveal. Unchanged since long
/// before the failure report existed, and deliberately so, a clean run needs one glance.
pub(super) unsafe fn report_clean_run(hwnd: HWND, counts: &str, can_open: bool) -> bool {
    let cap = wide("SageThumbs 2K");
    if !can_open {
        let text = wide(counts);
        MessageBoxW(
            Some(hwnd),
            PCWSTR(text.as_ptr()),
            PCWSTR(cap.as_ptr()),
            MB_OK | MB_ICONINFORMATION,
        );
        return false;
    }
    let text = wide(&format!("{counts}\n\n{}", t("cv_open_folder")));
    let r = MessageBoxW(
        Some(hwnd),
        PCWSTR(text.as_ptr()),
        PCWSTR(cap.as_ptr()),
        MB_YESNO | MB_ICONINFORMATION,
    );
    r == IDYES
}
