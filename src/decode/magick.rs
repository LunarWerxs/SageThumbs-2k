//! ImageMagick discovery, policy, process isolation, decode, and encode support.

use super::*;

/// Locate `magick.exe` once: bundled next to our DLL (preferred for a packaged
/// install), then any `C:\Program Files[ (x86)]\ImageMagick*`, else rely on PATH.
/// Cached — the filesystem probe runs at most once per process.
fn magick_exe() -> Option<&'static PathBuf> {
    static EXE: OnceLock<Option<PathBuf>> = OnceLock::new();
    EXE.get_or_init(find_magick).as_ref()
}

fn find_magick() -> Option<PathBuf> {
    // Test/diagnostic escape hatch: `ST2K_NO_MAGICK=1` makes this process behave
    // like the compact (no-ImageMagick) install even on a machine that has magick
    // bundled or in Program Files — so the regression harness can measure exactly
    // which formats depend on the magick tier without uninstalling anything.
    if std::env::var_os("ST2K_NO_MAGICK").is_some_and(|v| v == "1") {
        return None;
    }
    if let Ok(dll) = crate::module_path() {
        if let Some(dir) = std::path::Path::new(&dll).parent() {
            let p = dir.join("magick.exe");
            if p.exists() {
                return Some(p);
            }
        }
    }
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Ok(base) = std::env::var(var) {
            if let Ok(entries) = std::fs::read_dir(&base) {
                for e in entries.flatten() {
                    if e.file_name().to_string_lossy().starts_with("ImageMagick") {
                        let p = e.path().join("magick.exe");
                        if p.exists() {
                            return Some(p);
                        }
                    }
                }
            }
        }
    }
    // Deliberately NO bare-"magick.exe" PATH fallback: Windows' CreateProcess
    // search order includes the current directory, so a bare name could run a
    // malicious magick.exe planted in a browsed folder. We only ever launch an
    // absolute path (bundled or Program Files); if none is found the tier is
    // simply skipped and the obscure format falls back to its default icon.
    None
}

/// Point ImageMagick at OUR hardened `policy.xml` via `MAGICK_CONFIGURE_PATH`, so
/// the policy applies even when `find_magick` falls back to a *system* ImageMagick
/// (whose own `policy.xml` is permissive — without this, every hostile-input block
/// in our policy is silently inert on such machines). No-op when our `policy.xml`
/// isn't next to the DLL (e.g. a build tree): magick then uses whatever policy sits
/// beside it, which for the bundled installer is already our hardened copy.
fn apply_magick_policy(cmd: &mut Command) {
    static DIR: OnceLock<Option<std::ffi::OsString>> = OnceLock::new();
    let dir = DIR.get_or_init(|| {
        let dll = crate::module_path().ok()?;
        let parent = std::path::Path::new(&dll).parent()?;
        parent
            .join("policy.xml")
            .exists()
            .then(|| parent.as_os_str().to_os_string())
    });
    if let Some(dir) = dir {
        cmd.env("MAGICK_CONFIGURE_PATH", dir);
    }
}

/// Apply our shared ImageMagick resource caps (memory / map / time) to `cmd`. One
/// place so the decode and encode subprocess paths can't drift, and so the values
/// stay tied to [`limits`] (and, via the tests, to `policy.xml`).
pub(super) fn add_magick_limits(cmd: &mut Command) {
    cmd.args([
        "-limit",
        "memory",
        limits::MAGICK_MEMORY_LIMIT,
        "-limit",
        "map",
        limits::MAGICK_MAP_LIMIT,
        "-limit",
        "time",
        limits::MAGICK_TIME_LIMIT,
    ]);
}

/// Decode via the ImageMagick CLI as an isolated child process: write the image
/// bytes to its stdin, read a PNG back from its stdout, decode that PNG with the
/// safe `image` tier. Bounded by ImageMagick's own `-limit`s AND an external
/// kill-timeout so a hostile/looping input can't hang or crash our host.
pub(super) fn decode_via_magick(bytes: &[u8]) -> Result<DynamicImage> {
    // Metafiles get the tight METAFILE_TIMEOUT — a slow vector WMF would otherwise
    // grind ~5 s to a near-blank frame; everything else keeps the full 20 s budget for
    // heavy raster decodes.
    let is_meta = looks_like_metafile(bytes);
    let timeout = if is_meta {
        METAFILE_TIMEOUT
    } else {
        MAGICK_TIMEOUT
    };
    // DICOM files carry a TIFF-compatible 128-byte preamble that tricks magick's
    // content-sniffer into treating them as TIFF (which then fails).  Pass an
    // explicit `dcm:-` format specifier so magick invokes its DICOM coder instead.
    // CT/MR pixel data also occupies a narrow band of the 16-bit range (the real
    // contrast lives in the DICOM window/level, which magick does NOT apply), so
    // a raw linear map collapses to a near-uniform gray — `-auto-level` stretches
    // it back to the full range for a legible thumbnail. Default `-auto-level`
    // scales all channels by ONE global min/max (NOT per-channel — that needs
    // `+channel`), so it's hue-preserving: verified on real RGB DICOM to keep
    // colours exact, so it stays unconditional here (no MONOCHROME-vs-RGB gating).
    let (input, pre_ops): (&str, &[&str]) = if looks_like_dicom(bytes) {
        ("dcm:-", &["-auto-level"])
    } else {
        ("-", &[])
    };
    // A small EMF (icon-sized clip art) would rasterize at its tiny intrinsic size — a right-click
    // Convert then yielded a ~64px image, the same bug SVG had. Render it UP to a usable size by
    // passing `-density` (which must precede the input). Crisp, since it's a vector; only small EMFs
    // are bumped (large ones + WMF are left untouched — see `metafile_min_density`).
    let density = is_meta.then(|| metafile_min_density(bytes)).flatten();
    let density_str = density.map(|d| d.to_string());
    let pre_input: Vec<&str> = match density_str.as_deref() {
        Some(d) => vec!["-density", d],
        None => Vec::new(),
    };
    decode_via_magick_spec(bytes, &pre_input, input, pre_ops, MAGICK_MAX_EDGE, timeout)
}

/// The `-density` (DPI) that renders an EMF's LONG edge up to [`METAFILE_MIN_PX`] when its natural
/// (96-DPI) rasterization would be smaller — so a tiny clip-art EMF converts to a usable, crisp
/// image instead of ~64px. Returns None (magick's default density) when it's already big enough or
/// the frame is unreadable.
///
/// **EMF only, by design.** EMF's `ENHMETAHEADER.rclFrame` is authoritative — magick rasterizes
/// from it consistently, so the computed density matches the render. A *placeable WMF*'s header
/// bbox+`Inch` is NOT guaranteed to match the metafile body's own logical extents, so a
/// mismatched/hostile WMF header would make this compute a density that magick's WMF reader can't
/// honour (turning a file that decoded fine into a hard failure — caught in pre-1.0.1 review). WMF
/// is therefore left at its intrinsic size. The result is also capped ([`METAFILE_MAX_DENSITY`]) so
/// even an implausibly tiny declared EMF frame can't ask magick to build a canvas it chokes on.
pub(super) fn metafile_min_density(b: &[u8]) -> Option<u32> {
    const METAFILE_MIN_PX: f64 = 512.0;
    const DEFAULT_DPI: f64 = 96.0;
    const METAFILE_MAX_DENSITY: u32 = 1200;
    if !(b.len() >= 44 && b[0..4] == [0x01, 0x00, 0x00, 0x00] && &b[40..44] == b" EMF") {
        return None; // not an EMF (placeable/memory WMF → intrinsic size, see doc above)
    }
    // rclFrame (4x i32, units of 0.01 mm; 2540 per inch) at offset 24.
    let i32_at = |o: usize| -> Option<f64> {
        Some(i32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?) as f64)
    };
    let w = (i32_at(32)? - i32_at(24)?).abs(); // right - left
    let h = (i32_at(36)? - i32_at(28)?).abs(); // bottom - top
    let long_inches = w.max(h) / 2540.0;
    if !long_inches.is_finite()
        || long_inches <= 0.0
        || long_inches * DEFAULT_DPI >= METAFILE_MIN_PX
    {
        return None; // unreadable, or already large enough at the default density
    }
    Some(((METAFILE_MIN_PX / long_inches).ceil() as u32).min(METAFILE_MAX_DENSITY))
}

/// Is this a Windows metafile (placeable/memory WMF, or EMF)? Picks the shorter
/// [`METAFILE_TIMEOUT`] for the magick tier here, and is the single home for the
/// metafile magic bytes — `container::looks_like_raster` also calls it so the
/// signatures live in exactly one place.
pub(crate) fn looks_like_metafile(b: &[u8]) -> bool {
    b.starts_with(&[0xD7, 0xCD, 0xC6, 0x9A])                    // placeable WMF
        || b.starts_with(&[0x01, 0x00, 0x09, 0x00, 0x00, 0x03]) // memory WMF METAHEADER
        || (b.len() >= 44 && b[0..4] == [0x01, 0x00, 0x00, 0x00] && &b[40..44] == b" EMF")
    // EMF
}

/// DICOM files carry a 128-byte preamble (often zero-filled) followed by the
/// magic "DICM" at offset 128.  The preamble is TIFF-compatible ("II*\0" at
/// offset 0 in many real-world samples including pydicom's CT_small.dcm and
/// MR_small.dcm), so ImageMagick's content-sniffer misidentifies them as TIFF
/// and fails ("Can not read TIFF directory count").  The explicit `dcm:-`
/// format hint in [`decode_via_magick`] routes them to the DICOM coder instead.
fn looks_like_dicom(b: &[u8]) -> bool {
    b.len() > 132 && &b[128..132] == b"DICM"
}

/// The PSD/PSB composite at full resolution. Frame `[0]` of a PSD in ImageMagick
/// is the flattened composite (the file format's mandatory precomposed image-data
/// section), not a layer. Capped at MAX_DIM (bomb guard, shrink-only `>`) instead
/// of the thumbnail tier's 4096 — the whole point is keeping the real pixels.
///
/// The re-decode of magick's PNG runs with [`limits::PSD_COMPOSITE_MAX_ALLOC`]
/// (not the default 512 MiB): the resize cap is MAX_DIM, so a near-square
/// composite at ~16384² needs ~1 GiB and would otherwise be silently rejected by
/// the `image` tier — making a >~134 MP PSD fall back to its 160px baked-in
/// thumbnail. This PNG is OUR OWN re-encode (its dimensions are already bounded
/// by the resize spec), so the wider allocation is safe here.
pub(super) fn decode_psd_composite(bytes: &[u8]) -> Result<DynamicImage> {
    decode_via_magick_spec_alloc(
        bytes,
        &[],
        "-[0]",
        &[],
        limits::PSD_COMPOSITE_EDGE,
        limits::PSD_COMPOSITE_MAX_ALLOC,
        MAGICK_TIMEOUT,
    )
}

/// Shared ImageMagick child-process decode: `input` is the stdin spec (`-` for
/// "all frames", `-[0]` for the first), `pre_ops` are per-format operators
/// inserted right after the input (e.g. `-auto-level` for DICOM), `max_edge` the
/// `-resize` cap. The PNG magick returns is re-decoded under the default
/// [`limits::MAX_ALLOC`] budget.
fn decode_via_magick_spec(
    bytes: &[u8],
    pre_input: &[&str],
    input: &str,
    pre_ops: &[&str],
    max_edge: &str,
    timeout: Duration,
) -> Result<DynamicImage> {
    decode_via_magick_spec_alloc(
        bytes, pre_input, input, pre_ops, max_edge, MAX_ALLOC, timeout,
    )
}

/// As [`decode_via_magick_spec`], but with an explicit re-decode allocation
/// budget — used by the PSD composite path, whose larger resize cap needs a
/// matching `max_alloc` (see [`decode_psd_composite`]).
fn decode_via_magick_spec_alloc(
    bytes: &[u8],
    pre_input: &[&str],
    input: &str,
    pre_ops: &[&str],
    max_edge: &str,
    max_alloc: u64,
    timeout: Duration,
) -> Result<DynamicImage> {
    let exe = magick_exe().ok_or_else(|| Error::from(E_FAIL))?;
    let mut cmd = Command::new(exe);
    add_magick_limits(&mut cmd);
    let mut args: Vec<&str> = Vec::with_capacity(6 + pre_input.len() + pre_ops.len());
    // Pre-INPUT settings (e.g. `-density` for a small vector metafile) must precede the input so
    // they affect how it is rasterized — unlike `pre_ops`, which operate on the loaded image.
    args.extend_from_slice(pre_input);
    args.push(input); // read the image from stdin (format auto-detected)
                      // Per-format pre-processing operators (e.g. `-auto-level` for DICOM's narrow
                      // window/level range) run before -strip/-resize.
    args.extend_from_slice(pre_ops);
    args.extend_from_slice(&[
        // NO `-auto-orient`: `apply_exif_orientation` in `decode_image` is the
        // single rotation authority across all tiers. `-strip` already drops the
        // EXIF tags, so letting magick auto-orient too would double-rotate (it
        // rotates pixels, then we rotate again from the tags we read separately).
        "-strip", "-resize", max_edge, "PNG:-", // write a PNG to stdout
    ]);
    cmd.args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW);
    apply_magick_policy(&mut cmd);
    // Bound concurrent magick children (memory) across in-process + st2k fan-out.
    // Held until this function returns (after the child is reaped).
    let _permit = magick_gate::acquire();
    let mut child = cmd.spawn().map_err(|_| Error::from(E_FAIL))?;

    // Feed stdin on its own thread so a full stdout pipe can't deadlock us.
    let mut stdin = child.stdin.take().ok_or_else(|| Error::from(E_FAIL))?;
    let input = bytes.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
        // drop(stdin) here closes the pipe so ImageMagick sees EOF
    });

    // Read stdout on its own thread; the main thread enforces the timeout.
    let mut stdout = child.stdout.take().ok_or_else(|| Error::from(E_FAIL))?;
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });

    // Drain stderr on its own thread too (capped) so a chatty/failing magick
    // can't fill the pipe and stall, and so we have its diagnostics on failure.
    let stderr = child.stderr.take();
    let errdrain = stderr.map(|s| std::thread::spawn(move || drain_capped(s)));

    let png = match rx.recv_timeout(timeout) {
        Ok(buf) => buf,
        Err(_) => {
            // Hung past the deadline: kill, drain the threads, reap, fail.
            let _ = child.kill();
            let _ = writer.join();
            let _ = reader.join();
            let err = errdrain.and_then(|h| h.join().ok()).unwrap_or_default();
            let status = child.wait().ok();
            log_magick_failure("decode timed out", status, &err);
            return Err(Error::from(E_FAIL));
        }
    };
    // We have the output. Kill unconditionally so a child that closed stdout but
    // is still hung (e.g. not draining stdin, leaving the writer's write_all
    // blocked on a full pipe) can't deadlock writer.join()/wait() forever — the
    // whole reason the external timeout exists. kill() is a harmless no-op if it
    // already exited.
    let _ = child.kill();
    let _ = writer.join();
    let _ = reader.join();
    let err = errdrain.and_then(|h| h.join().ok()).unwrap_or_default();
    let status = child.wait().ok();
    if png.is_empty() {
        log_magick_failure("decode produced no output", status, &err);
        return Err(Error::from(E_FAIL));
    }
    // Validate by decoding rather than by exit status (which is unreliable now —
    // we may have killed a child that had already produced a complete PNG).
    // image::Limits bound this safe-tier decode.
    decode_with_image_alloc(&png, max_alloc)
}

/// Read a child pipe to EOF but keep at most ~4 KiB so a flood of magick warnings
/// can't balloon our memory; the captured head is plenty to diagnose a failure.
fn drain_capped<R: Read>(mut r: R) -> Vec<u8> {
    const CAP: usize = 4 * 1024;
    let mut out = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match r.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if out.len() < CAP {
                    let take = n.min(CAP - out.len());
                    out.extend_from_slice(&chunk[..take]);
                }
                // keep reading to EOF (drains the pipe) even once capped
            }
        }
    }
    out
}

/// Log a magick child-process failure: the captured (capped) stderr plus the
/// exit status, via `log_debug` so it's silent unless Debug is on.
fn log_magick_failure(what: &str, status: Option<std::process::ExitStatus>, stderr: &[u8]) {
    let err = String::from_utf8_lossy(stderr);
    let err = err.trim();
    crate::safety::log_debug(&format!(
        "magick {what} (status {status:?}): {}",
        if err.is_empty() { "<no stderr>" } else { err }
    ));
}

/// Is the bundled (or system) ImageMagick available? Gates the magick-backed
/// Convert targets in the dialog — they're hidden on a compact install.
pub fn magick_available() -> bool {
    magick_exe().is_some()
}

/// ENCODE `img` to `out` via ImageMagick (the output format is taken from `out`'s
/// extension). We feed magick a PNG on stdin and let it write the exotic target
/// (PSD/DDS/JP2/…) to the file — so OUR decode pipeline handles every input
/// format and magick is only the output coder. Same isolation as the decode
/// path: child process, `-limit`s, and an external kill-timeout. None of our
/// inputs reach magick's parsers (only our own re-encoded PNG does).
pub fn encode_via_magick(
    img: &DynamicImage,
    out: &std::path::Path,
    quality: Option<u8>,
) -> Result<()> {
    use std::io::{Read, Write};

    // Self-defend: this is the single chokepoint for the magick-backed Convert
    // targets, so gate the capability here rather than trusting every caller to
    // pre-check magick_available(). A distinct, logged error keeps "magick missing"
    // diagnosable instead of looking like a genuine encode failure (bare E_FAIL).
    let Some(exe) = magick_exe() else {
        crate::safety::log_debug("encode_via_magick: ImageMagick not available for this target");
        return Err(Error::from(E_FAIL));
    };
    let out_str = out.to_str().ok_or_else(|| Error::from(E_FAIL))?;

    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|_| Error::from(E_FAIL))?;

    let mut cmd = Command::new(exe);
    add_magick_limits(&mut cmd);
    // `png:-` (our own re-encode on stdin) → the target file (format inferred from the
    // extension). When a quality is given (lossy magick targets like AVIF/JXL), pass it
    // through as `-quality N`; lossless targets (PSD/DDS/…) pass `None` and get magick's
    // default. Owned Strings so the optional `-quality N` slots in without lifetime games.
    let mut args: Vec<String> = vec!["png:-".to_string()];
    if let Some(q) = quality {
        args.push("-quality".to_string());
        args.push(q.clamp(1, 100).to_string());
    }
    args.push(out_str.to_string());
    cmd.args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW);
    apply_magick_policy(&mut cmd);
    // Bound concurrent magick children (memory) across in-process + st2k fan-out.
    let _permit = magick_gate::acquire();
    let mut child = cmd.spawn().map_err(|_| Error::from(E_FAIL))?;

    let mut stdin = child.stdin.take().ok_or_else(|| Error::from(E_FAIL))?;
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&png); // drop closes the pipe → magick sees EOF
    });

    // magick writes to the FILE, not stdout — so stdout closes when it exits.
    // Reading it to EOF on a thread + recv_timeout enforces the same kill-deadline
    // the decode path uses.
    let mut stdout = child.stdout.take().ok_or_else(|| Error::from(E_FAIL))?;
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut sink = Vec::new();
        let _ = stdout.read_to_end(&mut sink);
        let _ = tx.send(());
    });

    // Drain stderr (capped) so we can log it on failure and it can't stall magick.
    let stderr = child.stderr.take();
    let errdrain = stderr.map(|s| std::thread::spawn(move || drain_capped(s)));

    let timed_out = rx.recv_timeout(MAGICK_TIMEOUT).is_err();
    // Capture magick's REAL exit status BEFORE the kill() safety net, if it has
    // already exited — the common case, since it closes stdout (our EOF signal) as
    // it exits after writing the file. If it hasn't exited yet, `try_wait` is None
    // and we keep the original output-file heuristic: we can't block on wait() here
    // because kill() must run before writer.join() to avoid a stdin-pipe deadlock.
    let exited = if timed_out {
        None
    } else {
        child.try_wait().ok().flatten()
    };
    let _ = child.kill();
    let _ = writer.join();
    let _ = reader.join();
    let err = errdrain.and_then(|h| h.join().ok()).unwrap_or_default();
    let status = exited.or_else(|| child.wait().ok());

    if timed_out {
        log_magick_failure("encode timed out", status, &err);
        let _ = std::fs::remove_file(out);
        return Err(Error::from(E_FAIL));
    }
    let wrote = std::fs::metadata(out).map(|m| m.len() > 0).unwrap_or(false);
    // Unlike the decode path, there is NO re-decode safety net here (magick writes
    // exotic PSD/DDS/JP2 we can't cheaply read back), so a magick that errored but
    // left a partial file must NOT be reported as a successful convert. Require a
    // non-empty file AND — when we observed the real exit code — a clean exit.
    // `exited == Some(non-zero)` is a known failure; `None` (status from the kill,
    // or not yet exited) keeps the original lenient behavior.
    let known_bad_exit = exited.is_some_and(|s| !s.success());
    if wrote && !known_bad_exit {
        Ok(())
    } else {
        log_magick_failure(
            if wrote {
                "encode exited non-zero (partial output)"
            } else {
                "encode produced no file"
            },
            status,
            &err,
        );
        let _ = std::fs::remove_file(out);
        Err(Error::from(E_FAIL))
    }
}
