//! `st2k` — the SageThumbs 2K command-line tool. A thin arg parser over
//! `sagethumbs2k_core::cli`, exposing the bundled engine (decode all registered formats, convert,
//! rotate, strip, OCR, PDF, thumbnail) to scripts and AI agents. Console
//! subsystem (no `windows_subsystem = "windows"`), so stdout/stderr work.

use sagethumbs2k_core::cli;

// The hidden video-decode child verbs (`flv-frame`: VP6 via nihav + Sorenson via h263-rs;
// `vp9-frame`: VP9 Profile 2/3 via vp9dec). Behind EXE-only features so the panicky /
// unsafe-heavy decoder crates exist ONLY in this console binary — see src/bin/vdec/mod.rs
// for the whole containment argument.
#[cfg(any(feature = "flash-video", feature = "vp9-video"))]
mod vdec;

const USAGE: &str = "\
st2k — SageThumbs 2K command line

USAGE:
  st2k thumbnail <in> <out.png> [--size N]      render any format to an image (N px, default 256)
  st2k batch <thumbnail|convert|info> <in|dir...> [--recurse] [--out DIR] [--size N] [--to EXT] [--quality N] [--resize WxH|N%] [--json]
                                                bulk-process many files/folders in parallel (one process);
                                                each input dir is scanned one level deep unless --recurse;
                                                a file that fails is listed with its cause, one 'failed' line
                                                each; --json returns the whole per-file report instead;
                                                'info' returns a JSON array (dimensions/EXIF/audio tags)
  st2k batch <thumbnail|convert> --retry-from report.json [same options]
                                                re-run only the files a saved --json report lists as failed,
                                                in place of <in|dir...>; the retry reports the same way
  st2k convert   <in> <out> [--quality N] [--webp-quality N] [--resize WxH|N%]   (--webp-quality → lossy WebP)
  st2k prebuild  <dir|file...> [--recurse] [--size N,N] [--rebuild-all] [--jobs N]
                                                fill Explorer's thumbnail cache ahead of browsing
                                                (--size defaults to 96,256,768 — one per Explorer view)
  st2k rotate    <in> --by right|left|180|fliph|flipv
  st2k compress  <in> --max-size 1MB|500KB|N    shrink to a target file size (JPEG, quality+scale search);
                                                fails and writes nothing if the target can't be met
  st2k strip     <in>                           strip EXIF/GPS metadata (JPEG/PNG/WebP/SVG(Z)/HEIC/HEIF/AVIF, lossless)
  st2k clip-pixels <in>                         decode -> stdout: `w h` (two little-endian u32) then
                                                top-down RGBA8 bytes, binary (no other output); powers
                                                the routed Copy-to-clipboard context menu verb
  st2k wallpaper-prepare <in> <out-dir>         decode + resize-to-screen -> a PNG written into <out-dir>;
                                                powers the routed Set-as-wallpaper context menu verb
  st2k folder-icon <in>                         set <in> as its containing folder's icon (hidden .ico +
                                                desktop.ini); powers the routed context menu verb
  st2k ocr       <in>                           recognize text → stdout
  st2k pdf       <out.pdf> <in> [in...] [--strict] [--json]   combine images into one PDF
  st2k cbz       <out.cbz> <in> [in...] [--strict] [--json]   combine images into one CBZ (comic-book zip)
                                                an input that can't be read or decoded is left out and
                                                listed (one 'omitted' line each); --strict fails instead
                                                of writing a partial file; --json returns the same as JSON
  st2k info      <in> [--json]                  dimensions + camera/date/GPS/bit depth/DPI, or audio tags
  st2k formats   [--json]                       list supported input formats
  st2k doctor    [file] [--bundle out.zip]       self-check: why are thumbnails not showing? (add a file to probe it;
                                                --bundle zips the report + log tail + formats --json for a bug report)
  st2k register  [--off|--status]               portable build: turn Explorer thumbnails on for this user
  st2k upload    <file> [--copy]                 upload a file to a keyless host, print the URL (--copy
                                                also puts it on the clipboard); needs SageThumbs2K.exe
                                                installed alongside st2k.exe (spawns it — no network
                                                code lives in the CLI itself)
  st2k upload-hosts [--open]                     show (or open) the editable upload-hosts config file
  st2k devmode   [on|off|status]                toggle the developer test-box flag
  st2k --mcp                                     run as an MCP server (stdio JSON-RPC, for AI agents)
  st2k --version | -V                            print the version and exit

Single-file commands take exactly the arguments shown: an extra file name is an error, and
nothing is written (use `batch` for many files). An <out> that is one of the inputs is refused.
";

/// Flags that take a following value; used to keep that value out of `pos` (the
/// positional-argument list) instead of it silently becoming an extra `<in>`/`<out>`.
/// Global across verbs on purpose: no name here means something different for two
/// different verbs, so one table is the single source of truth rather than a per-verb
/// copy that could disagree with itself.
const VALUE_FLAGS: &[&str] = &[
    "--size",
    "--quality",
    "--webp-quality",
    "--resize",
    "--out",
    "--to",
    "--by",
    "--max-size",
    "--jobs",
    // bench-decode's repeat count. It MUST be here or its value is parsed as an input path:
    // `--runs 3` left a bare "3" in the positionals, which then reported as `3<TAB>FAIL`.
    "--runs",
    // `doctor --bundle out.zip`'s destination path — must be excluded the same way `--out`
    // is, or "out.zip" would be read as a second doctor probe target.
    "--bundle",
    // `batch --retry-from report.json`: the previous run's `--json` report, whose failed
    // inputs become this run's input list (2026-09-05 audit, E01).
    "--retry-from",
];

/// Flags that take NO value — excluded from `pos` on their own, without consuming the
/// following token. `-r` is the short alias `prebuild --recurse` also accepts (and,
/// before this fix, was the one flag that could slip into `pos` as a bogus input path
/// since it doesn't start with `--`).
const BOOL_FLAGS: &[&str] = &[
    "--recurse",
    "-r",
    "--rebuild-all",
    "--json",
    "--status",
    "--off",
    "--open",
    // pdf/cbz: fail instead of writing a partial file (2026-09-05 audit, F31).
    "--strict",
    // upload: also put the resulting URL on the clipboard (printing it is the default).
    "--copy",
];

/// How many positional (file) arguments each verb accepts, or `None` for the verbs that
/// take a list. 2026-09-05 audit, F33: `strip first.svg second.svg` exited 0, stripped the
/// first, left the second untouched and said nothing, because `need(pos, 0)` read what it
/// wanted and ignored the rest. The table is checked BEFORE any verb runs, so a stray
/// argument can never cost a write.
fn max_positionals(verb: &str) -> Option<usize> {
    match verb {
        "thumbnail" | "thumb" | "convert" | "wallpaper-prepare" => Some(2),
        "rotate" | "compress" | "strip" | "ocr" | "info" | "doctor" | "diag" | "register"
        | "unregister" | "upload" | "upload-hosts" | "upload-host" | "devmode" | "clip-pixels"
        | "folder-icon" => Some(1),
        "formats" => Some(0),
        // batch, prebuild, pdf, cbz and bench-decode take as many inputs as given.
        _ => None,
    }
}

/// Reject the positionals a verb has no meaning for, naming the first one it did not expect.
fn check_arity(verb: &str, pos: &[&String]) -> Result<(), String> {
    match max_positionals(verb) {
        Some(max) if pos.len() > max => {
            let plural = if max == 1 { "" } else { "s" };
            Err(format!(
                "{verb} takes at most {max} file argument{plural} but got {}: unexpected \"{}\". \
                 Nothing was changed. (Use `st2k batch` to process many files.)",
                pos.len(),
                pos[max]
            ))
        }
        _ => Ok(()),
    }
}

/// Split `rest` into positional arguments, walking by index so a known value-flag's
/// value is consumed WITH it rather than falling through to `pos` as if it were an
/// `<in>`/`<out>` path (`st2k thumbnail --size 128 in.png out.png` used to read `pos[0]`
/// as `"128"`, off by one for everything after). An unrecognized `--flag` is still
/// excluded from `pos` on its own (matching the previous behaviour for flags no verb
/// here defines) but does not eat a value, since we have no table entry saying it should.
fn positionals(rest: &[String]) -> Vec<&String> {
    let mut pos = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        let a = rest[i].as_str();
        if VALUE_FLAGS.contains(&a) {
            i += 2; // the flag AND its value, whatever the value looks like
            continue;
        }
        if BOOL_FLAGS.contains(&a) || a.starts_with("--") {
            i += 1;
            continue;
        }
        pos.push(&rest[i]);
        i += 1;
    }
    pos
}

/// A flag's value token, or `None` if the flag is absent OR the next token is itself
/// another `--flag` (an unfinished `--size --recurse` must not silently treat
/// `--recurse` as the size).
fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .filter(|v| !v.starts_with("--"))
        .cloned()
}

fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

/// A numeric flag with a default for when it's simply absent — but an ERROR, not a
/// silent fallback to that default, when it's present and doesn't parse. A typo'd
/// `--size 12x` used to render at the wrong size with nothing telling the caller why.
fn flag_num<T: std::str::FromStr>(args: &[String], name: &str, default: T) -> Result<T, String> {
    match flag(args, name) {
        None => Ok(default),
        Some(v) => v
            .parse()
            .map_err(|_| format!("{name}: not a number: \"{v}\"")),
    }
}

/// Same as [`flag_num`] but for a flag with no default — `None` only when the flag is
/// genuinely absent, `Err` when it's present with an unparseable value (`--webp-quality
/// abc` must not silently behave as "lossy WebP not requested").
fn flag_num_opt<T: std::str::FromStr>(args: &[String], name: &str) -> Result<Option<T>, String> {
    match flag(args, name) {
        None => Ok(None),
        Some(v) => v
            .parse()
            .map(Some)
            .map_err(|_| format!("{name}: not a number: \"{v}\"")),
    }
}

fn run_thumbnail(pos: &[&String], rest: &[String]) -> Result<String, String> {
    let (i, o) = (need(pos, 0)?, need(pos, 1)?);
    let size = flag_num(rest, "--size", 256)?;
    cli::thumbnail(i, o, size)
}

fn run_convert(pos: &[&String], rest: &[String]) -> Result<String, String> {
    let (i, o) = (need(pos, 0)?, need(pos, 1)?);
    let q = flag_num(rest, "--quality", 90u8)?;
    let wq = flag_num_opt::<u8>(rest, "--webp-quality")?;
    let resize = cli::parse_resize(flag(rest, "--resize").as_deref())?;
    cli::convert(i, o, q, wq, resize)
}

/// The input list `batch --retry-from <report.json>` runs: the failed entries of a report
/// `batch --json` wrote earlier, verbatim (2026-09-05 audit, E01). Every other option still
/// comes from this command line, so a retry to a different `--out` folder or format is one
/// flag away, and the retry writes a normal report, so a second retry can chain off it.
///
/// Refused rather than merged when inputs are ALSO given on the command line: a retry is
/// exactly the failures, and a mixed list would make the report's counts mean two things.
/// A report with nothing failed is refused too, the same way an empty input list is, since
/// a run over nothing has no report to give.
fn retry_inputs(rest: &[String], given: &[String]) -> Result<Vec<String>, String> {
    let path = flag(rest, "--retry-from").ok_or("--retry-from needs the report's path")?;
    if let Some(first) = given.first() {
        return Err(format!(
            "--retry-from takes its inputs from the report; do not also give them on the \
             command line: unexpected \"{first}\""
        ));
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("--retry-from: cannot read {path}: {e}"))?;
    let inputs = sagethumbs2k_core::BatchReport::failed_inputs_from_json(&text).map_err(|why| {
        format!("--retry-from: {path} is {why}; it has to be a report `batch --json` wrote")
    })?;
    if inputs.is_empty() {
        return Err(format!(
            "--retry-from: {path} lists no failed files, nothing to retry"
        ));
    }
    Ok(inputs)
}

/// `batch <op> <inputs...> [--recurse] [--out DIR] [--size N] [--to EXT] [--quality N] [--resize ...] [--json] [--retry-from report.json]`
fn run_batch(pos: &[&String], rest: &[String]) -> Result<String, String> {
    let op = need(pos, 0)?;
    let given: Vec<String> = pos.iter().skip(1).map(|s| s.to_string()).collect();
    let inputs = if has_flag(rest, "--retry-from") {
        retry_inputs(rest, &given)?
    } else {
        given
    };
    if inputs.is_empty() {
        return Err("batch needs at least one input file or directory".to_string());
    }
    let size = flag_num(rest, "--size", 256)?;
    let q = flag_num(rest, "--quality", 90u8)?;
    let resize = cli::parse_resize(flag(rest, "--resize").as_deref())?;
    cli::batch(
        op,
        &inputs,
        has_flag(rest, "--recurse") || has_flag(rest, "-r"),
        flag(rest, "--out").as_deref(),
        size,
        flag(rest, "--to").as_deref(),
        q,
        resize,
        has_flag(rest, "--json"),
    )
}

/// `--size` takes a comma-separated LIST because the shell caches per size bucket; a
/// single number leaves every other Explorer view still building its tiles by hand.
///
/// The list is parsed by `prebuild::parse_size_list_str`, which is also what the MCP
/// `prebuild` tool's `sizes` array goes through (2026-09-05 audit, F12/F20): ONE policy,
/// one implementation, so the two front ends cannot disagree about what a size list means.
/// That module states the policy in full. The short version: every element is parsed, the
/// first bad one fails the whole call before any cache work starts, and only an ABSENT
/// `--size` means the defaults. Before this, `--size 96,typo,768` silently ran as
/// `96,768` and reported success.
fn prebuild_sizes(rest: &[String]) -> Result<Vec<u32>, String> {
    let Some(s) = flag(rest, "--size") else {
        return Ok(sagethumbs2k_core::prebuild::DEFAULT_SIZES.to_vec());
    };
    sagethumbs2k_core::prebuild::parse_size_list_str("--size", &s)
}

/// `prebuild <paths...> [--recurse] [--size N[,N…]] [--rebuild-all] [--jobs N]`
fn run_prebuild(pos: &[&String], rest: &[String]) -> Result<String, String> {
    let inputs: Vec<String> = pos.iter().map(|s| s.to_string()).collect();
    if inputs.is_empty() {
        return Err("prebuild needs at least one folder or file".to_string());
    }
    let sizes = prebuild_sizes(rest)?;
    cli::prebuild(
        &inputs,
        has_flag(rest, "--recurse") || has_flag(rest, "-r"),
        sizes,
        has_flag(rest, "--rebuild-all"),
        flag_num(rest, "--jobs", 3)?,
    )
}

fn run_rotate(pos: &[&String], rest: &[String]) -> Result<String, String> {
    let i = need(pos, 0)?;
    let by = flag(rest, "--by").ok_or("rotate needs --by right|left|180|fliph|flipv")?;
    cli::rotate(i, &by)
}

fn run_compress(pos: &[&String], rest: &[String]) -> Result<String, String> {
    let i = need(pos, 0)?;
    let max =
        flag(rest, "--max-size").ok_or("compress needs --max-size (e.g. 1MB, 500KB, 800000)")?;
    cli::compress(i, cli::parse_size(&max)?)
}

/// `wallpaper-prepare <in> <out-dir>` — the decode/resize-to-screen half of
/// Set-as-wallpaper, routed out of the shell host. See `cli::wallpaper_prepare`.
fn run_wallpaper_prepare(pos: &[&String], _rest: &[String]) -> Result<String, String> {
    let (i, out_dir) = (need(pos, 0)?, need(pos, 1)?);
    cli::wallpaper_prepare(i, out_dir)
}

/// `st2k clip-pixels <in>` — binary on success (a `w h` little-endian-u32 header then
/// top-down RGBA8), so it can't go through `run`'s `Result<String, String>` →
/// `println!` machinery like every other verb; `main` calls this directly instead of
/// dispatching through `run`, the same way the hidden `flv-frame`/`vp9-frame` verbs
/// do for their own binary stdout. Writes nothing to stderr on success.
fn run_clip_pixels(rest: &[String]) -> i32 {
    let pos = positionals(rest);
    if let Err(e) = check_arity("clip-pixels", &pos) {
        eprintln!("st2k: {e}");
        return 1;
    }
    let path = match need(&pos, 0) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("st2k: {e}");
            return 1;
        }
    };
    match cli::clip_pixels(path) {
        Ok(bytes) => {
            use std::io::Write;
            match std::io::stdout().write_all(&bytes) {
                Ok(()) => 0,
                Err(_) => 1,
            }
        }
        Err(e) => {
            eprintln!("st2k: {e}");
            1
        }
    }
}

// Dev/measurement verb (undocumented in --help, like the app EXE's --bench-* modes): time
// the decode of many files inside ONE process, so the numbers carry no per-file
// process-start noise. Used by scripts\check-decode-speed.ps1.
fn run_bench_decode(pos: &[&String], rest: &[String]) -> Result<String, String> {
    let inputs: Vec<String> = pos.iter().map(|s| s.to_string()).collect();
    if inputs.is_empty() {
        return Err("bench-decode needs at least one input file".to_string());
    }
    let size = flag_num(rest, "--size", 256u32)?;
    let runs = flag_num(rest, "--runs", 3u32)?;
    cli::bench_decode(&inputs, size, runs)
}

/// One verb, because "register --off" and "unregister" are the same action and having
/// both spellings fail differently would be its own bug report.
fn run_register(verb: &str, pos: &[&String], rest: &[String]) -> Result<String, String> {
    let off = verb == "unregister"
        || has_flag(rest, "--off")
        || pos.first().is_some_and(|p| p.as_str() == "off");
    let status = has_flag(rest, "--status") || pos.first().is_some_and(|p| p.as_str() == "status");
    cli::register_portable(off, status)
}

/// `pdf`/`cbz` share one argument shape: `<out> <in...> [--strict] [--json]`.
fn combine_opts(rest: &[String]) -> cli::CombineOpts {
    cli::CombineOpts {
        strict: has_flag(rest, "--strict"),
        json: has_flag(rest, "--json"),
    }
}

fn run(args: &[String]) -> Result<String, String> {
    let verb = args.first().map(|s| s.as_str()).unwrap_or("");
    let rest = &args[args.len().min(1)..];
    let pos = positionals(rest);
    check_arity(verb, &pos)?;

    match verb {
        "thumbnail" | "thumb" => run_thumbnail(&pos, rest),
        "convert" => run_convert(&pos, rest),
        "batch" => run_batch(&pos, rest),
        "prebuild" => run_prebuild(&pos, rest),
        "rotate" => run_rotate(&pos, rest),
        "compress" => run_compress(&pos, rest),
        "strip" => cli::strip_meta(need(&pos, 0)?),
        "wallpaper-prepare" => run_wallpaper_prepare(&pos, rest),
        "folder-icon" => cli::folder_icon(need(&pos, 0)?),
        "ocr" => cli::ocr(need(&pos, 0)?),
        "pdf" => {
            let out = need(&pos, 0)?;
            let inputs: Vec<String> = pos.iter().skip(1).map(|s| s.to_string()).collect();
            cli::pdf(out, &inputs, combine_opts(rest))
        }
        "cbz" => {
            let out = need(&pos, 0)?;
            let inputs: Vec<String> = pos.iter().skip(1).map(|s| s.to_string()).collect();
            cli::cbz(out, &inputs, combine_opts(rest))
        }
        "info" => cli::info(need(&pos, 0)?, has_flag(rest, "--json")),
        "bench-decode" => run_bench_decode(&pos, rest),
        "formats" => Ok(cli::list_formats(has_flag(rest, "--json"))),
        // Read-only; never fails, so it always prints a report rather than an error —
        // a user running this already has something broken. An optional file path adds a
        // per-file probe ("st2k doctor C:\path\to\that.xcf") that actually tries to decode
        // THAT file — the check that explains "registered fine but this one file is blank".
        // `--bundle <out.zip>` writes the report + the log's tail + `formats --json` into
        // one attachment instead of printing the report to stdout.
        "doctor" | "diag" => match flag(rest, "--bundle") {
            Some(out) => sagethumbs2k_core::doctor::bundle(
                std::path::Path::new(&out),
                pos.first().map(|s| s.as_str()),
            )
            .map(|_| format!("Diagnostics bundle written to {out}")),
            None => Ok(sagethumbs2k_core::doctor::report(
                pos.first().map(|s| s.as_str()),
            )),
        },
        "register" | "unregister" => run_register(verb, &pos, rest),
        "upload" => cli::upload(need(&pos, 0)?, has_flag(rest, "--copy")),
        "upload-hosts" | "upload-host" => {
            let open = has_flag(rest, "--open") || pos.first().map(|s| s.as_str()) == Some("open");
            cli::upload_hosts(open)
        }
        "devmode" => cli::devmode(pos.first().map(|s| s.as_str()).unwrap_or("status")),
        "" | "-h" | "--help" | "help" => Ok(USAGE.to_string()),
        other => Err(format!("unknown command '{other}'\n\n{USAGE}")),
    }
}

fn need<'a>(pos: &'a [&'a String], i: usize) -> Result<&'a str, String> {
    pos.get(i)
        .map(|s| s.as_str())
        .ok_or_else(|| format!("missing argument #{}", i + 1))
}

fn main() {
    // Capture panics to the diagnostics log before the process aborts (panic=abort).
    sagethumbs2k_core::safety::install_panic_hook("st2k");
    // WIC / WinRT decoders (HEIC, PDF, RAW via the OS) need COM.
    unsafe {
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Version (`st2k --version` / `-V`): print and exit 0, like every CLI tool.
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("st2k {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    // HIDDEN verbs, deliberately absent from USAGE (like the app's `--shot` harness): our
    // own DLL/EXE spawns `st2k flv-frame` with FLV bytes on stdin (first VP6/Sorenson
    // keyframe back as a PNG) or `st2k vp9-frame` with one raw VP9 keyframe on stdin
    // (decoded frame back as a PNG) — binary stdout, so neither may ever go through the
    // println! path below. Exit 0 with output; any failure is a non-zero exit and none.
    if args.first().is_some_and(|a| a == "flv-frame") {
        #[cfg(feature = "flash-video")]
        std::process::exit(vdec::run_flv());
        #[cfg(not(feature = "flash-video"))]
        {
            eprintln!("st2k: this build was compiled without the flash-video feature");
            std::process::exit(1);
        }
    }
    if args.first().is_some_and(|a| a == "vp9-frame") {
        #[cfg(feature = "vp9-video")]
        std::process::exit(vdec::run_vp9());
        #[cfg(not(feature = "vp9-video"))]
        {
            eprintln!("st2k: this build was compiled without the vp9-video feature");
            std::process::exit(1);
        }
    }

    // MCP server mode (`st2k --mcp` or `st2k mcp`): hand off to the stdio
    // JSON-RPC loop, which owns stdin/stdout until the client disconnects.
    if args.iter().any(|a| a == "--mcp") || args.first().map(|s| s == "mcp").unwrap_or(false) {
        if let Err(e) = sagethumbs2k_core::mcp::serve() {
            eprintln!("st2k --mcp: {e}");
            std::process::exit(1);
        }
        return;
    }

    // `clip-pixels` writes binary (a header + raw RGBA8) on success, which can't go
    // through `run`'s `Result<String, String>` → `println!` path — handled directly,
    // the same way the hidden `flv-frame`/`vp9-frame` verbs are above.
    if args.first().is_some_and(|a| a == "clip-pixels") {
        std::process::exit(run_clip_pixels(&args[1..]));
    }

    match run(&args) {
        Ok(out) => {
            println!("{out}");
        }
        Err(e) => {
            eprintln!("st2k: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn as_strs<'a>(pos: &'a [&'a String]) -> Vec<&'a str> {
        pos.iter().map(|s| s.as_str()).collect()
    }

    /// A008's headline case: `st2k thumbnail --size 128 in.png out.png` used to read
    /// `pos[0]` as `"128"` because `pos` only excluded tokens starting with `--`, not a
    /// known flag's VALUE.
    #[test]
    fn a_known_value_flags_value_never_lands_in_positionals() {
        let rest = args(&["--size", "128", "in.png", "out.png"]);
        assert_eq!(as_strs(&positionals(&rest)), vec!["in.png", "out.png"]);
    }

    #[test]
    fn a_known_value_flag_is_skipped_regardless_of_its_position_among_positionals() {
        // The flag arriving BEFORE the positional it belongs after used to shift every
        // positional over by one (`rotate --by right in.jpg` read pos[0] as "right").
        let rest = args(&["--by", "right", "in.jpg"]);
        assert_eq!(as_strs(&positionals(&rest)), vec!["in.jpg"]);
    }

    #[test]
    fn an_unrecognized_flags_own_token_is_excluded_but_its_value_is_not_swallowed() {
        let rest = args(&["--weird", "9", "in.png"]);
        // We have no table entry saying "--weird" takes a value, so we don't assume one:
        // only the flag's own spelling is excluded, matching pre-fix behaviour for a flag
        // no verb here defines.
        assert_eq!(as_strs(&positionals(&rest)), vec!["9", "in.png"]);
    }

    #[test]
    fn short_recurse_flag_does_not_pollute_positionals() {
        // Before this fix, `-r` (prebuild's short --recurse alias) didn't start with
        // "--", so it slipped straight into `pos` as a bogus input path.
        let rest = args(&["-r", "C:\\folder"]);
        assert_eq!(as_strs(&positionals(&rest)), vec!["C:\\folder"]);
        assert!(has_flag(&rest, "-r"));
    }

    #[test]
    fn every_value_flag_consumes_its_value_rather_than_leaking_it_as_an_input() {
        // A value-taking flag missing from VALUE_FLAGS does not error — its VALUE quietly
        // becomes a positional, i.e. a bogus input path. `bench-decode --runs 3` reported a
        // file literally named "3" as a decode FAILURE before `--runs` was registered. This
        // walks the whole list so the next flag added cannot repeat it.
        for f in VALUE_FLAGS {
            let rest = args(&[f, "7", "in.png"]);
            assert_eq!(
                as_strs(&positionals(&rest)),
                vec!["in.png"],
                "{f} did not consume its value; it leaked into the positionals"
            );
        }
    }

    #[test]
    fn flag_does_not_treat_the_next_flag_as_its_own_value() {
        let rest = args(&["--size", "--recurse", "in.png"]);
        assert_eq!(flag(&rest, "--size"), None);
    }

    #[test]
    fn flag_num_defaults_when_absent_but_errors_on_garbage() {
        let absent = args(&["thumbnail"]);
        assert_eq!(flag_num(&absent, "--size", 256u32), Ok(256));

        let bad = args(&["--size", "12x"]);
        assert!(
            flag_num(&bad, "--size", 256u32).is_err(),
            "an unparseable --size must error, not silently fall back to the default"
        );
    }

    #[test]
    fn flag_num_opt_distinguishes_absent_from_unparseable() {
        let absent = args(&["convert"]);
        assert_eq!(flag_num_opt::<u8>(&absent, "--webp-quality"), Ok(None));

        let bad = args(&["--webp-quality", "abc"]);
        assert!(
            flag_num_opt::<u8>(&bad, "--webp-quality").is_err(),
            "an unparseable --webp-quality must error, not silently behave as \"not requested\""
        );
    }

    /// 2026-09-05 audit, F12: `prebuild --size 96,typo,768` used to DROP the unparseable
    /// element and fill the cache for 96 and 768, reporting success for a run nobody asked
    /// for. Every element is parsed now, and the first bad one names itself. The rows below
    /// are the CLI half of the shared policy; `prebuild::parse_size_list_str` states it and
    /// `mcp.rs` runs the same rows through the JSON front end, so the two cannot drift.
    #[test]
    fn prebuild_size_list_parses_every_element_or_refuses_the_whole_flag() {
        let default = sagethumbs2k_core::prebuild::DEFAULT_SIZES.to_vec();
        assert_eq!(
            prebuild_sizes(&args(&["--recurse"])),
            Ok(default),
            "an ABSENT --size is the only route to the defaults"
        );

        for (spec, want) in [
            ("96,256,768", Ok(vec![96u32, 256, 768])),
            ("512", Ok(vec![512])),
            (" 96 , 256 ", Ok(vec![96, 256])),
            ("99999", Ok(vec![99999])),
            ("96,typo,768", Err("element 2")),
            ("96,,768", Err("element 2")),
            ("96,", Err("element 2")),
            ("0", Err("element 1")),
            ("96,0", Err("element 2")),
            ("-96", Err("element 1")),
            ("4294967296", Err("too large")),
            ("", Err("element 1")),
        ] {
            let got = prebuild_sizes(&args(&["--size", spec]));
            match want {
                Ok(sizes) => assert_eq!(got, Ok(sizes), "--size {spec:?}"),
                Err(fragment) => {
                    let err = got.expect_err("must be refused");
                    assert!(
                        err.starts_with("--size:") && err.contains(fragment),
                        "--size {spec:?}: got {err:?}"
                    );
                }
            }
        }
    }

    /// The refusal has to happen BEFORE any cache work: the run is rejected at argument
    /// parsing, so `cli::prebuild` (and its elevation guard, and the folder walk) is never
    /// reached. Checked through `run` rather than the helper, since that is the path a user
    /// actually takes.
    #[test]
    fn a_bad_prebuild_size_fails_before_the_run_starts() {
        let dir = scratch("prebuild_size");
        let err = run(&args(&[
            "prebuild",
            dir.to_str().unwrap(),
            "--size",
            "96,typo",
        ]))
        .expect_err("a partly invalid --size must not start a run");
        assert!(
            err.starts_with("--size:") && err.contains("typo"),
            "got {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "st2k_bin_{tag}_{}_{}",
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

    fn save_png(path: &std::path::Path) -> String {
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            40,
            30,
            image::Rgb([20, 120, 200]),
        ))
        .save(path)
        .unwrap();
        path.to_str().unwrap().to_string()
    }

    /// 2026-09-05 audit, F33: `thumbnail in.png out.png extra.png` used to render out.png and
    /// say nothing about the third name. The arity check runs before the verb, so nothing is
    /// written and the error names the stray argument. Against the pre-fix code `out.png`
    /// exists after the call.
    #[test]
    fn a_single_input_verb_rejects_an_extra_file_before_any_write() {
        let dir = scratch("arity");
        let src = save_png(&dir.join("in.png"));
        let extra = save_png(&dir.join("extra.png"));
        let extra_before = std::fs::read(&extra).unwrap();
        let out = dir.join("out.png");

        let err = run(&args(&["thumbnail", &src, out.to_str().unwrap(), &extra])).unwrap_err();
        assert!(err.contains("takes at most 2"), "{err}");
        assert!(
            err.contains("extra.png"),
            "must name the stray argument: {err}"
        );
        assert!(
            !out.exists(),
            "nothing may be written when the arguments are wrong"
        );
        assert_eq!(std::fs::read(&extra).unwrap(), extra_before);

        // Same shape without the flag noise: `strip a b` and `rotate a b`.
        let err = run(&args(&["strip", &src, &extra])).unwrap_err();
        assert!(err.contains("takes at most 1"), "{err}");
        let err = run(&args(&["rotate", &src, &extra, "--by", "right"])).unwrap_err();
        assert!(err.contains("takes at most 1"), "{err}");
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            2,
            "no sibling may have been written"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every single-input verb in the table refuses a surplus argument with the SAME message,
    /// so a script sees one shape; `formats` takes none at all. Against the pre-fix code
    /// `formats foo` prints the list and the rest fail for unrelated reasons.
    #[test]
    fn every_fixed_arity_verb_reports_a_surplus_argument_the_same_way() {
        for verb in [
            "thumbnail",
            "thumb",
            "convert",
            "rotate",
            "compress",
            "strip",
            "ocr",
            "info",
            "doctor",
            "diag",
            "register",
            "unregister",
            "upload",
            "upload-hosts",
            "upload-host",
            "devmode",
            "formats",
            "wallpaper-prepare",
            "folder-icon",
        ] {
            let err = run(&args(&[verb, "a", "b", "c"])).unwrap_err();
            assert!(
                err.contains("takes at most") && err.contains("unexpected"),
                "{verb}: {err}"
            );
        }
    }

    /// 2026-09-05 audit, E01: a `--json` batch report is something a script can ACT on.
    /// `--retry-from` runs exactly the files the saved report lists as failed, with the
    /// options given on this command line, and reports the retry the same way so a second
    /// retry chains off the first. The refusals are the ways a script gets it wrong: a file
    /// that is not a batch report, a report with nothing to retry, inputs given as well, or
    /// the flag with no path. Against the pre-fix code the flag is unknown and its value is
    /// read as an input path.
    #[test]
    fn retry_from_runs_only_the_failed_inputs_and_refuses_anything_else() {
        let dir = scratch("retry");
        let good = save_png(&dir.join("good.png"));
        let broken = dir.join("broken.png");
        std::fs::write(&broken, b"not a png").unwrap();
        let broken = broken.to_str().unwrap().to_string();
        let out = dir.join("out");
        let out = out.to_str().unwrap().to_string();

        let first = run(&args(&[
            "batch", "convert", &good, &broken, "--to", "webp", "--out", &out, "--json",
        ]))
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&first).unwrap();
        assert_eq!(v["status"], "partial");
        let report = dir.join("report.json");
        std::fs::write(&report, &first).unwrap();
        let report = report.to_str().unwrap().to_string();

        // The broken file is fixed between the runs, so the retry has something to convert.
        save_png(std::path::Path::new(&broken));
        let retry = run(&args(&[
            "batch",
            "convert",
            "--retry-from",
            &report,
            "--to",
            "webp",
            "--out",
            &out,
            "--json",
        ]))
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&retry).unwrap();
        assert_eq!(v["status"], "ok", "{retry}");
        assert_eq!(v["requested"], 1, "only the failed file is run: {retry}");
        assert_eq!(v["results"][0]["input"], broken.as_str());
        assert!(
            v["results"][0]["output"]
                .as_str()
                .unwrap()
                .starts_with(&out),
            "the retry honours this command line's --out: {retry}"
        );

        // Chaining: the retry's own report is a valid report, with nothing left to retry.
        let clean = dir.join("clean.json");
        std::fs::write(&clean, &retry).unwrap();
        let err = run(&args(&[
            "batch",
            "convert",
            "--retry-from",
            clean.to_str().unwrap(),
            "--to",
            "webp",
        ]))
        .unwrap_err();
        assert!(err.contains("nothing to retry"), "{err}");

        // Not a batch report: a pdf report, a bare array, and an image.
        for (what, bytes) in [
            (
                "pdf",
                br#"{"output":"x.pdf","status":"ok","requested":1,"combined":1,"omitted":[]}"#
                    .to_vec(),
            ),
            ("array", br#"[{"input":"a.png"}]"#.to_vec()),
            ("png", std::fs::read(&good).unwrap()),
        ] {
            let bad = dir.join(format!("{what}.json"));
            std::fs::write(&bad, bytes).unwrap();
            let err = run(&args(&[
                "batch",
                "convert",
                "--retry-from",
                bad.to_str().unwrap(),
                "--to",
                "webp",
            ]))
            .unwrap_err();
            assert!(
                err.starts_with("--retry-from:") && err.contains("not "),
                "{what}: the refusal must say the file is not a batch report: {err}"
            );
        }

        // Inputs alongside the flag, and the flag with no path.
        let err = run(&args(&[
            "batch",
            "convert",
            &good,
            "--retry-from",
            &report,
            "--to",
            "webp",
        ]))
        .unwrap_err();
        assert!(err.contains("do not also give them"), "{err}");
        let err = run(&args(&["batch", "convert", "--retry-from", "--to", "webp"])).unwrap_err();
        assert!(err.contains("needs the report's path"), "{err}");
        assert!(
            VALUE_FLAGS.contains(&"--retry-from"),
            "the report path must never be read as an input"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The list verbs keep taking a list: `pdf`/`cbz` with two inputs still combine, and the
    /// new `--strict`/`--json` flags reach the verb rather than being read as file names.
    #[test]
    fn multi_input_verbs_still_take_a_list_and_their_flags_are_not_inputs() {
        let dir = scratch("multi");
        let a = save_png(&dir.join("a.png"));
        let b = save_png(&dir.join("b.png"));
        let out = dir.join("out.pdf");
        let text = run(&args(&[
            "pdf",
            out.to_str().unwrap(),
            &a,
            &b,
            "--json",
            "--strict",
        ]))
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["combined"], 2);
        assert!(out.exists());

        let comic = dir.join("out.cbz");
        let text = run(&args(&["cbz", comic.to_str().unwrap(), &a, &b])).unwrap();
        assert_eq!(text, comic.to_str().unwrap());
        assert!(has_flag(&args(&["--strict"]), "--strict"));
        assert!(
            BOOL_FLAGS.contains(&"--strict"),
            "--strict must never be read as an input"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
