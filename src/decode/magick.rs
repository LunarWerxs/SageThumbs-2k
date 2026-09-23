//! ImageMagick discovery, policy, process isolation, decode, and encode support.

use super::*;
mod encode;
#[cfg(test)]
use encode::*;
mod named;
#[cfg(test)]
use named::*;
mod sniff;
use sniff::*;
mod budget;
use budget::*;
mod child;
pub(super) use budget::add_magick_limits;
pub(crate) use budget::Fidelity;
pub(crate) use child::await_magick_output;
use child::*;
pub use encode::{
    encode_via_magick, encode_via_magick_png, magick_output_extensions, magick_output_supported,
};
pub(crate) use named::is_raw_coder_ext;
pub(super) use named::{
    decode_named_extension, decode_named_extension_native, has_name_selected_coder,
};
pub(crate) use sniff::looks_like_metafile;
pub(super) use sniff::metafile_min_density;

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
    // Deliberately NO bare-"magick.exe" PATH fallback: Windows' CreateProcess
    // search order includes the current directory, so a bare name could run a
    // malicious magick.exe planted in a browsed folder. We only ever launch an
    // absolute path (bundled or Program Files); if none is found the tier is
    // simply skipped and the obscure format falls back to its default icon.
    bundled_magick().or_else(program_files_magick)
}

/// `magick.exe` next to this module (the Full install bundles it there).
fn bundled_magick() -> Option<PathBuf> {
    let dll = crate::host::module_path().ok()?;
    let p = std::path::Path::new(&dll).parent()?.join("magick.exe");
    p.exists().then_some(p)
}

/// The first `magick.exe` under any `C:\Program Files[ (x86)]\ImageMagick*` (a developer
/// PC's own install; never shipped).
fn program_files_magick() -> Option<PathBuf> {
    ["ProgramFiles", "ProgramFiles(x86)"]
        .into_iter()
        .filter_map(|var| std::env::var(var).ok())
        .filter_map(|base| std::fs::read_dir(base).ok())
        .flat_map(|entries| entries.flatten())
        .filter(|e| e.file_name().to_string_lossy().starts_with("ImageMagick"))
        .map(|e| e.path().join("magick.exe"))
        .find(|p| p.exists())
}

/// Constrain ImageMagick to the tree that contains the exact executable we found.
/// Setting only `MAGICK_CONFIGURE_PATH` is insufficient on Windows: an installed
/// ImageMagick registry entry can otherwise supply coder modules, making a broken
/// bundle appear healthy on a developer PC and fail on clean Windows.
///
/// The hardened app-local policy wins when present; a development fallback to a
/// Program Files executable otherwise uses that executable's own configuration.
fn apply_magick_environment(cmd: &mut Command, exe: &std::path::Path) {
    let Some(home) = exe.parent() else {
        return;
    };
    let coder_path = home.join("modules").join("coders");
    let filter_path = home.join("modules").join("filters");

    cmd.env("MAGICK_HOME", home);
    // Set these even if a damaged installation is missing the directories:
    // falling back to registry-discovered modules would hide the damage and
    // reintroduce cross-install module loading. A missing tree must fail closed.
    cmd.env("MAGICK_CODER_MODULE_PATH", &coder_path);
    cmd.env("MAGICK_FILTER_MODULE_PATH", &filter_path);

    let app_policy_dir = crate::host::module_path()
        .ok()
        .and_then(|module| {
            std::path::Path::new(&module)
                .parent()
                .map(std::path::Path::to_path_buf)
        })
        .filter(|dir| dir.join("policy.xml").is_file());
    let configure_path = app_policy_dir
        .as_deref()
        .or_else(|| home.join("policy.xml").is_file().then_some(home));
    if let Some(configure_path) = configure_path {
        cmd.env("MAGICK_CONFIGURE_PATH", configure_path);
    }

    // Keep PATH from reintroducing a second ImageMagick/MinGW tree. The Windows
    // loader searches the executable directory first; these entries retain only
    // inbox DLL discovery for the remainder.
    if let Some(system_root) = std::env::var_os("SystemRoot") {
        let mut path = home.as_os_str().to_os_string();
        path.push(";");
        path.push(std::path::Path::new(&system_root).join("System32"));
        path.push(";");
        path.push(system_root);
        cmd.env("PATH", path);
    }
}

/// Decode via the ImageMagick CLI as an isolated child process: write the image
/// bytes to its stdin, read a PNG back from its stdout, decode that PNG with the
/// safe `image` tier. Bounded by ImageMagick's own `-limit`s AND an external
/// kill-timeout so a hostile/looping input can't hang or crash our host.
///
/// Asks magick for no more than `max_edge` px on the long side. `None` means the
/// [`MAGICK_MAX_EDGE`] guard, i.e. full fidelity; every thumbnail caller passes its own
/// target instead. There is deliberately NO uncapped convenience alias: one existed, and
/// the AVIF/HEIC colour route reached for it by accident and rendered 4096 px for a 256 px
/// tile. Making the cap an explicit argument at every call site is what stops that
/// recurring.
///
/// The default ceiling is [`MAGICK_MAX_EDGE`] (4096), a MEMORY guard rather than a quality
/// floor: the result is downscaled to the caller's box straight afterwards. When the caller
/// already knows it wants a 256 px tile or a 1024 px preview, making magick render 4096 px
/// is work thrown away twice over, because we then PNG-encode that surface and decode it
/// back through the `image` tier.
///
/// It is not a small effect on big images. A 9958x7686 (76 MP) JPEG 2000 scan, the file from
/// issue #11, best of three on an idle machine:
///
/// | target | magick alone | PNG handed back | whole decode |
/// |---|---|---|---|
/// | 4096 (the old fixed cap) | 7.1 s | 22 MB | 9.0 s |
/// | 1024 (the preview's target) | 4.0 s | 1.8 MB | 5.0 s |
/// | 256 (an Explorer tile) | 3.6 s | 0.2 MB | 4.4 s |
///
/// Under load that 9 s crossed the pane's 12 s budget, so it gave up and showed nothing on a
/// file that decodes fine. What is left is openjpeg's own wavelet decode of 76 MP (the
/// "magick alone" column), which this cannot touch: we are now within ~0.5 s of that floor.
///
/// NOT usable for the rest: `-define jp2:reduce-factor=N` decodes a single resolution level
/// and looks like the obvious 17x win (0.29 s). On this file the bundled openjpeg returns the
/// correct REDUCED DIMENSIONS with the wrong CONTENT — the top-left quadrant rather than the
/// whole image downscaled — so it silently produces a thumbnail of the wrong thing. Verified
/// visually, not just by timing. Do not reach for it again without checking the pixels.
///
/// `max_edge` is clamped to the 4096 guard, so a caller can only ask for less, never more.
fn resize_spec(max_edge: Option<u32>) -> String {
    match max_edge {
        Some(e) => {
            // `>` keeps it shrink-only, so a small image is never blown up to the cap.
            let e = e.clamp(1, MAGICK_MAX_EDGE_PX);
            format!("{e}x{e}>")
        }
        None => MAGICK_MAX_EDGE.to_string(),
    }
}

pub(super) fn decode_via_magick_capped(
    bytes: &[u8],
    max_edge: Option<u32>,
    fidelity: Fidelity,
) -> Result<DynamicImage> {
    // Metafiles get a much tighter, format-specific child budget whoever is asking. A slow
    // vector WMF would otherwise grind for seconds to a near-blank frame; a raster decode
    // keeps the budget its caller's [`Fidelity`] earns.
    let is_meta = looks_like_metafile(bytes);
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
        (magick_stdin_spec(bytes), &[])
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
    let edge = resize_spec(max_edge);
    decode_via_magick_spec(bytes, &pre_input, input, pre_ops, &edge, fidelity, is_meta)
}

/// The low-overhead AVIF `mini` box is a valid top-level ISOBMFF image
/// container, but ImageMagick's stdin auto-sniffer does not recognize it. Its
/// HEIC coder does decode it when given an explicit AVIF input specifier.
///
/// Do not scan for the four bytes `mini`: random input could contain those,
/// and forcing it through the AVIF decoder would skip ImageMagick's normal
/// format detection. Require the low-overhead `mif3` structural brand plus its
/// `avif` codec minor-version signal, then walk only bounded, checked
/// *top-level* boxes looking for `mini`.
fn magick_stdin_spec(bytes: &[u8]) -> &'static str {
    if is_mini_avif(bytes) {
        "avif:-"
    } else if is_image_sequence(bytes) {
        // Only frame 0 is ever used, and a bare `-` decodes EVERY frame of the sequence first:
        // measured on the corpus's real.heics, 5.0 s against 0.54 s for the same picture, which
        // left a normal-size sequence's preview pane blank on a loaded machine.
        "-[0]"
    } else {
        "-"
    }
}

/// A HEIF or AVIF image SEQUENCE (major brand `msf1`, `hevs` or `avis`): many frames, of which
/// a thumbnail or preview takes the first.
fn is_image_sequence(bytes: &[u8]) -> bool {
    bytes.get(4..8) == Some(b"ftyp")
        && matches!(bytes.get(8..12), Some(b"msf1" | b"hevs" | b"avis"))
}

/// The PSD/PSB composite at full resolution. Frame `[0]` of a PSD in ImageMagick
/// is the flattened composite (the file format's mandatory precomposed image-data
/// section), not a layer. Capped at MAX_DIM (bomb guard, shrink-only `>`) instead
/// of the thumbnail tier's 4096 — the whole point is keeping the real pixels.
///
/// The re-decode of magick's PNG runs with [`limits::FULL_FIDELITY_MAX_ALLOC`]
/// (not the default 512 MiB): the resize cap is MAX_DIM, so a near-square
/// composite at ~16384² needs ~1 GiB and would otherwise be silently rejected by
/// the `image` tier — making a >~134 MP PSD fall back to its 160px baked-in
/// thumbnail. This PNG is OUR OWN re-encode (its dimensions are already bounded
/// by the resize spec), so the wider allocation is safe here.
///
/// Our own reader answers first (`container::psdmerged`): it reads the same composite, or
/// flattens the layers of a document saved without one, and it is the reader the shell's
/// stream cascade and the Quick preview use for a file past the input ceiling, so a document
/// cannot draw one picture under that ceiling and another past it (the big-file gate,
/// 2026-09-23). It also gets right what ImageMagick does not: a 32-bit document's linear light
/// (ImageMagick writes it without the sRGB curve) and Photoshop's D50 Lab. ImageMagick stays the
/// fallback for what it declines (Multichannel, a ZIP composite).
pub(super) fn decode_psd_composite(bytes: &[u8], fidelity: Fidelity) -> Result<DynamicImage> {
    let edge = limits::MAX_DIM;
    if let Some(img) = crate::container::psd_merged_from_reader(std::io::Cursor::new(bytes), edge) {
        return Ok(img);
    }
    decode_psd_composite_magick(bytes, fidelity)
}

/// [`decode_psd_composite`]'s ImageMagick half on its own: the fallback, and the independent
/// reading the Photoshop reader's tests compare against.
pub(crate) fn decode_psd_composite_magick(
    bytes: &[u8],
    fidelity: Fidelity,
) -> Result<DynamicImage> {
    decode_via_magick_spec_alloc(
        bytes,
        &[],
        "-[0]",
        &[],
        limits::FULL_FIDELITY_EDGE,
        FULL_FIDELITY_CAPS,
        fidelity,
        false, // PSD is never a metafile
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
    fidelity: Fidelity,
    is_meta: bool,
) -> Result<DynamicImage> {
    decode_via_magick_spec_alloc(
        bytes, pre_input, input, pre_ops, max_edge, TILE_CAPS, fidelity, is_meta,
    )
}

/// Worst-case bytes the decode path's stdout can legitimately carry: every call site
/// caps geometry at [`MAGICK_MAX_EDGE_PX`] (4096) before asking magick to write a PNG,
/// and the bundled build is Q16 and writes 16-BIT PNGs, so 4096x4096 16-bit RGBA
/// (128 MiB) is the ceiling with framing overhead on top. Without this, a starved-but-
/// alive magick child could stream unbounded bytes into this process for the whole
/// CPU/wall budget window below.
const MAGICK_PNG_CAP: usize =
    (MAGICK_MAX_EDGE_PX * MAGICK_MAX_EDGE_PX * 4 * 2 + 16 * 1024 * 1024) as usize;

/// Child-output cap for the FULL-FIDELITY paths (the PSD composite and the native RAW
/// re-read), whose resize edge is MAX_DIM rather than 4096.
///
/// [`MAGICK_PNG_CAP`] is sized for the 4096 tier and a full-fidelity decode blows straight
/// through it: the bundled magick is a Q16 build, so it writes 16-BIT PNGs, and the measured
/// hand-back for a 21 MP Mamiya `.mef` at native size is **107 MB**. Under the 64 MiB cap that
/// decode silently "failed" and the caller fell back to the 4096 result — the native path
/// shipped and did nothing.
///
/// 512 MiB (the same figure as `limits::MAX_ALLOC`) bounds our transient the same way, and
/// covers 16-bit photographic PNGs up to roughly 65-90 MP. It is a MEMORY bound, not a
/// geometry guarantee: a 150 MP Phase One back can legitimately exceed it, and when it does
/// the caller's capped retry still delivers the 4096 version rather than nothing.
const FULL_FIDELITY_PNG_CAP: usize = 512 * 1024 * 1024;

/// The two memory bounds a magick child decode runs under, raised IN STEP for the
/// full-fidelity paths: `max_alloc` bounds the `image`-tier re-decode of the PNG the
/// child hands back, `png_cap` bounds the hand-back itself. One struct because passing
/// them separately is how they drift apart — the native RAW path shipped with the alloc
/// raised and the PNG cap still at the 4096 tier's 64 MiB, so its 107 MB hand-back
/// "failed" and the feature silently did nothing.
#[derive(Clone, Copy)]
struct DecodeCaps {
    max_alloc: u64,
    png_cap: usize,
}

/// Ordinary raster decodes: the 4096-edge tier's budgets.
const TILE_CAPS: DecodeCaps = DecodeCaps {
    max_alloc: MAX_ALLOC,
    png_cap: MAGICK_PNG_CAP,
};

/// Full-fidelity decodes (PSD composite, native RAW re-read): the MAX_DIM edge with the
/// matching re-decode allocation and child-output cap (see [`FULL_FIDELITY_PNG_CAP`]).
const FULL_FIDELITY_CAPS: DecodeCaps = DecodeCaps {
    max_alloc: limits::FULL_FIDELITY_MAX_ALLOC,
    png_cap: FULL_FIDELITY_PNG_CAP,
};

/// As [`decode_via_magick_spec`], but with explicit memory caps — used by the
/// full-fidelity paths, whose larger resize edge needs both raised in step.
// Every parameter is a distinct knob its two callers set differently; bundling them would
// only move the list into a struct literal.
#[allow(clippy::too_many_arguments)]
fn decode_via_magick_spec_alloc(
    bytes: &[u8],
    pre_input: &[&str],
    input: &str,
    pre_ops: &[&str],
    max_edge: &str,
    caps: DecodeCaps,
    fidelity: Fidelity,
    is_meta: bool,
) -> Result<DynamicImage> {
    // Derived here, from the two facts that decide it, so a caller cannot hand in a budget
    // that disagrees with the fidelity its gate wait and its `-limit time` are taken from.
    let budget = budget_for(fidelity, is_meta);
    let DecodeCaps { max_alloc, png_cap } = caps;
    let Some(exe) = magick_exe() else {
        crate::safety::log_debug("magick decode: ImageMagick not available");
        return Err(Error::from(E_FAIL));
    };
    let mut cmd = Command::new(exe);
    add_magick_limits(&mut cmd, budget.wall);
    if is_meta {
        // Must follow the shared caps: ImageMagick applies the last resource
        // setting, leaving every non-metafile invocation on the normal budget.
        add_metafile_magick_limits(&mut cmd);
    }
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
    apply_magick_environment(&mut cmd, exe);
    // Bound concurrent magick children (memory) across in-process + st2k fan-out.
    // Held until this function returns (after the child is reaped).
    let _permit = magick_gate::acquire_for(fidelity);
    // Every failure below is LOGGED, not just returned. This tier is the one we route AVIF
    // to precisely because the fallback (WIC) gets those files wrong, so a silent Err here
    // reappears as a wrong-coloured thumbnail with nothing in the log to explain it — which
    // is how issue #9 stayed invisible. Process creation really can fail on a machine that
    // is out of resources, so it needs a breadcrumb like every other tier has.
    let mut child = cmd.spawn().map_err(|e| {
        crate::safety::log_debugf!("magick decode: could not start the child: {e}");
        Error::from(E_FAIL)
    })?;

    // stdin fed and stdout read on their own threads, so a full pipe can't deadlock us; the
    // main thread enforces the budget.
    let (tx, rx) = std::sync::mpsc::channel();
    let Some((writer, reader)) =
        crate::safety::start_child_pipes(&mut child, bytes.to_vec(), move |stdout| {
            let mut buf = Vec::new();
            // Capped so a hostile/misbehaving child can't balloon our memory before the
            // CPU/wall watchdog below gets a chance to kill it (see MAGICK_PNG_CAP).
            let _ = stdout.take((png_cap + 1) as u64).read_to_end(&mut buf);
            let _ = tx.send(buf);
        })
    else {
        crate::safety::log_debug("magick decode: the child's pipes or pipe threads failed");
        return Err(Error::from(E_FAIL));
    };

    // Drain stderr on its own thread too (capped) so a chatty/failing magick
    // can't fill the pipe and stall, and so we have its diagnostics on failure.
    let stderr = child.stderr.take();
    // Without the thread there are no diagnostics; the budget still bounds the child.
    let errdrain = stderr
        .and_then(|s| crate::safety::try_spawn("st2k-magick-stderr", move || drain_capped(s)));

    let png = match await_magick_output(&mut child, &rx, budget.cpu, budget.wall) {
        Ok(buf) => buf,
        Err(why) => {
            // Over budget: kill, drain the threads, reap, fail.
            let (status, err) = reap_magick_child(&mut child, writer, reader, errdrain);
            log_magick_failure(why, status, &err);
            return Err(Error::from(E_FAIL));
        }
    };
    // We have the output. Kill unconditionally so a child that closed stdout but
    // is still hung (e.g. not draining stdin, leaving the writer's write_all
    // blocked on a full pipe) can't deadlock writer.join()/wait() forever — the
    // whole reason the external timeout exists. kill() is a harmless no-op if it
    // already exited.
    let (status, err) = reap_magick_child(&mut child, writer, reader, errdrain);
    if png.is_empty() {
        log_magick_failure("decode produced no output", status, &err);
        return Err(Error::from(E_FAIL));
    }
    // Validate by decoding rather than by exit status (which is unreliable now —
    // we may have killed a child that had already produced a complete PNG).
    // image::Limits bound this safe-tier decode.
    decode_with_image_alloc(&png, max_alloc).inspect_err(|e| {
        crate::safety::log_debugf!(
            "magick decode: could not re-decode the {} byte PNG it returned: {e}",
            png.len()
        );
    })
}

// `GetProcessTimes` lives in kernel32, which is always linked. Declared here rather than
// switching on the `windows` crate's `Win32_System_Threading` feature for one call — the
// same approach `decode::magick_gate` already takes for the semaphore.
#[link(name = "kernel32")]
extern "system" {
    fn GetProcessTimes(
        process: *mut std::ffi::c_void,
        creation: *mut u32,
        exit: *mut u32,
        kernel: *mut u32,
        user: *mut u32,
    ) -> i32;
}

/// Is the bundled (or system) ImageMagick available? Gates the magick-backed
/// Convert targets in the dialog — they're hidden on a compact install.
pub fn magick_available() -> bool {
    magick_exe().is_some()
}

/// Staging tests live here rather than in `decode/tests.rs` because they drive
/// [`NamedTemp`] directly. Counting files in `%TEMP%` from the decode level looked like the
/// obvious test and was RACY: the sibling tests stage their own files in the SAME process,
/// so a pid filter does not separate them and the count moves under you. A test that fails
/// depending on what else is running is worse than no test.
#[cfg(test)]
mod stage_tests;
#[cfg(test)]
mod tests;
