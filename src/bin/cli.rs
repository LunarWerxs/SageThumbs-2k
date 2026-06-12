//! `st2k` — the SageThumbs 2K command-line tool. A thin arg parser over
//! `sagethumbs2k::cli`, exposing the bundled engine (decode 185 formats, convert,
//! rotate, strip, OCR, PDF, thumbnail) to scripts and AI agents. Console
//! subsystem (no `windows_subsystem = "windows"`), so stdout/stderr work.

use sagethumbs2k::cli;
use sagethumbs2k::Resize;

const USAGE: &str = "\
st2k — SageThumbs 2K command line

USAGE:
  st2k thumbnail <in> <out.png> [--size N]      render any format to an image (N px, default 256)
  st2k convert   <in> <out> [--quality N] [--resize WxH|N%]
  st2k rotate    <in> --by right|left|180|fliph|flipv
  st2k strip     <in>                           strip EXIF/GPS metadata (JPEG/PNG, lossless)
  st2k ocr       <in>                           recognize text → stdout
  st2k pdf       <out.pdf> <in> [in...]         combine images into one PDF
  st2k info      <in> [--json]                  dimensions + camera/date/GPS
  st2k formats   [--json]                       list supported input formats
  st2k --mcp                                     run as an MCP server (stdio JSON-RPC, for AI agents)
";

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

/// Parse "--resize 1280x720" or "--resize 50%" into a `Resize` (default None).
fn parse_resize(args: &[String]) -> Result<Resize, String> {
    let Some(v) = flag(args, "--resize") else {
        return Ok(Resize::None);
    };
    if let Some(p) = v.strip_suffix('%') {
        let pct: u32 = p.parse().map_err(|_| format!("bad percent '{v}'"))?;
        return Ok(Resize::Percent(pct.clamp(1, 1000)));
    }
    let (w, h) = v.split_once(['x', 'X']).ok_or_else(|| format!("bad --resize '{v}' (use WxH or N%)"))?;
    let w: u32 = w.parse().map_err(|_| format!("bad width in '{v}'"))?;
    let h: u32 = h.parse().map_err(|_| format!("bad height in '{v}'"))?;
    Ok(Resize::Fit(w.max(1), h.max(1)))
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
            cli::convert(i, o, q, parse_resize(rest)?)
        }
        "rotate" => {
            let i = need(&pos, 0)?;
            let by = flag(rest, "--by").ok_or("rotate needs --by right|left|180|fliph|flipv")?;
            cli::rotate(i, &by)
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
        "" | "-h" | "--help" | "help" => Ok(USAGE.to_string()),
        other => Err(format!("unknown command '{other}'\n\n{USAGE}")),
    }
}

fn need<'a>(pos: &'a [&'a String], i: usize) -> Result<&'a str, String> {
    pos.get(i).map(|s| s.as_str()).ok_or_else(|| format!("missing argument #{}", i + 1))
}

fn main() {
    // WIC / WinRT decoders (HEIC, PDF, RAW via the OS) need COM.
    unsafe {
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();

    // MCP server mode (`st2k --mcp` or `st2k mcp`): hand off to the stdio
    // JSON-RPC loop, which owns stdin/stdout until the client disconnects.
    if args.iter().any(|a| a == "--mcp") || args.first().map(|s| s == "mcp").unwrap_or(false) {
        if let Err(e) = sagethumbs2k::mcp::serve() {
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
