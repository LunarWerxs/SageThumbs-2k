//! Command-line / agent API — the verbs the `st2k` console binary exposes.
//!
//! Every verb reuses the exact same engine the shell extension uses (every format
//! we decode via `decode_full`, the convert/rotate/strip/OCR/PDF logic), so an
//! installed SageThumbs 2K doubles as an offline image toolbox for scripts and
//! AI agents — no extra installs. Each verb returns `Ok(stdout text)` or
//! `Err(message)`; the binary prints and maps to an exit code.

use std::path::Path;

use crate::{decode, formats, ocr, settings, strip, topdf, verbs};

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

/// `st2k devmode on|off|status`: toggle the developer-test-box flag (the HKCU `DevMachine`
/// value). When ON, this machine's startup manifest request carries `&dev=1`. A plain
/// machine-local flag, not an identifier; OFF on every real install.
/// Turn Explorer thumbnails on/off for THIS USER, pointing at the DLL shipped beside this
/// exe. This is what makes the portable zip more than a bag of tools: the handler is COM, so
/// it has to be registered somewhere, and `HKCU\Software\Classes` is the somewhere that needs
/// no installer and no admin. See `register::register_user` for what a per-user registration
/// can and cannot cover.
pub fn register_portable(off: bool, status: bool) -> Result<String, String> {
    let current = crate::register::user_registration_path();

    if status {
        return Ok(match current {
            Some(p) => format!("Explorer thumbnails: ON for this user\n  handler: {p}"),
            None => "Explorer thumbnails: OFF for this user".into(),
        });
    }

    if off {
        crate::register::unregister_user().map_err(|e| format!("could not unregister: {e}"))?;
        return Ok("Explorer thumbnails turned OFF for this user.".into());
    }

    // PORTABLE ONLY, and "the DLL is beside us" is NOT a good enough test for that: an installed
    // build has sagethumbs2k.dll right next to st2k.exe in Program Files, so the exists-check
    // below passes there too. Registering from an installed copy writes OUR CLSID into
    // HKCU\Software\Classes, which the shell merges AHEAD of the machine-wide view — and the
    // uninstaller only removes HKCU\Software\SageThumbs2K (the settings), never these class keys.
    // The result is a per-user handler that outlives the uninstall, still pointing at a deleted
    // Program Files DLL, silently killing thumbnails for that user with nothing to blame.
    if !settings::portable() {
        return Err(
            "this is an installed copy, which already registers thumbnails machine-wide.\n\
             `st2k register` is for the portable zip only — using it here would leave a per-user \
             registration behind that survives uninstall and blocks thumbnails.\n\
             Use Settings ▸ Diagnostics ▸ Repair file associations instead.\n\
             (If a previous run already did this, `st2k register --off` clears it.)"
                .into(),
        );
    }

    let exe = std::env::current_exe().map_err(|e| format!("could not locate this exe: {e}"))?;
    let dll = exe
        .parent()
        .ok_or("this exe has no parent directory")?
        .join("sagethumbs2k.dll");
    if !dll.exists() {
        return Err(format!(
            "{} is not here.\nThis verb is for the portable zip, which ships that DLL beside the exes.",
            dll.display()
        ));
    }
    let dll = dll.to_string_lossy().into_owned();

    crate::register::register_user(&dll).map_err(|e| format!("could not register: {e}"))?;
    let mut out = format!("Explorer thumbnails turned ON for this user.\n  handler: {dll}");
    // Moving the folder later leaves the keys aimed at a path that no longer exists, and the
    // symptom is thumbnails quietly not drawing, so say it once here where it is cheap.
    out.push_str("\n\nIf you move or delete this folder, run `st2k register --off` first.");
    Ok(out)
}

pub fn devmode(sub: &str) -> Result<String, String> {
    match sub {
        "on" | "enable" | "1" => {
            settings::set_dev_machine(true)
                .map_err(|_| "couldn't write the DevMachine flag".to_string())?;
            Ok("dev mode ON (this machine's manifest request carries &dev=1).".into())
        }
        "off" | "disable" | "0" => {
            settings::set_dev_machine(false)
                .map_err(|_| "couldn't clear the DevMachine flag".to_string())?;
            Ok("dev mode OFF (this machine's manifest request is unmodified).".into())
        }
        "status" | "" => Ok(format!(
            "dev mode is {} (HKCU\\Software\\SageThumbs2K\\DevMachine)",
            if settings::is_dev_machine() {
                "ON"
            } else {
                "OFF"
            }
        )),
        other => Err(format!(
            "unknown devmode '{other}' (use: on | off | status)"
        )),
    }
}

/// Render any supported image to `output` (format from its extension) at most
/// `max_dim` px on the long edge (`0` = full size). The headline verb: produces
/// previews for the formats Windows itself can't.
pub fn thumbnail(input: &str, output: &str, max_dim: u32) -> Result<String, String> {
    let archive_ext = Path::new(input)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    if crate::formats::is_archive(archive_ext) {
        reject_oversized_archive(input, crate::settings::max_file_size_bytes())?;
    }
    // Generic archive (.zip/.rar/.7z): the same list-then-extract path Explorer
    // uses — including the user's MaxSize gate before archive parsing — and the
    // contact sheet composes per the same Setting. Falls through to the normal
    // decode if it isn't really an archive (renamed file) so the magic-dispatch
    // tiers still get their shot.
    if let Some(img) = archive_thumbnail(input) {
        let out = if max_dim > 0 {
            img.thumbnail(max_dim, max_dim)
        } else {
            img
        };
        out.save(output).map_err(|e| e.to_string())?;
        return Ok(output.to_string());
    }
    // Cap the read at the shared input budget (metadata-checked before allocating)
    // so a scripted/agent/MCP call can't load a multi-GB file wholesale — the same
    // ceiling Explorer thumbnailing and the path verbs apply. Head-preview
    // containers (.blend / PSD-PSB) past the cap still render from a bounded prefix.
    // Preview fidelity (embedded/container previews OK) — that's what a
    // thumbnail is; `convert` is the full-fidelity verb. By PATH, so the streaming
    // rescues apply: an OpenEXR is scaled straight off the file handle instead of
    // being refused for exceeding the shared input budget (which a 12K render pass
    // always does), and anything else already PAST that budget gets one last try
    // through the OS codecs reading the file directly. Every format under the
    // budget takes the same bounded whole-file read as before, and a file neither
    // rescue can open still reports the same size-limit error text.
    let edge = if max_dim > 0 {
        max_dim
    } else {
        decode::EXR_PATH_EDGE
    };
    let img = match decode::decode_preview_streamed(input, edge) {
        Some(img) => img,
        None => {
            let bytes = decode::read_preview_capped(input).map_err(|e| e.to_string())?;
            // Cap the decode at the edge we're about to shrink to anyway — the streamed
            // path above already takes `edge`, and rendering ImageMagick's full 4096 first
            // costs seconds on a big scan for pixels this immediately discards.
            decode::decode_preview_capped_for_path(&bytes, edge, input)
                .map_err(|_| format!("cannot decode {input}"))?
        }
    };
    let out = if max_dim > 0 {
        img.thumbnail(max_dim, max_dim)
    } else {
        img
    };
    out.save(output).map_err(|e| e.to_string())?;
    Ok(output.to_string())
}

/// Fail before opening or parsing a generic archive when either the user's
/// MaxSize preference or the shared hard input ceiling rejects its metadata
/// length. `configured_max == u64::MAX` is Settings' "Unlimited" representation.
fn reject_oversized_archive(input: &str, configured_max: u64) -> Result<(), String> {
    let max = decode::effective_input_cap(configured_max);
    if let Ok(meta) = std::fs::metadata(input) {
        if meta.len() > max {
            return Err(format!(
                "input is {} bytes, over the effective archive limit of {max} bytes",
                meta.len()
            ));
        }
    }
    Ok(())
}

/// The generic-archive cover/contact-sheet for a `.zip`/`.rar`/`.7z` PATH, or None
/// to take the normal decode route (not an archive extension, unreadable, or no
/// image entries — the CLI then reports "cannot decode", mirroring the shell's
/// stock-icon fallback). 1024px edge matches the preview pane's compose target.
fn archive_thumbnail(input: &str) -> Option<image::DynamicImage> {
    use std::io::Read;
    let ext = Path::new(input)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    if !crate::formats::is_archive(ext) {
        return None;
    }
    let want = if crate::settings::archive_collage() {
        4
    } else {
        1
    };
    let mut f = std::fs::File::open(input).ok()?;
    let mut head = [0u8; 8];
    f.read_exact(&mut head).ok()?;
    std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(0)).ok()?;
    let covers = if crate::container::archive_needs_buffer(&head) {
        // RAR buffers whole (`rars` accepts no reader) — same bounded read as the
        // normal path, so a multi-GB .rar fails to the normal decode error.
        let bytes = decode::read_preview_capped(input).ok()?;
        crate::container::archive_covers(&bytes, want)?
    } else {
        crate::container::archive_covers_seek(&mut f, &head, want)?
    };
    let d = decode::thumbnail_from_covers(&covers, 1024).ok()?;
    image::RgbaImage::from_raw(d.width, d.height, d.rgba).map(image::DynamicImage::ImageRgba8)
}

/// Convert `input` to the exact `output` path at `quality`, optional `resize`.
/// `webp_quality = Some(q)` writes lossy WebP at quality `q` (only meaningful when
/// `output` is a `.webp`); `None` keeps WebP lossless.
pub fn convert(
    input: &str,
    output: &str,
    quality: u8,
    webp_quality: Option<u8>,
    resize: verbs::Resize,
) -> Result<String, String> {
    verbs::convert_to(input, Path::new(output), quality, webp_quality, resize)
        .map_err(|_| format!("convert failed: {input}"))?;
    Ok(output.to_string())
}

/// Rotate/flip → a "(edited)" sibling. `by` ∈ right|left|180|fliph|flipv.
pub fn rotate(input: &str, by: &str) -> Result<String, String> {
    let t = match by {
        "right" => verbs::Transform::Right90,
        "left" => verbs::Transform::Left90,
        "180" => verbs::Transform::Rotate180,
        "fliph" => verbs::Transform::FlipH,
        "flipv" => verbs::Transform::FlipV,
        _ => {
            return Err(format!(
                "unknown rotation '{by}' (right|left|180|fliph|flipv)"
            ))
        }
    };
    verbs::transform_file(input, t)
        .map(|p| p.display().to_string())
        .map_err(|_| format!("rotate failed: {input}"))
}

/// Decode `input` and return it as in-memory PNG bytes, fit within `max_dim` (0 = full
/// size). Powers the MCP `view` tool — lets an AI agent SEE any of our supported formats
/// directly (HEIC/RAW/PSD/ebook covers/CAD previews/…), not just convert them to a file.
pub fn view_png(input: &str, max_dim: u32) -> Result<Vec<u8>, String> {
    let bytes = decode::read_preview_capped(input).map_err(|e| e.to_string())?;
    let img = decode::decode_preview_capped_for_path(&bytes, 0, input)
        .map_err(|_| format!("cannot decode {input}"))?;
    let img = if max_dim > 0 {
        img.thumbnail(max_dim, max_dim)
    } else {
        img
    };
    let mut out = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(out)
}

/// Compress to a target file size → a "(compressed)" JPEG sibling at or under
/// `target_bytes` (quality binary-search + downscale fallback). See [`parse_size`].
pub fn compress(input: &str, target_bytes: u64) -> Result<String, String> {
    verbs::compress_to_size(input, target_bytes)
        .map(|p| p.display().to_string())
        .map_err(|_| format!("compress failed: {input}"))
}

/// Parse a human size — `"1MB"`, `"500KB"`, `"800kb"`, or a bare byte count `"800000"` —
/// into bytes. Decimal units (1KB = 1000 B), case-insensitive, optional trailing `B`.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let lower = s.trim().to_ascii_lowercase();
    let core = lower.strip_suffix('b').unwrap_or(&lower); // tolerate MB/KB/B
    let (num, mult) = if let Some(n) = core.strip_suffix('m') {
        (n, 1_000_000u64)
    } else if let Some(n) = core.strip_suffix('k') {
        (n, 1_000)
    } else {
        (core, 1)
    };
    let v: f64 = num
        .trim()
        .parse()
        .map_err(|_| format!("bad size '{s}' (try 1MB / 500KB / 800000)"))?;
    // f64::from_str accepts "inf"/"infinity"/"nan" (any case). Neither is caught by
    // `v <= 0.0` (INFINITY > 0.0 is true; every NaN comparison is false), and the
    // trailing `as u64` cast is Rust's SATURATING float->int cast — inf would
    // silently become u64::MAX and nan would become 0 as a "compress target".
    if !v.is_finite() {
        return Err(format!("size must be a finite number: '{s}'"));
    }
    if v <= 0.0 {
        return Err(format!("size must be positive: '{s}'"));
    }
    Ok((v * mult as f64) as u64)
}

/// Strip EXIF/IPTC/XMP/C2PA metadata in place (JPEG/PNG/WebP, lossless).
pub fn strip_meta(input: &str) -> Result<String, String> {
    strip::strip_metadata(input)
        .map_err(|_| format!("strip failed (JPEG/PNG/WebP only): {input}"))?;
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

/// Combine images into one PDF (one page each).
pub fn pdf(output: &str, inputs: &[String]) -> Result<String, String> {
    if inputs.is_empty() {
        return Err("no input images".to_string());
    }
    // Same JPEG quality the right-click Combine-to-PDF verb uses (the user's configured
    // setting) — a hardcoded 85 silently diverged from the menu path for no reason.
    topdf::combine_to_pdf(inputs, Path::new(output), crate::settings::jpeg_quality())
        .map_err(|_| "pdf build failed".to_string())?;
    Ok(output.to_string())
}

/// Image dimensions + EXIF (camera/date/GPS), as text or JSON.
pub fn info(input: &str, json: bool) -> Result<String, String> {
    let i = strip::read_info(input);
    if i.width == 0 && i.height == 0 {
        return Err(format!("cannot read {input}"));
    }
    if json {
        // A malformed EXIF rational (0 denominator) can produce inf/NaN; drop it
        // rather than emit `NaN`, which is not valid JSON.
        let gps = i
            .gps
            .filter(|(a, b)| a.is_finite() && b.is_finite())
            .map(|(a, b)| [a, b]);
        Ok(serde_json::json!({
            "width": i.width,
            "height": i.height,
            "make": i.make,
            "model": i.model,
            "datetime": i.datetime,
            "gps": gps,
        })
        .to_string())
    } else {
        let mut s = format!("{} x {} px", i.width, i.height);
        if let Some(m) = &i.make {
            s.push_str(&format!("\ncamera: {m}"));
        }
        if let Some(m) = &i.model {
            s.push_str(&format!(" {m}"));
        }
        if let Some(d) = &i.datetime {
            s.push_str(&format!("\ntaken: {d}"));
        }
        if let Some((la, lo)) = i.gps {
            s.push_str(&format!("\ngps: {la:.5}, {lo:.5}"));
        }
        Ok(s)
    }
}

/// Parse the optional `resize` argument ("WxH" fit, no upscale; or "N%" scale)
/// into a [`verbs::Resize`]. `None`/empty → `Resize::None`. Shared by the CLI
/// (`st2k convert --resize`) and the MCP `convert` tool so the syntax stays
/// identical in both front ends.
pub fn parse_resize(s: Option<&str>) -> Result<verbs::Resize, String> {
    let Some(v) = s.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(verbs::Resize::None);
    };
    if let Some(p) = v.strip_suffix('%') {
        let pct: u32 = p.trim().parse().map_err(|_| format!("bad percent '{v}'"))?;
        return Ok(verbs::Resize::Percent(pct.clamp(1, 1000)));
    }
    let (w, h) = v
        .split_once(['x', 'X'])
        .ok_or_else(|| format!("bad resize '{v}' (use WxH or N%)"))?;
    let w: u32 = w
        .trim()
        .parse()
        .map_err(|_| format!("bad width in '{v}'"))?;
    let h: u32 = h
        .trim()
        .parse()
        .map_err(|_| format!("bad height in '{v}'"))?;
    Ok(verbs::Resize::Fit(w.max(1), h.max(1)))
}

/// Is `p` a cloud-storage placeholder (OneDrive/Dropbox "free up space" file) whose
/// content isn't actually on disk yet? Symlink metadata, so a reparse point/junction
/// itself never triggers a download just to answer this.
///
/// `FILE_ATTRIBUTE_OFFLINE` | `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS` |
/// `FILE_ATTRIBUTE_RECALL_ON_OPEN` — the same trio `prebuild.rs`'s `OFFLINE_ATTRS`
/// checks (and `doctor.rs` diagnoses). A OneDrive/Dropbox placeholder carries one
/// of these; opening it to decode/convert DOWNLOADS the whole file, so `st2k batch`
/// over a cloud-synced folder used to silently hydrate every placeholder it met —
/// `prebuild` already guards against exactly this, `batch` did not. Shares
/// `prebuild::OFFLINE_ATTRS` rather than a second hand-typed copy of the three flags
/// (which is exactly how `doctor.rs`'s own copy once drifted); its own test pins them.
fn is_cloud_placeholder(p: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    std::fs::symlink_metadata(p)
        .map(|m| m.file_attributes() & crate::prebuild::OFFLINE_ATTRS != 0)
        .unwrap_or(false)
}

/// Expand `inputs` (files and/or directories) into a flat list of SUPPORTED image
/// files (directories are scanned one level deep; unsupported extensions dropped;
/// cloud placeholders dropped too — see [`is_cloud_placeholder`]). Second element
/// is how many were skipped as placeholders, so callers can tell the user rather
/// than silently hydrating them.
fn expand_inputs(inputs: &[String]) -> (Vec<String>, usize) {
    fn supported(p: &Path) -> bool {
        // `is_known` is ASCII-case-insensitive — no lowercase allocation needed.
        p.extension()
            .and_then(|e| e.to_str())
            .is_some_and(formats::is_known)
    }
    let mut out = Vec::new();
    let mut skipped_offline = 0usize;
    let mut consider = |candidate: &Path, owned: String| {
        if !supported(candidate) {
            return;
        }
        if is_cloud_placeholder(candidate) {
            skipped_offline += 1;
        } else {
            out.push(owned);
        }
    };
    for i in inputs {
        let p = Path::new(i);
        if p.is_dir() {
            if let Ok(rd) = std::fs::read_dir(p) {
                for e in rd.flatten() {
                    let ep = e.path();
                    if ep.is_file() {
                        let s = ep.to_string_lossy().into_owned();
                        consider(&ep, s);
                    }
                }
            }
        } else if p.is_file() {
            consider(p, i.clone());
        }
    }
    (out, skipped_offline)
}

/// BULK process many inputs (files and/or folders) in ONE process, fanned out
/// across all cores via the shared batch pool — the fast path for the regression
/// harness and AI agents (no more one `st2k` spawn per file). `op` is `thumbnail`
/// (→ PNG at `size`px) or `convert` (→ `to_ext`, honoring `quality`/`resize`).
/// Outputs go to `out_dir` (created if needed) or next to each source. Returns a
/// `done/total` summary.
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
fn reserve_batch_output(dir: &Path, stem: &str, ext: &str) -> std::path::PathBuf {
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
            Ok(_) => return cand,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
            // Couldn't create for another reason (permission / missing dir): hand
            // the name back anyway — the encode pass surfaces the real error.
            Err(_) => return cand,
        }
    }
}

pub fn batch(
    op: &str,
    inputs: &[String],
    out_dir: Option<&str>,
    size: u32,
    to_ext: Option<&str>,
    quality: u8,
    resize: verbs::Resize,
) -> Result<String, String> {
    let is_convert = match op {
        "thumbnail" | "thumb" => false,
        "convert" => true,
        other => return Err(format!("unknown batch op '{other}' (thumbnail|convert)")),
    };
    let ext = if is_convert {
        to_ext
            .ok_or("batch convert needs --to <ext>")?
            .trim_start_matches('.')
            .to_ascii_lowercase()
    } else {
        "png".to_string()
    };

    let (files, skipped_offline) = expand_inputs(inputs);
    if files.is_empty() {
        return Err("no supported image files found in the inputs".to_string());
    }
    if let Some(d) = out_dir {
        std::fs::create_dir_all(d).map_err(|e| format!("cannot create output dir {d}: {e}"))?;
    }

    // Reserve collision-free output paths SERIALLY and ATOMICALLY, so neither the
    // parallel pass below nor an EXTERNAL writer (a concurrent `st2k` invocation,
    // Explorer, a right-click verb) can land on the same name. See
    // `reserve_batch_output` for why (and why not a plain `used`/`exists()` check).
    let mut pairs: Vec<(String, std::path::PathBuf)> = Vec::with_capacity(files.len());
    for f in &files {
        let src = Path::new(f);
        let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("image");
        let dir = match out_dir {
            Some(d) => std::path::PathBuf::from(d),
            None => src
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| std::path::PathBuf::from(".")),
        };
        let out = reserve_batch_output(&dir, stem, &ext);
        pairs.push((f.clone(), out));
    }

    // Fan out: each (input, pre-reserved output) is independent → no naming race.
    let results = crate::parallel::map(&pairs, |_, (input, output)| -> bool {
        if is_convert {
            // `quality` is the only quality knob `batch` exposes; before this fix it
            // was dropped for WebP specifically (`None` = lossless, unconditionally),
            // so `batch convert --to webp --quality N` silently ignored N and always
            // wrote a large lossless file. Reuse it as the WebP quality too.
            let webp_quality = (ext == "webp").then_some(quality);
            verbs::convert_to(input, output, quality, webp_quality, resize).is_ok()
        } else {
            thumbnail(input, &output.to_string_lossy(), size).is_ok()
        }
    });
    // A failed encode never got past the reserved placeholder — clean up any that
    // are still zero bytes (mirrors OutSlot's own drop behavior), so a failed batch
    // item leaves nothing behind, same as before this fix reserved ahead of time.
    for ((_, out), &ok) in pairs.iter().zip(results.iter()) {
        if !ok {
            let empty = std::fs::metadata(out)
                .map(|m| m.len() == 0)
                .unwrap_or(false);
            if empty {
                let _ = std::fs::remove_file(out);
            }
        }
    }
    let done = results.iter().filter(|&&ok| ok).count();
    let total = files.len();
    let offline_note = if skipped_offline > 0 {
        format!(
            "\n  skipped  {skipped_offline} cloud placeholder(s) — opening these would download them"
        )
    } else {
        String::new()
    };
    // Total failure must FAIL the command (nonzero exit for scripts/CI/MCP callers) — a
    // "0/12 succeeded" with exit code 0 was indistinguishable from a good run without
    // parsing English stdout. Partial success stays Ok but now names the failure count.
    if done == 0 {
        return Err(format!("0/{total} succeeded"));
    }
    if done < total {
        return Ok(format!(
            "{done}/{total} succeeded ({} failed){offline_note}",
            total - done
        ));
    }
    Ok(format!("{done}/{total} succeeded{offline_note}"))
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

/// List every supported input extension (with category + description).
/// Time the DECODE of many files inside ONE process, and print `name<TAB>ms` per file.
///
/// Exists because measuring decode speed by timing `st2k thumbnail` once per file measures
/// PROCESS STARTUP as much as decoding. On a loaded machine that floor swings 28 -> 187 ms,
/// and it is not symmetric with what it gets compared against (Windows' own WIC decode, which
/// is in-process), so a busy box invents regressions in whichever formats happen to be slow.
/// The shell extension does not pay a spawn per thumbnail either, so the per-file spawn was
/// never part of what we actually wanted to measure.
///
/// Reports the MINIMUM of `runs`, which is the right statistic under background load: the
/// fastest observation is the one least polluted by other work. Decode only — no PNG is
/// written, since encoding the output is not what any of this is trying to measure.
///
/// A dev/measurement verb, deliberately undocumented in `--help`, like the app EXE's
/// `--bench-*` modes.
pub fn bench_decode(inputs: &[String], size: u32, runs: u32) -> Result<String, String> {
    use std::time::Instant;

    let edge = if size > 0 {
        size
    } else {
        decode::EXR_PATH_EDGE
    };
    let runs = runs.max(1);
    let mut out = String::new();
    for input in inputs {
        let mut best: Option<u128> = None;
        let mut ok = false;
        for _ in 0..runs {
            let t0 = Instant::now();
            let decoded = match decode::decode_preview_streamed(input, edge) {
                Some(img) => Some(img),
                None => match decode::read_preview_capped(input) {
                    Ok(bytes) => decode::decode_preview_capped_for_path(&bytes, edge, input).ok(),
                    Err(_) => None,
                },
            };
            // Fit to the target box too, THROUGH THE PROVIDER'S OWN FIT rather than a cheaper
            // stand-in. That is real per-thumbnail work - on a 12 MP image the reduction costs
            // about as much as the decode did - and measuring a different one would flatter
            // exactly the formats that decode huge and shrink hard, which is what this whole
            // measurement exists to catch.
            let decoded = decoded.map(|img| {
                if size > 0 {
                    decode::thumbnail_from_image(img, size).rgba.len()
                } else {
                    (img.width() as usize) * (img.height() as usize)
                }
            });
            let elapsed = t0.elapsed().as_micros();
            if decoded.is_some() {
                ok = true;
                best = Some(best.map_or(elapsed, |b: u128| b.min(elapsed)));
            }
        }
        let name = Path::new(input)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(input);
        match (ok, best) {
            (true, Some(us)) => {
                out.push_str(&format!("{name}\t{:.3}\n", us as f64 / 1000.0));
            }
            // A file we cannot decode is reported, not silently dropped: a format that stops
            // decoding must not look like a format that got faster.
            _ => out.push_str(&format!("{name}\tFAIL\n")),
        }
    }
    Ok(out)
}

pub fn list_formats(json: bool) -> String {
    if json {
        let items: Vec<_> = formats::FORMATS
            .iter()
            .map(|(ext, desc)| {
                serde_json::json!({
                    "ext": ext,
                    "category": formats::category_label(formats::category(ext)),
                    "description": desc,
                })
            })
            .collect();
        serde_json::Value::Array(items).to_string()
    } else {
        let mut s = format!("{} supported input formats:\n", formats::FORMATS.len());
        for (ext, desc) in formats::FORMATS {
            s.push_str(&format!("  .{ext:<6} {desc}\n"));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises the tests that touch the process-wide Ctrl+C `CANCEL` flag against each
    /// other and against anything that reads it. One flag, many test threads, so any test
    /// that WRITES it must hold this first.
    static CANCEL_FLAG_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    use std::io::Write;

    /// The flag part of the Ctrl+C wiring: `install` must start a fresh run from
    /// "not cancelled" even if a previous run's Ctrl+C left it set — the actual OS-level
    /// `SetConsoleCtrlHandler` registration and CTRL_C_EVENT delivery can't be exercised
    /// in-process (there is no safe way to raise a real console control event against the
    /// test runner itself), so this pins the one behaviour that IS a pure state check.
    ///
    /// SERIALISED, and it has to be: `CANCEL` is one process-wide flag that `cli::prebuild`
    /// also reads, cargo runs this file's tests on many threads in one process, and this test
    /// deliberately SETS the flag. Without the lock it can make a concurrent prebuild test see
    /// a cancellation nobody asked for, which is a flake that would look like a real bug in
    /// the cancel path. Same defect an audit found in the DPI-override tests, same fix.
    #[test]
    fn ctrlc_cancel_install_resets_the_flag() {
        use std::sync::atomic::Ordering;
        let _guard = CANCEL_FLAG_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        ctrlc_cancel::flag().store(true, Ordering::SeqCst);
        ctrlc_cancel::install();
        assert!(
            !ctrlc_cancel::flag().load(Ordering::SeqCst),
            "install() must clear a flag left set by an earlier run"
        );
        // Leave the shared flag as we found it, so a test that runs after this one is not
        // handed a stale cancellation.
        ctrlc_cancel::flag().store(false, Ordering::SeqCst);
    }

    #[test]
    fn cli_thumbnail_and_info_and_formats() {
        let dir = std::env::temp_dir().join(format!("st2k_cli_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("a.png");
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(400, 300))
            .save(&src)
            .unwrap();
        let sp = src.to_str().unwrap();

        let out = dir.join("t.png");
        thumbnail(sp, out.to_str().unwrap(), 128).unwrap();
        let d = image::open(&out).unwrap();
        assert!(d.width() <= 128 && d.height() <= 128 && d.width() == 128);

        let cv = dir.join("a.jpg");
        convert(
            sp,
            cv.to_str().unwrap(),
            85,
            None,
            verbs::Resize::Fit(100, 100),
        )
        .unwrap();
        assert!(image::open(&cv).unwrap().width() <= 100);

        assert!(info(sp, true).unwrap().contains("\"width\":400"));
        assert!(list_formats(false).contains(".png"));
        assert!(list_formats(true).starts_with('['));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unlimited_archive_setting_still_rejects_before_parse_at_hard_cap() {
        let path = std::env::temp_dir().join(format!(
            "st2k_cli_oversized_{}_{}.7z",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut file = std::fs::File::create(&path).unwrap();
        // A real 7z signature makes this representative if a future refactor
        // accidentally moves the gate after format probing. set_len keeps the
        // test sparse/fast instead of writing 256 MiB.
        file.write_all(b"7z\xBC\xAF\x27\x1C").unwrap();
        file.set_len(decode::limits::MAX_INPUT_BYTES + 1).unwrap();
        drop(file);

        let err = reject_oversized_archive(path.to_str().unwrap(), u64::MAX).unwrap_err();
        assert!(err.contains(&decode::limits::MAX_INPUT_BYTES.to_string()));

        let _ = std::fs::remove_file(path);
    }

    /// `f64::from_str` accepts "inf"/"infinity"/"nan" (any case) and neither trips
    /// the `v <= 0.0` guard (INFINITY > 0.0; every NaN comparison is false), so
    /// without the `is_finite` check the trailing saturating `as u64` cast would
    /// silently turn "inf" into `u64::MAX` and "nan" into `0` as a compress target.
    #[test]
    fn parse_size_rejects_non_finite_values() {
        assert!(parse_size("inf").is_err());
        assert!(parse_size("Infinity").is_err());
        assert!(parse_size("-inf").is_err());
        assert!(parse_size("nan").is_err());
        assert!(parse_size("NaN").is_err());
        // Still accepts ordinary sizes.
        assert_eq!(parse_size("1MB"), Ok(1_000_000));
        assert_eq!(parse_size("500KB"), Ok(500_000));
    }

    /// The exact A058 race: two callers reserving under the SAME (dir, stem, ext)
    /// concurrently must never both walk away with the same path. A check-then-
    /// create loop (`exists()` then open) can let two threads both pass the check
    /// for the same candidate before either creates it; `create_new` cannot.
    #[test]
    fn reserve_batch_output_is_race_safe_under_concurrent_callers() {
        let dir = std::env::temp_dir().join(format!(
            "st2k_cli_toctou_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::sync::Arc::new(dir);

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let dir = dir.clone();
                std::thread::spawn(move || reserve_batch_output(&dir, "race", "webp"))
            })
            .collect();
        let mut got: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        got.sort();
        let before = got.len();
        got.dedup();
        assert_eq!(
            got.len(),
            before,
            "two concurrent callers claimed the SAME output path — the exact race this fix closes"
        );

        let _ = std::fs::remove_dir_all(&*dir);
    }

    /// A name a batch's OWN earlier iteration already claimed, and a name some
    /// external writer created before `batch` ever ran, must both be skipped —
    /// the reservation itself is what proves it, not the (now-removed) `used` set.
    #[test]
    fn reserve_batch_output_skips_names_already_on_disk() {
        let dir = std::env::temp_dir().join(format!(
            "st2k_cli_reserve_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("img.webp"), b"external writer").unwrap();

        let first = reserve_batch_output(&dir, "img", "webp");
        assert_eq!(first, dir.join("img (1).webp"));
        let second = reserve_batch_output(&dir, "img", "webp");
        assert_eq!(second, dir.join("img (2).webp"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `batch convert --to webp --quality N` must actually vary output size with
    /// N — before this fix `webp_quality` was hard-coded to `None` (lossless) no
    /// matter what quality was requested. Gated like `verbs.rs`'s own
    /// `lossy_webp_is_smaller_and_keeps_alpha`: without `webp-lossy`, WebP is
    /// ALWAYS encoded losslessly regardless of `webp_quality`, so this can only
    /// prove anything when the feature (which every release build enables) is on.
    #[cfg(feature = "webp-lossy")]
    #[test]
    fn batch_convert_to_webp_honors_quality() {
        let dir = std::env::temp_dir().join(format!(
            "st2k_cli_webpq_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // Genuine per-pixel noise (an integer hash, not a linear/periodic formula):
        // a plain modular formula like `x*53 + y*17` has constant row/column
        // differences, which a LOSSLESS predictive coder crushes to near-nothing —
        // the opposite of what this test needs. Real noise is what quality-10
        // LOSSY WebP shrinks a lot and lossless does not.
        let img = image::RgbImage::from_fn(200, 200, |x, y| {
            let i = y.wrapping_mul(200).wrapping_add(x);
            let mut h = i.wrapping_mul(0x9E37_79B9) ^ 0x85EB_CA6B;
            h ^= h >> 16;
            h = h.wrapping_mul(0x045D_9F3B);
            h ^= h >> 16;
            image::Rgb([
                (h & 0xFF) as u8,
                ((h >> 8) & 0xFF) as u8,
                ((h >> 16) & 0xFF) as u8,
            ])
        });
        let src = dir.join("noise.png");
        image::DynamicImage::ImageRgb8(img).save(&src).unwrap();

        batch(
            "convert",
            &[src.to_str().unwrap().to_string()],
            Some(dir.to_str().unwrap()),
            256,
            Some("webp"),
            10, // aggressively lossy
            verbs::Resize::None,
        )
        .unwrap();
        let lossy_len = std::fs::metadata(dir.join("noise.webp")).unwrap().len();

        // The single-file path with an explicit `None` webp_quality is the known-
        // lossless baseline to compare against.
        let lossless = dir.join("noise_lossless.webp");
        verbs::convert_to(
            src.to_str().unwrap(),
            &lossless,
            10,
            None,
            verbs::Resize::None,
        )
        .unwrap();
        let lossless_len = std::fs::metadata(&lossless).unwrap().len();

        assert!(
            lossy_len < lossless_len,
            "batch webp at quality 10 ({lossy_len} bytes) should be smaller than lossless \
             ({lossless_len} bytes) — quality is not reaching the encoder"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A OneDrive/Dropbox "free up space" placeholder must be skipped, not
    /// silently hydrated (downloaded) by opening it for a decode.
    #[test]
    fn expand_inputs_skips_cloud_placeholders() {
        let dir = std::env::temp_dir().join(format!(
            "st2k_cli_offline_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let normal = dir.join("normal.png");
        std::fs::write(&normal, b"not a real png, just needs to exist").unwrap();
        let placeholder = dir.join("cloud.png");
        std::fs::write(&placeholder, b"placeholder").unwrap();

        // FILE_ATTRIBUTE_OFFLINE, set directly rather than needing a real cloud
        // provider to reproduce the flag.
        unsafe {
            use std::os::windows::ffi::OsStrExt;
            use windows::core::PCWSTR;
            use windows::Win32::Storage::FileSystem::{SetFileAttributesW, FILE_ATTRIBUTE_OFFLINE};
            let wide: Vec<u16> = placeholder
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            SetFileAttributesW(PCWSTR(wide.as_ptr()), FILE_ATTRIBUTE_OFFLINE).unwrap();
        }
        assert!(is_cloud_placeholder(&placeholder));
        assert!(!is_cloud_placeholder(&normal));

        let (files, skipped) = expand_inputs(&[dir.to_str().unwrap().to_string()]);
        assert_eq!(skipped, 1);
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("normal.png"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
