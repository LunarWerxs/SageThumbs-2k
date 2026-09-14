//! Reproducible encoder-size/quality scorecard for issue #684.
//!
//! The scorecard compares DjVuLibre's pinned `c44`/`cjb2` command-line
//! encoders with the corresponding archival-safe `PageEncoder` profiles:
//!
//! * `c44` vs `EncodeQuality::Photo` on the same PPM raster;
//! * `cjb2` vs `EncodeQuality::Lossless` on the same PBM raster.
//!
//! It records the tool versions, repository SHA, encoded bytes, wall time,
//! peak RSS (when the platform exposes it), and a decoded-quality gate.  It
//! deliberately measures existing profiles; no lossy or experimental option
//! is enabled by this harness.
//!
//! Usage:
//!
//! ```text
//! cargo run --release --example encoder_parity_scorecard -- --ocr
//! cargo run --release --example encoder_parity_scorecard -- \
//!     --case watchmaker-color --case cable-bilevel \
//!     --output target/encoder-parity.json
//! ```
//!
//! The corpus files are checked in under `tests/corpus/`.  `ddjvu`, `c44`, and
//! `cjb2` must be on `PATH`.  `tesseract` is optional; `--no-ocr` disables the
//! OCR readability probe even when it is installed.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Output, Stdio};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use djvu_rs::djvu_encode::{EncodeQuality, PageEncoder};
use djvu_rs::{Bitmap, Pixmap, quality};
use serde_json::{Value, json};

const DEFAULT_MAX_PIXELS: u64 = 20_000_000;
const DEFAULT_REPEATS: usize = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Photo,
    Lossless,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Photo => "iw44-photo",
            Self::Lossless => "jb2-lossless",
        }
    }

    fn baseline_tool(self) -> &'static str {
        match self {
            Self::Photo => "c44",
            Self::Lossless => "cjb2",
        }
    }

    fn format(self) -> &'static str {
        match self {
            Self::Photo => "ppm",
            Self::Lossless => "pbm",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct CaseSpec {
    name: &'static str,
    source: &'static str,
    page: u32,
    mode: Mode,
}

const DEFAULT_CASES: &[CaseSpec] = &[
    CaseSpec {
        name: "watchmaker-color",
        source: "tests/corpus/watchmaker.djvu",
        page: 0,
        mode: Mode::Photo,
    },
    CaseSpec {
        name: "goody-twoshoes-color",
        source: "tests/corpus/goody_twoshoes.djvu",
        page: 0,
        mode: Mode::Photo,
    },
    CaseSpec {
        name: "big-scanned-page-color",
        source: "tests/corpus/big_scanned_page.djvu",
        page: 0,
        mode: Mode::Photo,
    },
    CaseSpec {
        name: "cable-bilevel",
        source: "tests/corpus/cable_1973_100133.djvu",
        page: 0,
        mode: Mode::Lossless,
    },
    CaseSpec {
        name: "map-atlas-bilevel",
        source: "tests/corpus/map_atlas_sample.djvu",
        page: 0,
        mode: Mode::Lossless,
    },
    CaseSpec {
        name: "chinese-cookbook-bilevel",
        source: "tests/corpus/chinese_cookbook_sample.djvu",
        page: 0,
        mode: Mode::Lossless,
    },
];

#[derive(Clone, Debug, Eq, PartialEq)]
enum Raster {
    Ppm {
        width: u32,
        height: u32,
        rgb: Vec<u8>,
    },
    Pbm {
        width: u32,
        height: u32,
        packed: Vec<u8>,
    },
}

impl Raster {
    fn width(&self) -> u32 {
        match self {
            Self::Ppm { width, .. } | Self::Pbm { width, .. } => *width,
        }
    }

    fn height(&self) -> u32 {
        match self {
            Self::Ppm { height, .. } | Self::Pbm { height, .. } => *height,
        }
    }

    fn pixels(&self) -> u64 {
        self.width() as u64 * self.height() as u64
    }

    fn as_pixmap(&self) -> Option<Pixmap> {
        let Self::Ppm { width, height, rgb } = self else {
            return None;
        };
        let mut pixmap = Pixmap::new(*width, *height, 0, 0, 0, 255);
        if pixmap.data.len() != rgb.len() / 3 * 4 {
            return None;
        }
        for (i, pixel) in rgb.as_chunks::<3>().0.iter().enumerate() {
            let dst = &mut pixmap.data[i * 4..i * 4 + 4];
            dst[..3].copy_from_slice(pixel);
            dst[3] = 255;
        }
        Some(pixmap)
    }

    fn as_bitmap(&self) -> Option<Bitmap> {
        let Self::Pbm {
            width,
            height,
            packed,
        } = self
        else {
            return None;
        };
        let stride = (*width as usize).div_ceil(8);
        if packed.len() != stride * *height as usize {
            return None;
        }
        Some(Bitmap {
            width: *width,
            height: *height,
            data: packed.clone(),
        })
    }
}

/// Read the three ASCII integers that precede a binary PNM payload.
fn next_token(data: &[u8], pos: &mut usize) -> Option<Vec<u8>> {
    loop {
        while data.get(*pos).is_some_and(u8::is_ascii_whitespace) {
            *pos += 1;
        }
        if data.get(*pos) == Some(&b'#') {
            while let Some(&byte) = data.get(*pos) {
                *pos += 1;
                if byte == b'\n' {
                    break;
                }
            }
            continue;
        }
        break;
    }

    let start = *pos;
    while let Some(&byte) = data.get(*pos) {
        if byte.is_ascii_whitespace() || byte == b'#' {
            break;
        }
        *pos += 1;
    }
    (start != *pos).then(|| data[start..*pos].to_vec())
}

fn pnm_dimensions(data: &[u8], pos: &mut usize) -> Option<(u32, u32)> {
    let width = String::from_utf8(next_token(data, pos)?)
        .ok()?
        .parse()
        .ok()?;
    let height = String::from_utf8(next_token(data, pos)?)
        .ok()?
        .parse()
        .ok()?;
    Some((width, height))
}

/// Parse a binary P6 PPM. Kept separate from the PBM parser so the header
/// cursor cannot accidentally consume the first pixel while reading maxval.
fn parse_ppm(data: &[u8]) -> Option<Raster> {
    if data.get(0..2)? != b"P6" {
        return None;
    }
    let mut pos = 2;
    let (width, height) = pnm_dimensions(data, &mut pos)?;
    let maxval = String::from_utf8(next_token(data, &mut pos)?)
        .ok()?
        .parse::<u32>()
        .ok()?;
    if maxval != 255 || !data.get(pos)?.is_ascii_whitespace() {
        return None;
    }
    pos += 1;
    let len = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(3)?;
    let rgb = data.get(pos..pos.checked_add(len)?)?.to_vec();
    Some(Raster::Ppm { width, height, rgb })
}

fn parse_pbm(data: &[u8]) -> Option<Raster> {
    if data.get(0..2)? != b"P4" {
        return None;
    }
    let mut pos = 2;
    let (width, height) = pnm_dimensions(data, &mut pos)?;
    if !data.get(pos)?.is_ascii_whitespace() {
        return None;
    }
    pos += 1;
    let stride = (width as usize).checked_add(7)?.checked_div(8)?;
    let len = stride.checked_mul(height as usize)?;
    let packed = data.get(pos..pos.checked_add(len)?)?.to_vec();
    Some(Raster::Pbm {
        width,
        height,
        packed,
    })
}

fn parse_raster_for_mode(data: &[u8], mode: Mode) -> Option<Raster> {
    match mode {
        Mode::Photo => parse_ppm(data),
        Mode::Lossless => parse_pbm(data),
    }
}

fn bitmap_hamming(a: &Bitmap, b: &Bitmap) -> u64 {
    if a.width != b.width || a.height != b.height {
        return u64::MAX;
    }
    let mut differing = 0;
    for y in 0..a.height {
        for x in 0..a.width {
            differing += u64::from(a.get(x, y) != b.get(x, y));
        }
    }
    differing
}

#[derive(Debug)]
struct Measurement {
    bytes: u64,
    median_ms: f64,
    peak_rss_kb: Option<u64>,
}

fn json_f64(value: f64) -> Value {
    if value.is_finite() {
        json!(value)
    } else {
        Value::Null
    }
}

fn parse_rss_kb(stderr: &str) -> Option<u64> {
    for raw_line in stderr.lines() {
        let line = raw_line.trim_start();
        if let Some(value) = line.strip_prefix("ENCODER_PEAK_RSS_KB=")
            && let Ok(kb) = value.trim().parse()
        {
            return Some(kb);
        }
        if let Some(value) = line.strip_prefix("maximum resident set size:") {
            // macOS reports bytes; Linux's /usr/bin/time reports kbytes.
            if let Ok(bytes) = value.trim().parse::<u64>() {
                return Some(bytes.div_ceil(1024));
            }
        }
        if let Some(value) = line.strip_prefix("Maximum resident set size (kbytes):")
            && let Ok(kb) = value.trim().parse()
        {
            return Some(kb);
        }
    }
    None
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn run_once(program: &Path, args: &[String], use_time_wrapper: bool) -> Result<Output, String> {
    let mut command = if use_time_wrapper && Path::new("/usr/bin/time").exists() {
        let mut command = Command::new("/usr/bin/time");
        if cfg!(target_os = "macos") {
            command.arg("-l");
        } else {
            command.arg("-v");
        }
        command.arg(program);
        command
    } else {
        Command::new(program)
    };
    let output = command
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("spawn {}: {error}", program.display()))?;
    if !output.status.success() {
        return Err(format!(
            "{} failed ({}): {}",
            program.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output)
}

fn run_measured(
    program: &Path,
    args: &[String],
    output_path: &Path,
    repeats: usize,
    use_time_wrapper: bool,
) -> Result<Measurement, String> {
    let repeats = repeats.max(1);
    // Warm-up removes one-time dynamic-linker and filesystem effects from the
    // reported median, while the output is still overwritten for each sample.
    let _ = fs::remove_file(output_path);
    run_once(program, args, use_time_wrapper)?;

    let mut times = Vec::with_capacity(repeats);
    let mut peak_rss_kb: Option<u64> = None;
    for _ in 0..repeats {
        let _ = fs::remove_file(output_path);
        let started = Instant::now();
        let output = run_once(program, args, use_time_wrapper)?;
        times.push(started.elapsed().as_secs_f64() * 1000.0);
        peak_rss_kb = match (
            peak_rss_kb,
            parse_rss_kb(&String::from_utf8_lossy(&output.stderr)),
        ) {
            (Some(previous), Some(current)) => Some(previous.max(current)),
            (None, current) => current,
            (previous, None) => previous,
        };
    }
    let bytes = fs::metadata(output_path)
        .map_err(|error| format!("{} was not written: {error}", output_path.display()))?
        .len();
    Ok(Measurement {
        bytes,
        median_ms: median(&mut times),
        peak_rss_kb,
    })
}

fn render_with_ddjvu(input: &Path, mode: Mode, page: u32, output: &Path) -> Result<(), String> {
    let args = vec![
        format!("-page={}", page + 1),
        format!("-format={}", mode.format()),
        input.display().to_string(),
        output.display().to_string(),
    ];
    run_once(Path::new("ddjvu"), &args, false).map(|_| ())
}

fn command_first_line(program: &str, arg: &str) -> Option<String> {
    let output = Command::new(program).arg(arg).output().ok()?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .chain(String::from_utf8_lossy(&output.stderr).lines())
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

fn git_sha() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn which(program: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {program} >/dev/null 2>&1")])
        .status()
        .is_ok_and(|status| status.success())
}

fn ocr_stats(path: &Path) -> Option<Value> {
    if !which("tesseract") {
        return None;
    }
    let output = Command::new("tesseract")
        .args([path.to_string_lossy().as_ref(), "stdout", "--psm", "6"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Some(json!({
        "chars": text.chars().filter(|c| !c.is_whitespace()).count(),
        "words": text.split_whitespace().count(),
    }))
}

#[cfg(unix)]
fn peak_rss_for(who: libc::c_int) -> Option<u64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: `getrusage` initializes the supplied rusage structure when it
    // returns zero; the pointer is valid for the duration of the call.
    let result = unsafe { libc::getrusage(who, usage.as_mut_ptr()) };
    if result != 0 {
        return None;
    }
    // macOS reports bytes; Linux and the other Unix targets report KiB.
    let raw = unsafe { usage.assume_init().ru_maxrss as u64 };
    #[cfg(target_os = "macos")]
    return Some(raw.div_ceil(1024));
    #[cfg(not(target_os = "macos"))]
    Some(raw)
}

fn self_peak_rss_kb() -> Option<u64> {
    #[cfg(unix)]
    {
        peak_rss_for(libc::RUSAGE_SELF)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

fn children_peak_rss_kb() -> Option<u64> {
    #[cfg(unix)]
    {
        peak_rss_for(libc::RUSAGE_CHILDREN)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

fn external_worker(args: &[String]) -> Result<(), String> {
    let program = args.first().ok_or("external worker needs a program")?;
    let status = Command::new(program)
        .args(&args[1..])
        .status()
        .map_err(|error| format!("spawn {program}: {error}"))?;
    if !status.success() {
        return Err(format!("{program} failed ({status})"));
    }
    if let Some(rss) = children_peak_rss_kb() {
        eprintln!("ENCODER_PEAK_RSS_KB={rss}");
    }
    Ok(())
}

fn worker(args: &[String]) -> Result<(), String> {
    let mut mode = None;
    let mut input = None;
    let mut output = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--mode" => {
                i += 1;
                mode = Some(match args.get(i).map(String::as_str) {
                    Some("iw44-photo") => Mode::Photo,
                    Some("jb2-lossless") => Mode::Lossless,
                    _ => return Err("--mode must be iw44-photo or jb2-lossless".into()),
                });
            }
            "--input" => {
                i += 1;
                input = args.get(i).map(PathBuf::from);
            }
            "--output" => {
                i += 1;
                output = args.get(i).map(PathBuf::from);
            }
            value => return Err(format!("unknown worker argument: {value}")),
        }
        i += 1;
    }

    let mode = mode.ok_or("worker needs --mode")?;
    let input = input.ok_or("worker needs --input")?;
    let output = output.ok_or("worker needs --output")?;
    let data = fs::read(&input).map_err(|error| format!("read {}: {error}", input.display()))?;
    let raster = parse_raster_for_mode(&data, mode).ok_or_else(|| {
        format!(
            "{} is not a valid {} raster",
            input.display(),
            mode.format()
        )
    })?;
    let encoded = match mode {
        Mode::Photo => {
            let pixmap = raster.as_pixmap().ok_or("photo worker needs PPM input")?;
            PageEncoder::from_pixmap(&pixmap)
                .with_quality(EncodeQuality::Photo)
                .encode()
                .map_err(|error| format!("photo encode: {error:?}"))?
        }
        Mode::Lossless => {
            let bitmap = raster
                .as_bitmap()
                .ok_or("lossless worker needs PBM input")?;
            PageEncoder::from_bitmap(&bitmap)
                .with_quality(EncodeQuality::Lossless)
                .encode()
                .map_err(|error| format!("lossless encode: {error:?}"))?
        }
    };
    fs::write(&output, encoded).map_err(|error| format!("write {}: {error}", output.display()))?;
    if let Some(rss) = self_peak_rss_kb() {
        eprintln!("ENCODER_PEAK_RSS_KB={rss}");
    }
    Ok(())
}

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Result<Self, String> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| format!("clock: {error}"))?
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "djvu-rs-encoder-parity-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir(&path).map_err(|error| format!("create {}: {error}", path.display()))?;
        Ok(Self(path))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn case_value(
    spec: CaseSpec,
    source: &Raster,
    baseline: &Measurement,
    ours: &Measurement,
    quality_value: Value,
    ocr_value: Value,
) -> Value {
    let ratio = ours.bytes as f64 / baseline.bytes.max(1) as f64;
    json!({
        "name": spec.name,
        "source": spec.source,
        "page": spec.page,
        "mode": spec.mode.as_str(),
        "width": source.width(),
        "height": source.height(),
        "pixels": source.pixels(),
        "baseline": {
            "tool": spec.mode.baseline_tool(),
            "bytes": baseline.bytes,
            "encode_ms_median": baseline.median_ms,
            "peak_rss_kb": baseline.peak_rss_kb,
        },
        "ours": {
            "tool": "djvu-rs",
            "bytes": ours.bytes,
            "encode_ms_median": ours.median_ms,
            "peak_rss_kb": ours.peak_rss_kb,
        },
        "size_ratio_ours_over_baseline": ratio,
        "size_gap_pct": (ratio - 1.0) * 100.0,
        "encode_time_ratio_ours_over_baseline": ours.median_ms / baseline.median_ms.max(f64::EPSILON),
        "quality": quality_value,
        "ocr": ocr_value,
        "status": "pass",
    })
}

fn measure_case(spec: CaseSpec, config: &Config, exe: &Path) -> Result<Value, String> {
    let source_path = Path::new(spec.source);
    if !source_path.is_file() {
        return Err(format!("missing corpus file {}", source_path.display()));
    }
    let temp = TempDir::new()?;
    let raster_path = temp.0.join(format!("source.{}", spec.mode.format()));
    render_with_ddjvu(source_path, spec.mode, spec.page, &raster_path)?;
    let raster_data = fs::read(&raster_path)
        .map_err(|error| format!("read {}: {error}", raster_path.display()))?;
    let raster = parse_raster_for_mode(&raster_data, spec.mode)
        .ok_or_else(|| format!("ddjvu emitted invalid {}", spec.mode.format()))?;
    if raster.pixels() > config.max_pixels {
        return Ok(json!({
            "name": spec.name,
            "source": spec.source,
            "page": spec.page,
            "mode": spec.mode.as_str(),
            "width": raster.width(),
            "height": raster.height(),
            "pixels": raster.pixels(),
            "status": "skipped",
            "reason": format!("{} pixels exceeds --max-pixels {}", raster.pixels(), config.max_pixels),
        }));
    }

    let baseline_path = temp.0.join("baseline.djvu");
    let ours_path = temp.0.join("ours.djvu");
    let raster_arg = raster_path.display().to_string();
    let baseline_args = vec![
        "--external-worker".to_owned(),
        spec.mode.baseline_tool().to_owned(),
        raster_arg.clone(),
        baseline_path.display().to_string(),
    ];
    let baseline = run_measured(exe, &baseline_args, &baseline_path, config.repeats, false)?;
    let ours_args = vec![
        "--worker".to_owned(),
        "--mode".to_owned(),
        spec.mode.as_str().to_owned(),
        "--input".to_owned(),
        raster_arg,
        "--output".to_owned(),
        ours_path.display().to_string(),
    ];
    let ours = run_measured(exe, &ours_args, &ours_path, config.repeats, false)?;

    let baseline_raster_path = temp.0.join(format!("baseline.{}", spec.mode.format()));
    let ours_raster_path = temp.0.join(format!("ours.{}", spec.mode.format()));
    render_with_ddjvu(&baseline_path, spec.mode, 0, &baseline_raster_path)?;
    render_with_ddjvu(&ours_path, spec.mode, 0, &ours_raster_path)?;
    let baseline_raster = parse_raster_for_mode(
        &fs::read(&baseline_raster_path)
            .map_err(|error| format!("read baseline raster: {error}"))?,
        spec.mode,
    )
    .ok_or("invalid baseline render")?;
    let ours_raster = parse_raster_for_mode(
        &fs::read(&ours_raster_path).map_err(|error| format!("read ours raster: {error}"))?,
        spec.mode,
    )
    .ok_or("invalid ours render")?;
    if baseline_raster.width() != raster.width()
        || baseline_raster.height() != raster.height()
        || ours_raster.width() != raster.width()
        || ours_raster.height() != raster.height()
    {
        return Err(format!(
            "{} changed dimensions: source {}x{}, baseline {}x{}, ours {}x{}",
            spec.name,
            raster.width(),
            raster.height(),
            baseline_raster.width(),
            baseline_raster.height(),
            ours_raster.width(),
            ours_raster.height()
        ));
    }

    let quality_value = match spec.mode {
        Mode::Photo => {
            let source_pm = raster.as_pixmap().ok_or("source PPM conversion failed")?;
            let baseline_pm = baseline_raster
                .as_pixmap()
                .ok_or("baseline PPM conversion failed")?;
            let ours_pm = ours_raster
                .as_pixmap()
                .ok_or("ours PPM conversion failed")?;
            let baseline_quality = quality::compare_color(&source_pm, &baseline_pm);
            let ours_quality = quality::compare_color(&source_pm, &ours_pm);
            json!({
                "baseline": {
                    "psnr_db": json_f64(quality::compare(&source_pm, &baseline_pm).psnr_db),
                    "ssim_y": baseline_quality.ssim_y,
                    "ssim_combined": baseline_quality.ssim_combined,
                    "delta_e_mean": baseline_quality.delta_e_mean,
                    "delta_e_max": baseline_quality.delta_e_max,
                },
                "ours": {
                    "psnr_db": json_f64(quality::compare(&source_pm, &ours_pm).psnr_db),
                    "ssim_y": ours_quality.ssim_y,
                    "ssim_combined": ours_quality.ssim_combined,
                    "delta_e_mean": ours_quality.delta_e_mean,
                    "delta_e_max": ours_quality.delta_e_max,
                },
            })
        }
        Mode::Lossless => {
            let source_bm = raster.as_bitmap().ok_or("source PBM conversion failed")?;
            let baseline_bm = baseline_raster
                .as_bitmap()
                .ok_or("baseline PBM conversion failed")?;
            let ours_bm = ours_raster
                .as_bitmap()
                .ok_or("ours PBM conversion failed")?;
            let baseline_hamming = bitmap_hamming(&source_bm, &baseline_bm);
            let ours_hamming = bitmap_hamming(&source_bm, &ours_bm);
            json!({
                "baseline": {
                    "hamming_pixels": baseline_hamming,
                    "hamming_pct": baseline_hamming as f64 / raster.pixels().max(1) as f64 * 100.0,
                    "pixel_exact": baseline_hamming == 0,
                },
                "ours": {
                    "hamming_pixels": ours_hamming,
                    "hamming_pct": ours_hamming as f64 / raster.pixels().max(1) as f64 * 100.0,
                    "pixel_exact": ours_hamming == 0,
                },
            })
        }
    };

    let ocr_value = if config.ocr {
        let source_ocr = ocr_stats(&raster_path);
        let baseline_ocr = ocr_stats(&baseline_raster_path);
        let ours_ocr = ocr_stats(&ours_raster_path);
        json!({
            "source": source_ocr,
            "baseline": baseline_ocr,
            "ours": ours_ocr,
        })
    } else {
        json!({"status": "not_requested"})
    };

    Ok(case_value(
        spec,
        &raster,
        &baseline,
        &ours,
        quality_value,
        ocr_value,
    ))
}

#[derive(Debug)]
struct Config {
    cases: Vec<String>,
    output: PathBuf,
    repeats: usize,
    max_pixels: u64,
    ocr: bool,
}

fn usage() {
    eprintln!(
        "usage: encoder_parity_scorecard [--case NAME]... [--output PATH] [--repeats N] [--max-pixels N] [--ocr|--no-ocr]"
    );
    eprintln!("cases:");
    for case in DEFAULT_CASES {
        eprintln!(
            "  {:<28} {} ({})",
            case.name,
            case.source,
            case.mode.as_str()
        );
    }
}

fn parse_config(args: &[String]) -> Result<Config, String> {
    let mut config = Config {
        cases: Vec::new(),
        output: PathBuf::from("target/encoder_parity_scorecard.json"),
        repeats: DEFAULT_REPEATS,
        max_pixels: DEFAULT_MAX_PIXELS,
        ocr: true,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--case" => {
                i += 1;
                config
                    .cases
                    .push(args.get(i).ok_or("--case needs a name")?.clone());
            }
            "--output" => {
                i += 1;
                config.output = PathBuf::from(args.get(i).ok_or("--output needs a path")?);
            }
            "--repeats" => {
                i += 1;
                config.repeats = args
                    .get(i)
                    .ok_or("--repeats needs a positive integer")?
                    .parse()
                    .map_err(|_| "--repeats needs a positive integer")?;
                if config.repeats == 0 {
                    return Err("--repeats needs a positive integer".into());
                }
            }
            "--max-pixels" => {
                i += 1;
                config.max_pixels = args
                    .get(i)
                    .ok_or("--max-pixels needs a positive integer")?
                    .parse()
                    .map_err(|_| "--max-pixels needs a positive integer")?;
            }
            "--ocr" => config.ocr = true,
            "--no-ocr" => config.ocr = false,
            "--help" | "-h" => return Err(String::new()),
            value => return Err(format!("unknown argument: {value}")),
        }
        i += 1;
    }
    Ok(config)
}

fn select_cases(names: &[String]) -> Result<Vec<CaseSpec>, String> {
    if names.is_empty() {
        return Ok(DEFAULT_CASES.to_vec());
    }
    let mut selected = Vec::with_capacity(names.len());
    for name in names {
        let case = DEFAULT_CASES
            .iter()
            .find(|case| case.name == name)
            .copied()
            .ok_or_else(|| format!("unknown case {name}"))?;
        selected.push(case);
    }
    Ok(selected)
}

fn scorecard(config: Config) -> Result<(), String> {
    let cases = select_cases(&config.cases)?;
    let exe = env::current_exe().map_err(|error| format!("current executable: {error}"))?;
    let mut case_values = Vec::with_capacity(cases.len());
    let mut failures = 0;
    for case in cases {
        match measure_case(case, &config, &exe) {
            Ok(value) => {
                let status = value["status"].as_str().unwrap_or("unknown");
                let ratio = value["size_ratio_ours_over_baseline"]
                    .as_f64()
                    .map(|ratio| format!("{ratio:.3}x"))
                    .unwrap_or_else(|| "-".into());
                eprintln!("{:<28} {:<13} size {}", case.name, status, ratio);
                case_values.push(value);
            }
            Err(error) => {
                failures += 1;
                eprintln!("{:<28} ERROR {error}", case.name);
                case_values.push(json!({
                    "name": case.name,
                    "source": case.source,
                    "page": case.page,
                    "mode": case.mode.as_str(),
                    "status": "error",
                    "error": error,
                }));
            }
        }
    }

    if let Some(parent) = config
        .output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    let report = json!({
        "schema": 1,
        "kind": "djvu-rs encoder parity scorecard",
        "git_sha": git_sha(),
        "djvu_rs_version": env!("CARGO_PKG_VERSION"),
        "tool_versions": {
            "ddjvu": command_first_line("ddjvu", "--help"),
            "c44": command_first_line("c44", "-version"),
            "cjb2": command_first_line("cjb2", "-version"),
            "tesseract": if config.ocr { command_first_line("tesseract", "--version") } else { None },
        },
        "repeats": config.repeats,
        "max_pixels": config.max_pixels,
        "ocr_requested": config.ocr,
        "cases": case_values,
    });
    fs::write(
        &config.output,
        serde_json::to_vec_pretty(&report).map_err(|error| format!("JSON: {error}"))?,
    )
    .map_err(|error| format!("write {}: {error}", config.output.display()))?;
    eprintln!("scorecard: {}", config.output.display());
    if failures == 0 {
        Ok(())
    } else {
        Err(format!("{failures} case(s) failed"))
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "--worker") {
        return match worker(&args[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("worker: {error}");
                ExitCode::FAILURE
            }
        };
    }
    if args.first().is_some_and(|arg| arg == "--external-worker") {
        return match external_worker(&args[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("external worker: {error}");
                ExitCode::FAILURE
            }
        };
    }
    let config = match parse_config(&args) {
        Ok(config) => config,
        Err(error) => {
            usage();
            if !error.is_empty() {
                eprintln!("error: {error}");
            }
            return if error.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            };
        }
    };
    match scorecard(config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("scorecard: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ppm_comments_and_payload() {
        let data = b"P6\n# source\n2 1\n255\n\x00\x01\x02\x03\x04\x05";
        assert_eq!(
            parse_ppm(data),
            Some(Raster::Ppm {
                width: 2,
                height: 1,
                rgb: vec![0, 1, 2, 3, 4, 5],
            })
        );
    }

    #[test]
    fn parses_pbm_row_padding() {
        let data = b"P4\n9 1\n\x80\x80";
        assert_eq!(
            parse_pbm(data),
            Some(Raster::Pbm {
                width: 9,
                height: 1,
                packed: vec![0x80, 0x80],
            })
        );
    }

    #[test]
    fn rejects_truncated_pnm_payload() {
        assert!(parse_ppm(b"P6\n1 1\n255\n\x00\x01").is_none());
        assert!(parse_pbm(b"P4\n9 1\n\x80").is_none());
    }

    #[test]
    fn hamming_ignores_padding_bits() {
        let a = Bitmap {
            width: 1,
            height: 1,
            data: vec![0x80],
        };
        let b = Bitmap {
            width: 1,
            height: 1,
            data: vec![0xff],
        };
        assert_eq!(bitmap_hamming(&a, &b), 0);
    }

    #[test]
    fn parses_rss_from_worker_and_platform_time() {
        assert_eq!(parse_rss_kb("ENCODER_PEAK_RSS_KB=1234\n"), Some(1234));
        assert_eq!(
            parse_rss_kb("Maximum resident set size (kbytes): 42\n"),
            Some(42)
        );
        assert_eq!(parse_rss_kb("maximum resident set size: 2049\n"), Some(3));
        assert_eq!(parse_rss_kb("no rss here"), None);
    }

    #[test]
    fn non_finite_quality_is_json_null() {
        assert_eq!(json_f64(f64::INFINITY), Value::Null);
        assert_eq!(json_f64(1.5), json!(1.5));
    }
}
