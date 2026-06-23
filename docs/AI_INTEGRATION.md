# SageThumbs 2K — AI / Agent Integration (design notes)

> **Status: Phase 1 (CLI) and Phase 2 (MCP server) both SHIPPED.** Cross-referenced from
> [FEATURES.md](FEATURES.md) §6a.
>
> **Phase 1 — `st2k.exe`** is built and installed: a console tool with verbs
> `thumbnail / convert / batch / rotate / strip / ocr / pdf / info [--json] / formats
> [--json]`, all over the existing engine (`src/cli.rs` is the verb logic, reused
> by the binary and unit-tested). Run `st2k` with no args for usage.

## The idea

SageThumbs already bundles real image capabilities. Expose them to AI agents (and
scripts) through a console / endpoints so that **a user who installs SageThumbs
doesn't need a separate image toolkit** — the agent reuses what's already here.
**Do not bundle anything new** — only surface the functions we already ship.

## What we already have to expose (zero new dependencies)

All of this logic already exists in the `lib` crate and is exercised by the shell
extension today:

| Capability | Backed by | Lives in |
|---|---|---|
| Decode **314 formats** → PNG (RAW, HEIC, PSD, MS Office, ebook/comic covers, PDF page 1, SVG, …) | image crate + WIC + bundled ImageMagick + resvg | `decode::decode_full` |
| **Convert** (PNG/JPG/WebP/BMP/GIF/TIFF/ICO) + quality + **resize** | image crate | `verbs::convert_to` (CLI exact-path) / `verbs::convert_file_opts` (GUI) |
| **Rotate / flip** | image crate | `verbs::transform_file` |
| **Strip metadata** (lossless EXIF/IPTC/XMP) | img-parts | `strip::strip_metadata` |
| **Read EXIF / image info** (dims, camera, date, GPS) | kamadak-exif | `strip::read_info` |
| **OCR → text** | in-box `Windows.Media.Ocr` | `ocr::recognize_bytes` |
| **Images → PDF** | hand-rolled `/DCTDecode` | `topdf::combine_to_pdf` |

The companion EXE (`sagethumbs2k-app.exe`) already links the lib and already parses
one argument mode (`--convert <listfile>`). Extending it is the natural home.

## Approach A — a real CLI — ✅ SHIPPED as `st2k.exe`

Decided (see Open questions §1, now resolved): shipped as a **tiny standalone
`st2k.exe`** (console subsystem), NOT a verb flag on the Options app. Usage
`st2k <verb> [args] [--json]`:

```
convert   <in> <out> [--format webp] [--quality 90] [--resize 1280x720|50%]
rotate    <in> --by right|left|180|fliph|flipv   # writes an '(edited)' sibling, never in place
strip     <in>                        # in place, lossless (JPEG/PNG; keeps the ICC profile)
info      <in> [--json]               # dims + EXIF/GPS  -> stdout (JSON)
ocr       <in> [--json]               # recognized text  -> stdout (needs a Windows OCR language pack)
pdf       <out> <in...>               # combine images into one PDF
thumbnail <in> <out.png> [--size 256] # render any of the 314 types to PNG (default 256px)
batch     <thumbnail|convert> <in|dir...> [--out DIR] [--size N] [--to EXT] [--quality N] [--resize WxH|N%]
                                      # bulk-process many files/folders in ONE process, fanned out across all cores
formats   [--json]                    # list supported extensions + categories
```

- Each verb is a thin arg-parser over the existing functions — no new logic, no new
  deps. `info`/`ocr`/`formats` print JSON to stdout so an agent can parse them.
- Exit codes: 0 ok, non-zero on failure; errors to stderr.
- This alone makes SageThumbs a free, **offline** image toolbox for any agent that
  can shell out (`thumbnail` is the headline — preview formats Windows itself can't).

## Approach B — MCP server (`--mcp`, stdio) — **SHIPPED**

`st2k --mcp` (or `st2k mcp`) speaks MCP over stdio (newline-delimited JSON-RPC 2.0)
and exposes the same verbs as MCP tools, so an agent can **discover and call them
directly** ("OCR this screenshot", "convert this HEIC to JPG", "what's the EXIF
here", "thumbnail this CR3"). Implemented in `src/mcp.rs`:

- `serve()` runs the stdin→stdout loop; `handle(&Value)` is a pure, testable
  dispatcher for `initialize` / `tools/list` / `tools/call` / `ping` (notifications
  draw no reply; unknown requests → JSON-RPC `-32601`; a stray UTF-8 BOM is tolerated).
- 10 tools = the CLI verbs **minus `batch`** (thumbnail / convert / rotate / strip / ocr /
  pdf / info / formats — `batch` is CLI-only) **plus two agent-native tools** (`view` /
  `compress`), each with a JSON-Schema, each calling `cli::*`. Tool failures return a
  result with `isError: true`; protocol faults use JSON-RPC errors.
  - **`view`** — decode any of the 314 formats and return it as an MCP image content
    block (base64 PNG) so the calling agent can **SEE** the file directly (RAW, HEIC,
    PSD, ebook/comic covers, PDF page 1, …), no intermediate file on disk.
  - **`compress`** — re-encode an image to a smaller file (format / quality / resize),
    over the same `verbs::convert_*` pipeline as the CLI.
- One dep added: **`serde_json`** (pure-Rust, MIT/Apache) — reachable only from the
  CLI binary, so LTO dead-strips it from the shell-extension DLL. No network.
- **Use it:** point an MCP client (Claude Desktop, an IDE agent) at
  `C:\Program Files\SageThumbs2K\st2k.exe` with the single arg `--mcp`.

## Constraints / guardrails

- **Reuse only.** No new bundled tools or crates; if a verb would need something we
  don't already ship, it's out of scope for this feature.
- **Offline by default.** No network calls.
- **Explicit writes.** File-creating/overwriting verbs require an explicit output
  argument; never write implicitly.
- **Compact-install awareness.** The "compact" installer omits ImageMagick, so verbs
  that depend on it (some RAW/exotic decodes) should degrade gracefully and say so.

## Open questions

- ~~One combined binary (`--mcp` flag on the app) vs. a tiny separate CLI?~~
  **RESOLVED** — shipped as a tiny separate **`st2k.exe`** (console subsystem); the
  `--mcp` server mode lives on the same binary, as planned.
- ~~MCP transport: stdio only?~~ **RESOLVED** — stdio, newline-delimited JSON-RPC 2.0
  (the standard agent-launched transport).
- Should the installer add the install dir to `PATH` (discoverability) or document a
  full-path invocation? *Current state: NOT on PATH; callers use the full path
  `C:\Program Files\SageThumbs2K\st2k.exe`. (`scripts\install.ps1` now copies
  `st2k.exe` into the install dir — fixed 2026-06-11.)*
- Batch/glob handling in the CLI, or leave globbing to the caller's shell?

> **MCP status:** Phase 2 **shipped (2026-06-11)** — `src/mcp.rs`, `serde_json` dep
> (CLI-only). Verified with unit tests + a live stdio handshake.
