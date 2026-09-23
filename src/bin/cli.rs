//! `st2k` — the SageThumbs 2K command-line tool. A thin arg parser over
//! `sagethumbs2k_core::cli`, exposing the bundled engine (decode all registered formats, convert,
//! rotate, strip, OCR, PDF, thumbnail) to scripts and AI agents. Console
//! subsystem (no `windows_subsystem = "windows"`), so stdout/stderr work.

use sagethumbs2k_core::cli;

// The hidden video-decode child verbs (`flv-frame`: VP6 via nihav + Sorenson via h263-rs;
// `vp9-frame`: VP9 Profile 2/3 via vp9dec; `mpeg-frame`: MPEG-1/2 via oxideav-mpeg12video). Behind EXE-only features so the panicky /
// unsafe-heavy decoder crates exist ONLY in this console binary — see src/bin/vdec/mod.rs
// for the whole containment argument.
#[cfg(any(feature = "flash-video", feature = "vp9-video", feature = "mpeg-video"))]
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
                                                also puts it on the clipboard) and, on stderr, when the
                                                host deletes it; needs SageThumbs2K.exe installed
                                                alongside st2k.exe (spawns it — no network code lives
                                                in the CLI itself)
  st2k upload-hosts [--open]                     show (or open) the editable upload-hosts config file
  st2k upload-history [--json]                   every uploaded link, newest first, with its expiry
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
    "--strip-metadata",
    "--lockscreen",
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
        "formats" | "upload-history" => Some(0),
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
    cli::convert(i, o, q, wq, resize, has_flag(rest, "--strip-metadata"))
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

/// Owned copy of the positional paths, rejected with `empty_msg` when the verb got none.
fn input_paths(pos: &[&String], empty_msg: &str) -> Result<Vec<String>, String> {
    let inputs: Vec<String> = pos.iter().map(|s| s.to_string()).collect();
    if inputs.is_empty() {
        return Err(empty_msg.to_string());
    }
    Ok(inputs)
}

/// `prebuild <paths...> [--recurse] [--size N[,N…]] [--rebuild-all] [--jobs N]`
fn run_prebuild(pos: &[&String], rest: &[String]) -> Result<String, String> {
    let inputs = input_paths(pos, "prebuild needs at least one folder or file")?;
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
fn run_wallpaper_prepare(pos: &[&String], rest: &[String]) -> Result<String, String> {
    let (i, out_dir) = (need(pos, 0)?, need(pos, 1)?);
    cli::wallpaper_prepare(i, out_dir, has_flag(rest, "--lockscreen"))
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
    let inputs = input_paths(pos, "bench-decode needs at least one input file")?;
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

    // `flag` returns `None` for a value-taking flag that is present but unfinished (`--out`
    // as the last token, or `--out --json`) exactly as for an absent one, so every caller
    // that substitutes a default would silently ignore the request — `batch ... --out` used
    // to write next to each source. Refuse once, here, before any verb runs. `--retry-from`
    // is left to `retry_inputs`, which names the report's path in its own refusal.
    for f in VALUE_FLAGS {
        if *f != "--retry-from" && has_flag(rest, f) && flag(rest, f).is_none() {
            return Err(format!("{f} needs a value"));
        }
    }

    if let Some(r) = dispatch_file_verb(verb, &pos, rest) {
        return r;
    }
    if let Some(r) = dispatch_helper_verb(verb, &pos, rest) {
        return r;
    }
    if let Some(r) = dispatch_admin_verb(verb, &pos, rest) {
        return r;
    }
    match verb {
        "" | "-h" | "--help" | "help" => Ok(USAGE.to_string()),
        other => Err(format!("unknown command '{other}'\n\n{USAGE}")),
    }
}

/// File-processing verbs (the bulk of the surface: decode/convert/combine/inspect one or
/// more images). `None` means "not one of mine" — [`run`] tries the next group.
fn dispatch_file_verb(
    verb: &str,
    pos: &[&String],
    rest: &[String],
) -> Option<Result<String, String>> {
    Some(match verb {
        "thumbnail" | "thumb" => run_thumbnail(pos, rest),
        "convert" => run_convert(pos, rest),
        "batch" => run_batch(pos, rest),
        "prebuild" => run_prebuild(pos, rest),
        "rotate" => run_rotate(pos, rest),
        "compress" => run_compress(pos, rest),
        "strip" => need(pos, 0).and_then(cli::strip_meta),
        "wallpaper-prepare" => run_wallpaper_prepare(pos, rest),
        "folder-icon" => need(pos, 0).and_then(cli::folder_icon),
        "ocr" => need(pos, 0).and_then(cli::ocr),
        "pdf" => need(pos, 0).and_then(|out| {
            let inputs: Vec<String> = pos.iter().skip(1).map(|s| s.to_string()).collect();
            cli::pdf(out, &inputs, combine_opts(rest))
        }),
        "cbz" => need(pos, 0).and_then(|out| {
            let inputs: Vec<String> = pos.iter().skip(1).map(|s| s.to_string()).collect();
            cli::cbz(out, &inputs, combine_opts(rest))
        }),
        "info" => need(pos, 0).and_then(|f| cli::info(f, has_flag(rest, "--json"))),
        _ => return None,
    })
}

/// Helper / child-process verbs: bench harness, format listing, and the keyless-upload
/// pair. `None` means "not one of mine".
fn dispatch_helper_verb(
    verb: &str,
    pos: &[&String],
    rest: &[String],
) -> Option<Result<String, String>> {
    Some(match verb {
        "bench-decode" => run_bench_decode(pos, rest),
        "formats" => Ok(cli::list_formats(has_flag(rest, "--json"))),
        "upload" => need(pos, 0).and_then(|f| cli::upload(f, has_flag(rest, "--copy"))),
        "upload-hosts" | "upload-host" => {
            let open = has_flag(rest, "--open") || pos.first().map(|s| s.as_str()) == Some("open");
            cli::upload_hosts(open)
        }
        "upload-history" => cli::upload_history(has_flag(rest, "--json")),
        _ => return None,
    })
}

/// Admin / diagnostic verbs: doctor, register/unregister, devmode. `None` means "not one
/// of mine".
fn dispatch_admin_verb(
    verb: &str,
    pos: &[&String],
    rest: &[String],
) -> Option<Result<String, String>> {
    Some(match verb {
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
        "register" | "unregister" => run_register(verb, pos, rest),
        "devmode" => cli::devmode(pos.first().map(|s| s.as_str()).unwrap_or("status")),
        _ => return None,
    })
}

fn need<'a>(pos: &'a [&'a String], i: usize) -> Result<&'a str, String> {
    pos.get(i)
        .map(|s| s.as_str())
        .ok_or_else(|| format!("missing argument #{}", i + 1))
}

/// Exits 1 with the "compiled without" notice for a video-decoder feature this build
/// does not have (the hidden `flv-frame`/`vp9-frame`/`mpeg-frame` verbs).
#[cfg(not(all(feature = "flash-video", feature = "vp9-video", feature = "mpeg-video")))]
fn missing_feature(feature: &str) -> ! {
    eprintln!("st2k: this build was compiled without the {feature} feature");
    std::process::exit(1)
}

fn main() {
    // Capture panics to the diagnostics log before the process aborts (panic=abort).
    st2k_base::safety::install_panic_hook("st2k");
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
    // keyframe back as a PNG), `st2k vp9-frame` with one raw VP9 keyframe on stdin, or
    // `st2k mpeg-frame` with one MPEG-1/2 intra-picture unit on stdin (decoded frame back
    // as a PNG) — binary stdout, so none may ever go through the println! path below.
    // Exit 0 with output; any failure is a non-zero exit and none.
    if args.first().is_some_and(|a| a == "flv-frame") {
        #[cfg(feature = "flash-video")]
        std::process::exit(vdec::run_flv());
        #[cfg(not(feature = "flash-video"))]
        missing_feature("flash-video");
    }
    if args.first().is_some_and(|a| a == "vp9-frame") {
        #[cfg(feature = "vp9-video")]
        std::process::exit(vdec::run_vp9());
        #[cfg(not(feature = "vp9-video"))]
        missing_feature("vp9-video");
    }
    if args.first().is_some_and(|a| a == "mpeg-frame") {
        #[cfg(feature = "mpeg-video")]
        std::process::exit(vdec::run_mpeg());
        #[cfg(not(feature = "mpeg-video"))]
        missing_feature("mpeg-video");
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

// A crate root's children sit BESIDE it, and Cargo would auto-discover a `src/bin/tests.rs` as
// a binary named `tests`; the path attribute keeps this file's tests under `src/bin/cli/`.
#[cfg(test)]
#[path = "cli/tests.rs"]
mod tests;
