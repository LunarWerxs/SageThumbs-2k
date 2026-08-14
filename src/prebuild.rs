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
    /// Edges in pixels, built per file. The shell caches SEPARATELY PER SIZE BUCKET, so this
    /// is a list and not a number: building only 256 leaves Explorer's Medium-icons view
    /// (96) empty, and the user still watches tiles build in exactly the situation this
    /// feature exists to prevent. See [`DEFAULT_SIZES`].
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
pub const DEFAULT_SIZES: [u32; 3] = [96, 256, 768];

/// Windows' actual cache buckets. A request lands in the smallest bucket that fits it, so
/// asking for 200 fills the 256 one; normalising up front keeps the report honest about what
/// was really built and stops two requested sizes silently doing the same work twice.
const BUCKETS: [u32; 6] = [32, 96, 256, 768, 1024, 1920];
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

/// `FILE_ATTRIBUTE_OFFLINE` | `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`. A OneDrive/Dropbox
/// placeholder carries one of these; extracting its thumbnail DOWNLOADS the whole file, so a
/// "pre-build my library" run over a cloud folder would silently pull the entire library onto
/// the disk it was meant to stay off. Skipped, and counted so the report says it happened.
const OFFLINE_ATTRS: u32 = 0x0000_1000 | 0x0040_0000;
/// `FILE_ATTRIBUTE_REPARSE_POINT` — junctions and symlinks, which the walk does not follow.
const REPARSE: u32 = 0x0000_0400;

fn attrs(p: &Path) -> u32 {
    use std::os::windows::fs::MetadataExt;
    std::fs::symlink_metadata(p)
        .map(|m| m.file_attributes())
        .unwrap_or(0)
}

/// Is this extension one we hook AND the user still has enabled? A format they turned off has
/// no SageThumbs thumbnail to build, and asking the shell for one just burns a round trip.
fn wanted(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| formats::is_known(e) && crate::settings::format_enabled(&e.to_lowercase()))
}

/// Collect the supported files under `root`. Non-recursive by default; never follows a
/// reparse point, and stops at `max_depth` so a junction cycle cannot spin forever.
fn walk(root: &Path, opts: &Options, depth: u32, out: &mut Vec<String>, rep: &mut Report) {
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
                walk(&p, opts, depth + 1, out, rep);
            }
        } else if p.is_file() && wanted(&p) {
            if a & OFFLINE_ATTRS != 0 {
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
fn parsing_path(path: &str) -> String {
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
            if cache
                .GetThumbnail(&item, size, WTS_EXTRACT, Some(&mut bmp), None, None)
                .is_ok()
            {
                built = true;
            }
        }
        if built {
            Ok(Outcome::Built)
        } else if already {
            Ok(Outcome::Already)
        } else {
            Ok(Outcome::Failed)
        }
    })();

    r.unwrap_or(Outcome::Failed)
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
    for i in inputs {
        let p = Path::new(i);
        if p.is_dir() {
            walk(p, opts, 0, &mut files, &mut rep);
        } else if p.is_file() && wanted(p) {
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
    // don't extract the same thumbnail twice.
    let opts = Options {
        recurse: opts.recurse,
        sizes: rep.sizes.clone(),
        rebuild_all: opts.rebuild_all,
        jobs: opts.jobs,
        max_depth: opts.max_depth,
    };

    // These calls serialise inside the shell; a wide pool only adds contention and makes the
    // machine unusable while it runs. 1..=4 is the whole useful range.
    let workers = opts.jobs.clamp(1, 4);
    let (built, already, failed, skipped) = (
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
    rep.cancelled = skipped.into_inner() > 0;
    rep
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut shallow = Vec::new();
        walk(&root, &Options::default(), 0, &mut shallow, &mut rep);
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
        walk(&root, &opts, 0, &mut deep, &mut rep);
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
        let mut out = Vec::new();
        let opts = Options {
            recurse: true,
            max_depth: 1,
            ..Default::default()
        };
        walk(&root, &opts, 0, &mut out, &mut rep);
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
            vec![1920],
            "anything past the top bucket clamps to it rather than being dropped"
        );
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
