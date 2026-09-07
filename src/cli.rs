//! Command-line / agent API — the verbs the `st2k` console binary exposes.
//!
//! Every verb reuses the exact same engine the shell extension uses (every format
//! we decode via `decode_full`, the convert/rotate/strip/OCR/PDF logic), so an
//! installed SageThumbs 2K doubles as an offline image toolbox for scripts and
//! AI agents — no extra installs. Each verb returns `Ok(stdout text)` or
//! `Err(message)`; the binary prints and maps to an exit code.

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::E_FAIL;

use crate::{decode, formats, ocr, settings, strip, topdf, verbs};

/// Refuse an `output` that is one of the `inputs`, before anything is read or written
/// (2026-09-05 audit, F30). Every exact-destination verb here reads its inputs and then
/// replaces the destination, so `thumbnail same.png same.png` destroyed the original; a
/// case variant, a relative or `..` spelling and a hard link all did the same and none of
/// them is caught by comparing the strings. `fsutil::same_file` compares canonical paths
/// and, for hard links, the file's own identity. No verb here offers an in-place form, so
/// there is no flag that lifts this.
fn reject_output_alias<'a>(
    output: &str,
    inputs: impl IntoIterator<Item = &'a str>,
) -> Result<(), String> {
    match crate::fsutil::aliased_input(Path::new(output), inputs) {
        Some(input) => Err(format!(
            "output {output} is the same file as input {input}; refusing to overwrite the \
             source (write to a different path)"
        )),
        None => Ok(()),
    }
}

/// The composer behind a verb writes exactly one file type, so its destination has to say
/// so (2026-09-05 audit, F30): `pdf same.png same.png` used to put PDF bytes into a `.png`.
/// Case-insensitive, like every other extension test in this crate.
fn require_output_ext(output: &str, ext: &str) -> Result<(), String> {
    let got = Path::new(output)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    if got.eq_ignore_ascii_case(ext) {
        Ok(())
    } else {
        Err(format!(
            "output must be a .{ext} file (got {output}); this command only writes .{ext}"
        ))
    }
}

/// Write `img` to `output` through the shared atomic writer (temp sibling + rename), the
/// same path the convert verbs take. `thumbnail` used to call `DynamicImage::save`, which
/// truncates the destination BEFORE encoding, so a failed encode left a 0-byte file where a
/// good one had been (2026-09-05 audit, F30). `format` was decided from `output`'s extension
/// up front, because the temp file's own extension is `.st2ktmp`.
fn save_atomic(
    img: &image::DynamicImage,
    output: &str,
    format: image::ImageFormat,
) -> Result<(), String> {
    verbs::write_atomic(Path::new(output), |tmp| {
        img.save_with_format(tmp, format)
            .map_err(|e| windows::core::Error::new(E_FAIL, format!("write {output}: {e}")))
    })
    .map_err(|e| e.message())
}

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
/// The CLI's fit-to-size, kept deliberately SEPARATE from the shell extension's `fit_to_box`.
///
/// Shrinking uses the one shared reduction, so `st2k thumbnail` and the MCP `view` tool now
/// produce the SAME pixels the thumbnail provider does - which is the whole point, since every
/// visual gate in this repo drives this path and used to validate a picture Explorer never
/// drew (see `decode::thumb`'s `the_gates_reduce_a_thumbnail_the_way_the_shell_extension_does`).
///
/// ENLARGING is left exactly as it was, `DynamicImage::thumbnail`, and that is not an oversight.
/// `--size` has always FILLED the box here: a 72x72 APK icon asked for at 256 came back 256x256,
/// verified against the shipped 2.1.2 binary. Routing the small case through the shared
/// reduction (which never enlarges) would have returned 72x72 instead - a better picture by
/// most arguments, and a silent change to the OUTPUT DIMENSIONS of a shipped CLI that scripts
/// and the MCP tool depend on. Improving the filter is not a licence to change the contract, so
/// the two are decided separately: shrink better, enlarge identically.
fn fit_for_cli(img: image::DynamicImage, max_dim: u32) -> image::DynamicImage {
    if max_dim == 0 {
        return img;
    }
    if img.width() > max_dim || img.height() > max_dim {
        decode::reduce_to_fit(img, max_dim, max_dim)
    } else {
        img.thumbnail(max_dim, max_dim)
    }
}

/// `max_dim` px on the long edge (`0` = full size). The headline verb: produces
/// previews for the formats Windows itself can't.
///
/// Refuses an `output` that is the `input` under any spelling, and an `output` whose
/// extension names no writable format, before any decode work (2026-09-05 audit, F30).
pub fn thumbnail(input: &str, output: &str, max_dim: u32) -> Result<String, String> {
    thumbnail_reporting(input, output, max_dim).map_err(|(_, e)| e)
}

/// [`thumbnail`], keeping the PHASE that failed alongside the message (2026-09-05 audit,
/// F11), so `batch` can say whether a file was unreadable, undecodable or simply had a
/// destination it could not use. The message is handed back unchanged, so `thumbnail`
/// above reads exactly as it always did.
///
/// The save phase reports as `Unencodable` for the same reason `convert_to_reporting`'s
/// does: `save_atomic` returns encode and rename failures through one error, and `batch`
/// has already created the destination file by the time this runs, so an unwritable
/// destination has failed earlier as `Unwritable`.
fn thumbnail_reporting(
    input: &str,
    output: &str,
    max_dim: u32,
) -> std::result::Result<String, (verbs::OmitCause, String)> {
    use verbs::OmitCause;

    reject_output_alias(output, [input]).map_err(|e| (OmitCause::Unwritable, e))?;
    let format = image::ImageFormat::from_path(output).map_err(|_| {
        (
            OmitCause::Unencodable,
            format!("cannot write {output}: the extension names no image format"),
        )
    })?;
    let archive_ext = Path::new(input)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    if crate::formats::is_archive(archive_ext) {
        reject_oversized_archive(input, crate::settings::max_file_size_bytes())
            .map_err(|e| (OmitCause::Unreadable, e))?;
    }
    // Generic archive (.zip/.rar/.7z): the same list-then-extract path Explorer
    // uses — including the user's MaxSize gate before archive parsing — and the
    // contact sheet composes per the same Setting. Falls through to the normal
    // decode if it isn't really an archive (renamed file) so the magic-dispatch
    // tiers still get their shot.
    if let Some(img) = archive_thumbnail(input) {
        let out = fit_for_cli(img, max_dim);
        save_atomic(&out, output, format).map_err(|e| (OmitCause::Unencodable, e))?;
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
            // `..._for`: this verb named a size, so a head prefix whose baked preview
            // cannot reach it must not stand in for the real picture (issue #33).
            let bytes = decode::read_preview_capped_for(input, edge)
                .map_err(|e| (OmitCause::Unreadable, e.to_string()))?;
            // Cap the decode at the edge we're about to shrink to anyway — the streamed
            // path above already takes `edge`, and rendering ImageMagick's full 4096 first
            // costs seconds on a big scan for pixels this immediately discards.
            decode::decode_preview_capped_for_path(&bytes, edge, input)
                .map_err(|_| (OmitCause::Undecodable, format!("cannot decode {input}")))?
        }
    };
    let out = fit_for_cli(img, max_dim);
    save_atomic(&out, output, format).map_err(|e| (OmitCause::Unencodable, e))?;
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
    let prefs = crate::container::select::CoverPrefs::from_settings();
    let covers = if crate::container::archive_needs_buffer(&head) {
        // RAR buffers whole (`rars` accepts no reader) — same bounded read as the
        // normal path, so a multi-GB .rar fails to the normal decode error.
        let bytes = decode::read_preview_capped(input).ok()?;
        crate::container::archive_covers(&bytes, want, &prefs)?
    } else {
        crate::container::archive_covers_seek(&mut f, &head, want, &prefs)?
    };
    let d = decode::thumbnail_from_covers(&covers, 1024).ok()?;
    image::RgbaImage::from_raw(d.width, d.height, d.rgba).map(image::DynamicImage::ImageRgba8)
}

/// Convert `input` to the exact `output` path at `quality`, optional `resize`.
/// `webp_quality = Some(q)` writes lossy WebP at quality `q` (only meaningful when
/// `output` is a `.webp`); `None` keeps WebP lossless. Refuses an `output` that is the
/// `input` under any spelling (2026-09-05 audit, F30): the write replaces the destination,
/// and there is no in-place form of this verb.
pub fn convert(
    input: &str,
    output: &str,
    quality: u8,
    webp_quality: Option<u8>,
    resize: verbs::Resize,
) -> Result<String, String> {
    reject_output_alias(output, [input])?;
    // Clamp HERE, not just at each front end, so the CLI (which only clamped via
    // `u8::from_str` rejecting out-of-range strings, not in-range-but-silly ones like 0 or
    // 255) and the MCP surface (which already clamped) actually agree on what a "quality"
    // argument means, regardless of which one a caller went through.
    let quality = quality.clamp(1, 100);
    let webp_quality = webp_quality.map(|w| w.clamp(1, 100));
    verbs::convert_to(input, Path::new(output), quality, webp_quality, resize)
        .map_err(|e| format!("convert failed: {input}: {e}"))?;
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
        .map_err(|e| format!("rotate failed: {input}: {e}"))
}

/// Decode `input` and return it as in-memory PNG bytes, fit within `max_dim` (0 = full
/// size). Powers the MCP `view` tool — lets an AI agent SEE any of our supported formats
/// directly (HEIC/RAW/PSD/ebook covers/CAD previews/…), not just convert them to a file.
pub fn view_png(input: &str, max_dim: u32) -> Result<Vec<u8>, String> {
    // An agent asking for a big view of a PSD wants the composite, not the 160 px baked
    // preview stretched to fill it (issue #33). `max_dim == 0` means full size, which is the
    // opposite of `ANY_PREVIEW` - ask for the largest edge there is.
    let want = if max_dim == 0 { u32::MAX } else { max_dim };
    let bytes = decode::read_preview_capped_for(input, want).map_err(|e| e.to_string())?;
    let img = decode::decode_preview_capped_for_path(&bytes, 0, input)
        .map_err(|_| format!("cannot decode {input}"))?;
    let img = fit_for_cli(img, max_dim);
    let mut out = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(out)
}

/// Compress to a target file size → a "(compressed)" JPEG sibling at or under
/// `target_bytes` (quality binary-search + downscale). See [`parse_size`]. A target the
/// search cannot meet fails and writes nothing; the error names the smallest size it can
/// reach (2026-09-05 audit, F32). The MCP `compress` tool shares this exact contract.
pub fn compress(input: &str, target_bytes: u64) -> Result<String, String> {
    verbs::compress_to_size(input, target_bytes)
        .map(|p| p.display().to_string())
        .map_err(|e| format!("compress failed: {input}: {e}"))
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

/// How the multi-input verbs (`pdf`, `cbz`) report and police omitted inputs, from either
/// front door (2026-09-05 audit, F31).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CombineOpts {
    /// Fail, writing nothing, if any input would be left out (`--strict`).
    pub strict: bool,
    /// Return the [`verbs::Combined`] JSON instead of text (`--json`; always on over MCP).
    pub json: bool,
}

impl CombineOpts {
    fn on_omit(self) -> verbs::OnOmit {
        if self.strict {
            verbs::OnOmit::Fail
        } else {
            verbs::OnOmit::Report
        }
    }
}

/// Render a composer's result for the caller. The all-good text form is exactly the output
/// path, as it always was, so a script reading stdout keeps working; a partial result adds
/// a `partial:` status line and one tab-separated `omitted` line per left-out input, the
/// same lines a `--strict` refusal carries. The JSON form is [`verbs::Combined::to_json`].
fn combined_report(c: &verbs::Combined, opts: CombineOpts) -> String {
    if opts.json {
        return c.to_json().to_string();
    }
    let mut s = c.output.display().to_string();
    if c.is_partial() {
        s.push_str(&format!(
            "\npartial: {} of {} inputs combined, {} omitted",
            c.used,
            c.requested(),
            c.omitted.len()
        ));
        for o in &c.omitted {
            s.push('\n');
            s.push_str(&o.as_line());
        }
    }
    s
}

/// Combine images into one PDF (one page each). The destination must be a `.pdf` and must
/// not be one of the inputs (2026-09-05 audit, F30); inputs the composer cannot use are
/// reported per input, or refused outright under `opts.strict` (F31).
pub fn pdf(output: &str, inputs: &[String], opts: CombineOpts) -> Result<String, String> {
    if inputs.is_empty() {
        return Err("no input images".to_string());
    }
    require_output_ext(output, "pdf")?;
    // Same JPEG quality the right-click Combine-to-PDF verb uses (the user's configured
    // setting) — a hardcoded 85 silently diverged from the menu path for no reason.
    let combined = topdf::combine_to_pdf(
        inputs,
        Path::new(output),
        crate::settings::jpeg_quality(),
        opts.on_omit(),
    )
    .map_err(|e| format!("pdf build failed: {}", e.message()))?;
    Ok(combined_report(&combined, opts))
}

/// Combine images into one CBZ (comic-book zip) archive, natural-sorted, with a
/// `ComicInfo.xml` sidecar as the first entry. Same combiner the right-click
/// "Combine to CBZ" verb uses (`verbs::actions::handle_combine_to_cbz`) — this is
/// just its CLI/MCP front door, which never existed even though the PDF sibling
/// always had one. Same destination and omission contract as [`pdf`].
pub fn cbz(output: &str, inputs: &[String], opts: CombineOpts) -> Result<String, String> {
    if inputs.is_empty() {
        return Err("no input images".to_string());
    }
    require_output_ext(output, "cbz")?;
    let combined = verbs::combine_to_cbz(inputs, Path::new(output), opts.on_omit())
        .map_err(|e| format!("cbz build failed: {}", e.message()))?;
    Ok(combined_report(&combined, opts))
}

/// Image dimensions + EXIF (camera/date/GPS/bit depth/DPI), as text or JSON — or, for one
/// of the 18 audio extensions this product already reads tags for (via the property
/// handler and the "Rename by tag" verb), the artist/album/title/track/duration/bitrate
/// tag set instead. Before this fix, every audio `info` call hit the `width == 0` guard
/// below and returned a bare "cannot read" with no indication tags existed at all.
pub fn info(input: &str, json: bool) -> Result<String, String> {
    let ext = Path::new(input)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if formats::category(&ext) == formats::Category::Audio {
        return info_audio(input, json);
    }
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
        // 0.0 means "absent" per `ImageInfo::dpi_x/dpi_y`'s own doc comment; `is_finite`
        // alone would let that through as a bogus "0 dpi" instead of an absent field.
        let dpi_x = (i.dpi_x > 0.0 && i.dpi_x.is_finite()).then_some(i.dpi_x);
        let dpi_y = (i.dpi_y > 0.0 && i.dpi_y.is_finite()).then_some(i.dpi_y);
        Ok(serde_json::json!({
            "width": i.width,
            "height": i.height,
            "bit_depth": i.bit_depth,
            "dpi_x": dpi_x,
            "dpi_y": dpi_y,
            "make": i.make,
            "model": i.model,
            "datetime": i.datetime,
            "gps": gps,
        })
        .to_string())
    } else {
        let mut s = format!("{} x {} px", i.width, i.height);
        if i.bit_depth > 0 {
            s.push_str(&format!("\nbit depth: {}", i.bit_depth));
        }
        if i.dpi_x > 0.0 || i.dpi_y > 0.0 {
            s.push_str(&format!("\ndpi: {:.0} x {:.0}", i.dpi_x, i.dpi_y));
        }
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

/// The audio half of [`info`]: tags via `strip::read_audio_tags` (the same `lofty`
/// read path the "Rename by tag" verb uses), returned as text or JSON. Only errors when
/// NOTHING useful was read (no tag AND no duration AND no bitrate) — per the fix, a file
/// with some tags found must not be reported as unreadable just because others are absent.
fn info_audio(input: &str, json: bool) -> Result<String, String> {
    let t = strip::read_audio_tags(input);
    let found = t.artist.is_some()
        || t.album.is_some()
        || t.title.is_some()
        || t.track.is_some()
        || t.genre.is_some()
        || t.year.is_some()
        || t.duration_ms > 0
        || t.bitrate_kbps > 0;
    if !found {
        return Err(format!("cannot read {input}"));
    }
    if json {
        Ok(serde_json::json!({
            "kind": "audio",
            "artist": t.artist,
            "album": t.album,
            "title": t.title,
            "track": t.track,
            "genre": t.genre,
            "year": t.year,
            "duration_ms": t.duration_ms,
            "bitrate_kbps": t.bitrate_kbps,
        })
        .to_string())
    } else {
        let mut s = String::new();
        if let Some(v) = &t.artist {
            s.push_str(&format!("artist: {v}\n"));
        }
        if let Some(v) = &t.album {
            s.push_str(&format!("album: {v}\n"));
        }
        if let Some(v) = &t.title {
            s.push_str(&format!("title: {v}\n"));
        }
        if let Some(v) = t.track {
            s.push_str(&format!("track: {v}\n"));
        }
        if let Some(v) = &t.genre {
            s.push_str(&format!("genre: {v}\n"));
        }
        if let Some(v) = t.year {
            s.push_str(&format!("year: {v}\n"));
        }
        if t.duration_ms > 0 {
            s.push_str(&format!(
                "duration: {:.1}s\n",
                t.duration_ms as f64 / 1000.0
            ));
        }
        if t.bitrate_kbps > 0 {
            s.push_str(&format!("bitrate: {} kbps\n", t.bitrate_kbps));
        }
        Ok(s.trim_end().to_string())
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

fn expand_inputs_attrs(p: &Path) -> u32 {
    use std::os::windows::fs::MetadataExt;
    std::fs::symlink_metadata(p)
        .map(|m| m.file_attributes())
        .unwrap_or(0)
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
    let a = expand_inputs_attrs(p);
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
/// [`is_cloud_placeholder`]. Second element is how many were skipped as placeholders, so
/// callers can tell the user rather than silently hydrating them.
///
/// `recurse = false` scans each directory ONE level deep (the historical, still-default
/// behaviour — an agent pointed at a photo tree with subfolders used to get a partial
/// result and a clean "N/N succeeded" with no way to ask for more). `recurse = true` walks
/// the whole tree, never following a reparse point (junction/symlink) and capped at
/// [`MAX_RECURSE_DEPTH`] so a cycle can't spin forever — the same two guards
/// `prebuild::walk` applies, duplicated rather than shared because that function is
/// private to `prebuild.rs` and unreachable from here.
fn expand_inputs(inputs: &[String], recurse: bool) -> (Vec<String>, usize) {
    let mut out = Vec::new();
    let mut skipped_offline = 0usize;
    for i in inputs {
        let p = Path::new(i);
        if p.is_dir() {
            expand_inputs_walk(p, recurse, 0, &mut out, &mut skipped_offline);
        } else if p.is_file() && expand_inputs_is_supported(p) {
            if is_cloud_placeholder(p) {
                skipped_offline += 1;
            } else {
                out.push(i.clone());
            }
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

    let (files, skipped_offline) = expand_inputs(inputs, recurse);
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
    let outcomes = crate::parallel::map(&pairs, |_, (input, slot)| job.run(input, slot));
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
    let (files, _skipped_offline) = expand_inputs(inputs, recurse);
    if files.is_empty() {
        return Err("no supported image files found in the inputs".to_string());
    }
    let results = crate::parallel::map(&files, |_, f: &String| -> serde_json::Value {
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
                None => match decode::read_preview_capped_for(input, edge) {
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

        let first = reserve_batch_output(&dir, "img", "webp").unwrap();
        assert_eq!(first, dir.join("img (1).webp"));
        let second = reserve_batch_output(&dir, "img", "webp").unwrap();
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
            false,
            Some(dir.to_str().unwrap()),
            256,
            Some("webp"),
            10, // aggressively lossy
            verbs::Resize::None,
            false,
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

        let (files, skipped) = expand_inputs(&[dir.to_str().unwrap().to_string()], false);
        assert_eq!(skipped, 1);
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("normal.png"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `expand_inputs` must find only the top-level file when `recurse` is false (the
    /// historical default — an agent pointed at a photo tree with subfolders used to get a
    /// partial result and a clean "N/N succeeded" with no way to ask for more), and every
    /// file at every depth when `recurse` is true.
    #[test]
    fn expand_inputs_recurse_flag_controls_subdirectory_depth() {
        let dir = std::env::temp_dir().join(format!(
            "st2k_cli_recurse_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let nested = dir.join("sub").join("deeper");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(dir.join("top.png"), b"top").unwrap();
        std::fs::write(dir.join("sub").join("mid.png"), b"mid").unwrap();
        std::fs::write(nested.join("bottom.png"), b"bottom").unwrap();

        let (shallow, _) = expand_inputs(&[dir.to_str().unwrap().to_string()], false);
        assert_eq!(
            shallow.len(),
            1,
            "non-recursive scan must stay one level deep"
        );
        assert!(shallow[0].ends_with("top.png"));

        let (deep, _) = expand_inputs(&[dir.to_str().unwrap().to_string()], true);
        assert_eq!(deep.len(), 3, "recursive scan must find every depth");
        assert!(deep.iter().any(|p| p.ends_with("top.png")));
        assert!(deep.iter().any(|p| p.ends_with("mid.png")));
        assert!(deep.iter().any(|p| p.ends_with("bottom.png")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `batch`'s `"info"` op returns a JSON array, one element per input, in the same
    /// shape `info(_, true)` returns for a single file — and a per-file decode failure must
    /// not fail the whole batch, just carry an `"error"` field on that one element.
    #[test]
    fn batch_info_op_returns_a_json_array_with_per_file_results() {
        let dir = std::env::temp_dir().join(format!(
            "st2k_cli_batchinfo_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("ok.png");
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(64, 48))
            .save(&good)
            .unwrap();

        let out = batch(
            "info",
            &[good.to_str().unwrap().to_string()],
            false,
            None,
            256,
            None,
            90,
            verbs::Resize::None,
            false,
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["width"], serde_json::json!(64));
        assert_eq!(arr[0]["height"], serde_json::json!(48));
        assert!(arr[0]["input"].as_str().unwrap().ends_with("ok.png"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 2026-09-05 audit, F11, the whole acceptance case in one run: one file that converts,
    /// one whose bytes no decoder takes, and one whose output name cannot be claimed. The
    /// counts have to add up, the two failures have to carry DIFFERENT causes (they need
    /// different fixes: replace the file, versus write somewhere else), and every failure
    /// has to name its input path exactly as it was passed, because that path IS the retry.
    /// Before this, all three came back as `1/3 succeeded (2 failed)` and nothing else.
    ///
    /// The unwritable destination is a FOLDER already sitting on the output name, not an
    /// ACL: Windows answers `create_new` on a directory with access-denied, so this is a
    /// real permission failure the test can set up in one line and clean up in one more.
    #[test]
    fn batch_reports_a_distinct_cause_per_failure_and_a_retryable_list() {
        let dir = std::env::temp_dir().join(format!(
            "st2k_cli_causes_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("good.png");
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(8, 8))
            .save(&good)
            .unwrap();
        let broken = dir.join("broken.png");
        std::fs::write(&broken, b"this is not a PNG, or anything else").unwrap();
        let blocked = dir.join("blocked.png");
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(8, 8))
            .save(&blocked)
            .unwrap();
        std::fs::create_dir(dir.join("blocked.webp")).unwrap();

        let inputs: Vec<String> = [&good, &broken, &blocked]
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let run = |json| {
            batch(
                "convert",
                &inputs,
                false,
                None,
                256,
                Some("webp"),
                90,
                verbs::Resize::None,
                json,
            )
        };

        let v: serde_json::Value = serde_json::from_str(&run(true).unwrap()).unwrap();
        assert_eq!(v["status"], "partial");
        assert_eq!(v["requested"], 3);
        assert_eq!(v["succeeded"], 1);
        assert_eq!(v["failed"], 2);
        let results = v["results"].as_array().unwrap();
        assert_eq!(results.len(), 3, "one entry per input, in input order");

        assert_eq!(results[0]["status"], "ok");
        assert_eq!(results[0]["cause"], serde_json::Value::Null);
        assert!(results[0]["output"]
            .as_str()
            .unwrap()
            .ends_with("good.webp"));

        assert_eq!(results[1]["status"], "failed");
        assert_eq!(
            results[1]["cause"], "undecodable",
            "bytes no decoder takes is a bad INPUT: {}",
            results[1]
        );
        assert_eq!(results[2]["status"], "failed");
        assert_eq!(
            results[2]["cause"], "unwritable",
            "a destination that cannot be claimed is a bad OUTPUT: {}",
            results[2]
        );
        assert_ne!(
            results[1]["cause"], results[2]["cause"],
            "two different problems must not report one cause"
        );
        for (i, expected) in [(1usize, &broken), (2, &blocked)] {
            assert_eq!(
                results[i]["input"].as_str().unwrap(),
                expected.to_string_lossy(),
                "a failed entry must be retryable by the path it was given"
            );
            assert!(
                !results[i]["detail"].as_str().unwrap().is_empty(),
                "a cause without a sentence explains nothing to a person"
            );
        }
        assert!(
            results[2]["output"].is_null(),
            "nothing was written for the blocked file"
        );

        // The human form keeps its one-line summary and adds one tab-separated line per
        // failure, the same shape `pdf`/`cbz` print for an omitted input.
        let text = run(false).unwrap();
        assert!(
            text.starts_with("1/3 succeeded (2 failed)"),
            "the summary line is unchanged: {text}"
        );
        assert!(
            text.contains(&format!("failed\t{}\tundecodable\t", broken.display()))
                && text.contains(&format!("failed\t{}\tunwritable\t", blocked.display())),
            "each failure is named with its cause: {text}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A caller that bypasses both front ends' own clamping (a raw library call, or a
    /// future front end that forgets to clamp) must still get a sane encoder quality —
    /// `convert`/`batch` own the clamp now, not just `mcp.rs`.
    #[test]
    fn convert_and_batch_clamp_quality_into_1_to_100() {
        let dir = std::env::temp_dir().join(format!(
            "st2k_cli_qclamp_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("a.png");
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(32, 32))
            .save(&src)
            .unwrap();

        // quality: 0 must not panic/misbehave the encoder — it should behave as if 1 was
        // requested, not literally zero.
        let out = dir.join("a.jpg");
        convert(
            src.to_str().unwrap(),
            out.to_str().unwrap(),
            0,
            None,
            verbs::Resize::None,
        )
        .unwrap();
        assert!(out.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The CBZ front door must exist and actually produce a readable archive —
    /// `combine_to_cbz` itself is already tested in `verbs.rs`; this pins the CLI/MCP-facing
    /// `cli::cbz` wrapper specifically (the missing piece the review found).
    #[test]
    fn cbz_combines_images_into_a_readable_archive() {
        let dir = std::env::temp_dir().join(format!(
            "st2k_cli_cbz_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("page1.png");
        let b = dir.join("page2.png");
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(10, 10))
            .save(&a)
            .unwrap();
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(10, 10))
            .save(&b)
            .unwrap();

        let out = dir.join("comic.cbz");
        let text = cbz(
            out.to_str().unwrap(),
            &[
                a.to_str().unwrap().to_string(),
                b.to_str().unwrap().to_string(),
            ],
            CombineOpts::default(),
        )
        .unwrap();
        assert_eq!(
            text,
            out.to_str().unwrap(),
            "an all-good combine prints exactly the output path"
        );
        assert!(out.exists());
        let f = std::fs::File::open(&out).unwrap();
        let zip = zip::ZipArchive::new(f).unwrap();
        assert!(
            zip.len() >= 2,
            "expected at least the two pages plus the sidecar"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An audio file must never hit the "cannot read" path just because it has
    /// no width/height — and a file with NO readable tags at all (garbage bytes) must still
    /// error, rather than claiming success with an empty tag set.
    #[test]
    fn info_on_unparseable_audio_bytes_still_errors() {
        let dir = std::env::temp_dir().join(format!(
            "st2k_cli_audioinfo_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let bogus = dir.join("not_really.mp3");
        std::fs::write(&bogus, b"this is not a real mp3 file").unwrap();

        let err = info(bogus.to_str().unwrap(), true).unwrap_err();
        assert!(err.contains("cannot read"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "st2k_cli_{tag}_{}_{}",
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

    fn save_png(path: &Path, w: u32, h: u32) -> String {
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            w,
            h,
            image::Rgb([40, 90, 200]),
        ))
        .save(path)
        .unwrap();
        path.to_str().unwrap().to_string()
    }

    /// 2026-09-05 audit, F30: `thumbnail same.png same.png` used to decode the file and then
    /// save the 128 px result over it, destroying the original. Every spelling of the input
    /// (itself, a case variant, a `..` detour, a hard link) must be refused BEFORE anything
    /// is written, and the input must be byte-identical afterwards. Against the pre-fix code
    /// this fails at the first `is_err` (the call succeeds) and the bytes differ.
    #[test]
    fn thumbnail_refuses_every_spelling_of_its_own_input() {
        let dir = scratch("alias");
        let src = save_png(&dir.join("Photo.png"), 300, 200);
        let before = std::fs::read(&src).unwrap();
        let link = dir.join("link.png");
        std::fs::hard_link(&src, &link).unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();

        let aliases = [
            src.clone(),
            dir.join("PHOTO.PNG").to_string_lossy().into_owned(),
            dir.join("sub")
                .join("..")
                .join("Photo.png")
                .to_string_lossy()
                .into_owned(),
            link.to_string_lossy().into_owned(),
        ];
        for alias in &aliases {
            let err = thumbnail(&src, alias, 128).expect_err(alias);
            assert!(err.contains("same file"), "the refusal must say why: {err}");
            assert_eq!(
                std::fs::read(&src).unwrap(),
                before,
                "the input was modified via alias {alias}"
            );
        }

        // A distinct destination still works.
        let out = dir.join("thumb.png");
        thumbnail(&src, out.to_str().unwrap(), 128).unwrap();
        assert!(image::open(&out).unwrap().width() <= 128);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 2026-09-05 audit, F30: `thumbnail` wrote its output with a plain `DynamicImage::save`,
    /// which truncates the destination BEFORE encoding, so an encode failure left a 0-byte
    /// file where a good one had been. The write now goes through the shared atomic writer.
    /// ICO is the deterministic failure: its header cannot express an edge over 256 px, so a
    /// full-size render of a 400 px source fails inside the encoder, after the file is open.
    /// Against the pre-fix code the existing `out.ico` is left at 0 bytes.
    #[test]
    fn a_failed_thumbnail_write_leaves_an_existing_output_intact() {
        let dir = scratch("atomic");
        let src = save_png(&dir.join("big.png"), 400, 300);
        let out = dir.join("out.ico");
        let existing = b"an existing icon the user wants to keep".to_vec();
        std::fs::write(&out, &existing).unwrap();

        let err = thumbnail(&src, out.to_str().unwrap(), 0).expect_err("ico cannot hold 400 px");
        assert!(!err.is_empty());
        assert_eq!(
            std::fs::read(&out).unwrap(),
            existing,
            "a failed write must not touch the existing destination"
        );
        assert!(
            !verbs::with_tmp_suffix(&out).exists(),
            "the temp file must be cleaned up"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 2026-09-05 audit, F31: a PDF from one good PNG, one corrupt PNG and one missing file
    /// used to exit 0 printing only the output path. The report now carries the counts, a
    /// `partial` status and one machine-readable line per omitted input with a DISTINCT
    /// cause (corrupt = undecodable, missing = unreadable), in both the text and the JSON
    /// form; `strict` refuses to write at all. Against the pre-fix code the text is the bare
    /// path (no `partial:` line) and the JSON form does not exist.
    #[test]
    fn pdf_reports_each_omitted_input_with_its_cause_and_strict_refuses() {
        let dir = scratch("pdf_partial");
        let good = save_png(&dir.join("good.png"), 30, 20);
        let corrupt = dir.join("corrupt.png");
        std::fs::write(&corrupt, b"not a png at all").unwrap();
        let corrupt = corrupt.to_str().unwrap().to_string();
        let missing = dir.join("missing.png").to_str().unwrap().to_string();
        let inputs = [good.clone(), corrupt.clone(), missing.clone()];

        let out = dir.join("out.pdf");
        let text = pdf(out.to_str().unwrap(), &inputs, CombineOpts::default()).unwrap();
        let mut lines = text.lines();
        assert_eq!(
            lines.next(),
            out.to_str(),
            "first line stays the output path"
        );
        assert_eq!(
            lines.next(),
            Some("partial: 1 of 3 inputs combined, 2 omitted")
        );
        let omitted: Vec<Vec<&str>> = lines.map(|l| l.split('\t').collect()).collect();
        assert_eq!(omitted.len(), 2, "{text}");
        for row in &omitted {
            assert_eq!(row[0], "omitted");
            assert_eq!(
                row.len(),
                4,
                "omitted<TAB>input<TAB>cause<TAB>detail: {row:?}"
            );
        }
        let cause_of = |input: &str| {
            omitted
                .iter()
                .find(|r| r[1] == input)
                .map(|r| r[2])
                .unwrap_or_else(|| panic!("{input} not listed in {text}"))
        };
        assert_eq!(cause_of(&corrupt), "undecodable");
        assert_eq!(cause_of(&missing), "unreadable");

        // The JSON form carries the same facts for an MCP caller.
        let json_out = dir.join("out2.pdf");
        let opts = CombineOpts {
            json: true,
            ..CombineOpts::default()
        };
        let v: serde_json::Value =
            serde_json::from_str(&pdf(json_out.to_str().unwrap(), &inputs, opts).unwrap()).unwrap();
        assert_eq!(v["status"], "partial");
        assert_eq!(v["requested"], 3);
        assert_eq!(v["combined"], 1);
        assert_eq!(v["output"], json_out.to_str().unwrap());
        let causes: Vec<(&str, &str)> = v["omitted"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| (o["input"].as_str().unwrap(), o["cause"].as_str().unwrap()))
            .collect();
        assert!(causes.contains(&(corrupt.as_str(), "undecodable")), "{v}");
        assert!(causes.contains(&(missing.as_str(), "unreadable")), "{v}");

        // Strict: fail, name both, write nothing.
        let strict_out = dir.join("strict.pdf");
        let opts = CombineOpts {
            strict: true,
            ..CombineOpts::default()
        };
        let err = pdf(strict_out.to_str().unwrap(), &inputs, opts).unwrap_err();
        assert!(!strict_out.exists(), "strict must not write a partial PDF");
        assert!(err.contains("strict"), "{err}");
        assert!(err.contains(&corrupt) && err.contains(&missing), "{err}");

        // An all-good combine still prints exactly the path, and its JSON says "ok".
        let clean = dir.join("clean.pdf");
        assert_eq!(
            pdf(
                clean.to_str().unwrap(),
                std::slice::from_ref(&good),
                CombineOpts::default()
            )
            .unwrap(),
            clean.to_str().unwrap()
        );
        let opts = CombineOpts {
            json: true,
            ..CombineOpts::default()
        };
        let clean2 = dir.join("clean2.pdf");
        let v: serde_json::Value =
            serde_json::from_str(&pdf(clean2.to_str().unwrap(), &[good], opts).unwrap()).unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["omitted"].as_array().map(Vec::len), Some(0));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The CBZ sibling of the test above: a missing input is listed as `unreadable` with its
    /// count, and `strict` writes nothing. Against the pre-fix code the text is the bare path.
    #[test]
    fn cbz_reports_a_missing_input_as_unreadable_and_strict_refuses() {
        let dir = scratch("cbz_partial");
        let good = save_png(&dir.join("p1.png"), 10, 10);
        let missing = dir.join("p2.png").to_str().unwrap().to_string();
        let inputs = [good, missing.clone()];

        let out = dir.join("out.cbz");
        let text = cbz(out.to_str().unwrap(), &inputs, CombineOpts::default()).unwrap();
        assert!(
            text.contains("partial: 1 of 2 inputs combined, 1 omitted"),
            "{text}"
        );
        assert!(
            text.contains(&format!("omitted\t{missing}\tunreadable\t")),
            "{text}"
        );
        let zip = zip::ZipArchive::new(std::fs::File::open(&out).unwrap()).unwrap();
        assert_eq!(zip.len(), 2, "the sidecar plus the one readable page");

        let strict_out = dir.join("strict.cbz");
        let opts = CombineOpts {
            strict: true,
            ..CombineOpts::default()
        };
        let err = cbz(strict_out.to_str().unwrap(), &inputs, opts).unwrap_err();
        assert!(!strict_out.exists(), "strict must not write a partial CBZ");
        assert!(err.contains(&missing), "{err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 2026-09-05 audit, F30: `pdf same.png same.png` and `cbz same.png same.png` exited 0
    /// and replaced the PNG with PDF/ZIP bytes. The destination must carry the composer's
    /// extension, and an output that IS an input (here a real PDF/CBZ re-combined over itself,
    /// the same-type alias the extension test cannot catch) is refused with the source left
    /// byte-identical. Against the pre-fix code every one of these calls succeeds.
    #[test]
    fn pdf_and_cbz_refuse_a_foreign_extension_and_an_output_that_is_an_input() {
        let dir = scratch("pdf_cbz_alias");
        let png = save_png(&dir.join("same.png"), 30, 20);
        let png_before = std::fs::read(&png).unwrap();

        type Verb = fn(&str, &[String], CombineOpts) -> Result<String, String>;
        let verbs: [(Verb, &str); 2] = [(pdf, "pdf"), (cbz, "cbz")];
        for (verb, name) in verbs {
            let err =
                verb(&png, std::slice::from_ref(&png), CombineOpts::default()).expect_err(name);
            assert!(err.contains(&format!(".{name}")), "{name}: {err}");
            assert_eq!(
                std::fs::read(&png).unwrap(),
                png_before,
                "{name} touched the PNG"
            );
        }

        let doc = dir.join("doc.pdf");
        pdf(
            doc.to_str().unwrap(),
            std::slice::from_ref(&png),
            CombineOpts::default(),
        )
        .unwrap();
        let doc_before = std::fs::read(&doc).unwrap();
        let err = pdf(
            dir.join("DOC.PDF").to_str().unwrap(),
            &[png.clone(), doc.to_str().unwrap().to_string()],
            CombineOpts::default(),
        )
        .expect_err("a PDF re-combined over itself");
        assert!(err.contains("same file"), "{err}");
        assert_eq!(
            std::fs::read(&doc).unwrap(),
            doc_before,
            "the PDF was modified"
        );

        let comic = dir.join("comic.cbz");
        cbz(
            comic.to_str().unwrap(),
            std::slice::from_ref(&png),
            CombineOpts::default(),
        )
        .unwrap();
        let comic_before = std::fs::read(&comic).unwrap();
        let err = cbz(
            comic.to_str().unwrap(),
            &[png, comic.to_str().unwrap().to_string()],
            CombineOpts::default(),
        )
        .expect_err("a CBZ re-combined over itself");
        assert!(err.contains("same file"), "{err}");
        assert_eq!(
            std::fs::read(&comic).unwrap(),
            comic_before,
            "the CBZ was modified"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 2026-09-05 audit, F30: `convert` has no in-place form either, so the same alias
    /// guard applies; a distinct destination still converts.
    #[test]
    fn convert_refuses_an_output_that_is_its_input() {
        let dir = scratch("convert_alias");
        let src = save_png(&dir.join("a.png"), 20, 20);
        let before = std::fs::read(&src).unwrap();
        let err = convert(&src, &src, 90, None, verbs::Resize::None).unwrap_err();
        assert!(err.contains("same file"), "{err}");
        assert_eq!(std::fs::read(&src).unwrap(), before);
        let out = dir.join("a.jpg");
        convert(&src, out.to_str().unwrap(), 90, None, verbs::Resize::None).unwrap();
        assert!(out.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 2026-09-05 audit, F32: `compress x.png --max-size 1` used to succeed with a 628-byte
    /// JPEG while the help promised "at or under". One policy now: a target the search cannot
    /// meet (0, 1) fails, writes nothing, and names the smallest size it can reach; an exact
    /// fit and a normal target succeed at or under the target. Against the pre-fix code the
    /// first two calls succeed and leave a "(compressed)" file behind.
    #[test]
    fn compress_refuses_an_impossible_target_and_honours_a_feasible_one() {
        let dir = scratch("compress_policy");
        // Per-pixel noise so the JPEG has real size to search over.
        let img = image::RgbImage::from_fn(96, 96, |x, y| {
            let h = (x.wrapping_mul(0x9E37_79B9) ^ y.wrapping_mul(0x85EB_CA6B)).rotate_left(7);
            image::Rgb([h as u8, (h >> 8) as u8, (h >> 16) as u8])
        });
        let src = dir.join("noise.png");
        image::DynamicImage::ImageRgb8(img).save(&src).unwrap();
        let src = src.to_str().unwrap().to_string();
        let sibling_count = || {
            std::fs::read_dir(&dir)
                .unwrap()
                .filter(|e| e.is_ok())
                .count()
        };

        for impossible in [0u64, 1] {
            let err = compress(&src, impossible).unwrap_err();
            assert!(
                err.contains(&format!("cannot fit in {impossible} bytes"))
                    && err.contains("smallest")
                    && err.contains("nothing was written"),
                "{err}"
            );
            assert_eq!(
                sibling_count(),
                1,
                "a refused compress must write nothing: {err}"
            );
        }

        // A normal target: the output is at or under it.
        let normal = compress(&src, 100_000).unwrap();
        let normal_len = std::fs::metadata(&normal).unwrap().len();
        assert!(
            normal_len <= 100_000,
            "{normal_len} bytes over a 100000-byte target"
        );

        // An exact fit: asking for precisely what the search produced must succeed at
        // exactly that size, not be refused as "over".
        let exact = compress(&src, normal_len).unwrap();
        assert_eq!(std::fs::metadata(&exact).unwrap().len(), normal_len);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `compress`'s `map_err` must not drop the underlying error — an MCP/agent caller
    /// needs the REAL reason (missing file, decode failure, ...), not a bare "compress
    /// failed: <input>" with nothing else to act on.
    #[test]
    fn compress_error_message_keeps_the_underlying_reason() {
        let err = compress("this_file_does_not_exist_at_all.png", 100_000).unwrap_err();
        assert!(
            err.len() > "compress failed: this_file_does_not_exist_at_all.png".len(),
            "error message dropped the underlying reason: {err}"
        );
    }
}
