//! Encoding through ImageMagick: the output coders, the child and its pipes.

use super::*;

/// (extension, ImageMagick coder name) for every Magick-backed output exposed by
/// the Convert dialog. This is the ONE source of truth for what ImageMagick can
/// write: `output_coder`, `magick_output_supported`, and `magick_output_extensions`
/// all read it, and `src/bin/app/convert.rs`'s `CV_MAGICK_FORMATS` (the dialog's
/// hand-typed labels) is asserted against `magick_output_extensions()` by a test
/// there, so a coder added here without a matching label fails the build's tests.
pub(super) const OUTPUT_CODERS: &[(&str, &str)] = &[
    ("avif", "AVIF"),
    ("jxl", "JXL"),
    ("psd", "PSD"),
    ("dds", "DDS"),
    ("jp2", "JP2"),
    ("pcx", "PCX"),
    ("sgi", "SGI"),
    ("pfm", "PFM"),
    ("dpx", "DPX"),
    ("fits", "FITS"),
    ("xpm", "XPM"),
    ("pict", "PICT"),
    ("ras", "RAS"),
    ("palm", "PALM"),
];

/// Return the explicit ImageMagick coder for a Magick-backed output extension.
/// Never let ImageMagick infer these from a filename: when a module is absent,
/// it can otherwise preserve the input encoding and still exit successfully,
/// producing (for example) PNG bytes in an `.avif` file.
pub(super) fn output_coder(extension: &str) -> Option<&'static str> {
    let ext = extension.to_ascii_lowercase();
    OUTPUT_CODERS
        .iter()
        .find(|(candidate, _)| *candidate == ext)
        .map(|(_, coder)| *coder)
}

/// Every extension ImageMagick has a tested output coder for. The Convert
/// dialog's `CV_MAGICK_FORMATS` must carry exactly this set (a unit test there
/// checks both directions).
pub fn magick_output_extensions() -> &'static [&'static str] {
    static EXTENSIONS: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    EXTENSIONS
        .get_or_init(|| OUTPUT_CODERS.iter().map(|(ext, _)| *ext).collect())
        .as_slice()
}

/// Whether `extension` has an explicit, tested ImageMagick output coder.
///
/// Keep every caller routed through this predicate instead of duplicating the
/// writer list. An extension merely being decodable does not mean either
/// `image` or ImageMagick can safely encode it.
pub fn magick_output_supported(extension: &str) -> bool {
    let ext = extension.to_ascii_lowercase();
    magick_output_extensions()
        .iter()
        .any(|candidate| *candidate == ext)
}

/// What the [`encode_via_magick`] watchdog loop should do after one process poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EncodeWait {
    Continue,
    TimedOut,
    CpuExceeded,
}

/// Pure decision core of the encode watchdog loop: has the child exceeded its CPU
/// budget, or only the wall-clock deadline? Split out of the loop so the CPU branch —
/// the budget the decode path already enforces via `await_magick_output`, which the
/// encode path used to lack entirely — is directly testable without spawning and
/// starving a real magick process.
pub(super) fn encode_wait_decision(
    cpu: Option<Duration>,
    cpu_budget: Duration,
    now: std::time::Instant,
    deadline: std::time::Instant,
) -> EncodeWait {
    if cpu.is_some_and(|c| c > cpu_budget) {
        EncodeWait::CpuExceeded
    } else if now >= deadline {
        EncodeWait::TimedOut
    } else {
        EncodeWait::Continue
    }
}

/// ImageMagick opens the Convert dialog's output path (`verbs/encode/slots.rs` derives it
/// from the source name plus a suffix) as a raw, unprefixed path — no `\\?\` long-path
/// prefix — so a name past Windows' legacy limits is silently truncated or refused by the
/// OS rather than by us. Same two ceilings NTFS itself enforces without the prefix: a
/// 255 UTF-16-unit file-name component, and a ~32000 UTF-16-unit full path.
pub(super) const MAGICK_TARGET_NAME_MAX_UTF16: usize = 255;

pub(super) const MAGICK_TARGET_PATH_MAX_UTF16: usize = 32_000;

/// Is `out` short enough for ImageMagick's raw, unprefixed `coder:path` spec to reach
/// reliably? Checked before spawning so an oversized name fails as a distinct, logged
/// error instead of a silent truncation or a bare "encode failed".
pub(super) fn encode_target_length_ok(out: &std::path::Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    let name_len = out
        .file_name()
        .map(|n| n.encode_wide().count())
        .unwrap_or(0);
    let path_len = out.as_os_str().encode_wide().count();
    name_len <= MAGICK_TARGET_NAME_MAX_UTF16 && path_len <= MAGICK_TARGET_PATH_MAX_UTF16
}

/// Resolve the magick executable and the `coder:path` output target for `target_ext`.
/// Self-defend: this is the single chokepoint for the magick-backed Convert targets,
/// so gate the capability here rather than trusting every caller to pre-check
/// `magick_available()`. A distinct, logged error keeps "magick missing" diagnosable
/// instead of looking like a genuine encode failure (bare E_FAIL).
pub(super) fn magick_encode_target(
    target_ext: &str,
    out: &std::path::Path,
) -> Result<(&'static PathBuf, String)> {
    let Some(exe) = magick_exe() else {
        crate::safety::log_debug("encode_via_magick: ImageMagick not available for this target");
        return Err(Error::from(E_FAIL));
    };
    let coder = output_coder(target_ext).ok_or_else(|| {
        crate::safety::log_debug("encode_via_magick: unsupported output extension");
        Error::from(E_FAIL)
    })?;
    if !encode_target_length_ok(out) {
        crate::safety::log_debugf!(
            "encode_via_magick: target path too long for magick's raw, unprefixed coder \
             spec: {}",
            out.display()
        );
        return Err(Error::from(E_FAIL));
    }
    let out_str = out.to_str().ok_or_else(|| Error::from(E_FAIL))?;
    Ok((exe, format!("{coder}:{out_str}")))
}

/// `png:-` (our own re-encode on stdin) → an EXPLICIT coder + target path. The prefix
/// is load-bearing: without it, a missing output module can make ImageMagick silently
/// preserve the PNG input while naming it `.avif`, `.jxl`, etc. When a quality is given
/// (lossy AVIF/JXL), pass it through as `-quality N`; lossless targets use ImageMagick's
/// default.
pub(super) fn magick_encode_args(output_spec: String, quality: Option<u8>) -> Vec<String> {
    let mut args: Vec<String> = vec!["png:-".to_string()];
    if let Some(q) = quality {
        args.push("-quality".to_string());
        args.push(q.clamp(1, 100).to_string());
    }
    args.push(output_spec);
    args
}

/// Spawn ImageMagick with `args`, bound by the shared magick concurrency gate (memory)
/// across in-process + st2k fan-out. The returned permit must be held by the caller
/// until the child has been waited on.
pub(super) fn spawn_magick_child(
    exe: &std::path::Path,
    args: &[String],
) -> Result<(std::process::Child, Option<magick_gate::Permit>)> {
    let mut cmd = Command::new(exe);
    // The ENCODE path's own watchdog is `FULL_FIDELITY_MAGICK_TIMEOUT` (see
    // `wait_for_magick_child`), so the child's self-limit is derived from the same figure.
    add_magick_limits(&mut cmd, FULL_FIDELITY_MAGICK_TIMEOUT);
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW);
    apply_magick_environment(&mut cmd, exe);
    // The ENCODE path writes what the user asked for, so it queues like any other
    // full-fidelity worker rather than slipping past the cap after five seconds.
    let permit = magick_gate::acquire_for(Fidelity::Full);
    let child = cmd.spawn().map_err(|_| Error::from(E_FAIL))?;
    Ok((child, permit))
}

/// Wire up the child's stdin/stdout/stderr pipes: a writer thread feeds `png` in (drop
/// closes the pipe so magick sees EOF), a reader thread drains stdout and signals `rx`
/// when done (magick writes to the FILE, not stdout, so this only exists to observe that
/// EOF; the bytes are never used, but draining through the same capped helper stderr
/// uses below avoids an unbounded read), and an optional stderr-drain thread captures
/// diagnostics for a failure log.
pub(super) type MagickEncodePipes = (
    std::thread::JoinHandle<()>,
    std::thread::JoinHandle<()>,
    std::sync::mpsc::Receiver<()>,
    Option<std::thread::JoinHandle<Vec<u8>>>,
);

pub(super) fn pipe_magick_encode(
    child: &mut std::process::Child,
    png: Vec<u8>,
) -> Result<MagickEncodePipes> {
    use std::io::Write;

    let mut stdin = child.stdin.take().ok_or_else(|| Error::from(E_FAIL))?;
    let Some(writer) = crate::safety::try_spawn("st2k-magick-stdin", move || {
        let _ = stdin.write_all(&png); // drop closes the pipe → magick sees EOF
    }) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(Error::from(E_FAIL));
    };

    let stdout = child.stdout.take().ok_or_else(|| Error::from(E_FAIL))?;
    let (tx, rx) = std::sync::mpsc::channel();
    let Some(reader) = crate::safety::try_spawn("st2k-magick-stdout", move || {
        let _ = drain_capped(stdout);
        let _ = tx.send(());
    }) else {
        let _ = child.kill();
        let _ = writer.join();
        let _ = child.wait();
        return Err(Error::from(E_FAIL));
    };

    // Drain stderr (capped) so we can log it on failure and it can't stall magick.
    let stderr = child.stderr.take();
    let errdrain = stderr
        .and_then(|s| crate::safety::try_spawn("st2k-magick-stderr", move || drain_capped(s)));

    Ok((writer, reader, rx, errdrain))
}

/// Poll the child through the wall-clock deadline, escalating on either a CPU-budget or
/// wall-clock timeout. EOF on stdout (`rx`) normally means the process is about to
/// exit, but it is not proof: a hostile/broken child can close stdout early, stop
/// reading stdin, and stay alive, so this keeps polling the real process on the SAME
/// deadline instead of trusting the `rx` signal alone.
pub(super) fn wait_for_magick_child(
    child: &mut std::process::Child,
    rx: std::sync::mpsc::Receiver<()>,
) -> (bool, bool, bool, Option<std::process::ExitStatus>) {
    use std::sync::mpsc::RecvTimeoutError;
    let deadline = std::time::Instant::now() + FULL_FIDELITY_MAGICK_TIMEOUT;
    let mut timed_out = false;
    let mut cpu_exceeded = false;
    let mut wait_failed = false;
    let mut status = None;
    // Poll in WATCHDOG_SLICE steps from the very first iteration, exactly like the decode
    // path's `await_magick_output`. The previous shape blocked on ONE `recv_timeout` for the
    // full wall ceiling before ever entering the CPU-check loop, so a child that spins on a
    // malformed re-encode input without closing stdout was bounded only by the 120 s wall
    // clock, never by the much tighter CPU budget this watchdog exists to enforce.
    let mut stdout_closed = false;

    while !timed_out && !cpu_exceeded && status.is_none() {
        if !stdout_closed {
            match rx.recv_timeout(WATCHDOG_SLICE) {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => stdout_closed = true,
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
        match child.try_wait() {
            Ok(Some(value)) => status = Some(value),
            Ok(None) => {
                let now = std::time::Instant::now();
                match encode_wait_decision(
                    child_cpu_time(child),
                    FULL_FIDELITY_MAGICK_CPU_BUDGET,
                    now,
                    deadline,
                ) {
                    EncodeWait::CpuExceeded => cpu_exceeded = true,
                    EncodeWait::TimedOut => timed_out = true,
                    EncodeWait::Continue => {
                        // While stdout is still open the `recv_timeout` above already paced this
                        // loop; once it has closed, pace it here instead of spinning.
                        if stdout_closed {
                            std::thread::sleep(
                                std::time::Duration::from_millis(10).min(deadline - now),
                            );
                        }
                    }
                }
            }
            Err(_) => wait_failed = true,
        }
        if wait_failed {
            break;
        }
    }
    if timed_out || cpu_exceeded || wait_failed {
        let _ = child.kill();
    }
    if status.is_none() {
        status = child.wait().ok();
    }
    (timed_out, cpu_exceeded, wait_failed, status)
}

/// Interpret the wait outcome into the final `Result`, logging and removing any partial
/// output file on every failure path. A partial file or an unavailable coder must never
/// be reported as a successful convert; requiring an observed clean exit complements
/// the explicit coder prefix `magick_encode_target` built.
pub(super) fn finish_magick_encode(
    out: &std::path::Path,
    timed_out: bool,
    cpu_exceeded: bool,
    wait_failed: bool,
    status: Option<std::process::ExitStatus>,
    err: &[u8],
) -> Result<()> {
    if timed_out || cpu_exceeded {
        log_magick_failure(
            if cpu_exceeded {
                "encode exceeded its CPU budget"
            } else {
                "encode timed out"
            },
            status,
            err,
        );
        let _ = std::fs::remove_file(out);
        return Err(Error::from(E_FAIL));
    }
    if wait_failed {
        log_magick_failure("could not observe encode process", status, err);
        let _ = std::fs::remove_file(out);
        return Err(Error::from(E_FAIL));
    }
    let wrote = std::fs::metadata(out).map(|m| m.len() > 0).unwrap_or(false);
    let clean_exit = status.is_some_and(|value| value.success());
    if wrote && clean_exit {
        Ok(())
    } else {
        log_magick_failure(
            if wrote {
                "encode did not exit successfully (partial output)"
            } else {
                "encode produced no file"
            },
            status,
            err,
        );
        let _ = std::fs::remove_file(out);
        Err(Error::from(E_FAIL))
    }
}

/// PNG-encode `$img` into a fresh byte buffer, mapping an encoder failure through
/// `$map_err` so each caller keeps its own error text. `#[macro_export]` rather than a
/// `fn` because the two magick encode call sites (this module and the `verbs::encode::
/// magickpath` path) sit behind private modules that cannot name each other's items.
#[macro_export]
macro_rules! magick_png_bytes {
    ($img:expr, $map_err:expr) => {{
        let mut png = Vec::new();
        $img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .map_err($map_err)?;
        png
    }};
}

/// ENCODE `img` to `out` via ImageMagick using the explicit `target_ext` coder.
/// We feed magick a PNG on stdin and let it write the exotic target
/// (PSD/DDS/JP2/…) to the file — so OUR decode pipeline handles every input
/// format and magick is only the output coder. Same isolation as the decode
/// path: child process, `-limit`s, and an external kill-timeout. None of our
/// inputs reach magick's parsers (only our own re-encoded PNG does).
pub fn encode_via_magick(
    img: &DynamicImage,
    out: &std::path::Path,
    target_ext: &str,
    quality: Option<u8>,
) -> Result<()> {
    let png = crate::magick_png_bytes!(img, |_| Error::from(E_FAIL));
    encode_via_magick_png(png, out, target_ext, quality)
}

/// Same as [`encode_via_magick`], but takes PNG bytes the caller already built
/// instead of re-encoding `img` here with no metadata of its own. Callers that
/// need EXIF/XMP/ICC to survive an exotic magick-only target graft the carried
/// chunks onto the PNG bytes first (see `verbs::encode::carry`). ImageMagick
/// reads `eXIf`/`iTXt`-XMP/`iCCP` off the PNG it receives on stdin and
/// propagates that metadata into whatever it writes, for the formats that can
/// hold it.
pub fn encode_via_magick_png(
    png: Vec<u8>,
    out: &std::path::Path,
    target_ext: &str,
    quality: Option<u8>,
) -> Result<()> {
    let (exe, output_spec) = magick_encode_target(target_ext, out)?;
    let args = magick_encode_args(output_spec, quality);

    // Bound concurrent magick children (memory) across in-process + st2k fan-out.
    let (mut child, _permit) = spawn_magick_child(exe, &args)?;
    let (writer, reader, rx, errdrain) = pipe_magick_encode(&mut child, png)?;

    // Never join the writer until the child has exited or been killed, or a full
    // stdin pipe can hang us forever.
    let (timed_out, cpu_exceeded, wait_failed, status) = wait_for_magick_child(&mut child, rx);
    let _ = writer.join();
    let _ = reader.join();
    let err = errdrain.and_then(|h| h.join().ok()).unwrap_or_default();

    finish_magick_encode(out, timed_out, cpu_exceeded, wait_failed, status, &err)
}
