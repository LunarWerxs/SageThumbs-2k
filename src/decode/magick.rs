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

    let app_policy_dir = crate::module_path()
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

/// Metafiles are untrusted vector programs rather than ordinary raster input.
/// Keep their ImageMagick child especially small: a normal Office/Visio preview
/// renders in a fraction of a second, while malformed or enormously complex WMF
/// and EMF content can otherwise consume the general-purpose 512 MiB / 20 s
/// budget merely to produce a useless frame. These are deliberately command-line
/// overrides, after [`add_magick_limits`], so they constrain only this decode
/// invocation and do not weaken the broader Magick policy or raster/PSD support.
const METAFILE_MAGICK_MEMORY_LIMIT: &str = "96MiB";
const METAFILE_MAGICK_MAP_LIMIT: &str = "96MiB";
/// Metafile CPU budget, and the elapsed-time backstop that goes with it. Same split as the
/// general-purpose pair (see [`limits::MAGICK_CPU_SECS`]): 3 s of CPU still kills a complex
/// or malformed WMF/EMF exactly as before, while the wider elapsed allowance keeps a busy
/// machine from failing a metafile that only needed a fraction of a second of real work.
const METAFILE_MAGICK_TIME_LIMIT: &str = "18";
const METAFILE_MAGICK_TIMEOUT: Duration = Duration::from_secs(18);
const METAFILE_MAGICK_CPU_BUDGET: Duration = Duration::from_secs(3);

/// How often the watchdog wakes to re-check the child while waiting for its output.
const WATCHDOG_SLICE: Duration = Duration::from_millis(250);

/// The two limits one magick child runs under: CPU time is the real budget, elapsed time
/// only the backstop for a child that hangs without burning any.
#[derive(Clone, Copy)]
struct MagickBudget {
    cpu: Duration,
    wall: Duration,
}

/// Ordinary raster decodes.
const RASTER_BUDGET: MagickBudget = MagickBudget {
    cpu: MAGICK_CPU_BUDGET,
    wall: MAGICK_TIMEOUT,
};
/// Metafiles, which get a much tighter CPU budget (see [`add_metafile_magick_limits`]).
const METAFILE_BUDGET: MagickBudget = MagickBudget {
    cpu: METAFILE_MAGICK_CPU_BUDGET,
    wall: METAFILE_MAGICK_TIMEOUT,
};

fn add_metafile_magick_limits(cmd: &mut Command) {
    cmd.args([
        "-limit",
        "memory",
        METAFILE_MAGICK_MEMORY_LIMIT,
        "-limit",
        "map",
        METAFILE_MAGICK_MAP_LIMIT,
        "-limit",
        "time",
        METAFILE_MAGICK_TIME_LIMIT,
    ]);
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
pub(super) fn decode_via_magick_capped(
    bytes: &[u8],
    max_edge: Option<u32>,
) -> Result<DynamicImage> {
    // Metafiles get a much tighter, format-specific child budget. A slow vector
    // WMF would otherwise grind for seconds to a near-blank frame; everything
    // else keeps the full 20 s budget for heavy raster decodes.
    let is_meta = looks_like_metafile(bytes);
    let budget = if is_meta {
        METAFILE_BUDGET
    } else {
        RASTER_BUDGET
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
    // `>` keeps it shrink-only, so a small image is never blown up to the cap.
    let capped = max_edge
        .map(|e| e.clamp(1, MAGICK_MAX_EDGE_PX))
        .map(|e| format!("{e}x{e}>"));
    let edge = capped.as_deref().unwrap_or(MAGICK_MAX_EDGE);
    decode_via_magick_spec(bytes, &pre_input, input, pre_ops, edge, budget)
}

/// Extensions whose ImageMagick coder is chosen by FILE NAME and never by
/// content, so the stdin pipe above can never reach them.
///
/// `decode_via_magick_spec_alloc` hands magick a nameless stream. That is fine
/// for the ~93 formats carrying a signature magick can sniff (`magick -list
/// magic`) and is why this tier works at all. It is not fine for these:
/// `magick identify sample.rla` succeeds only because the extension named the
/// coder, and the identical bytes arriving on stdin come back "no decode
/// delegate for this image format". Each of these was a registered, advertised
/// format that could not thumbnail on any surface.
///
/// Membership comes from ImageMagick's own tables rather than taste: an entry
/// belongs here when `magick -list format` maps it to a reading coder that has
/// no `magick -list magic` signature AND no other tier of ours can read it.
/// Formats that DO sniff are deliberately absent — naming a coder for those
/// would bypass magick's own detection and could decode bytes as a format they
/// are not.
const NAME_SELECTED_EXTS: &[&str] = &["cut", "jnx", "mac", "pix", "rla", "scr", "tim"];

/// Camera RAW, which rides magick's equally name-selected `dng` coder.
///
/// Separate from [`NAME_SELECTED_EXTS`] because the justification differs: RAW
/// normally never reaches magick at all, since `tiers::largest_embedded_jpeg`
/// lifts the camera's own preview out first and far more cheaply. This is the
/// backstop for a RAW whose embedded preview is missing or unreadable, which is
/// exactly what `sample.mdc` (Minolta) turned out to be — magick demosaics it
/// fine, and before this it produced no thumbnail at all.
///
/// A SUPERSET of `formats::RAW_EXTS`, not a mirror: `rmf` and `sti` are filed under
/// Images in `FORMATS` but `magick -list format` routes both through the same `dng`
/// module as the real camera RAW, so they belong here on decode grounds. Membership
/// is about which coder reads the bytes, never about the Settings category.
/// A RAW extension missing here just keeps the old no-thumbnail behaviour, so the
/// lists drifting degrades rather than breaks.
const RAW_CODER_EXTS: &[&str] = &[
    "3fr", "arw", "bay", "cap", "cr2", "cr3", "crw", "dcr", "dcs", "dng", "drf", "erf", "fff",
    "iiq", "k25", "kdc", "mdc", "mef", "mos", "mrw", "nef", "nrw", "orf", "ori", "pef", "ptx",
    "pxn", "raf", "rmf", "rw2", "rwl", "sr2", "srf", "srw", "sti", "x3f",
];

/// Is `ext` a camera RAW that magick reads through its name-selected `dng` coder?
///
/// Exposed so the full-fidelity path can ask "would the named coder do better than the generic
/// tier here", which is a different question from [`has_name_selected_coder`]: that one also
/// covers signature-less non-RAW formats like `.rla`, where there is no second opinion to seek.
pub(crate) fn is_raw_coder_ext(ext: &str) -> bool {
    RAW_CODER_EXTS.contains(&ext)
}

/// Would [`decode_named_extension`] have a coder to offer for `ext`?
pub(super) fn has_name_selected_coder(ext: &str) -> bool {
    let ext = ext.trim_start_matches('.').to_ascii_lowercase();
    NAME_SELECTED_EXTS.contains(&ext.as_str()) || RAW_CODER_EXTS.contains(&ext.as_str())
}

/// An extension is only ever used to build a temp file NAME, so it must not be
/// able to steer that name anywhere. Real extensions are short and alphanumeric;
/// anything else is refused rather than escaped.
fn safe_ext(ext: &str) -> Option<String> {
    let ext = ext.trim_start_matches('.').to_ascii_lowercase();
    let ok = !ext.is_empty() && ext.len() <= 8 && ext.bytes().all(|b| b.is_ascii_alphanumeric());
    ok.then_some(ext)
}

/// How many names to try before giving up. Only a genuinely hostile or wedged
/// %TEMP% can burn these, since the counter alone already makes a collision
/// improbable; the loop exists so `create_new` cannot turn a squatted name into a
/// permanent denial of the whole tier.
const MAX_STAGE_ATTEMPTS: u32 = 8;

/// A temp file that deletes itself, named so ImageMagick's coder tables can see
/// the extension. Process-id suffixed like every other temp path in this repo, so
/// concurrent `cargo test` runs and a parallel `st2k batch` fan-out cannot collide.
struct NamedTemp(std::path::PathBuf);

impl NamedTemp {
    /// Claim `path` EXCLUSIVELY and fill it, or decline. Split out from [`Self::create`] so
    /// the exclusivity property is testable on a name the test controls: driving it through
    /// the shared counter instead made the test race its own siblings and quietly stop
    /// exercising anything (it passed against the very behaviour it was meant to catch).
    ///
    /// `create_new`, never `File::create`. Windows' create-and-truncate follows hard links and
    /// reparse points, so an existing name in `%TEMP%` would have our image bytes written
    /// straight THROUGH it into whatever it really points at. The name is predictable - the pid
    /// is public and the counter restarts at 0 each process - so refusing an existing name is
    /// the guard, not the obscurity of the name.
    fn claim(path: std::path::PathBuf, bytes: &[u8]) -> Option<Self> {
        use std::io::Write;
        let mut file = std::fs::File::options()
            .write(true)
            .create_new(true)
            .open(&path)
            .ok()?;
        // The guard owns the path from HERE, BEFORE a single byte is written. Building it
        // after the write meant a write that failed part-way - a large RAW meeting a full
        // disk - returned early with the file already created and nothing to unlink it.
        let guard = Self(path);
        let wrote = file.write_all(bytes).is_ok();
        // Close before handing the name to a child process that is about to open it.
        drop(file);
        wrote.then_some(guard)
    }

    fn create(bytes: &[u8], ext: &str) -> Option<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        for _ in 0..MAX_STAGE_ATTEMPTS {
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            if let Some(staged) =
                Self::claim(dir.join(format!("st2k-coder-{pid}-{n}.{ext}")), bytes)
            {
                return Some(staged);
            }
        }
        crate::safety::log_debug("magick decode: could not claim a staging name in %TEMP%");
        None
    }
}

impl Drop for NamedTemp {
    fn drop(&mut self) {
        // Best effort: a leftover file in %TEMP% is a nuisance, a panic here (in a
        // `panic = "abort"` shell host) is a crash.
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Decode by handing ImageMagick a real file whose NAME carries `ext`, so its own
/// coder tables pick the reader — the one thing the nameless stdin pipe cannot do.
///
/// LAST RESORT ONLY. Every caller runs this after the ordinary tiers have already
/// declined, and that ordering is what makes naming a coder safe: when the name is
/// wrong the decode simply fails and the caller returns the error it already had.
///
/// A temp file rather than a forced `rla:-` stdin spec, because a coder prefix makes
/// magick read the pipe directly instead of spooling it, and the coders disagree about
/// whether they tolerate that: `rla:-` and `mdc:-` work, while `tim:-` dies with
/// "insufficient image data" on the very file `magick sample.tim` reads perfectly.
/// Magick already spools stdin to a temp file of its own on the auto-detect path, so
/// this adds no exposure the normal path does not already carry.
pub(super) fn decode_named_extension(
    bytes: &[u8],
    ext: &str,
    max_edge: Option<u32>,
) -> Result<DynamicImage> {
    let capped = max_edge
        .map(|e| e.clamp(1, MAGICK_MAX_EDGE_PX))
        .map(|e| format!("{e}x{e}>"));
    decode_named_extension_spec(
        bytes,
        ext,
        capped.as_deref().unwrap_or(MAGICK_MAX_EDGE),
        TILE_CAPS,
    )
}

/// As [`decode_named_extension`], at NATIVE resolution: the resize cap is the MAX_DIM bomb
/// guard (shrink-only), not the 4096 memory guard, with the matching re-decode allocation.
/// Exactly the pairing [`decode_psd_composite`] uses, and for the same reason — this is a
/// full-fidelity path, and the whole point is keeping the real pixels. A Mamiya `.mef`
/// through the 4096 guard came out 3078x4096; through this it is its native 4016x5344.
///
/// Only `decode_full_for_path` calls it, and that caller falls back to the capped variant on
/// failure: past roughly 65-90 MP the 16-bit PNG magick hands back can exceed
/// [`FULL_FIDELITY_PNG_CAP`], and a medium-format back at 4096 beats one at nothing.
pub(super) fn decode_named_extension_native(bytes: &[u8], ext: &str) -> Result<DynamicImage> {
    decode_named_extension_spec(bytes, ext, limits::FULL_FIDELITY_EDGE, FULL_FIDELITY_CAPS)
}

fn decode_named_extension_spec(
    bytes: &[u8],
    ext: &str,
    edge: &str,
    caps: DecodeCaps,
) -> Result<DynamicImage> {
    let Some(ext) = safe_ext(ext).filter(|e| has_name_selected_coder(e)) else {
        return Err(Error::from(E_FAIL));
    };
    // Check for magick BEFORE staging: on the compact (no-ImageMagick) install this
    // path is reached for every one of these formats, and writing up to a few hundred
    // MB to %TEMP% only to discover there is no decoder is pure waste.
    if magick_exe().is_none() {
        return Err(Error::from(E_FAIL));
    }
    let Some(temp) = NamedTemp::create(bytes, &ext) else {
        crate::safety::log_debug("magick decode: could not stage a named temp file");
        return Err(Error::from(E_FAIL));
    };
    let Some(spec) = temp.0.to_str() else {
        return Err(Error::from(E_FAIL));
    };
    // Empty stdin on purpose: the child reads the file, so shovelling the bytes down a
    // pipe nobody drains would only duplicate the write (and, for a big RAW, the wait).
    let out = decode_via_magick_spec_alloc(&[], &[], spec, &[], edge, caps, RASTER_BUDGET);
    drop(temp);
    out
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

/// Is this a Windows metafile (placeable/memory WMF, or EMF)? Selects the
/// metafile-specific limits for the magick tier and is the single home for the
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
    } else {
        "-"
    }
}

/// Maximum number of top-level ISOBMFF boxes we inspect. Real AVIFs put `mini`
/// immediately after `ftyp`; the cap prevents a tiny-box flood from turning
/// this cheap routing predicate into an unbounded parser.
const MAX_ISOBMFF_TOP_LEVEL_BOXES: usize = 64;

/// Return a checked top-level box's type, body start, and end offset.
/// `None` covers truncation, invalid lengths, and sizes that do not fit usize.
fn isobmff_box_at(bytes: &[u8], offset: usize) -> Option<([u8; 4], usize, usize)> {
    let header = bytes.get(offset..offset.checked_add(8)?)?;
    let size32 = u32::from_be_bytes(header[0..4].try_into().ok()?);
    let typ = header[4..8].try_into().ok()?;
    let extended = if size32 == 1 {
        let raw = bytes.get(offset.checked_add(8)?..offset.checked_add(16)?)?;
        Some(u64::from_be_bytes(raw.try_into().ok()?))
    } else {
        None
    };
    let (size, header_len) = crate::container::boxhdr::decode_box_size(
        size32,
        extended,
        offset as u64,
        bytes.len() as u64,
    )?;
    let size = usize::try_from(size).ok()?;
    let header_len = usize::try_from(header_len).ok()?;
    Some((typ, offset + header_len, offset + size))
}

fn is_mini_avif(bytes: &[u8]) -> bool {
    let Some((typ, body, mut offset)) = isobmff_box_at(bytes, 0) else {
        return false;
    };
    if typ != *b"ftyp" || !ftyp_describes_mini_avif(&bytes[body..offset]) {
        return false;
    }

    for _ in 0..MAX_ISOBMFF_TOP_LEVEL_BOXES {
        if offset == bytes.len() {
            return false;
        }
        let Some((typ, _, end)) = isobmff_box_at(bytes, offset) else {
            return false;
        };
        if typ == *b"mini" {
            return true;
        }
        offset = end;
    }
    false
}

/// A MinimizedImageBox file uses the `mif3` structural brand. For AV1, the
/// FileTypeBox minor-version word is the codec brand `avif` (ISO BMFF's
/// low-overhead-image amendment); it deliberately is not a compatible brand.
fn ftyp_describes_mini_avif(body: &[u8]) -> bool {
    if body.len() < 8 || !(body.len() - 8).is_multiple_of(4) {
        return false;
    }
    let has_mif3 = body[..4] == *b"mif3" || body[8..].chunks_exact(4).any(|brand| brand == b"mif3");
    has_mif3 && body[4..8] == *b"avif"
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
pub(super) fn decode_psd_composite(bytes: &[u8]) -> Result<DynamicImage> {
    decode_via_magick_spec_alloc(
        bytes,
        &[],
        "-[0]",
        &[],
        limits::FULL_FIDELITY_EDGE,
        FULL_FIDELITY_CAPS,
        RASTER_BUDGET,
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
    budget: MagickBudget,
) -> Result<DynamicImage> {
    decode_via_magick_spec_alloc(
        bytes, pre_input, input, pre_ops, max_edge, TILE_CAPS, budget,
    )
}

/// Worst-case bytes the decode path's stdout can legitimately carry: every call site
/// caps geometry at [`MAGICK_MAX_EDGE_PX`] (4096) before asking magick to write a PNG,
/// so 4096x4096 raw RGBA is the ceiling with only framing overhead on top. Same value
/// and reasoning as `flv.rs`'s `FLASH_PNG_CAP` for its sibling out-of-process harness.
/// Without this, a starved-but-alive magick child could stream unbounded bytes into
/// this process for the whole CPU/wall budget window below.
const MAGICK_PNG_CAP: usize = 64 * 1024 * 1024;

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
fn decode_via_magick_spec_alloc(
    bytes: &[u8],
    pre_input: &[&str],
    input: &str,
    pre_ops: &[&str],
    max_edge: &str,
    caps: DecodeCaps,
    budget: MagickBudget,
) -> Result<DynamicImage> {
    let DecodeCaps { max_alloc, png_cap } = caps;
    let Some(exe) = magick_exe() else {
        crate::safety::log_debug("magick decode: ImageMagick not available");
        return Err(Error::from(E_FAIL));
    };
    let mut cmd = Command::new(exe);
    add_magick_limits(&mut cmd);
    if looks_like_metafile(bytes) {
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
    let _permit = magick_gate::acquire();
    // Every failure below is LOGGED, not just returned. This tier is the one we route AVIF
    // to precisely because the fallback (WIC) gets those files wrong, so a silent Err here
    // reappears as a wrong-coloured thumbnail with nothing in the log to explain it — which
    // is how issue #9 stayed invisible. Process creation really can fail on a machine that
    // is out of resources, so it needs a breadcrumb like every other tier has.
    let mut child = cmd.spawn().map_err(|e| {
        crate::safety::log_debug(&format!("magick decode: could not start the child: {e}"));
        Error::from(E_FAIL)
    })?;

    // Feed stdin on its own thread so a full stdout pipe can't deadlock us.
    let Some(mut stdin) = child.stdin.take() else {
        crate::safety::log_debug("magick decode: child has no stdin pipe");
        let _ = child.kill();
        let _ = child.wait();
        return Err(Error::from(E_FAIL));
    };
    let input = bytes.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
        // drop(stdin) here closes the pipe so ImageMagick sees EOF
    });

    // Read stdout on its own thread; the main thread enforces the budget.
    let Some(stdout) = child.stdout.take() else {
        crate::safety::log_debug("magick decode: child has no stdout pipe");
        let _ = child.kill();
        let _ = writer.join();
        let _ = child.wait();
        return Err(Error::from(E_FAIL));
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        // Capped so a hostile/misbehaving child can't balloon our memory before the
        // CPU/wall watchdog below gets a chance to kill it (see MAGICK_PNG_CAP).
        let _ = stdout.take((png_cap + 1) as u64).read_to_end(&mut buf);
        let _ = tx.send(buf);
    });

    // Drain stderr on its own thread too (capped) so a chatty/failing magick
    // can't fill the pipe and stall, and so we have its diagnostics on failure.
    let stderr = child.stderr.take();
    let errdrain = stderr.map(|s| std::thread::spawn(move || drain_capped(s)));

    let png = match await_magick_output(&mut child, &rx, budget.cpu, budget.wall) {
        Ok(buf) => buf,
        Err(why) => {
            // Over budget: kill, drain the threads, reap, fail.
            let _ = child.kill();
            let _ = writer.join();
            let _ = reader.join();
            let err = errdrain.and_then(|h| h.join().ok()).unwrap_or_default();
            let status = child.wait().ok();
            log_magick_failure(why, status, &err);
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
    decode_with_image_alloc(&png, max_alloc).inspect_err(|e| {
        crate::safety::log_debug(&format!(
            "magick decode: could not re-decode the {} byte PNG it returned: {e}",
            png.len()
        ));
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

/// Total CPU time (kernel + user) this child has consumed so far.
///
/// `None` when the OS won't say — the caller then falls back to the wall-clock backstop
/// alone, i.e. exactly the behaviour that predates the CPU budget.
fn child_cpu_time(child: &std::process::Child) -> Option<Duration> {
    use std::os::windows::io::AsRawHandle;
    // FILETIME is two 32-bit halves and is only 4-byte aligned, so take it as a pair of
    // u32 and recombine rather than letting the OS write a u64 into a maybe-underaligned
    // slot.
    let (mut creation, mut exit, mut kernel, mut user) =
        ([0u32; 2], [0u32; 2], [0u32; 2], [0u32; 2]);
    let ok = unsafe {
        GetProcessTimes(
            child.as_raw_handle().cast(),
            creation.as_mut_ptr(),
            exit.as_mut_ptr(),
            kernel.as_mut_ptr(),
            user.as_mut_ptr(),
        )
    };
    if ok == 0 {
        return None;
    }
    let ticks = |v: [u32; 2]| (u64::from(v[1]) << 32) | u64::from(v[0]);
    // FILETIME counts 100-nanosecond intervals.
    Some(Duration::from_nanos(
        ticks(kernel)
            .saturating_add(ticks(user))
            .saturating_mul(100),
    ))
}

/// Wait for the child's PNG on `rx`, enforcing a CPU budget with a wall-clock backstop.
///
/// `Err` is the message to log; the caller kills and reaps. Two cases are deliberately NOT
/// failures, and both are why this is a loop instead of one `recv_timeout`:
///
///  * the child is alive but starved — it has burned almost no CPU, so it keeps its budget
///    however long the machine makes it wait;
///  * the child has already EXITED — its stdout is closed, so the pending `read_to_end`
///    returns as soon as that thread is scheduled, and killing at that point would throw
///    away a decode that already succeeded (issue #9 logged this as
///    `decode timed out (status Some(ExitStatus(0)))`).
pub(crate) fn await_magick_output(
    child: &mut std::process::Child,
    rx: &std::sync::mpsc::Receiver<Vec<u8>>,
    cpu_budget: Duration,
    wall_ceiling: Duration,
) -> std::result::Result<Vec<u8>, &'static str> {
    use std::sync::mpsc::RecvTimeoutError;
    let start = std::time::Instant::now();
    loop {
        match rx.recv_timeout(WATCHDOG_SLICE) {
            Ok(buf) => return Ok(buf),
            // The reader thread went away without sending: nothing more is coming. Report
            // it as empty output so the caller's existing `png.is_empty()` check handles it.
            Err(RecvTimeoutError::Disconnected) => return Ok(Vec::new()),
            Err(RecvTimeoutError::Timeout) => {}
        }
        let still_running = !matches!(child.try_wait(), Ok(Some(_)));
        if still_running && child_cpu_time(child).is_some_and(|cpu| cpu > cpu_budget) {
            return Err("decode exceeded its CPU budget");
        }
        if start.elapsed() > wall_ceiling {
            return Err("decode timed out");
        }
    }
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

/// Return the explicit ImageMagick coder for every Magick-backed output exposed
/// by the Convert dialog. Never let ImageMagick infer these from a filename:
/// when a module is absent, it can otherwise preserve the input encoding and
/// still exit successfully, producing (for example) PNG bytes in an `.avif` file.
fn output_coder(extension: &str) -> Option<&'static str> {
    match extension.to_ascii_lowercase().as_str() {
        "avif" => Some("AVIF"),
        "jxl" => Some("JXL"),
        "psd" => Some("PSD"),
        "dds" => Some("DDS"),
        "jp2" => Some("JP2"),
        "pcx" => Some("PCX"),
        "sgi" => Some("SGI"),
        "pfm" => Some("PFM"),
        "dpx" => Some("DPX"),
        "fits" => Some("FITS"),
        "xpm" => Some("XPM"),
        "pict" => Some("PICT"),
        "ras" => Some("RAS"),
        "palm" => Some("PALM"),
        _ => None,
    }
}

/// Whether `extension` has an explicit, tested ImageMagick output coder.
///
/// Keep every caller routed through this predicate instead of duplicating the
/// writer list. An extension merely being decodable does not mean either
/// `image` or ImageMagick can safely encode it.
pub fn magick_output_supported(extension: &str) -> bool {
    output_coder(extension).is_some()
}

/// What the [`encode_via_magick`] watchdog loop should do after one process poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EncodeWait {
    Continue,
    TimedOut,
    CpuExceeded,
}

/// Pure decision core of the encode watchdog loop: has the child exceeded its CPU
/// budget, or only the wall-clock deadline? Split out of the loop so the CPU branch —
/// the budget the decode path already enforces via `await_magick_output`, which the
/// encode path used to lack entirely — is directly testable without spawning and
/// starving a real magick process.
fn encode_wait_decision(
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

/// Resolve the magick executable and the `coder:path` output target for `target_ext`.
/// Self-defend: this is the single chokepoint for the magick-backed Convert targets,
/// so gate the capability here rather than trusting every caller to pre-check
/// `magick_available()`. A distinct, logged error keeps "magick missing" diagnosable
/// instead of looking like a genuine encode failure (bare E_FAIL).
fn magick_encode_target(
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
    let out_str = out.to_str().ok_or_else(|| Error::from(E_FAIL))?;
    Ok((exe, format!("{coder}:{out_str}")))
}

/// `png:-` (our own re-encode on stdin) → an EXPLICIT coder + target path. The prefix
/// is load-bearing: without it, a missing output module can make ImageMagick silently
/// preserve the PNG input while naming it `.avif`, `.jxl`, etc. When a quality is given
/// (lossy AVIF/JXL), pass it through as `-quality N`; lossless targets use ImageMagick's
/// default.
fn magick_encode_args(output_spec: String, quality: Option<u8>) -> Vec<String> {
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
fn spawn_magick_child(
    exe: &std::path::Path,
    args: &[String],
) -> Result<(std::process::Child, Option<magick_gate::Permit>)> {
    let mut cmd = Command::new(exe);
    add_magick_limits(&mut cmd);
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW);
    apply_magick_environment(&mut cmd, exe);
    let permit = magick_gate::acquire();
    let child = cmd.spawn().map_err(|_| Error::from(E_FAIL))?;
    Ok((child, permit))
}

/// Wire up the child's stdin/stdout/stderr pipes: a writer thread feeds `png` in (drop
/// closes the pipe so magick sees EOF), a reader thread drains stdout and signals `rx`
/// when done (magick writes to the FILE, not stdout, so this only exists to observe that
/// EOF; the bytes are never used, but draining through the same capped helper stderr
/// uses below avoids an unbounded read), and an optional stderr-drain thread captures
/// diagnostics for a failure log.
type MagickEncodePipes = (
    std::thread::JoinHandle<()>,
    std::thread::JoinHandle<()>,
    std::sync::mpsc::Receiver<()>,
    Option<std::thread::JoinHandle<Vec<u8>>>,
);

fn pipe_magick_encode(child: &mut std::process::Child, png: Vec<u8>) -> Result<MagickEncodePipes> {
    use std::io::Write;

    let mut stdin = child.stdin.take().ok_or_else(|| Error::from(E_FAIL))?;
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&png); // drop closes the pipe → magick sees EOF
    });

    let stdout = child.stdout.take().ok_or_else(|| Error::from(E_FAIL))?;
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let _ = drain_capped(stdout);
        let _ = tx.send(());
    });

    // Drain stderr (capped) so we can log it on failure and it can't stall magick.
    let stderr = child.stderr.take();
    let errdrain = stderr.map(|s| std::thread::spawn(move || drain_capped(s)));

    Ok((writer, reader, rx, errdrain))
}

/// Poll the child through the wall-clock deadline, escalating on either a CPU-budget or
/// wall-clock timeout. EOF on stdout (`rx`) normally means the process is about to
/// exit, but it is not proof: a hostile/broken child can close stdout early, stop
/// reading stdin, and stay alive — so this keeps polling the real process on the SAME
/// deadline instead of trusting the `rx` signal alone.
fn wait_for_magick_child(
    child: &mut std::process::Child,
    rx: std::sync::mpsc::Receiver<()>,
) -> (bool, bool, bool, Option<std::process::ExitStatus>) {
    let deadline = std::time::Instant::now() + MAGICK_TIMEOUT;
    let mut timed_out = rx.recv_timeout(MAGICK_TIMEOUT).is_err();
    let mut cpu_exceeded = false;
    let mut wait_failed = false;
    let mut status = None;

    while !timed_out && !cpu_exceeded && status.is_none() {
        match child.try_wait() {
            Ok(Some(value)) => status = Some(value),
            Ok(None) => {
                let now = std::time::Instant::now();
                match encode_wait_decision(child_cpu_time(child), MAGICK_CPU_BUDGET, now, deadline)
                {
                    EncodeWait::CpuExceeded => cpu_exceeded = true,
                    EncodeWait::TimedOut => timed_out = true,
                    EncodeWait::Continue => {
                        std::thread::sleep(
                            std::time::Duration::from_millis(10).min(deadline - now),
                        );
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
fn finish_magick_encode(
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
    let (exe, output_spec) = magick_encode_target(target_ext, out)?;
    let args = magick_encode_args(output_spec, quality);

    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|_| Error::from(E_FAIL))?;

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

#[cfg(test)]
mod tests {
    use super::{
        add_magick_limits, add_metafile_magick_limits, apply_magick_environment,
        encode_wait_decision, magick_output_supported, magick_stdin_spec, output_coder, EncodeWait,
        FULL_FIDELITY_PNG_CAP, MAGICK_CPU_BUDGET, MAGICK_PNG_CAP, MAX_ISOBMFF_TOP_LEVEL_BOXES,
        METAFILE_MAGICK_CPU_BUDGET, METAFILE_MAGICK_MAP_LIMIT, METAFILE_MAGICK_MEMORY_LIMIT,
        METAFILE_MAGICK_TIMEOUT, METAFILE_MAGICK_TIME_LIMIT,
    };
    use std::collections::HashMap;
    use std::process::Command;
    use std::time::{Duration, Instant};

    fn isobmff_box(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let size = u32::try_from(8 + body.len()).unwrap();
        [&size.to_be_bytes()[..], &typ[..], body].concat()
    }

    fn ftyp_with_minor(
        major_brand: &[u8; 4],
        minor_version: &[u8; 4],
        compatible: &[[u8; 4]],
    ) -> Vec<u8> {
        let mut body = Vec::from(&major_brand[..]);
        body.extend_from_slice(minor_version);
        for brand in compatible {
            body.extend_from_slice(brand);
        }
        isobmff_box(b"ftyp", &body)
    }

    fn ftyp(major_brand: &[u8; 4], compatible: &[[u8; 4]]) -> Vec<u8> {
        ftyp_with_minor(major_brand, &[0; 4], compatible)
    }

    #[test]
    fn mini_avif_uses_explicit_avif_stdin_spec() {
        let mut bytes = ftyp_with_minor(b"mif3", b"avif", &[]);
        bytes.extend(isobmff_box(b"mini", &[0x80, 0x01, 0xFE]));
        assert_eq!(magick_stdin_spec(&bytes), "avif:-");

        // A derived structural major brand may carry mif3 as compatible.
        let mut compatible = ftyp_with_minor(b"mif1", b"avif", &[*b"mif3"]);
        compatible.extend(isobmff_box(b"mini", &[0x80]));
        assert_eq!(magick_stdin_spec(&compatible), "avif:-");
    }

    #[test]
    fn ordinary_avif_keeps_magick_auto_detection() {
        let mut bytes = ftyp(b"avif", &[*b"mif1"]);
        bytes.extend(isobmff_box(b"meta", &[0, 0, 0, 0]));
        assert_eq!(magick_stdin_spec(&bytes), "-");
    }

    #[test]
    fn mini_stdin_routing_rejects_malformed_or_hostile_boxes() {
        // A `mini` byte sequence outside a checked top-level box is not enough.
        assert_eq!(magick_stdin_spec(b"not an avif mini"), "-");

        // The declared ftyp length extends beyond the buffer.
        assert_eq!(
            magick_stdin_spec(&[0, 0, 0, 32, b'f', b't', b'y', b'p', b'a', b'v', b'i', b'f']),
            "-"
        );

        // An extended-size box must have its complete 16-byte header and body.
        let mut truncated_extended = ftyp_with_minor(b"mif3", b"avif", &[]);
        truncated_extended.extend_from_slice(&[0, 0, 0, 1, b'm', b'i', b'n', b'i']);
        assert_eq!(magick_stdin_spec(&truncated_extended), "-");

        // Stop before an attacker-controlled run of arbitrarily many tiny boxes.
        let mut flooded = ftyp_with_minor(b"mif3", b"avif", &[]);
        for _ in 0..MAX_ISOBMFF_TOP_LEVEL_BOXES {
            flooded.extend(isobmff_box(b"free", &[]));
        }
        flooded.extend(isobmff_box(b"mini", &[0x80]));
        assert_eq!(magick_stdin_spec(&flooded), "-");
    }

    #[test]
    fn non_avif_mini_keeps_magick_auto_detection() {
        let mut bytes = ftyp_with_minor(b"mif3", &[0; 4], &[]);
        bytes.extend(isobmff_box(b"mini", &[0x80]));
        assert_eq!(magick_stdin_spec(&bytes), "-");
    }

    #[test]
    fn ftyp_minor_version_cannot_spoof_an_avif_brand() {
        let mut bytes = ftyp_with_minor(b"mif1", b"avif", &[]);
        // The AV1 codec signal alone is not enough without the mif3 structure.
        bytes.extend(isobmff_box(b"mini", &[0x80]));
        assert_eq!(magick_stdin_spec(&bytes), "-");
    }

    #[test]
    fn every_advertised_magick_output_uses_an_explicit_coder() {
        let expected = [
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

        for (extension, coder) in expected {
            assert_eq!(output_coder(extension), Some(coder));
            assert_eq!(output_coder(&extension.to_ascii_uppercase()), Some(coder));
            assert!(magick_output_supported(extension));
            assert!(magick_output_supported(&extension.to_ascii_uppercase()));
        }

        assert_eq!(output_coder(""), None);
        assert_eq!(output_coder("png"), None);
        assert_eq!(output_coder("not-a-real-format"), None);
        assert!(!magick_output_supported(""));
        assert!(!magick_output_supported("png"));
        assert!(!magick_output_supported("not-a-real-format"));
    }

    #[test]
    fn metafile_limits_override_the_shared_magick_budget() {
        let mut command = Command::new("magick.exe");
        add_magick_limits(&mut command);
        add_metafile_magick_limits(&mut command);
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();

        assert_eq!(
            args,
            [
                "-limit",
                "memory",
                "512MiB",
                "-limit",
                "map",
                "1GiB",
                "-limit",
                "time",
                "120",
                "-limit",
                "memory",
                METAFILE_MAGICK_MEMORY_LIMIT,
                "-limit",
                "map",
                METAFILE_MAGICK_MAP_LIMIT,
                "-limit",
                "time",
                METAFILE_MAGICK_TIME_LIMIT,
            ]
        );
        // Metafiles keep their much tighter CPU budget; only the elapsed-time backstop is
        // widened, so a busy machine cannot fail a metafile that needed 0.1 s of real work.
        assert_eq!(
            METAFILE_MAGICK_CPU_BUDGET,
            std::time::Duration::from_secs(3)
        );
        assert_eq!(METAFILE_MAGICK_TIMEOUT, std::time::Duration::from_secs(18));
        assert_eq!(
            METAFILE_MAGICK_TIME_LIMIT.parse::<u64>().unwrap(),
            METAFILE_MAGICK_TIMEOUT.as_secs(),
            "magick's own elapsed limit must match the metafile wall backstop",
        );
    }

    #[test]
    fn magick_command_is_pinned_to_its_own_module_tree() {
        let root = std::env::temp_dir().join(format!(
            "st2k-magick-env-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("modules").join("coders")).unwrap();
        std::fs::create_dir_all(root.join("modules").join("filters")).unwrap();
        std::fs::write(root.join("policy.xml"), b"<policymap/>").unwrap();
        let exe = root.join("magick.exe");
        let coders = root.join("modules").join("coders");
        let filters = root.join("modules").join("filters");

        let mut command = Command::new(&exe);
        apply_magick_environment(&mut command, &exe);
        let environment: HashMap<_, _> = command
            .get_envs()
            .filter_map(|(key, value)| value.map(|value| (key.to_owned(), value.to_owned())))
            .collect();

        assert_eq!(
            environment
                .get(std::ffi::OsStr::new("MAGICK_HOME"))
                .map(std::ffi::OsString::as_os_str),
            Some(root.as_os_str())
        );
        assert_eq!(
            environment
                .get(std::ffi::OsStr::new("MAGICK_CODER_MODULE_PATH"))
                .map(std::ffi::OsString::as_os_str),
            Some(coders.as_os_str())
        );
        assert_eq!(
            environment
                .get(std::ffi::OsStr::new("MAGICK_FILTER_MODULE_PATH"))
                .map(std::ffi::OsString::as_os_str),
            Some(filters.as_os_str())
        );
        assert_eq!(
            environment
                .get(std::ffi::OsStr::new("MAGICK_CONFIGURE_PATH"))
                .map(std::ffi::OsString::as_os_str),
            Some(root.as_os_str())
        );

        let _ = std::fs::remove_dir_all(root);
    }

    /// A starved-but-alive encode child (near-zero CPU burned, wall deadline still far
    /// off) must trip on CPU budget, not just coast until the wall ceiling. Before this
    /// branch existed, `encode_via_magick`'s wait loop had no CPU check at all, so this
    /// case fell through to `EncodeWait::Continue` regardless of `cpu`.
    #[test]
    fn encode_wait_trips_cpu_budget_before_the_wall_deadline() {
        let now = Instant::now();
        let deadline = now + Duration::from_secs(600); // wall ceiling nowhere close
        let decision = encode_wait_decision(
            Some(MAGICK_CPU_BUDGET + Duration::from_millis(1)),
            MAGICK_CPU_BUDGET,
            now,
            deadline,
        );
        assert_eq!(decision, EncodeWait::CpuExceeded);
    }

    #[test]
    fn encode_wait_keeps_polling_a_busy_but_within_budget_child() {
        let now = Instant::now();
        let deadline = now + Duration::from_secs(600);
        let decision = encode_wait_decision(
            Some(Duration::from_millis(1)),
            MAGICK_CPU_BUDGET,
            now,
            deadline,
        );
        assert_eq!(decision, EncodeWait::Continue);
    }

    #[test]
    fn encode_wait_falls_back_to_the_wall_ceiling_when_cpu_time_is_unknown() {
        // `child_cpu_time` returns `None` when the OS won't say (see its own doc comment);
        // the loop must still fail closed via the wall deadline rather than spin forever.
        let now = Instant::now();
        let deadline = now - Duration::from_millis(1); // already past
        assert_eq!(
            encode_wait_decision(None, MAGICK_CPU_BUDGET, now, deadline),
            EncodeWait::TimedOut
        );
    }

    /// The decode and encode magick harnesses now cap their stdout reads the same way
    /// `flv.rs`'s sibling child harness caps its own (`FLASH_PNG_CAP`) — this pins the
    /// value so it can't silently drift below what `-resize {MAGICK_MAX_EDGE_PX}x...>`
    /// can legitimately produce.
    #[test]
    fn magick_png_cap_covers_the_geometry_ceiling() {
        const MAGICK_MAX_EDGE_PX: u64 = 4096;
        let worst_case_raw_rgba = MAGICK_MAX_EDGE_PX * MAGICK_MAX_EDGE_PX * 4;
        assert!(
            MAGICK_PNG_CAP as u64 >= worst_case_raw_rgba,
            "MAGICK_PNG_CAP must cover a full {MAGICK_MAX_EDGE_PX}x{MAGICK_MAX_EDGE_PX} RGBA frame"
        );
    }

    /// The full-fidelity cap must comfortably cover the MEASURED hand-back that broke the
    /// native RAW path: the bundled Q16 magick writes 16-BIT PNGs, and a 21 MP Mamiya `.mef`
    /// at native size is 107 MB — silently "failing" under the 64 MiB tier cap, so the whole
    /// feature shipped and did nothing. Pinned at 2x that so a merely-bigger camera does not
    /// re-open the same hole one model later.
    #[test]
    fn full_fidelity_png_cap_covers_the_measured_native_raw() {
        let measured_mef_native_png: usize = 107_389_077;
        assert!(
            FULL_FIDELITY_PNG_CAP >= 2 * measured_mef_native_png,
            "FULL_FIDELITY_PNG_CAP must cover a native medium-format 16-bit PNG with headroom"
        );
    }
}

/// Staging tests live here rather than in `decode/tests.rs` because they drive
/// [`NamedTemp`] directly. Counting files in `%TEMP%` from the decode level looked like the
/// obvious test and was RACY: the sibling tests stage their own files in the SAME process,
/// so a pid filter does not separate them and the count moves under you. A test that fails
/// depending on what else is running is worse than no test.
#[cfg(test)]
mod stage_tests {
    use super::*;

    /// The invariant the leak fix is about: the guard owns the path from the moment the
    /// file exists, so every exit unlinks it.
    #[test]
    fn the_staged_file_lives_exactly_as_long_as_its_guard() {
        let guard = NamedTemp::create(b"payload", "rla").expect("staging must succeed in %TEMP%");
        let path = guard.0.clone();
        assert!(
            path.is_file(),
            "the staged file should exist while the guard does"
        );
        assert_eq!(
            std::fs::read(&path).expect("staged file must be readable"),
            b"payload",
            "the staged bytes must be the ones handed to ImageMagick"
        );
        // create_new on the same name must now refuse — proof the file is really claimed,
        // which is what stops a pre-planted hard link or reparse point being written through.
        let second = std::fs::File::options()
            .write(true)
            .create_new(true)
            .open(&path);
        assert!(
            second.is_err_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists),
            "the staged name must be exclusively held"
        );
        drop(guard);
        assert!(
            !path.exists(),
            "dropping the guard must remove the staged file, got a leftover at {path:?}"
        );
    }

    /// The counter must hand out distinct names, or two concurrent decodes would fight over
    /// one file and the first to finish would delete the other's input.
    #[test]
    fn concurrently_staged_files_never_share_a_name() {
        let guards: Vec<_> = (0..8)
            .map(|_| NamedTemp::create(b"x", "tim").expect("staging must succeed"))
            .collect();
        let mut paths: Vec<_> = guards.iter().map(|g| g.0.clone()).collect();
        paths.sort();
        let unique = paths.len();
        paths.dedup();
        assert_eq!(paths.len(), unique, "staged names collided: {paths:?}");
        for g in &guards {
            assert!(g.0.is_file(), "every staged file should exist: {:?}", g.0);
        }
        drop(guards);
        for p in &paths {
            assert!(!p.exists(), "leftover after drop: {p:?}");
        }
    }

    /// The one with real teeth, and deterministic: the name is ours, so nothing else in the
    /// process can consume it first.
    ///
    /// This is the assertion that fails against `File::create`, which maps to Windows
    /// CREATE_ALWAYS: that follows hard links and reparse points and truncates whatever the
    /// name resolves to, so a planted name in %TEMP% received our image bytes.
    #[test]
    fn a_squatted_name_is_refused_rather_than_written_through() {
        const SENTINEL: &[u8] = b"do not clobber me";
        let path = std::env::temp_dir().join(format!(
            "st2k-coder-squat-{}-{:p}.rla",
            std::process::id(),
            &SENTINEL
        ));
        std::fs::write(&path, SENTINEL).expect("the test must be able to plant a file");

        let claimed = NamedTemp::claim(path.clone(), b"image bytes that must not land here");
        assert!(
            claimed.is_none(),
            "an already-existing name must be refused, not claimed"
        );
        assert_eq!(
            std::fs::read(&path).ok().as_deref(),
            Some(SENTINEL),
            "the existing file was written THROUGH instead of being left alone"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// The other half: a name nobody holds is claimed, filled, and released on drop.
    #[test]
    fn a_free_name_is_claimed_filled_and_released() {
        let path = std::env::temp_dir().join(format!(
            "st2k-coder-free-{}-{:p}.rla",
            std::process::id(),
            &MAX_STAGE_ATTEMPTS
        ));
        let _ = std::fs::remove_file(&path);
        let guard =
            NamedTemp::claim(path.clone(), b"payload").expect("a free name must be claimed");
        assert_eq!(std::fs::read(&path).ok().as_deref(), Some(&b"payload"[..]));
        drop(guard);
        assert!(!path.exists(), "dropping the guard must remove {path:?}");
    }

    /// A refused extension must never reach the filesystem at all.
    #[test]
    fn a_refused_extension_stages_nothing() {
        for ext in ["../../evil", "png", "", "waytoolongextension"] {
            assert!(
                decode_named_extension(b"whatever", ext, None).is_err(),
                "{ext:?} must be refused"
            );
        }
    }
}
