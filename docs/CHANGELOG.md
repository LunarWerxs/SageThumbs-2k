# Changelog

All notable user-facing changes to **SageThumbs 2K**. Newest first.

## 0.6.1

- **Crisp thumbnails at large/Hi-DPI icon sizes.** Raised the maximum generated thumbnail
  edge from 256 px to 1024 px. On 4K displays and the larger ("jumbo") icon views, Explorer
  asks for thumbnails bigger than 256 px; we used to hand back an undersized 256 px image,
  which looked soft *and* couldn't be cached durably — so Explorer re-generated it on every
  refresh, re-decoding a frame from each (potentially multi-GB) video each time. Now we honor
  the requested size up to 1024 px, so big thumbnails are sharp and stay cached. Smaller views
  are unchanged.
- **Audio waveform thumbnails.** WAV, AIFF and AIFF-C files with no embedded cover art now
  show a drawn waveform instead of a blank icon — a quick visual of the sound. Files that do
  have album art still show the artwork; compressed formats (MP3/FLAC/…) are unchanged.
- **+1 format — AIFF-C (`.aifc`).** The audio handler now also covers AIFF-C, bringing the
  total to **315** supported file types.

## 0.6.0

- **Details in Explorer for 300+ formats Windows can't read.** A new property handler
  surfaces image dimensions, EXIF camera info, and audio tags in Explorer's Details pane,
  hover tooltips, and sortable/groupable columns — for the formats Windows has no idea how
  to read on its own. Read-only and crash-isolated behind a panic boundary, like the
  thumbnail provider.
- **Proper color management.** Embedded ICC profiles and wide-gamut images (Display P3 /
  Adobe RGB) now render in correct sRGB instead of looking over-saturated. AVIF/HEIC read
  their profile from the ISOBMFF `colr` box (including the CICP nclx Display-P3 signal that
  iPhone HEIC uses), and CMYK JPEGs are color-managed through their embedded CMYK profile.
  Pure Rust — no C dependencies.
- **Autodesk Fusion 360 (.f3d)** thumbnails — read from the zstd-compressed preview inside
  the file's ZIP container — bringing the total to **314**.
- **Repair file associations** button (Settings → Diagnostics): re-registers SageThumbs for
  all your enabled formats when another app has taken over the thumbnails, then clears the
  thumbnail cache.
- **MCP `view` and `compress` tools.** The AI/agent server gained a `view` tool that decodes
  any of the 314 formats to a PNG image block so an agent can actually *see* the file, plus a
  `compress` tool.
- **Smaller installer (~1.5 MB lighter).** The bundled ImageMagick text-shaping stack
  (glib/harfbuzz/freetype/fribidi/raqm) is stubbed out — we only decode raster images, never
  render text.
- **Hardening.** Fuzzing and Miri over the untrusted-input parsers, COM round-trip tests for
  the preview and property handlers, dead-code cleanup, and the test corpus extended to cover
  all 314 formats.

## 0.5.0

- **Video thumbnails, done properly.** Explorer now reliably shows a thumbnail for your
  videos — and it's a *representative* frame from about a third of the way in, not the black
  intro, fade-in, or studio logo you'd get from the opening frame. Covers **MP4, MOV, M4V,
  MKV, WebM, AVI, and WMV**.
- **Fast even on huge 4K files.** For MP4 and MKV we read the video's own index and pull just
  the single frame we need (a few megabytes) instead of scanning the file — so a folder of
  multi-gigabyte movies on a slow drive thumbnails quickly, and can no longer peg a CPU core
  or leave blank tiles that never resolve.
- Formats Windows itself has no codec for (MPEG-1/2 **.mpg/.mpeg**, Flash **.flv**) keep the
  normal file icon — nothing can produce a thumbnail for them without an installed codec.

## 0.4.9

- **Correct colors for wide-gamut photos.** Thumbnails of Display-P3 / Adobe RGB images
  (most modern phone and camera photos) are now color-managed to sRGB, so they match what
  you see in Photos or a browser instead of looking over-saturated.
- **Crisp pixel art & icons.** Tiny images (sprites, 16–64 px icons) now scale up sharp
  instead of being blurred into a smudge.
- **Compress to a target file size.** The `st2k` command-line tool gained
  `compress <file> --max-size 1MB` (or `500KB`, etc.) — it finds the best quality that
  fits under your size limit.
- **No more stuck blank thumbnails.** If a file decodes to nothing, Explorer now shows the
  normal file icon instead of caching an empty tile you couldn't clear.
- **Apple Live Photos (.livp)** now show their still image — bringing the total to **313**.

## 0.4.8

- **Thumbnails now work on a clean Windows install.** The shell extension no longer
  depends on the Visual C++ runtime, so it registers and shows thumbnails even on a fresh
  machine that's missing the VC++ redistributable — previously that produced no thumbnails
  and a cryptic "failed to register" error during install.
- **More EPUB covers show up.** Books that reference their cover through a wrapper page
  instead of the image directly — e.g. Standard Ebooks and many older EPUBs — now display
  the real cover rather than a blank icon.
- **Very large comic archives thumbnail again.** A CBZ or CB7 over 256 MB now shows its
  cover, read straight from the archive without loading the whole file into memory, instead
  of falling back to a generic icon.
- **Two more formats** — GeoGebra worksheets (**.ggb**) and **.phz** comic archives —
  bringing the total to **312**. A JPEG-2000 page inside a comic archive can now serve as
  the cover too (on the full install).
- **DjVu hardening verified** — the specific scanned documents that crashed the previous
  generation of this kind of extension render cleanly here.

## 0.4.7

- **Fixed preview-pane hangs.** Selecting an image in a file dialog or the Explorer reading/
  preview pane could freeze and sometimes need the preview host killed (or a reboot). Previews
  now decode off the host's UI thread, and an internal concurrency lock that could leak when a
  host was force-killed is now self-healing — so the hang can no longer build up over time.
- **Right-clicking an exotic file no longer freezes Explorer.** The classic right-click menu's
  preview now uses only the fast built-in decoders, never a slow external one on the shell
  thread.
- **Video previews and thumbnails are time-bounded**, so a stalling codec can't hang the
  preview or thumbnail.
- **Right-click actions run in the background.** Convert, Resize, Rotate, Strip metadata, and
  the rest no longer freeze the Explorer window while they work — even across many files.
- **Automatic update check.** Opening **Settings** now does a quiet, once-a-day background
  check for a newer version and flags the "Check for updates" button when one is available —
  no nagging pop-ups, and never more than once a day.

## 0.4.6

- **Video thumbnails** — Explorer now shows a representative frame for video files
  (Matroska **.mkv**, **.webm**, **.mp4**, **.mov**, **.avi**, and more) using the OS's own
  codecs, so it bundles **zero** extra bytes and streams the file instead of loading it.
- **Settings import / export** — back up your whole configuration, or move it to another PC,
  as a single human-readable JSON file (Settings → Diagnostics).
- **Check for updates** — a button that asks GitHub whether a newer release is out and points
  you to the download (Settings → Diagnostics).
- **Rebuild thumbnail cache** — clears Windows' stale thumbnail cache and restarts Explorer,
  so a format/size change shows up immediately (Settings → Diagnostics).
- **More reliable camera-RAW thumbnails** — RAW files now fall back to their embedded preview
  even when it's small, so they thumbnail on a clean Windows install with no extra codecs.
- About box now credits the original author and shows the license.

## 0.4.5

- **Screenshot capture tool:** explicit **Ctrl+C** (copy) / **Ctrl+S** (save) keys, plus an
  optional fixed save folder for Ctrl+S (otherwise it prompts each time).

## 0.4.4

- **Fully customizable right-click menu** — drag to reorder entries *and* their dividers
  (WYSIWYG), and show/hide any item; the menu mirrors your layout exactly.
- "Tools" submenu flattened to individually toggleable top-level verbs; a **"Show menu on all
  file types"** option (a condensed file-utility menu on unsupported files).
- **Image info** is now a verbose, copyable dialog — every EXIF tag plus a GPS map link.
- Settings window is **vertically resizable**, with flicker-free scrolling.
- **Diagnostics** section: a user-sendable log with crash capture (Settings → Diagnostics).

## Earlier (0.4.x)

- **288 file formats** — camera RAW, Photoshop (PSD/PSB), HEIC/AVIF, JPEG XR, JPEG XL,
  MS Office, DjVu, ebooks & comics, 3D-print files, and the obscure long tail.
- **Right-click toolkit** — convert, resize, lossless rotate/flip, combine-to-PDF / -CBZ,
  shrink-for-email, OCR, a system-wide eyedropper, strip metadata, copy, set-as-folder-icon,
  set-as-wallpaper, and folder utilities. Multi-file jobs run in parallel across every core.
- **Native Windows 11 UI** with system-following **dark mode** and **36 languages**.
- A searchable **per-format on/off** list, and tunable thumbnail size + JPEG/PNG quality.
- Built-in **screenshot capture** tool with a configurable global hotkey.
- **Crash-isolated** — a corrupt or malicious file can't take down File Explorer (runs
  out-of-process, panic-guarded, with a sandboxed decoder).
