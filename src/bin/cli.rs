//! `st2k` — the SageThumbs 2K command-line tool. A thin arg parser over
//! `sagethumbs2k_core::cli`, exposing the bundled engine (decode all registered formats, convert,
//! rotate, strip, OCR, PDF, thumbnail) to scripts and AI agents. Console
//! subsystem (no `windows_subsystem = "windows"`), so stdout/stderr work.

use sagethumbs2k_core::cli;

const USAGE: &str = "\
st2k — SageThumbs 2K command line

USAGE:
  st2k thumbnail <in> <out.png> [--size N]      render any format to an image (N px, default 256)
  st2k batch <thumbnail|convert> <in|dir...> [--out DIR] [--size N] [--to EXT] [--quality N] [--resize WxH|N%]
                                                bulk-process many files/folders in parallel (one process)
  st2k convert   <in> <out> [--quality N] [--webp-quality N] [--resize WxH|N%]   (--webp-quality → lossy WebP)
  st2k rotate    <in> --by right|left|180|fliph|flipv
  st2k compress  <in> --max-size 1MB|500KB|N    shrink to a target file size (JPEG, quality+scale search)
  st2k strip     <in>                           strip EXIF/GPS metadata (JPEG/PNG, lossless)
  st2k ocr       <in>                           recognize text → stdout
  st2k pdf       <out.pdf> <in> [in...]         combine images into one PDF
  st2k info      <in> [--json]                  dimensions + camera/date/GPS
  st2k formats   [--json]                       list supported input formats
  st2k doctor                                   self-check: why are thumbnails not showing?
  st2k upload-hosts [--open]                     show (or open) the editable upload-hosts config file
  st2k devmode   [on|off|status]                toggle the developer test-box flag
  st2k --mcp                                     run as an MCP server (stdio JSON-RPC, for AI agents)
  st2k --version | -V                            print the version and exit
";

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn run(args: &[String]) -> Result<String, String> {
    let verb = args.first().map(|s| s.as_str()).unwrap_or("");
    let rest = &args[args.len().min(1)..];
    let pos: Vec<&String> = rest.iter().filter(|a| !a.starts_with("--")).collect();

    match verb {
        "thumbnail" | "thumb" => {
            let (i, o) = (need(&pos, 0)?, need(&pos, 1)?);
            let size = flag(rest, "--size").and_then(|s| s.parse().ok()).unwrap_or(256);
            cli::thumbnail(i, o, size)
        }
        "convert" => {
            let (i, o) = (need(&pos, 0)?, need(&pos, 1)?);
            let q = flag(rest, "--quality").and_then(|s| s.parse().ok()).unwrap_or(90u8);
            let wq = flag(rest, "--webp-quality").and_then(|s| s.parse::<u8>().ok());
            cli::convert(i, o, q, wq, cli::parse_resize(flag(rest, "--resize").as_deref())?)
        }
        "batch" => {
            // batch <op> <inputs...> [--out DIR] [--size N] [--to EXT] [--quality N] [--resize ...]
            let op = need(&pos, 0)?;
            let inputs: Vec<String> = pos.iter().skip(1).map(|s| s.to_string()).collect();
            if inputs.is_empty() {
                return Err("batch needs at least one input file or directory".to_string());
            }
            let size = flag(rest, "--size").and_then(|s| s.parse().ok()).unwrap_or(256);
            let q = flag(rest, "--quality").and_then(|s| s.parse().ok()).unwrap_or(90u8);
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
        "formats" => Ok(cli::list_formats(has_flag(rest, "--json"))),
        // Read-only; never fails, so it always prints a report rather than an error —
        // a user running this already has something broken.
        "doctor" | "diag" => Ok(sagethumbs2k_core::doctor::report()),
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
    pos.get(i).map(|s| s.as_str()).ok_or_else(|| format!("missing argument #{}", i + 1))
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
