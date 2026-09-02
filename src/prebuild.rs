//! Pre-build Explorer thumbnails for a folder tree, without browsing it first.
//!
//! # Why this cannot just call our own decoder
//!
//! `thumbcache_*.db` is written by Windows, not by us. The shell calls our
//! [`IThumbnailProvider`](crate::thumbprovider) and caches what it gets back, so a thumbnail
//! we render ourselves is a bitmap nobody records — the folder would still build every tile
//! from scratch on first browse. To fill the cache we have to ask the SHELL for the
//! thumbnail and let it call us. That is the same round trip `doctor::shell_roundtrip`
//! already documents: extracting a thumbnail "lets Windows populate its own thumbnail cache
//! for this item — exactly what browsing to the folder would have done."
//!
//! [`IThumbnailCache`] is the right primitive rather than `IShellItemImageFactory`, because
//! it exposes `WTS_INCACHEONLY`: a probe that answers "is this already cached?" without
//! extracting anything. That is what makes "only build what is missing" cheap instead of a
//! full re-extract of a library that was already done.
//!
//! # What this deliberately does NOT do
//!
//! Rebuild-changed-only and pause/resume are not here. The shell already keys its cache on
//! path + size + mtime, so the `WTS_INCACHEONLY` probe IS change detection for free; a
//! separate "changed files" mode would need our own sidecar index to beat it. Pause needs a
//! gate inside [`crate::parallel`]'s worker loop, which has no such primitive today — v1
//! offers cancel (Ctrl+C) instead.
//!
//! Purging is not here either: no API deletes cache entries for one folder, only the whole
//! per-user database, which Settings ▸ Advanced ▸ "Rebuild thumbnail cache" already does
//! (it restarts Explorer, which is not something a batch verb should do behind your back).

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::formats;

/// Files whose thumbnail we will not ask for, and why the count is reported separately.
#[derive(Default)]
pub struct Report {
    /// Supported files found by the walk.
    pub found: usize,
    /// Thumbnails the shell extracted (and therefore cached) this run.
    pub built: usize,
    /// Already in the cache at the requested size — no work done.
    pub already: usize,
    /// The shell refused a thumbnail (unsupported, corrupt, handler not registered).
    pub failed: usize,
    /// Cached at some requested sizes but not all — the view that reads a missing bucket will
    /// still extract it on first browse. Counted separately so a run cannot claim to have
    /// finished work it did not do.
    pub partial: usize,
    /// Cloud placeholders left alone so we never trigger a multi-gigabyte rehydration.
    pub skipped_offline: usize,
    /// Directories the walk could not read (permissions, vanished mid-walk).
    pub unreadable_dirs: usize,
    /// The size buckets actually used, after [`normalize_sizes`]. Reported rather than assumed
    /// so the caller can say which views are now warm instead of echoing what was asked for.
    pub sizes: Vec<u32>,
    /// The run stopped early because the caller set its cancel flag.
    pub cancelled: bool,
}

/// Knobs for [`run`]. Defaults match the CLI's defaults.
pub struct Options {
    pub recurse: bool,
    /// Edges in pixels, **in the order they will be attempted**. [`run`] puts them in
    /// [`build_order`] (largest first) before the workers ever see them, and that order is
    /// load-bearing rather than cosmetic — read [`build_order`] before changing it.
    pub sizes: Vec<u32>,
    /// Skip the `WTS_INCACHEONLY` probe and extract every file.
    pub rebuild_all: bool,
    /// Worker threads. Clamped hard: these calls serialise inside the shell, so a wide pool
    /// buys nothing and starves the machine.
    pub jobs: usize,
    /// Guard against a symlink/junction cycle turning a walk into an infinite one.
    pub max_depth: u32,
}

/// The buckets Explorer's own views read: 96 is Medium icons, 256 is Large, 768 is Extra
/// large. Details/List/Small icons draw no thumbnail at all, so there is nothing to prefill
/// for them, and the giant buckets above 768 are only reached by a slider most people never
/// touch — building those by default would multiply the run time for a view nobody is in.
///
/// Because one extraction fills every SMALLER bucket (see [`build_order`]), the entry that
/// actually decides what this run costs and what it accomplishes is the LARGEST one. The other
/// two cost a probe each and exist to verify the fill really happened.
pub const DEFAULT_SIZES: [u32; 3] = [96, 256, 768];

/// Windows' actual cache buckets. A request lands in the smallest bucket that fits it, so
/// asking for 200 fills the 256 one; normalising up front keeps the report honest about what
/// was really built and stops two requested sizes silently doing the same work twice.
///
/// CORRECTED 2026-08-14 against the real set of `thumbcache_*.db` files Windows 10/11 ship:
/// 16, 32, 48, 96, 256, 768, 1280, 1920, 2560. The old list carried **1024**, which is a
/// Windows 7 era bucket that no longer exists, and was missing 1280 and 2560 — so a request in
/// the 769..=1280 range normalised to a bucket Windows does not keep, and 2560 (newly
/// reachable since `settings::THUMB_MAX` was raised) clamped down to 1920.
///
/// This only ever affected our own dedup and the "what did I build" report: `one()` passes the
/// raw size to `IThumbnailCache`, which does its own bucket selection regardless of what we
/// think the buckets are. So this is an honesty fix, not a behaviour fix — worth having
/// precisely because the report is what anyone debugging a missing thumbnail reads first.
const BUCKETS: [u32; 9] = [16, 32, 48, 96, 256, 768, 1280, 1920, 2560];
/// The last entry of [`BUCKETS`], spelled as a constant so the clamp below needs no runtime
/// unwrap for a value the array literal already guarantees.
const LARGEST: u32 = BUCKETS[BUCKETS.len() - 1];

/// Round each requested edge up to the bucket that will actually hold it, then dedupe.
pub fn normalize_sizes(requested: &[u32]) -> Vec<u32> {
    let mut out: Vec<u32> = requested
        .iter()
        .filter(|s| **s > 0)
        // Anything past the top bucket clamps to it rather than being dropped, so a silly
        // request still builds something instead of silently doing nothing.
        .map(|s| BUCKETS.iter().copied().find(|b| b >= s).unwrap_or(LARGEST))
        .collect();
    out.sort_unstable();
    out.dedup();
    if out.is_empty() {
        out.push(256);
    }
    out
}

/// The order buckets must be extracted in: **LARGEST FIRST**. This is the whole of the
/// "pre-build didn't do anything for my folder" bug, and it is the opposite of what the code
/// did for its entire life, so the reasoning is recorded rather than asserted.
///
/// # What the shell actually does, measured
///
/// One extraction fills EVERY smaller bucket. Asking `IThumbnailCache` for three sizes does
/// NOT call [`crate::thumbprovider`] three times — it calls it exactly once, for whichever
/// size is asked for first, and satisfies the rest by deriving from that one bitmap:
///
/// ```text
/// requested 96,256,768  -> provider called once, cx=96   (768 bucket then holds 96x72)
/// requested 768         -> provider called once, cx=768
///   then 256, then 96   -> provider NOT called; both already satisfied
/// ```
///
/// # Why ascending was silently broken
///
/// [`normalize_sizes`] sorts, so the shipped default `[96, 256, 768]` always extracted at 96
/// and never at anything else. Windows would happily report the 256 and 768 buckets as
/// "cached" — `WTS_INCACHEONLY` SUCCEEDS for them — so the run reported total success. But the
/// only real thumbnail was 96 px, and the moment the user opened that folder in Large or
/// Extra-large icons the shell threw the derived entry away and re-extracted from scratch:
/// exactly the slow tile-by-tile build this feature exists to prevent, after a run that said
/// it had prevented it. Verified by asking the shell the way Explorer does (no
/// `SIIGBF_INCACHEONLY`): after an ascending pre-build it re-extracted at 768; after a
/// descending one it served 768 from the cache and never touched the provider.
///
/// Note what this means for the honesty fix that shipped alongside it: [`Outcome::Partial`]
/// could not have caught this, because nothing FAILED. Every probe succeeded and every
/// extraction succeeded. The report was accurate about the calls it made and wrong about what
/// they accomplished.
///
/// # The cost, stated plainly
///
/// Largest-first is SLOWER per file — one render at 768 costs more than one at 96 — and that
/// is the correct trade, because the old speed was the speed of not doing the job. It is still
/// ONE render per file, not one per bucket.
fn build_order(sizes: &[u32]) -> Vec<u32> {
    let mut v = sizes.to_vec();
    v.sort_unstable_by(|a, b| b.cmp(a));
    v
}

impl Default for Options {
    fn default() -> Self {
        Self {
            recurse: false,
            sizes: DEFAULT_SIZES.to_vec(),
            rebuild_all: false,
            jobs: 3,
            max_depth: 64,
        }
    }
}

/// `FILE_ATTRIBUTE_OFFLINE` | `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS` | `FILE_ATTRIBUTE_RECALL_ON_OPEN`.
/// A OneDrive/Dropbox placeholder carries one of these; extracting its thumbnail DOWNLOADS the
/// whole file, so a "pre-build my library" run over a cloud folder would silently pull the
/// entire library onto the disk it was meant to stay off. Skipped, and counted so the report
/// says it happened.
///
/// `pub(crate)` (not private): `doctor.rs`'s cloud-placeholder diagnostic and `cli.rs`'s
/// `expand_inputs` (the `st2k batch` cloud guard) check the SAME trio — this used to be three
/// independently hand-typed copies of the same three flags, which is exactly how one of them
/// (`doctor.rs`) drifted to missing RECALL_ON_OPEN. One definition now; a file carrying only
/// RECALL_ON_OPEN must be skipped/flagged everywhere that reads this constant.
pub(crate) const OFFLINE_ATTRS: u32 = 0x0000_1000 | 0x0040_0000 | 0x0004_0000;
/// `FILE_ATTRIBUTE_REPARSE_POINT` — junctions and symlinks, which the walk does not follow.
const REPARSE: u32 = 0x0000_0400;

fn attrs(p: &Path) -> u32 {
    use std::os::windows::fs::MetadataExt;
    std::fs::symlink_metadata(p)
        .map(|m| m.file_attributes())
        .unwrap_or(0)
}

/// True when `path` is a cloud placeholder (a OneDrive/Dropbox file not yet downloaded to this
/// machine): a `symlink_metadata` attribute check ONLY, so calling this never itself triggers
/// hydration. `pub` (not `pub(crate)`): `cli.rs`'s `expand_inputs` cloud guard and
/// `bin/app/tools/convert.rs` check the exact same trio and used to each hand-type their own
/// copy of [`OFFLINE_ATTRS`] (one of which, `doctor.rs`, drifted to missing a flag) — one
/// definition now, reused everywhere a caller needs to decide "would extracting this file's
/// thumbnail download the whole thing?" (item C13).
pub fn is_cloud_placeholder(path: &Path) -> bool {
    attrs(path) & OFFLINE_ATTRS != 0
}

/// Is this extension one we hook AND the user still has enabled? A format they turned off has
/// no SageThumbs thumbnail to build, and asking the shell for one just burns a round trip.
/// Takes a pre-taken [`crate::settings::FormatEnabledSnapshot`] rather than calling
/// [`crate::settings::format_enabled`] per file: in portable mode that reparses the WHOLE ini
/// from disk on every single call, so a 50,000-file prebuild used to do 50,000 full ini parses
/// to decide what it wanted (item 134).
fn wanted(p: &Path, snap: &crate::settings::FormatEnabledSnapshot) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| formats::is_known(e) && snap.enabled(&e.to_lowercase()))
}

/// Collect the supported files under `root`. Non-recursive by default; never follows a
/// reparse point, and stops at `max_depth` so a junction cycle cannot spin forever. `snap` is
/// one [`crate::settings::format_enabled_snapshot`] shared across the whole walk — see
/// [`wanted`].
fn walk(
    root: &Path,
    opts: &Options,
    depth: u32,
    out: &mut Vec<String>,
    rep: &mut Report,
    snap: &crate::settings::FormatEnabledSnapshot,
) {
    if depth > opts.max_depth {
        return;
    }
    let Ok(rd) = std::fs::read_dir(root) else {
        rep.unreadable_dirs += 1;
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        let a = attrs(&p);
        if a & REPARSE != 0 {
            continue;
        }
        if p.is_dir() {
            if opts.recurse {
                walk(&p, opts, depth + 1, out, rep, snap);
            }
        } else if p.is_file() && wanted(&p, snap) {
            if is_cloud_placeholder(&p) {
                rep.skipped_offline += 1;
                continue;
            }
            out.push(p.to_string_lossy().into_owned());
        }
    }
}

/// Absolute path in the grammar `SHCreateItemFromParsingName` accepts.
///
/// `canonicalize` returns the extended-length form and the parsing-name grammar rejects it, so
/// strip it back: `\\?\C:\…` -> `C:\…`, and the UNC form `\\?\UNC\server\share` -> the plain
/// `\\server\share` (stripping only `\\?\` there would leave `UNC\…`, which resolves nowhere).
///
/// `pub(crate)`: `doctor.rs`'s `shell_roundtrip` needs the exact same normalization before its
/// own `SHCreateItemFromParsingName` call, and used to carry a hand-copied duplicate of this
/// logic (relocated from a fn into an inline block, near-verbatim) rather than importing it.
pub(crate) fn parsing_path(path: &str) -> String {
    Path::new(path)
        .canonicalize()
        .map(|p| {
            let s = p.to_string_lossy().into_owned();
            if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
                format!(r"\\{rest}")
            } else if let Some(rest) = s.strip_prefix(r"\\?\") {
                rest.to_string()
            } else {
                s
            }
        })
        .unwrap_or_else(|_| path.to_string())
}

/// What happened to one file.
enum Outcome {
    Built,
    Already,
    /// Some size buckets are in the cache and at least one is NOT. The distinction matters
    /// because the shell reads a SPECIFIC bucket for a given view: a file that built at 96 but
    /// not at 256 looks finished in the summary and then re-extracts, slowly, the moment the
    /// user opens the folder in Large Icons. Reported as "Built" it was a lie the run told
    /// about itself (issue #26).
    Partial,
    Failed,
}

/// COM apartment for the calling thread, initialised once and released when the thread ends.
///
/// The pool calls [`one`] per FILE, and an init/uninit pair around each of thousands of files
/// is pure overhead — so the guard lives in thread-local storage and Rust runs its destructor
/// when the worker thread exits.
struct Apartment(bool);
impl Drop for Apartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe { windows::Win32::System::Com::CoUninitialize() };
        }
    }
}
thread_local! {
    static APARTMENT: Apartment = {
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
        Apartment(unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok())
    };
}

/// The verdict for one file, given what happened across its size buckets.
///
/// `None` means "nothing landed at any size" and the caller logs + reports [`Outcome::Failed`];
/// splitting it out keeps that logging in `one` while making the RULE testable without COM.
///
/// The rule that matters is the first arm. Before this, a file that built at 96 and failed at
/// 256 reported as plain Built, so a run could say it had finished work it had not done, and
/// the user found out only when Explorer re-extracted the missing bucket on first browse
/// (issue #26). Partial is the honest answer.
fn verdict(built: bool, already: bool, all_sizes_landed: bool) -> Option<Outcome> {
    match (built || already, all_sizes_landed) {
        (true, false) => Some(Outcome::Partial),
        (true, true) if built => Some(Outcome::Built),
        (true, true) => Some(Outcome::Already),
        (false, _) => None,
    }
}

/// Ask the shell for one file's thumbnail, which is what gets it into Windows' own cache.
fn one(path: &str, opts: &Options) -> Outcome {
    use windows::core::HSTRING;
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
    use windows::Win32::UI::Shell::{
        IShellItem, IThumbnailCache, LocalThumbnailCache, SHCreateItemFromParsingName, WTS_EXTRACT,
        WTS_INCACHEONLY,
    };

    APARTMENT.with(|_| {});
    let abs = parsing_path(path);

    // One file, every requested bucket. The file's verdict is the most significant thing that
    // happened to it: built if any size needed work, already if every size was there, failed
    // only if no size could be produced at all — so a file that is cached at 96 but missing at
    // 256 reports as built, which is what actually happened.
    let r: windows::core::Result<Outcome> = (|| unsafe {
        let cache: IThumbnailCache =
            CoCreateInstance(&LocalThumbnailCache, None, CLSCTX_INPROC_SERVER)?;
        let item: IShellItem = SHCreateItemFromParsingName(&HSTRING::from(abs.as_str()), None)?;

        let (mut built, mut already) = (false, false);
        let mut missing: Vec<u32> = Vec::new();
        for &size in &opts.sizes {
            if !opts.rebuild_all {
                // Probe first. This never extracts, so a library that is already built costs
                // one cheap call per size instead of a full re-render.
                let mut bmp = None;
                if cache
                    .GetThumbnail(&item, size, WTS_INCACHEONLY, Some(&mut bmp), None, None)
                    .is_ok()
                {
                    already = true;
                    continue;
                }
            }
            let mut bmp = None;
            match cache.GetThumbnail(&item, size, WTS_EXTRACT, Some(&mut bmp), None, None) {
                Ok(()) => built = true,
                // SAY WHICH FILE, WHICH SIZE, AND WHY. A per-size failure only ever landed in
                // a total count, so "it didn't pre-build my PDFs" (issue #26.3) could not be
                // told apart from "it never tried", from "the shell refused this one format",
                // from "this size is not one the view reads". All three look identical in a
                // summary line, and the reporter and I both ended up guessing.
                //
                // Verbose-log gated, so a 40,000 file library does not write 40,000 lines
                // unless someone has turned diagnostics on to find exactly this.
                Err(e) => {
                    crate::safety::log_debug(&format!(
                        "prebuild: {abs} size {size} not built: {e}"
                    ));
                    missing.push(size);
                }
            }
        }
        // ONE retry for the sizes that did not land, after the rest of this file is done.
        //
        // The dominant failure here is a TIMEOUT, not a refusal: several worker threads drive
        // the OS rasterizer at once, so a bucket can miss its budget purely because it was
        // unlucky. That matters more since `build_order` put the LARGEST bucket first — the
        // expensive render is now the one that runs while contention is highest, and it is also
        // the one whose loss costs the most (lose it and every smaller bucket is derived from
        // whatever renders next instead). Bounded to one pass so a genuinely unsupported file
        // costs one extra cheap refusal, not an unbounded loop.
        if !missing.is_empty() {
            missing.retain(|&size| {
                let mut bmp = None;
                match cache.GetThumbnail(&item, size, WTS_EXTRACT, Some(&mut bmp), None, None) {
                    Ok(()) => {
                        built = true;
                        false // landed on the retry — no longer missing
                    }
                    Err(e) => {
                        crate::safety::log_debug(&format!(
                            "prebuild: {abs} size {size} still not built after retry: {e}"
                        ));
                        true
                    }
                }
            });
        }
        if let Some(o) = verdict(built, already, missing.is_empty()) {
            Ok(o)
        } else {
            // No size produced anything AND none was already cached. Worth a line even at
            // normal verbosity would be too much for a big run, so it stays debug-gated, but
            // it is the one that names a file the user will actually notice.
            crate::safety::log_debug(&format!(
                "prebuild: {abs} produced no thumbnail at any size"
            ));
            Ok(Outcome::Failed)
        }
    })();

    r.unwrap_or(Outcome::Failed)
}

/// Repair the one way Explorer's `"%1"` substitution can hand us a broken path: a DRIVE ROOT.
///
/// [`crate::foldermenu`] registers a static verb whose command is a plain registry string, and
/// it quotes the folder token because most folders anyone points this at contain spaces:
///
/// ```text
/// "C:\Program Files\SageThumbs2K\SageThumbs2K.exe" --prebuild "%1"
/// ```
///
/// Explorer substitutes the literal path. For an ordinary folder that yields `"E:\Photos"` and
/// everything is fine. For a **drive root** it yields `"E:\"`, and `CommandLineToArgvW` reads
/// the closing `\"` as an ESCAPED QUOTE rather than a backslash followed by the terminator. The
/// argument that reaches `main` is therefore `E:"` — a path that cannot exist — so the menu
/// entry looked completely dead on a drive root while working on every normal folder. That is
/// the first half of issue #26.
///
/// It cannot be fixed on the registry side: every token Explorer offers (`%1`, `%V`, `%L`)
/// expands to `E:\` for a drive root, and appending a further argument does not help because
/// the unterminated quote swallows the rest of the line into the same argument. Repairing it
/// HERE has the additional advantage of fixing installs that already wrote the old command
/// string, which a registry-side change could not do.
///
/// The repair is unambiguous: `"` is not a legal character in a Windows path, so an argument
/// ending in one cannot be a real path and can only have come from this mangling.
pub fn unmangle_shell_path(arg: &str) -> String {
    match arg.strip_suffix('"') {
        Some(rest) => format!(r"{rest}\"),
        None => arg.to_string(),
    }
}

/// True when this process is running elevated.
///
/// The thumbnail cache is PER USER (`%LocalAppData%\Microsoft\Windows\Explorer`). Run from an
/// admin prompt, every thumbnail lands in the administrator's cache and the user sees exactly
/// no change — the most confusing possible failure, because it reports total success.
pub fn is_elevated() -> bool {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut el = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(core::ptr::addr_of_mut!(el).cast()),
            core::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
        .is_ok();
        let _ = windows::Win32::Foundation::CloseHandle(token);
        ok && el.TokenIsElevated != 0
    }
}

/// True when the process `pid` is running elevated.
///
/// Cross-process, and callable from an ORDINARY process, which is the non-obvious part:
/// opening a higher-integrity process for MEMORY access is refused, but
/// `PROCESS_QUERY_LIMITED_INFORMATION` plus `TOKEN_QUERY` is granted for the same user, so the
/// elevation flag can be read directly instead of inferred from something else failing.
///
/// Any refusal answers "not elevated". Both callers use this to EXPLAIN a problem, so a wrong
/// "yes" would invent one; a wrong "no" just leaves things as they were.
pub fn process_is_elevated(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    if pid == 0 {
        return false;
    }
    unsafe {
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let mut token = HANDLE::default();
        let mut elevated = false;
        if OpenProcessToken(process, TOKEN_QUERY, &mut token).is_ok() {
            let mut el = TOKEN_ELEVATION::default();
            let mut len = 0u32;
            if GetTokenInformation(
                token,
                TokenElevation,
                Some(core::ptr::addr_of_mut!(el).cast()),
                core::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut len,
            )
            .is_ok()
            {
                elevated = el.TokenIsElevated != 0;
            }
            let _ = CloseHandle(token);
        }
        let _ = CloseHandle(process);
        elevated
    }
}

/// Walk `inputs` and fill the shell's thumbnail cache for everything supported inside them.
///
/// `progress` is called with (done, total) roughly as work completes, for a counter or bar.
/// `cancel`, when supplied and set, stops the run at the next file: the pool has no cancel
/// primitive, so every remaining item is visited and skipped rather than truly interrupted.
/// That is instant in practice because skipping is free, and it avoids threading a new
/// abort path through [`crate::parallel`].
///
/// The caller must still tell the user that the cache is **LRU-capped**: pre-building a whole
/// 32 TB drive evicts its own early work long before it finishes, so a run can report complete
/// success and leave the far end of the library unbuilt. Scope it to the folders that matter.
pub fn run(
    inputs: &[String],
    opts: &Options,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    progress: impl Fn(usize, usize) + Sync,
) -> Report {
    let mut rep = Report::default();
    let mut files: Vec<String> = Vec::new();
    // ONE snapshot for the whole sweep (the walk plus this per-input loop), not one ini
    // reparse per file (item 134).
    let snap = crate::settings::format_enabled_snapshot();
    for i in inputs {
        let p = Path::new(i);
        if p.is_dir() {
            walk(p, opts, 0, &mut files, &mut rep, &snap);
        } else if p.is_file() && wanted(p, &snap) {
            files.push(i.clone());
        }
    }
    files.sort();
    files.dedup();
    rep.found = files.len();
    rep.sizes = normalize_sizes(&opts.sizes);
    if files.is_empty() {
        return rep;
    }

    // Work against the normalised buckets, so two requested sizes that land in the same bucket
    // don't extract the same thumbnail twice — and in BUILD ORDER, which is what makes the run
    // fill the big buckets at all. `rep.sizes` stays ascending because that is the order the
    // summary line reads naturally in; only the workers see the reordered list.
    let opts = Options {
        recurse: opts.recurse,
        sizes: build_order(&rep.sizes),
        rebuild_all: opts.rebuild_all,
        jobs: opts.jobs,
        max_depth: opts.max_depth,
    };

    // These calls serialise inside the shell; a wide pool only adds contention and makes the
    // machine unusable while it runs. 1..=4 is the whole useful range.
    let workers = opts.jobs.clamp(1, 4);
    let (built, already, failed, partial, skipped) = (
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    );
    let done = AtomicUsize::new(0);
    let total = files.len();
    let stopping = || cancel.is_some_and(|c| c.load(Ordering::Relaxed));

    crate::parallel::map_indexed(
        &files,
        workers,
        |_, path: &String| {
            if stopping() {
                skipped.fetch_add(1, Ordering::Relaxed);
                return;
            }
            match one(path, &opts) {
                Outcome::Built => built.fetch_add(1, Ordering::Relaxed),
                Outcome::Already => already.fetch_add(1, Ordering::Relaxed),
                Outcome::Failed => failed.fetch_add(1, Ordering::Relaxed),
                Outcome::Partial => partial.fetch_add(1, Ordering::Relaxed),
            };
        },
        || {
            let d = done.fetch_add(1, Ordering::Relaxed) + 1;
            progress(d, total);
        },
    );

    rep.built = built.into_inner();
    rep.already = already.into_inner();
    rep.failed = failed.into_inner();
    rep.partial = partial.into_inner();
    rep.cancelled = skipped.into_inner() > 0;
    rep
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file that landed at SOME sizes and missed others must not report as done. This is the
    /// whole point of the Partial verdict: the old code collapsed it to Built, so a run claimed
    /// 100% while Explorer still had to extract the missing bucket on first browse (issue #26).
    #[test]
    fn a_missed_size_is_never_reported_as_built() {
        assert!(matches!(
            verdict(true, false, false),
            Some(Outcome::Partial)
        ));
        assert!(matches!(
            verdict(false, true, false),
            Some(Outcome::Partial)
        ));
        assert!(matches!(verdict(true, true, false), Some(Outcome::Partial)));
    }

    /// The clean outcomes still say what they always said, so the summary a user reads for a
    /// healthy run is unchanged.
    #[test]
    fn a_complete_file_reports_built_or_already() {
        assert!(matches!(verdict(true, false, true), Some(Outcome::Built)));
        assert!(matches!(verdict(false, true, true), Some(Outcome::Already)));
        // Built wins over already: some size needed real work, which is what happened.
        assert!(matches!(verdict(true, true, true), Some(Outcome::Built)));
    }

    /// Nothing anywhere is the caller's Failed path, and it must stay distinguishable from
    /// Partial — "we got some of it" and "we got none of it" are different user-facing answers.
    #[test]
    fn nothing_anywhere_is_not_a_partial() {
        assert!(verdict(false, false, false).is_none());
        assert!(verdict(false, false, true).is_none());
    }

    /// PROVE THE PREMISE, don't assume it. Everything above rests on the claim that Windows
    /// really does turn our registered command into the argument `E:"` for a drive root. That
    /// is a claim about `CommandLineToArgvW`'s escaping rules, so ask `CommandLineToArgvW`.
    ///
    /// Without this, the fix could be repairing a mangling that never happens (and quietly
    /// corrupting nothing, but also fixing nothing) and every other test here would still pass,
    /// because they all take the mangled string as a given.
    #[test]
    fn windows_really_does_mangle_a_quoted_drive_root() {
        use windows::core::PCWSTR;
        use windows::Win32::UI::Shell::CommandLineToArgvW;

        // Exactly what `foldermenu::apply` writes, with `%1` substituted by the shell.
        let parse = |line: &str| -> Vec<String> {
            let wide: Vec<u16> = line.encode_utf16().chain(std::iter::once(0)).collect();
            let mut argc = 0i32;
            unsafe {
                let argv = CommandLineToArgvW(PCWSTR(wide.as_ptr()), &mut argc);
                assert!(!argv.is_null(), "CommandLineToArgvW failed on {line}");
                let out = (0..argc as usize)
                    .map(|i| (*argv.add(i)).to_string().expect("argv is valid UTF-16"))
                    .collect();
                let _ = windows::Win32::Foundation::LocalFree(Some(
                    windows::Win32::Foundation::HLOCAL(argv.cast()),
                ));
                out
            }
        };

        let exe = r"C:\Program Files\SageThumbs2K\SageThumbs2K.exe";

        // An ordinary folder: three clean arguments, the path intact. This is why the entry
        // always worked here.
        let ok = parse(&format!("\"{exe}\" --prebuild \"E:\\Photos\""));
        assert_eq!(ok.len(), 3, "ordinary folder should parse cleanly: {ok:?}");
        assert_eq!(ok[2], r"E:\Photos");

        // A drive root: the trailing `\"` is read as an escaped quote, so the path is
        // destroyed. THIS is issue #26.1, demonstrated rather than asserted.
        let broken = parse(&format!("\"{exe}\" --prebuild \"E:\\\""));
        assert_eq!(
            broken[2], "E:\"",
            "the premise of unmangle_shell_path no longer holds: Windows parsed the drive root \
             as {:?} rather than the expected mangled `E:\"`",
            broken[2]
        );
        assert!(
            !std::path::Path::new(&broken[2]).exists(),
            "the mangled argument must be a path that cannot exist, which is why the verb \
             silently did nothing"
        );

        // And the repair turns that back into the drive root the user right-clicked.
        assert_eq!(unmangle_shell_path(&broken[2]), r"E:\");
    }

    /// A drive root reaches us as `E:"`, because Explorer substituted `E:\` into a quoted
    /// token and `CommandLineToArgvW` then ate the backslash as a quote escape. This is the
    /// whole of issue #26.1: the entry worked on every folder and did nothing on a drive.
    #[test]
    fn a_mangled_drive_root_is_repaired() {
        assert_eq!(unmangle_shell_path("E:\""), r"E:\");
        assert_eq!(unmangle_shell_path("C:\""), r"C:\");
        // A UNC share root mangles identically and repairs identically.
        assert_eq!(
            unmangle_shell_path("\\\\server\\share\""),
            r"\\server\share\"
        );
    }

    /// Ordinary folders are NOT touched. `%1` only produces a trailing backslash for a root,
    /// so every normal path must come through byte-for-byte — including one ending in a
    /// quote-free backslash, and one containing the spaces the quoting exists for.
    #[test]
    fn ordinary_folder_paths_pass_through_untouched() {
        for p in [
            r"E:\Photos",
            r"C:\Users\sam\My Pictures",
            r"D:\a b\c d\e",
            r"E:\Photos\", // already correct: no quote, so nothing to repair
            "",
        ] {
            assert_eq!(unmangle_shell_path(p), p, "must not rewrite {p}");
        }
    }

    /// `OFFLINE_ATTRS` must cover all three HSM/cloud-placeholder flags `doctor.rs` enumerates
    /// (OFFLINE, RECALL_ON_OPEN, RECALL_ON_DATA_ACCESS) — a file carrying only RECALL_ON_OPEN
    /// used to sail past this mask and get hydrated/downloaded by WTS_EXTRACT.
    #[test]
    fn offline_attrs_mask_covers_all_three_recall_flags() {
        const OFFLINE: u32 = 0x0000_1000;
        const RECALL_ON_OPEN: u32 = 0x0004_0000;
        const RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
        for (name, flag) in [
            ("OFFLINE", OFFLINE),
            ("RECALL_ON_OPEN", RECALL_ON_OPEN),
            ("RECALL_ON_DATA_ACCESS", RECALL_ON_DATA_ACCESS),
        ] {
            assert!(
                OFFLINE_ATTRS & flag != 0,
                "OFFLINE_ATTRS must include {name} ({flag:#010x})"
            );
        }
    }

    /// The walk must pick up supported files, honour `recurse`, and never wander into a
    /// junction — the loop guard that keeps "pre-build my D: drive" from never terminating.
    #[test]
    fn walk_is_shallow_by_default_and_deep_on_request() {
        let root = std::env::temp_dir().join(format!("st2k-prebuild-{}", std::process::id()));
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).expect("scratch tree");
        std::fs::write(root.join("a.png"), b"x").expect("a");
        std::fs::write(sub.join("b.png"), b"x").expect("b");
        std::fs::write(root.join("notes.txt"), b"x").expect("txt");

        let mut rep = Report::default();
        let snap = crate::settings::format_enabled_snapshot();
        let mut shallow = Vec::new();
        walk(&root, &Options::default(), 0, &mut shallow, &mut rep, &snap);
        assert_eq!(shallow.len(), 1, "non-recursive must stop at the top level");
        assert!(
            shallow[0].ends_with("a.png"),
            "and must skip the unsupported .txt"
        );

        let mut deep = Vec::new();
        let opts = Options {
            recurse: true,
            ..Default::default()
        };
        walk(&root, &opts, 0, &mut deep, &mut rep, &snap);
        assert_eq!(deep.len(), 2, "recursive must reach the subfolder");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The depth cap is the only thing standing between a junction cycle and an endless walk.
    #[test]
    fn walk_stops_at_the_depth_cap() {
        let root = std::env::temp_dir().join(format!("st2k-depth-{}", std::process::id()));
        let deep = root.join("a").join("b").join("c");
        std::fs::create_dir_all(&deep).expect("tree");
        std::fs::write(deep.join("x.png"), b"x").expect("x");

        let mut rep = Report::default();
        let snap = crate::settings::format_enabled_snapshot();
        let mut out = Vec::new();
        let opts = Options {
            recurse: true,
            max_depth: 1,
            ..Default::default()
        };
        walk(&root, &opts, 0, &mut out, &mut rep, &snap);
        assert!(out.is_empty(), "a file below the cap must not be collected");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A relative path has to become the absolute, non-extended form the shell parses, or
    /// every item fails with FILE_NOT_FOUND and the run reports a 100% failure rate.
    #[test]
    fn parsing_path_is_absolute_and_carries_no_extended_prefix() {
        let f = std::env::temp_dir().join(format!("st2k-pp-{}.png", std::process::id()));
        std::fs::write(&f, b"x").expect("write");
        let got = parsing_path(&f.to_string_lossy());
        assert!(
            !got.starts_with(r"\\?\"),
            "extended prefix must be stripped"
        );
        assert!(Path::new(&got).is_absolute(), "must be absolute");
        let _ = std::fs::remove_file(&f);
    }

    /// Requested edges must land on real cache buckets, and two requests that resolve to the
    /// same bucket must collapse — otherwise the run extracts the same thumbnail twice and
    /// the report claims work that never happened.
    #[test]
    fn sizes_snap_to_cache_buckets_and_dedupe() {
        assert_eq!(
            normalize_sizes(&[256]),
            vec![256],
            "an exact bucket is kept"
        );
        assert_eq!(
            normalize_sizes(&[200]),
            vec![256],
            "a request rounds UP to the bucket that will hold it"
        );
        assert_eq!(
            normalize_sizes(&[100, 200, 250]),
            vec![256],
            "three requests inside one bucket collapse to a single extraction"
        );
        assert_eq!(
            normalize_sizes(&[768, 96, 256]),
            vec![96, 256, 768],
            "order is normalised so the report reads predictably"
        );
        assert_eq!(
            normalize_sizes(&[99_999]),
            vec![2560],
            "anything past the top bucket clamps to it rather than being dropped"
        );
        // The three buckets the corrected list added or fixed. 1024 is NOT a Windows 10/11
        // bucket (it was Windows 7's), so a request in that range belongs in 1280 — getting
        // this wrong made the run report a size Windows does not keep.
        assert_eq!(normalize_sizes(&[1024]), vec![1280], "1024 is not a bucket");
        assert_eq!(normalize_sizes(&[1281]), vec![1920]);
        assert_eq!(
            normalize_sizes(&[2000]),
            vec![2560],
            "the raised thumbnail ceiling must have a bucket to land in"
        );
        assert_eq!(normalize_sizes(&[40]), vec![48], "small buckets exist too");
        assert_eq!(
            normalize_sizes(&[0]),
            vec![256],
            "a zero is not a size; fall back to the default rather than asking for nothing"
        );
        assert_eq!(normalize_sizes(&[]), vec![256], "empty falls back too");
        assert_eq!(
            normalize_sizes(&DEFAULT_SIZES),
            DEFAULT_SIZES.to_vec(),
            "the shipped default must already be canonical, or every run pays to normalise it"
        );
    }

    /// THE REGRESSION THAT SHIPPED FOR THE WHOLE LIFE OF THIS FEATURE. One extraction fills
    /// every smaller bucket, so whichever size is attempted FIRST is the only one that gets a
    /// real render. Ascending order therefore built 96 and derived the rest, and Explorer threw
    /// the derived entries away and re-extracted on first browse — after a run that reported
    /// complete success. Largest-first is the fix; see `build_order` for the measurements.
    #[test]
    fn the_largest_bucket_is_always_extracted_first() {
        assert_eq!(build_order(&[96, 256, 768]), vec![768, 256, 96]);
        // The shipped default is the case that was broken, so pin it specifically rather than
        // trusting the general property above.
        let shipped = build_order(&normalize_sizes(&DEFAULT_SIZES));
        assert_eq!(
            shipped.first().copied(),
            Some(768),
            "the default run must extract at its LARGEST bucket first, or every bigger view \
             re-extracts on first browse; got {shipped:?}"
        );
        // `normalize_sizes` sorts ascending, so a caller that forgets to reorder gets exactly
        // the old bug back. Prove the two disagree, or this test proves nothing.
        assert_ne!(
            normalize_sizes(&DEFAULT_SIZES),
            shipped,
            "build_order must actually reorder; if these ever match, the guard is vacuous"
        );
    }

    /// Reordering must not lose, duplicate or invent a bucket — the run would then report
    /// sizes it never attempted.
    #[test]
    fn build_order_is_a_permutation_of_its_input() {
        for req in [
            vec![96u32, 256, 768],
            vec![256],
            vec![16, 32, 48, 96, 256, 768, 1280, 1920, 2560],
            vec![],
        ] {
            let mut got = build_order(&req);
            let mut want = req.clone();
            got.sort_unstable();
            want.sort_unstable();
            assert_eq!(got, want, "build_order changed the SET for {req:?}");
            // And every adjacent pair really is descending.
            let ordered = build_order(&req);
            assert!(
                ordered.windows(2).all(|w| w[0] > w[1]),
                "not descending: {ordered:?}"
            );
        }
    }

    /// A path that does not exist must come back unchanged rather than panicking — the walk
    /// races with a user deleting files underneath it.
    #[test]
    fn parsing_path_passes_through_a_missing_file() {
        assert_eq!(
            parsing_path("Z:\\nope\\missing.png"),
            "Z:\\nope\\missing.png"
        );
    }
}
