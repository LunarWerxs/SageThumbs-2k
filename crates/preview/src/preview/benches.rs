//! The three --bench-* modes: preview decode, held-key mash, and keyed navigation.

use super::*;

/// Where a bench writes its report: the argv slot after the bench's own arguments, else a
/// named file in the temp folder. Results go to a FILE, not stdout: this EXE is a
/// windows-subsystem binary with no console of its own, so a `println!` here writes to a
/// closed handle.
pub(super) fn bench_output(arg_index: usize, default_name: &str) -> impl Fn(&str) {
    let dest = std::env::args().nth(arg_index).unwrap_or_else(|| {
        std::env::temp_dir()
            .join(default_name)
            .to_string_lossy()
            .into_owned()
    });
    move |text: &str| {
        let _ = std::fs::write(&dest, text);
    }
}

/// The files in `dir` whose (lower-cased) extension `keep` accepts, or why the folder could
/// not be read.
pub(super) fn bench_files(
    dir: &str,
    keep: impl Fn(&str) -> bool,
) -> Result<Vec<std::path::PathBuf>, String> {
    let rd = std::fs::read_dir(dir).map_err(|e| format!("cannot read {dir}: {e}"))?;
    Ok(rd
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|x| x.to_str())
                .map(|x| keep(&x.to_ascii_lowercase()))
                .unwrap_or(false)
        })
        .collect())
}

/// The navigation benches' shared start: the previewable files of `dir` in Explorer's
/// logical order (the viewer walks the folder that way, so expectations must too, or "where
/// did it land" compares against the wrong sequence), the viewer opened on the first one and
/// settled, so the first measured step starts from a painted window rather than from whatever
/// the constructor left mid-flight. `Err` carries the one line the bench should report.
pub(super) fn bench_viewer(
    hinst: HINSTANCE,
    dir: &str,
) -> Result<(Vec<std::path::PathBuf>, HWND), String> {
    let files = bench_files(dir, window::navigate::is_previewable_ext)?;
    if files.len() < 2 {
        return Err(format!("need at least 2 previewable files in {dir}"));
    }
    let files = window::navigate::sort_paths_like_explorer(files);
    let first = files[0].to_string_lossy().into_owned();
    let hwnd = unsafe { window::create_viewer(hinst, false, Some(first.clone()), None) }
        .ok_or_else(|| "could not create the viewer window".to_string())?;
    let _ = wait_until_loaded(hwnd, &first, 15_000);
    Ok((files, hwnd))
}

/// The mash/nav benches' shared start: the viewer opened headlessly on `dir`'s ordered previewable
/// files, plus a report buffer that already carries `tag`'s headline line. `None` means the viewer
/// would not start, and the reason has already gone to `flush`.
pub(super) fn bench_prepare(
    hinst: HINSTANCE,
    dir: &str,
    tag: &str,
    flush: &dyn Fn(&str),
) -> Option<(Vec<std::path::PathBuf>, HWND, String)> {
    let (files, hwnd) = match bench_viewer(hinst, dir) {
        Ok(ready) => ready,
        Err(e) => {
            flush(&format!("{tag}: {e}\n"));
            return None;
        }
    };
    let mut out = String::new();
    let _ = writeln!(out, "{tag}: {} files in {dir}", files.len());
    Some((files, hwnd, out))
}

/// The mash/nav benches' prologue: the report sink, the viewer and the report buffer, ready to
/// start driving keys into `hwnd`. `None` means the viewer would not start, and the reason has
/// already gone to the report file.
#[allow(
    clippy::type_complexity,
    reason = "one setup tuple, destructured once by each of the three benches"
)]
pub(super) fn bench_start(
    hinst: HINSTANCE,
    dir: &str,
    tag: &str,
    out_name: &str,
) -> Option<(impl Fn(&str), Vec<std::path::PathBuf>, HWND, String)> {
    let flush = bench_output(4, out_name);
    let (files, hwnd, out) = bench_prepare(hinst, dir, tag, &flush)?;
    Some((flush, files, hwnd, out))
}

/// `--bench-preview <dir>`: measure what a ←/→ step actually costs.
///
/// Runs the viewer's REAL decode entry point (`content::bench_decode_uncached`, the same
/// `read_and_decode` the worker calls) over every previewable image in `dir`, twice:
///
///   * COLD — nothing cached, which is what every arrow-key press used to cost;
///   * WARM — served from the decode cache, which is what a revisit costs now.
///
/// Prints per-file and totals. Deliberately single-threaded and in-process: it is measuring
/// decode + cache, not process startup or window creation, and mixing those in was how
/// "it feels faster" replaced an actual number.
/// Results go to a FILE, not stdout: this EXE is a windows-subsystem binary with no console
/// of its own, so a `println!` here writes to a closed handle. Attaching the parent console
/// would need a new `windows` crate feature on a binary whose size is release-gated — not
/// worth it for a dev tool. Second argument overrides the path.
pub fn run_bench(dir: &str) {
    let mut out = String::new();
    let flush = bench_output(3, "st2k-bench.txt");
    if dir.is_empty() {
        flush("usage: SageThumbs2K.exe --bench-preview <folder> [out.txt]\n");
        return;
    }
    let mut files = match bench_files(dir, st2k_base::formats::is_known) {
        Ok(files) => files,
        Err(e) => {
            flush(&format!("bench: {e}\n"));
            return;
        }
    };
    files.sort();
    if files.is_empty() {
        flush(&format!("bench: no decodable files in {dir}\n"));
        return;
    }

    let _ = writeln!(out, "bench: {} files from {dir}", files.len());
    let _ = writeln!(out, "{}\n", load_snapshot());
    let _ = writeln!(
        out,
        "{:<34} {:>10} {:>10} {:>10} {:>11}",
        "file", "cold", "warm", "to-DIB", "wic-scaled"
    );
    let _ = writeln!(out, "{}", "-".repeat(82));

    let (mut cold_total, mut warm_total, mut counted) = (0u128, 0u128, 0usize);
    for p in &files {
        let path = p.to_string_lossy().into_owned();
        let name: String = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        let t0 = std::time::Instant::now();
        let cold = content::bench_decode_uncached(&path);
        let cold_us = t0.elapsed().as_micros();
        if cold.is_none() {
            let _ = writeln!(out, "{name:<34} {:>10} {:>10}  (no decode)", "-", "-");
            continue;
        }

        let t1 = std::time::Instant::now();
        let warm = content::bench_decode_cached(&path);
        let warm_us = t1.elapsed().as_micros();
        if warm.is_none() {
            let _ = writeln!(
                out,
                "{name:<34} {:>10} {:>10}  CACHE MISS",
                fmt_us(cold_us),
                "-"
            );
            continue;
        }

        cold_total += cold_us;
        warm_total += warm_us;
        counted += 1;
        let speedup = if warm_us > 0 {
            format!("{:.0}x", cold_us as f64 / warm_us as f64)
        } else {
            ">1000x".to_string()
        };
        let dib = content::bench_make_render(&path)
            .map(fmt_us)
            .unwrap_or_else(|| "-".to_string());
        let scaled = content::bench_scaled_decode(&path)
            .map(fmt_us)
            .unwrap_or_else(|| "-".to_string());
        let _ = writeln!(
            out,
            "{name:<34} {:>10} {:>10} {:>10} {:>11}   {speedup}",
            fmt_us(cold_us),
            fmt_us(warm_us),
            dib,
            scaled
        );
    }

    if counted == 0 {
        out.push_str("\nbench: nothing decoded\n");
        flush(&out);
        return;
    }
    let _ = writeln!(out, "{}", "-".repeat(70));
    let _ = writeln!(
        out,
        "{:<34} {:>10} {:>10}  {:.0}x",
        format!("TOTAL ({counted} files)"),
        fmt_us(cold_total),
        fmt_us(warm_total),
        cold_total as f64 / warm_total.max(1) as f64
    );
    let _ = writeln!(
        out,
        "{:<34} {:>10} {:>10}",
        "MEAN per step",
        fmt_us(cold_total / counted as u128),
        fmt_us(warm_total / counted as u128)
    );
    flush(&out);
}

/// `--bench-mash <dir> <keys>`: hold the arrow key down, then time the catch-up.
///
/// This posts every keypress at a real key-repeat cadence WITHOUT waiting, then measures from the
/// first press to the moment the LAST file is on screen. That is the number a user feels when
/// they lean on the key, and the only one that shows whether abandoning superseded work helps.
///
/// Set `ST2K_NO_CANCEL=1` to measure the same binary with abandonment switched off.
pub fn run_mash_bench(hinst: HINSTANCE, dir: &str, keys: usize) {
    let Some((flush, files, hwnd, mut out)) =
        bench_start(hinst, dir, "bench-mash", "st2k-mashbench.txt")
    else {
        return;
    };
    let _ = writeln!(
        out,
        "cancellation: {}",
        if std::env::var_os("ST2K_NO_CANCEL").is_some() {
            "OFF (ST2K_NO_CANCEL)"
        } else {
            "on"
        }
    );
    let _ = writeln!(out, "{}\n", load_snapshot());

    // ~30 ms between presses is a normal Windows key-repeat rate once the initial delay has
    // elapsed. Faster than any decode, which is the whole point.
    const REPEAT_MS: u64 = 30;
    let target = files[keys % files.len()].to_string_lossy().into_owned();
    let name = files[keys % files.len()]
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    let t0 = std::time::Instant::now();
    let mut last_press = t0;
    for _ in 0..keys {
        let _ = unsafe {
            PostMessageW(
                Some(hwnd),
                WM_KEYDOWN,
                windows::Win32::Foundation::WPARAM(VK_RIGHT.0 as usize),
                windows::Win32::Foundation::LPARAM(0),
            )
        };
        // Pump so the window actually processes the press, the way a real key repeat arrives
        // into a running message loop rather than all at once into a full queue.
        last_press = std::time::Instant::now();
        let deadline = last_press + std::time::Duration::from_millis(REPEAT_MS);
        while std::time::Instant::now() < deadline {
            drain_messages();
        }
    }
    let landed = wait_until_loaded(hwnd, &target, 30_000);
    let us = t0.elapsed().as_micros();
    // The CATCH-UP is the number that matters. Total time includes `keys * REPEAT_MS` of
    // pressing, a fixed floor that swamps everything; what abandoning superseded work can
    // change is only how long the viewer takes to settle AFTER the user stops.
    let tail_us = last_press.elapsed().as_micros();

    let _ = writeln!(out, "{keys} keypresses at {REPEAT_MS} ms, target {name}");
    let _ = writeln!(
        out,
        "first keypress -> final file painted: {}",
        if landed {
            fmt_us(us)
        } else {
            "TIMEOUT".to_string()
        }
    );
    let _ = writeln!(
        out,
        "workers abandoned: {}   <- work not done for files already scrolled past",
        content::bench_abandoned_count()
    );
    let _ = writeln!(
        out,
        "LAST keypress -> final file painted: {}   <- the catch-up",
        if landed {
            fmt_us(tail_us)
        } else {
            "TIMEOUT".to_string()
        }
    );
    flush(&out);
}

/// `--bench-nav <dir> <steps> [out.txt]`: what a RIGHT-ARROW press actually costs, end to end.
///
/// Unlike [`run_bench`], which times the decode in isolation, this drives the real thing: it
/// builds the real viewer window, posts real `WM_KEYDOWN VK_RIGHT` messages into its real
/// wndproc, and pumps the real message loop until the next file is genuinely installed and
/// painted. That covers the folder listing, the sort, the prefetch, the cache, the worker
/// hand-off and the repaint — everything between the key going down and the picture being up,
/// which is the only number a user experiences.
///
/// The window is created off-screen and hidden (same construction the headless `--shot` uses),
/// so this needs no desktop, steals no focus, and runs unattended.
///
/// `--bench-nav` waits for each step to paint before pressing again, so it never has two
/// decodes in flight and cannot see the cost of work the user has already scrolled past.
pub fn run_nav_bench(hinst: HINSTANCE, dir: &str, steps: usize) {
    let Some((flush, files, hwnd, mut out)) =
        bench_start(hinst, dir, "bench-nav", "st2k-navbench.txt")
    else {
        return;
    };
    let _ = writeln!(out, "{}\n", load_snapshot());
    let _ = writeln!(
        out,
        "{:<6} {:<34} {:>12}",
        "step", "landed on", "keypress->painted"
    );
    let _ = writeln!(out, "{}", "-".repeat(60));

    let (mut total, mut worst, mut counted) = (0u128, 0u128, 0usize);
    let mut timeouts = 0usize;
    for step in 1..=steps {
        let expected = files[step % files.len()].to_string_lossy().into_owned();
        let name = files[step % files.len()]
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let t0 = std::time::Instant::now();
        // The REAL key path: the wndproc's WM_KEYDOWN arm decides this is navigation.
        let _ = unsafe {
            PostMessageW(
                Some(hwnd),
                WM_KEYDOWN,
                windows::Win32::Foundation::WPARAM(VK_RIGHT.0 as usize),
                windows::Win32::Foundation::LPARAM(0),
            )
        };
        let landed = wait_until_loaded(hwnd, &expected, 15_000);
        let us = t0.elapsed().as_micros();
        if landed {
            total += us;
            worst = worst.max(us);
            counted += 1;
            let _ = writeln!(out, "{step:<6} {name:<34} {:>12}", fmt_us(us));
        } else {
            timeouts += 1;
            let _ = writeln!(out, "{step:<6} {name:<34} {:>12}", "TIMEOUT");
        }
    }
    unsafe { window::request_close(hwnd) };

    let _ = writeln!(out, "{}", "-".repeat(60));
    if counted > 0 {
        let _ = writeln!(
            out,
            "{:<41} {:>12}",
            format!("MEAN of {counted} steps"),
            fmt_us(total / counted as u128)
        );
        let _ = writeln!(out, "{:<41} {:>12}", "WORST step", fmt_us(worst));
    }
    let _ = writeln!(out, "{:<41} {:>12}", "timeouts", timeouts);
    flush(&out);
}

/// `--probe-preview <file> <out.png>`: the picture the Quick preview ends up showing for one
/// file (its first stage and the composite chase, resolved exactly as `--shot` resolves them),
/// written to `<out.png>`, with `<out.png>.tsv` holding `width  height  milliseconds`
/// (`0 0 ms` when nothing decoded). The big-file gate (`scripts/bigfiles/bigfiles.py`) runs a
/// document grown past the size gates and its normal-size twin through this; issue #46, a
/// 300 MB Photoshop document left on its 160-pixel preview, is exactly what it catches.
/// A picture wider than 4096 is saved at 4096: the gate compares at a common size anyway.
pub fn run_probe(file: &str, out: &str) {
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    let start = std::time::Instant::now();
    let decoded = content::decode_sync(file);
    let ms = start.elapsed().as_millis();
    let (w, h) = decoded.as_ref().map_or((0, 0), |d| (d.w, d.h));
    let img = decoded
        .and_then(|d| image::RgbaImage::from_raw(d.w as u32, d.h as u32, d.rgba))
        .map(|img| probe_sized(image::DynamicImage::ImageRgba8(img)));
    if let Some(img) = img {
        let _ = img.save(out);
    }
    let _ = std::fs::write(format!("{out}.tsv"), format!("{w}\t{h}\t{ms}\n"));
}

/// At most 4096 on the long side, which is all the comparison needs.
fn probe_sized(img: image::DynamicImage) -> image::DynamicImage {
    if img.width().max(img.height()) <= 4096 {
        return img;
    }
    img.resize(4096, 4096, image::imageops::FilterType::Triangle)
}
