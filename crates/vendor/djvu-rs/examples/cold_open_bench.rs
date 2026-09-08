//! Cold-open measurement harness (COLD_OPEN, round 36).
//!
//! Unblocks the round-8 backlog item "B6 madvise / B7 speculative next-page
//! decode — need a cold-disk harness; no such bench exists". Answers three
//! questions, each its own `--mode`:
//!
//! - `validate` — does this harness actually produce a cold-vs-warm gap? Run
//!   this first; if `cold/warm` isn't clearly > 1, the other modes' numbers
//!   aren't trustworthy.
//! - `madvise`  — B6: does `MADV_WILLNEED` on the header/DIRM region + the
//!   target page's byte range shrink cold open→first-render latency?
//! - `prefetch` — B7: does `DjVuDocument::prefetch_page` (background decode
//!   while the reader is still looking at the current page) shrink the next
//!   page's render latency?
//!
//! ## Cold strategy
//!
//! True cold-disk measurement needs pages absent from the OS page cache.
//! Automating that without root is the hard part on macOS (no `posix_fadvise`
//! DONTNEED like Linux; `sudo purge` needs a password prompt). This harness
//! copies the corpus file to a fresh temp path on every iteration, writing
//! the destination through a file descriptor flagged `F_NOCACHE` (macOS) so
//! the written pages are not retained in the unified buffer cache — a
//! `mmap`/read of that fresh path should require genuine disk I/O rather than
//! reusing whatever's already resident from a previous iteration or from the
//! copy's own write-back. Falls back to a plain copy (no cache hint) on
//! non-macOS Unix; still defeats page-cache identity (new inode) even
//! without the fcntl, just less reliably.
//!
//! `sudo purge` (macOS) or `echo 3 | sudo tee /proc/sys/vm/drop_caches`
//! (Linux) before a single-iteration `--mode validate --iters 1` run remains
//! the gold-standard manual cross-check if the automated numbers look
//! suspicious (e.g. `cold/warm` collapsing to ~1x, meaning the copy trick
//! stopped defeating the cache on that machine/kernel).
//!
//! ## Usage
//!
//! ```sh
//! cargo run --release --example cold_open_bench --features mmap,parallel -- \
//!   --file tests/corpus/pathogenic_bacteria_1896.djvu --mode validate --iters 9
//!
//! cargo run --release --example cold_open_bench --features mmap,parallel -- \
//!   --mode madvise --iters 9 --page 0
//!
//! cargo run --release --example cold_open_bench --features mmap,parallel -- \
//!   --mode prefetch --iters 9 --page 0 --dwell-ms 150
//! ```

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use djvu_rs::{
    djvu_document::{DjVuPage, MmapDocument},
    djvu_render::{RenderOptions, render_pixmap},
};

#[cfg(target_os = "macos")]
use std::os::unix::io::AsRawFd;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut file = PathBuf::from("tests/corpus/pathogenic_bacteria_1896.djvu");
    let mut mode = "validate".to_string();
    let mut iters = 9usize;
    let mut page_idx = 0usize;
    let mut dwell_ms = 150u64;
    let mut delay_ms = 0u64;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--file" => file = PathBuf::from(args.next().ok_or("--file requires a value")?),
            "--mode" => mode = args.next().ok_or("--mode requires a value")?,
            "--iters" => iters = args.next().ok_or("--iters requires a value")?.parse()?,
            "--page" => page_idx = args.next().ok_or("--page requires a value")?.parse()?,
            "--dwell-ms" => dwell_ms = args.next().ok_or("--dwell-ms requires a value")?.parse()?,
            "--delay-ms" => delay_ms = args.next().ok_or("--delay-ms requires a value")?.parse()?,
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }

    if !file.exists() {
        return Err(format!(
            "corpus file not found: {} (see SOURCES.md / tests/corpus)",
            file.display()
        )
        .into());
    }

    match mode.as_str() {
        "validate" => validate_cold_vs_warm(&file, page_idx, iters)?,
        "madvise" => madvise_lever(&file, page_idx, iters, delay_ms)?,
        "prefetch" => prefetch_lever(&file, page_idx, iters, dwell_ms)?,
        other => {
            return Err(format!("unknown --mode {other} (want validate|madvise|prefetch)").into());
        }
    }
    Ok(())
}

// ---- cold-copy machinery ----------------------------------------------------

/// Write `data` to a fresh file at `dst`, best-effort defeating the OS page
/// cache for the destination (see module docs). Returns once the bytes are
/// physically synced.
fn write_cold(dst: &Path, data: &[u8]) -> io::Result<()> {
    let file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(dst)?;
    disable_cache(&file);
    {
        use std::io::Write;
        let mut w = &file;
        w.write_all(data)?;
        w.flush()?;
    }
    file.sync_all()?;
    Ok(())
}

/// Copy `src` to a fresh temp path under `dir` tagged `tag`, written cold.
fn make_cold_copy(src: &Path, dir: &Path, tag: u64) -> io::Result<PathBuf> {
    let data = fs::read(src)?;
    let dst = dir.join(format!("cold_open_bench_{tag}.djvu"));
    write_cold(&dst, &data)?;
    Ok(dst)
}

#[cfg(target_os = "macos")]
fn disable_cache(file: &fs::File) {
    let fd = file.as_raw_fd();
    // SAFETY: `fd` is a valid, open file descriptor owned by `file` for the
    // duration of this call. `fcntl(fd, F_NOCACHE, 1)` only flips a kernel-side
    // flag on the vnode's per-fd state — it does not touch memory through any
    // pointer, so there is no aliasing or lifetime hazard. Best-effort: a
    // non-zero return (unsupported fd type) is intentionally ignored, the
    // caller falls back to whatever caching behaviour the OS gives it.
    unsafe {
        libc::fcntl(fd, libc::F_NOCACHE, 1);
    }
}

#[cfg(not(target_os = "macos"))]
fn disable_cache(_file: &fs::File) {}

// ---- stats -------------------------------------------------------------------

fn median(samples: &mut [f64]) -> f64 {
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = samples.len();
    if n == 0 {
        return f64::NAN;
    }
    if n % 2 == 1 {
        samples[n / 2]
    } else {
        (samples[n / 2 - 1] + samples[n / 2]) / 2.0
    }
}

/// Median absolute deviation — a robust dispersion measure that isn't
/// dragged around by the occasional thermal-noise/scheduler-jitter outlier
/// the way stdev is.
fn mad(samples: &[f64], med: f64) -> f64 {
    let mut dev: Vec<f64> = samples.iter().map(|s| (s - med).abs()).collect();
    median(&mut dev)
}

fn report(label: &str, mut samples_ms: Vec<f64>) -> f64 {
    let med = median(&mut samples_ms);
    let d = mad(&samples_ms, med);
    let min = samples_ms.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = samples_ms.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    println!(
        "{label:<24} median={med:>9.3} ms   MAD={d:>7.3} ms   min={min:>9.3}   max={max:>9.3}   n={}",
        samples_ms.len()
    );
    med
}

// ---- shared render helper ----------------------------------------------------

fn native_opts(page: &DjVuPage) -> RenderOptions {
    RenderOptions {
        width: page.width() as u32,
        height: page.height() as u32,
        ..RenderOptions::default()
    }
}

fn render_native(page: &DjVuPage) -> Result<(), Box<dyn std::error::Error>> {
    let opts = native_opts(page);
    let _pm = render_pixmap(page, &opts)?;
    Ok(())
}

/// Open `path` via `mmap`, optionally issue the B6 `madvise(WILLNEED)` hint,
/// sleep `delay_ms` (simulating whatever the caller does between "know I'll
/// need this page" and "actually render it"), then render `page_idx` at
/// native resolution. Returns wall time from the start of `open` to the end
/// of `render_pixmap`.
fn open_and_render(
    path: &Path,
    page_idx: usize,
    advise: bool,
    delay_ms: u64,
) -> Result<Duration, Box<dyn std::error::Error>> {
    let start = Instant::now();
    let doc = MmapDocument::open(path)?;
    if advise {
        #[cfg(unix)]
        {
            let _ = doc.advise_page_willneed(page_idx);
        }
    }
    if delay_ms > 0 {
        std::thread::sleep(Duration::from_millis(delay_ms));
    }
    let page = doc.page(page_idx)?;
    render_native(page)?;
    Ok(start.elapsed())
}

// ---- mode: validate -----------------------------------------------------------

fn validate_cold_vs_warm(
    file: &Path,
    page_idx: usize,
    iters: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let tmp_dir = std::env::temp_dir().join("djvu_cold_open_bench");
    fs::create_dir_all(&tmp_dir)?;

    println!("=== validate: cold (F_NOCACHE fresh copy) vs warm (page-cache hit) ===");
    println!(
        "file={} page={page_idx} iters={iters} (harness sanity check — cold should be clearly > warm)",
        file.display()
    );

    // Warm baseline: warm the real corpus path up first, then measure repeat
    // opens+renders of the SAME path (OS page cache hit on every one).
    let _ = open_and_render(file, page_idx, false, 0)?;
    let mut warm = Vec::with_capacity(iters);
    for _ in 0..iters {
        warm.push(open_and_render(file, page_idx, false, 0)?.as_secs_f64() * 1000.0);
    }

    // Cold: fresh cold copy each iteration, deleted right after use so it
    // can't warm up a later iteration.
    let mut cold = Vec::with_capacity(iters);
    for i in 0..iters {
        let copy_path = make_cold_copy(file, &tmp_dir, i as u64)?;
        let d = open_and_render(&copy_path, page_idx, false, 0)?;
        cold.push(d.as_secs_f64() * 1000.0);
        let _ = fs::remove_file(&copy_path);
    }

    let warm_med = report("warm (same path, hot)", warm);
    let cold_med = report("cold (F_NOCACHE copy)", cold);
    let ratio = cold_med / warm_med.max(1e-9);
    println!("cold/warm ratio: {ratio:.2}x");
    if ratio > 1.3 {
        println!("VERDICT: harness shows a clear cold >> warm gap — trust the other modes.");
    } else {
        println!(
            "VERDICT: gap is weak/inconclusive on this machine — cross-check with `sudo purge` \
             (macOS) + `--mode validate --iters 1` before trusting madvise/prefetch deltas."
        );
    }
    Ok(())
}

// ---- mode: madvise (B6) --------------------------------------------------------

fn madvise_lever(
    file: &Path,
    page_idx: usize,
    iters: usize,
    delay_ms: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let tmp_dir = std::env::temp_dir().join("djvu_cold_open_bench");
    fs::create_dir_all(&tmp_dir)?;

    println!("=== B6 madvise(WILLNEED): cold open -> first render, page {page_idx} ===");
    println!("file={} iters={iters} delay_ms={delay_ms}", file.display());

    let mut no_advise = Vec::with_capacity(iters);
    let mut with_advise = Vec::with_capacity(iters);

    // Interleaved (no-advise, advise) pairs so any drift (thermal throttling,
    // background load) affects both arms evenly instead of biasing whichever
    // arm runs in the second half.
    for i in 0..iters {
        let p1 = make_cold_copy(file, &tmp_dir, i as u64 * 2)?;
        let d1 = open_and_render(&p1, page_idx, false, delay_ms)?;
        no_advise.push(d1.as_secs_f64() * 1000.0);
        let _ = fs::remove_file(&p1);

        let p2 = make_cold_copy(file, &tmp_dir, i as u64 * 2 + 1)?;
        let d2 = open_and_render(&p2, page_idx, true, delay_ms)?;
        with_advise.push(d2.as_secs_f64() * 1000.0);
        let _ = fs::remove_file(&p2);
    }

    let base_med = report("no madvise", no_advise);
    let hint_med = report("madvise(WILLNEED)", with_advise);
    let delta = (base_med - hint_med) / base_med.max(1e-9) * 100.0;
    println!("delta: {delta:+.1}% ({}ms -> {}ms)", base_med, hint_med);
    Ok(())
}

// ---- mode: prefetch (B7) --------------------------------------------------------

fn prefetch_lever(
    file: &Path,
    page_idx: usize,
    iters: usize,
    dwell_ms: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    // Page-turn latency is a CPU/decode-overlap question, not a disk-cold
    // question — warm the whole file into the OS cache up front so both arms
    // start from the same (warm) I/O baseline and any delta is purely from
    // decode work being (or not being) overlapped with the reader's dwell time.
    let _ = fs::read(file)?;

    let next_idx = page_idx + 1;
    println!("=== B7 prefetch_page: render latency of page {next_idx} after page {page_idx} ===");
    println!("file={} iters={iters} dwell_ms={dwell_ms}", file.display());

    let mut no_prefetch = Vec::with_capacity(iters);
    let mut with_prefetch = Vec::with_capacity(iters);

    for _ in 0..iters {
        // Fresh document each iteration so render caches start cold — mirrors
        // a reader who has just opened the book and is turning pages for the
        // first time (the case prefetch is meant to help).
        let doc = Arc::new(MmapDocument::open(file)?.into_document());
        if next_idx >= doc.page_count() {
            return Err("--page + 1 is out of range for this corpus".into());
        }
        render_native(doc.page(page_idx)?)?;
        std::thread::sleep(Duration::from_millis(dwell_ms));
        let start = Instant::now();
        render_native(doc.page(next_idx)?)?;
        no_prefetch.push(start.elapsed().as_secs_f64() * 1000.0);
    }

    for _ in 0..iters {
        let doc = Arc::new(MmapDocument::open(file)?.into_document());
        render_native(doc.page(page_idx)?)?;
        doc.prefetch_page(next_idx);
        std::thread::sleep(Duration::from_millis(dwell_ms));
        let start = Instant::now();
        render_native(doc.page(next_idx)?)?;
        with_prefetch.push(start.elapsed().as_secs_f64() * 1000.0);
    }

    let base_med = report("no prefetch", no_prefetch);
    let pf_med = report("prefetch_page", with_prefetch);
    let delta = (base_med - pf_med) / base_med.max(1e-9) * 100.0;
    println!("delta: {delta:+.1}% ({}ms -> {}ms)", base_med, pf_med);
    Ok(())
}
