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
  st2k batch <thumbnail|convert> <in|dir...> [--out DIR] [--size N] [--to EXT] [--quality N] [--resize WxH|N%]
                                                bulk-process many files/folders in parallel (one process)
  st2k convert   <in> <out> [--quality N] [--webp-quality N] [--resize WxH|N%]   (--webp-quality → lossy WebP)
  st2k prebuild  <dir|file...> [--recurse] [--size N,N] [--rebuild-all] [--jobs N]
                                                fill Explorer's thumbnail cache ahead of browsing
                                                (--size defaults to 96,256,768 — one per Explorer view)
  st2k rotate    <in> --by right|left|180|fliph|flipv
  st2k compress  <in> --max-size 1MB|500KB|N    shrink to a target file size (JPEG, quality+scale search)
  st2k strip     <in>                           strip EXIF/GPS metadata (JPEG/PNG/WebP/SVG(Z)/HEIC/HEIF/AVIF, lossless)
  st2k ocr       <in>                           recognize text → stdout
  st2k pdf       <out.pdf> <in> [in...]         combine images into one PDF
  st2k info      <in> [--json]                  dimensions + camera/date/GPS
  st2k formats   [--json]                       list supported input formats
  st2k doctor    [file]                         self-check: why are thumbnails not showing? (add a file to probe it)
  st2k register  [--off|--status]               portable build: turn Explorer thumbnails on for this user
  st2k upload-hosts [--open]                     show (or open) the editable upload-hosts config file
  st2k devmode   [on|off|status]                toggle the developer test-box flag
  st2k --mcp                                     run as an MCP server (stdio JSON-RPC, for AI agents)
  st2k --version | -V                            print the version and exit
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
];

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

fn run(args: &[String]) -> Result<String, String> {
    let verb = args.first().map(|s| s.as_str()).unwrap_or("");
    let rest = &args[args.len().min(1)..];
    let pos = positionals(rest);

    match verb {
        "thumbnail" | "thumb" => {
            let (i, o) = (need(&pos, 0)?, need(&pos, 1)?);
            let size = flag_num(rest, "--size", 256)?;
            cli::thumbnail(i, o, size)
        }
        "convert" => {
            let (i, o) = (need(&pos, 0)?, need(&pos, 1)?);
            let q = flag_num(rest, "--quality", 90u8)?;
            let wq = flag_num_opt::<u8>(rest, "--webp-quality")?;
            cli::convert(
                i,
                o,
                q,
                wq,
                cli::parse_resize(flag(rest, "--resize").as_deref())?,
            )
        }
        "batch" => {
            // batch <op> <inputs...> [--out DIR] [--size N] [--to EXT] [--quality N] [--resize ...]
            let op = need(&pos, 0)?;
            let inputs: Vec<String> = pos.iter().skip(1).map(|s| s.to_string()).collect();
            if inputs.is_empty() {
                return Err("batch needs at least one input file or directory".to_string());
            }
            let size = flag_num(rest, "--size", 256)?;
            let q = flag_num(rest, "--quality", 90u8)?;
            cli::batch(
                op,
                &inputs,
                flag(rest, "--out").as_deref(),
                size,
                flag(rest, "--to").as_deref(),
                q,
                cli::parse_resize(flag(rest, "--resize").as_deref())?,
            )
        }
        "prebuild" => {
            // prebuild <paths...> [--recurse] [--size N[,N…]] [--rebuild-all] [--jobs N]
            let inputs: Vec<String> = pos.iter().map(|s| s.to_string()).collect();
            if inputs.is_empty() {
                return Err("prebuild needs at least one folder or file".to_string());
            }
            // `--size` takes a LIST because the shell caches per size bucket; a single number
            // leaves every other Explorer view still building its tiles by hand.
            let sizes: Vec<u32> = match flag(rest, "--size") {
                Some(s) => {
                    let parsed: Vec<u32> =
                        s.split(',').filter_map(|p| p.trim().parse().ok()).collect();
                    if parsed.is_empty() {
                        return Err(format!("--size: no number found in \"{s}\""));
                    }
                    parsed
                }
                None => sagethumbs2k_core::prebuild::DEFAULT_SIZES.to_vec(),
            };
            cli::prebuild(
                &inputs,
                has_flag(rest, "--recurse") || has_flag(rest, "-r"),
                sizes,
                has_flag(rest, "--rebuild-all"),
                flag_num(rest, "--jobs", 3)?,
            )
        }
        "rotate" => {
            let i = need(&pos, 0)?;
            let by = flag(rest, "--by").ok_or("rotate needs --by right|left|180|fliph|flipv")?;
            cli::rotate(i, &by)
        }
        "compress" => {
            let i = need(&pos, 0)?;
            let max = flag(rest, "--max-size")
                .ok_or("compress needs --max-size (e.g. 1MB, 500KB, 800000)")?;
            cli::compress(i, cli::parse_size(&max)?)
        }
        "strip" => cli::strip_meta(need(&pos, 0)?),
        "ocr" => cli::ocr(need(&pos, 0)?),
        "pdf" => {
            let out = need(&pos, 0)?;
            let inputs: Vec<String> = pos.iter().skip(1).map(|s| s.to_string()).collect();
            cli::pdf(out, &inputs)
        }
        "info" => cli::info(need(&pos, 0)?, has_flag(rest, "--json")),
        // Dev/measurement verb (undocumented in --help, like the app EXE's --bench-* modes):
        // time the decode of many files inside ONE process, so the numbers carry no per-file
        // process-start noise. Used by scripts\check-decode-speed.ps1.
        "bench-decode" => {
            let inputs: Vec<String> = pos.iter().map(|s| s.to_string()).collect();
            if inputs.is_empty() {
                return Err("bench-decode needs at least one input file".to_string());
            }
            let size = flag_num(rest, "--size", 256u32)?;
            let runs = flag_num(rest, "--runs", 3u32)?;
            cli::bench_decode(&inputs, size, runs)
        }
        "formats" => Ok(cli::list_formats(has_flag(rest, "--json"))),
        // Read-only; never fails, so it always prints a report rather than an error —
        // a user running this already has something broken. An optional file path adds a
        // per-file probe ("st2k doctor C:\path\to\that.xcf") that actually tries to decode
        // THAT file — the check that explains "registered fine but this one file is blank".
        "doctor" | "diag" => Ok(sagethumbs2k_core::doctor::report(
            pos.first().map(|s| s.as_str()),
        )),
        "register" | "unregister" => {
            // One verb, because "register --off" and "unregister" are the same action and
            // having both spellings fail differently would be its own bug report.
            let off = verb == "unregister"
                || has_flag(rest, "--off")
                || pos.first().is_some_and(|p| p.as_str() == "off");
            let status =
                has_flag(rest, "--status") || pos.first().is_some_and(|p| p.as_str() == "status");
            cli::register_portable(off, status)
        }
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
}
