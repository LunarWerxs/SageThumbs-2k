//! The timing sweep, which reports rather than asserts.
//! It prints a per-format table of how long the scaled pre-pass takes and how
//! faithful it is. Numbers to read, not a gate.

use super::*;

/// MEASUREMENT INSTRUMENT — what the codec-scaled pre-pass costs PER FORMAT, against what
/// that format's thumbnail decode costs today. Prints a table; asserts nothing.
///
/// The pre-pass ([`wic_scaled_from_bytes_if_codec_scales`]) is gated to JPEG, and that gate's
/// doc comment says widening it is a RE-MEASUREMENT rather than a relaxed magic test. This is
/// that measurement, banked in the repo so the next person reads a number instead of rebuilding
/// the rig. Point it at a folder of LARGE samples — a 256 px thumbnail of a 256 px image proves
/// nothing, and every format here has a small file for which the answer is "don't bother".
///
/// Columns:
///   * `scales` — what `IWICBitmapSourceTransform::GetClosestSize` answers, plus the size it
///     offers. `no` ends the discussion for that format: there is nothing to win.
///   * `probe` — factory + decoder + `GetFrame` + `GetClosestSize` and no decode. This is the
///     pure cost a DECLINED probe adds to every file of that format, and therefore what the
///     `MIN_SCALED_BYTES` floor is really buying.
///   * `pre-pass` — the scaled decode itself, magic gate bypassed.
///   * `today` — [`decode_preview_capped`], i.e. exactly what ships.
///
/// Widening pays only where `pre-pass` is materially under `today`. A format whose tier already
/// lifts an embedded preview (camera RAW, PSD) shows the OPPOSITE, which is precisely why the
/// gate cannot be a magic-byte list.
///
/// ```text
/// $env:ST2K_FMT_DIR = "...\samples"
/// cargo test --release --lib scaled_pre_pass_sweep -- --ignored --nocapture
/// ```
/// Fastest of `reps` runs, in microseconds, plus whether the work ever succeeded.
fn sweep_best_us(reps: usize, mut f: impl FnMut() -> bool) -> (u128, bool) {
    let (mut best, mut ok) = (u128::MAX, false);
    for _ in 0..reps.max(1) {
        let t = std::time::Instant::now();
        ok |= f();
        best = best.min(t.elapsed().as_micros());
    }
    (best, ok)
}

fn sweep_ms(us: u128) -> String {
    format!("{:.1}", us as f64 / 1000.0)
}

/// The "scales"/"dims" columns: what `GetClosestSize` answers, kept as three distinct cases on
/// purpose. An earlier version of this sweep printed "wic declines" for both "no codec" and
/// "opened it, exposes no transform interface", which reported TIFF as unreadable when WIC
/// reads it perfectly well.
fn sweep_dims_and_scales(bytes: &[u8]) -> (String, String) {
    match unsafe { wic::wic_scaling_answer(bytes) } {
        wic::ScalingAnswer::CannotOpen => ("-".to_string(), "wic cannot open".to_string()),
        wic::ScalingAnswer::NoTransform { w, h } => {
            (format!("{w}x{h}"), "no transform iface".to_string())
        }
        wic::ScalingAnswer::Offers { w, h, cw, ch } => (
            format!("{w}x{h}"),
            if cw < w || ch < h {
                format!("yes {cw}x{ch}")
            } else {
                "no (full size back)".to_string()
            },
        ),
    }
}

/// FIDELITY, not just speed — the column without which this table is a trap. Several codecs
/// answer `GetClosestSize` with a size far BELOW the request (a HEIF `thmb` item, a tile
/// count), and a scaler that takes such an offer and upscales is enormously fast and
/// completely wrong. Compare the two decodes on a common grid: a real reduced-resolution
/// decode differs from the reference by resampling noise, a upscaled postage stamp differs by
/// a mile.
fn sweep_fidelity(bytes: &[u8], edge: u32, head: &[u8], name: &str) -> String {
    match (
        unsafe { wic::wic_decode_bytes_if_codec_scales(bytes, edge, head) },
        decode_preview_capped(bytes, edge),
    ) {
        (Ok(pre), Ok(reference)) => {
            let (pw, ph) = (pre.width(), pre.height());
            let a = pre.resize_exact(64, 64, image::imageops::FilterType::Triangle);
            let b = reference.resize_exact(64, 64, image::imageops::FilterType::Triangle);
            let (a, b) = (a.to_rgb8(), b.to_rgb8());
            let sum: u64 = a
                .pixels()
                .zip(b.pixels())
                .map(|(x, y)| (0..3).map(|c| x.0[c].abs_diff(y.0[c]) as u64).sum::<u64>())
                .sum();
            let mad = sum as f64 / (64.0 * 64.0 * 3.0);
            if let Ok(dir) = std::env::var("ST2K_FMT_OUT") {
                let _ = std::fs::create_dir_all(&dir);
                let _ = pre.save(std::path::Path::new(&dir).join(format!("{name}.pre.png")));
                let _ = reference.save(std::path::Path::new(&dir).join(format!("{name}.ref.png")));
            }
            format!("{mad:.1} {pw}x{ph}")
        }
        _ => "-".to_string(),
    }
}

/// One file's whole row: dims/scales, the probe/pre-pass/today timings, fidelity, and the
/// printed line itself.
fn sweep_print_row(p: &std::path::Path, edge: u32, reps: usize) {
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let Ok(bytes) = std::fs::read(p) else {
        return;
    };
    let mb = bytes.len() as f64 / (1024.0 * 1024.0);

    let (dims, scales) = sweep_dims_and_scales(&bytes);
    let (probe_us, _) = sweep_best_us(reps, || {
        !matches!(
            unsafe { wic::wic_scaling_answer(&bytes) },
            wic::ScalingAnswer::CannotOpen
        )
    });

    let head = &bytes[..bytes.len().min(COLOR_HEAD_BYTES)];
    let (pre_us, pre_ok) = sweep_best_us(reps, || unsafe {
        wic::wic_decode_bytes_if_codec_scales(&bytes, edge, head).is_ok()
    });
    let (today_us, today_ok) = sweep_best_us(reps, || decode_preview_capped(&bytes, edge).is_ok());

    let fidelity = sweep_fidelity(&bytes, edge, head, &name);

    // A JPEG over the floor ALREADY takes the pre-pass, so its `today` is the fast number
    // and the ratio is 1.0 by construction. Mark it rather than let the table read as
    // "JPEG gains nothing".
    let shipped = bytes.starts_with(&[0xFF, 0xD8, 0xFF]) && bytes.len() >= 512 * 1024;
    let ratio = match (pre_ok, today_ok) {
        (true, true) if shipped => "(wired)".to_string(),
        (true, true) => format!("{:.1}x", today_us as f64 / pre_us.max(1) as f64),
        (false, _) => "declined".to_string(),
        (_, false) => "no decode".to_string(),
    };
    println!(
        "{:<14} {:>7.1} {:>12} {:>18} {:>8} {:>10} {:>10} {:>8}  {}",
        name,
        mb,
        dims,
        scales,
        sweep_ms(probe_us),
        if pre_ok { sweep_ms(pre_us) } else { "-".into() },
        if today_ok {
            sweep_ms(today_us)
        } else {
            "-".into()
        },
        ratio,
        fidelity
    );
}

#[test]
#[ignore = "measurement over a folder of large samples; set ST2K_FMT_DIR and run --release"]
fn scaled_pre_pass_sweep_by_format() {
    let Ok(dir) = std::env::var("ST2K_FMT_DIR") else {
        println!("set ST2K_FMT_DIR to a folder of large samples first");
        return;
    };
    let edge: u32 = std::env::var("ST2K_FMT_EDGE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(256);
    // Best-of, not mean: this box regularly sits at high background load, and the minimum is
    // the closest thing to "what the work actually costs" that a noisy machine will give up.
    let reps: usize = std::env::var("ST2K_FMT_REPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
        );
    }

    let mut files: Vec<std::path::PathBuf> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .map(|e| e.path())
            .collect(),
        Err(e) => {
            println!("cannot read {dir}: {e}");
            return;
        }
    };
    files.sort();

    println!("\nscaled pre-pass sweep — target edge {edge} px, best of {reps}");
    println!("dir: {dir}\n");
    println!(
        "{:<14} {:>7} {:>12} {:>18} {:>8} {:>10} {:>10} {:>8}  MAD/out",
        "file", "MB", "pixels", "scales", "probe", "pre-pass", "today", "ratio"
    );
    println!("{}", "-".repeat(112));

    for p in &files {
        sweep_print_row(p, edge, reps);
    }
    println!(
        "\n(times ms; `probe` is what a DECLINED probe adds per file. MAD is mean absolute \n\
         per-channel difference from the shipping decode on a common 64x64 grid — single \n\
         digits are resampling noise, tens mean the pre-pass returned a different picture.)"
    );
}
