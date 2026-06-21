# SageThumbs 2K — Features & Decisions Reference

A living, manual-ready catalog of everything SageThumbs 2K does and the key
decisions behind it. Source for the eventual README / user manual / store
listing. (This file is organized by feature area for end-user-facing
documentation.)

> **What it is:** a modern, crash-isolated Windows 11 shell extension (Rust) that
> rebuilds the abandoned SageThumbs — Explorer thumbnails for 313 file types plus
> a rich right-click image toolkit — and folds in XnShell/XnView-style conversion.
> Free for personal use (PolyForm Noncommercial 1.0.0). No personal data, no
> per-user tracking — just an anonymous install count (see the README's Privacy
> section).

---

## 1. Thumbnails in Explorer

SageThumbs draws Explorer thumbnails for file types Windows can't, via a tiered
decoder (`image` crate → Windows WIC → a trimmed bundled ImageMagick → resvg for
SVG), with embedded-cover/first-page extraction for containers.

**313 registered extensions, in six categories** (also how the Options list is
grouped):

| Category | Examples | How |
|---|---|---|
| **Image** (187) | png, jpg, gif, bmp, tiff, webp, heic/heif/**heics/heifs/hif**, avif, psd, **psp/pspimage** (Paint Shop Pro), **iff/ilbm/lbm** (Amiga ILBM), **c4d** (Cinema 4D preview), **cdr/cdt/cmx** (CorelDRAW DISP preview), tga, dds, exr, ico, **icns** (Apple), **jxr/wdp/hdp/wmp** (JPEG XR / HD Photo), jp2/**jpf/jpx**, hdr/**rgbe/xyze**, svg/svgz, **wmf/emf/emz/wmz** (metafiles), **sketch/procreate/skp/3dm/dwg/max/c4d/xd/cdr/cdt** (design/CAD/3D), **blend/.blend1–32** (Blender + auto-saves), **ai** (Illustrator), **eps** (DOS-EPS preview), … | image crate / WIC / ImageMagick / resvg (SVG) |
| **Camera RAW** (34) | cr2/cr3, nef, arw, dng, raf, orf, rw2, pef, x3f, **bay/cap/dcs/drf/ori/ptx/pxn**, … | WIC (Raw Image Extension) / ImageMagick / embedded-JPEG preview |
| **Ebook & comics** (12) | epub, mobi/azw/azw3, **prc** (Mobipocket), fb2/fbz, cbz, cb7, **cbr**, **cbt**, **phz** (zip comic) | native-Rust cover extraction (zip/7z/tar/**rar** via the pure-Rust `rars` crate + hand-parsed MOBI) |
| **Document** (42) | **pdf** (page 1), **djv/djvu** (pure-Rust `djvu-rs` codec), **doc/docx/docm + dot/dotx** (Word), **xls/xlsx/xlsm/xlsb + xlt/xltx** (Excel), **ppt/pptx/pptm + pps/ppsx + pot/potx** (PowerPoint), **odt/ods/odp/odg/…** (OpenDocument), **key/pages/numbers** (Apple iWork), **indd** (InDesign), **vsd/vsdx/vsdm** (Visio), **pub** (Publisher), **ggb** (GeoGebra) | OS `Windows.Data.Pdf` (PDF); pure-Rust `djvu-rs` (DjVu); embedded preview extraction (Office OOXML `docProps/thumbnail` + legacy OLE `\x05SummaryInformation` / iWork / InDesign / Visio / Publisher) |
| **Audio** (16) | mp3, flac, ogg, opus, m4a, wma, ape, wavpack, musepack, wav, aiff | embedded album art via `lofty` — **plus a hand-rolled ASF parser for WMA** (cover art + tags), which `lofty` can't read |
| **Video** (22) | **mkv** (Matroska), **webm**, mp4/m4v, mov, avi, wmv, flv, mpg/mpeg, ts/m2ts/mts, 3gp/3g2, vob, ogv, … | a representative frame via the OS **Media Foundation** codecs (no bundled bytes) — streamed from disk |

*Counts sum to **313** (canonical source: `formats::FORMATS.len()`; `st2k formats` prints
it). DjVu (`.djv/.djvu`) thumbnails are decoded by the **maintained pure-Rust `djvu-rs`
crate** (MIT — no C, no GPL): the page's pre-rendered thumbnail when present, else the
rendered first page (IW44 background + anti-aliased JB2 text + foreground palette),
including multipage shared-dictionary pages. EPS thumbnails come from the
DOS-EPS embedded TIFF preview (no Ghostscript needed); plain-text EPS has no preview to
extract. Photoshop files thumbnail from their embedded preview, but Convert / Resize /
Image info use the REAL full-resolution composite.*

**Notable wins Windows itself can't do:** ebook/comic covers, Office/ODF previews
(no in-box handler), PDF first page (no in-box thumbnailer), Ogg/Opus/APE album
art, any RAW/HEIC the OS codec is missing, and a deep set of **art / CAD / 3D / design
project files** whose baked-in preview we extract directly (no rendering, no
codecs, works even on the ImageMagick-free compact install): **Photoshop**
`.psd/.psb`, **Affinity** `.afphoto/.afdesign/.afpub`, **Clip Studio** `.clip`,
**Krita** `.kra`, **OpenRaster** `.ora`, **Blender** `.blend`, **3MF** `.3mf`,
**FreeCAD** `.fcstd`, and 3D-printer **G-code** `.gcode`. *No Windows tool
thumbnails most of these — PSD now works without ImageMagick, and `.clip`'s preview
is read straight out of its embedded SQLite database (no extra dependency).*

**Per-type toggle:** the Options dialog lists every format with a checkbox; turn
any on/off (multi-select with Shift/Ctrl, then Space or right-click → Check/
Uncheck/Toggle selected).

---

## 2. Right-click image toolkit (`SageThumbs 2K ▸`)

A nested submenu on both the classic and Windows-11 context menus. Appears only
when the selection contains a supported image.

**Multi-file jobs run in parallel.** When the selection has several files, Convert /
Resize / Rotate / Strip and Combine-to-PDF fan out across every CPU core via a tiny
dependency-free scoped thread pool (`src/parallel.rs`) — 6–15× faster than the old
one-at-a-time pass, with no rayon weight added to the in-Explorer DLL. Each worker
initializes COM, which incidentally fixed HEIC/RAW silently failing in the Convert path.

- **Convert into ▸** PNG · JPG · WebP · WebP (lossless) · **AVIF** · BMP · GIF · TIFF ·
  Icon (.ico) — one-click, writes a new file next to the original (never overwrites).
  *HEIC→JPG works here too (a `.heic` decodes via WIC).* *(AVIF is written via the bundled
  ImageMagick; on a compact no-magick install that one verb just no-ops.)*
- **Convert…** (top-level) — opens the **Convert dialog** (XnView-style): an
  **Output format** dropdown — native **JPG · PNG · WebP · BMP · GIF · TIFF · ICO ·
  TGA · QOI · PNM · PDF**, plus (on a full install with the bundled ImageMagick)
  **16 more: PSD · DDS · JP2 · PCX · SGI · EXR · HDR · Farbfeld · PAM · PFM · DPX ·
  FITS · XPM · PICT · RAS · PALM** — a per-format **Settings…** button (JPEG quality ·
  **WebP lossless/lossy + quality** · PNG compression), a **Resize** checkbox with
  presets *or* a custom **W × H**, an output-folder picker, and a progress bar. Batch:
  applies to the whole selection. On completion it offers to **open the output folder**.
  *(The magick-only formats are hidden on the compact install that ships without
  ImageMagick.)*
- **Combine into PDF** — selected images → one PDF (one image per page).
- **Combine into CBZ (comic)** — selected images → one `.cbz` comic archive (a ZIP,
  stored uncompressed), pages natural-sorted by name (page 2 before page 10).
- **Resize ▸** Fit 1920×1080 · 1280×720 · 800×600 · Scale 50% · 25% — quick
  presets that write a "(resized)" copy and never upscale.
- **Shrink for email ▸** Small (640 px) · Medium (1024 px) · Large (1600 px) —
  caps the longest edge and writes a small "(email)" JPEG (q82, flattened onto
  white); never upscales, never touches the original.
- **Rotate / flip ▸** right 90° · left 90° · 180° · flip H · flip V (writes a
  "(edited)" copy, never touches the original). **JPEGs rotate/flip losslessly** —
  the DCT coefficients are rearranged directly (no re-compression, zero quality
  loss), for baseline JPEGs with block-aligned dimensions; other JPEGs and formats
  fall back to a normal re-encode.
- **Rename ▸** By date taken · By camera + date · **By artist - title** ·
  **By track - title** — batch-renames the selection from its metadata: photos from
  EXIF (`YYYY-MM-DD HH.MM.SS`, optional camera prefix), music from audio tags (via
  `lofty`: `Artist - Title`, or zero-padded `NN - Title`). Files missing the needed
  metadata are left untouched; name clashes get a `(2)`, `(3)`… suffix.
- **Files to folder** — create a folder and move the selected file(s) into it
  (works on any file type). One file → a folder named after it (no prompt); several
  → a name-prompt dialog. Always makes a *fresh* folder (never merges into an
  existing one); collisions get a `(n)` suffix.
- **Sort into folders ▸**
  - **By image size** — move each selected image into a `WIDTHxHEIGHT` subfolder of
    its own folder.
  - **By audio tag…** — sort selected music files into folders from their tags. A
    dialog takes a destination, a folder-name **template** (`$artist - $album`,
    tokens `$artist`/`$album`/`$title`/`$track`, `\` to nest), a "missing tag" text,
    and **copy-vs-move**. Tags read via `lofty`.
- **Copy text (OCR)** — extract text from the image to the clipboard.
- **Image info** — a verbose, **copyable** metadata window: file size & type, image
  format, colour depth/channels, dimensions, and **every EXIF tag** the file carries
  (camera, lens, exposure, software, date, GPS with a map link, …) — scrollable, far
  more than a one-line popup.
- **Pick color (eyedropper)…** — a **system-wide screen color picker**: your
  mouse becomes an eyedropper anywhere on screen, with a 10× magnifier loupe that
  follows the cursor; **press Space (or click)** to copy the pixel's `#RRGGBB` to
  the clipboard, Esc to cancel. Picks from anywhere — not just the selected image.
- **Strip metadata (EXIF/GPS)** — lossless removal of EXIF/IPTC/XMP/comments
  (keeps the ICC color profile).

  *(These four were a "Tools ▸" submenu; they're now individual top-level entries —
  show/hide + reorder each like any other menu item.)*
- **Copy to clipboard** — the image as a bitmap.
- **Set as folder icon** — makes the selected image the icon of its containing
  folder (writes a hidden square `.ico` + `desktop.ini`, marks the folder
  customized, and refreshes Explorer — the same mechanism as Explorer's own
  Customize ▸ Change Icon).
- **Set as wallpaper ▸** Stretched · Tiled · Centered.
- **Settings** — opens the Settings window (same as the Start-menu shortcut), so
  settings are reachable straight from the right-click menu.

**Context-menu preview** (the signature SageThumbs/XnShell touch): right-click a
single image and the menu itself shows a small **thumbnail + filename +
dimensions/size** — at the top of the `SageThumbs 2K ▸` submenu by default, or
directly on the main menu, or off (Options → "Menu preview:"). Clicking the
preview opens the image. Works for every format we decode (HEIC, RAW, ebook
covers, …). Transparent images sit on a **subtle checkerboard** (on by default,
toggleable) so see-through areas don't vanish into the menu — it follows the
light/dark menu theme automatically. *Note: appears in the classic menu ("Show
more options" on stock Windows 11) — the modern Win11 menu doesn't allow
custom-drawn items.*

---

## 3. Options dialog

Reached from the Start-menu shortcut (`SageThumbs 2K`). Native Win32,
dark-mode aware, 36 languages. **Resizable taller** — drag the bottom edge to grow
the window and the left options get a bigger scroll viewport (width stays fixed).

- **Thumbnails:** enable thumbnails, prefer embedded (EXIF) thumbnails, enable the
  right-click menu, **show the menu on all file types** (so it's there everywhere — an
  unsupported file gets a condensed file-utility set: Files to folder / Sort into folders
  / Rename / Pick color), menu-preview placement (off / submenu / main menu), **a subtle
  checkerboard behind transparent previews** (on by default), and **show quick actions
  (Convert / Resize / Rotate) directly in the main right-click menu**.
- **Limits & quality:** max file size (MB), max thumbnail size (px), JPEG quality,
  PNG compression.
- **Saving:** **keep the original file's date/time on Convert / Resize / Rotate /
  Shrink output** (opt-in; off = "now", like most tools).
- **Menu items:** an XnShell-style checklist to **show/hide _and reorder_ each
  SageThumbs 2K right-click entry**. Tick/untick to show or hide an item; **drag the rows
  to reorder them**, and **drag the divider rows** to regroup the menu — the right-click
  menu mirrors your arrangement exactly (group dividers included; an accent line shows the
  drop point as you drag, and adjacent/edge dividers tidy themselves). A **Reset order**
  button restores the default. Applies to both the classic and the modern Win11 menus.
- **Ebook & comic covers:** sort archive pages naturally, prefer a "cover" image,
  skip scanlation filler (credits/logos).
- **Screenshots:** enable the capture hotkey (default Ctrl+PrtScn; a plain PrtScn
  preset is offered) for the region editor, **plus an optional second "quick-save"
  hotkey** that grabs the whole screen straight to the clipboard + a timestamped PNG
  with no editor (Off by default). In the editor, **Ctrl+C copies to the clipboard and
  Ctrl+S saves** (Enter copies too). **Save to a set folder on Ctrl+S** (a toggle): when
  on, Ctrl+S auto-saves a timestamped PNG to a folder you pick with **Set save folder…**
  (defaults to your Desktop); when off, Ctrl+S asks where to save each time. **Hide the
  tray icon** (the hotkey still fires), plus a live status line + Restart button. Quitting
  from the tray disables the hotkey for good.
- **Supported file types:** the full per-extension checklist (Extension / Category
  / Description columns), with bulk select and a **Defaults** button that re-ticks the
  recommended set — this resets *only* the file-type list (the whole-dialog factory reset
  is the **Reset all settings** button under Diagnostics).
- **Language:** system default or any of 36 translations.
- **Diagnostics:** a **Verbose logging** toggle and an **Open diagnostics log** button —
  the app writes a rotating log (version/OS header + a panic hook that records any crash
  by `file:line` before `panic = abort` aborts) to `%LOCALAPPDATA%\SageThumbs2K.log`, so a
  bug report can ship a real repro. Plus a **Reset all settings** button that restores
  every option to its factory default.
- **About:** logo, version, a link, a tagline. The main view also carries a small
  author credit and an (optionally remote) promo banner.

---

## 4. Design decisions (the "why")

- **Crash isolation:** runs in Explorer's isolated thumbnail host; `panic=abort` +
  `catch_unwind` at every COM boundary, bounded allocation, checked slicing — bad
  input yields the default icon, never a crash.
- **Use the OS, bundle nothing extra, where possible:** PDF rendering and OCR both
  use in-box WinRT APIs (`Windows.Data.Pdf`, `Windows.Media.Ocr`) — zero added
  bytes. PDF *writing* (Combine-to-PDF) is a hand-rolled minimal `/DCTDecode` PDF —
  no PDF library. HEIC/AVIF decode via WIC.
- **Trimmed ImageMagick** is bundled for the long tail of formats (RAW, DICOM, PCX,
  J2K, …); the installer's "compact" mode omits it, so nothing must-have depends on
  it.
- **Lossless where it matters:** metadata strip rewrites JPEG segments / PNG chunks
  without touching pixels; rotate writes a copy rather than re-compressing in place.
- **Permissive, lean dependencies:** MIT/Apache/BSD only; no GPL/AGPL, **no
  proprietary code**. MOBI is hand-parsed to avoid libmobi's LGPL; RAR/CBR uses the
  pure-Rust `rars` crate (MIT/Apache, `#![forbid(unsafe)]`) instead of the proprietary
  UnRAR C++ (swapped 2026-06-15), so `cargo deny` passes the whole graph with no license
  exceptions. WebP encodes lossless via the pure-Rust `image` crate, or lossy via
  `libwebp` (the one optional bundled C dep, compiled with `cc`).
- **Dark mode** is honored throughout (title bar, controls, list header text,
  combo, owner-drawn menus) using the standard uxtheme app-mode ordinals.
- **Single source of truth** for the menu (a `MenuItem` tree) drives both the
  classic `IContextMenu` and modern `IExplorerCommand` surfaces.

---

## 5. Packaging

- Inno Setup installer (~10.6 MB full, with the trimmed ImageMagick), `full` / `compact` / `custom`.
- Registers the thumbnail provider + context-menu handlers under HKLM (admin);
  cleanly unregisters on uninstall.
- App/installer/shortcut icon embedded from the logo.
- **Pre-1.0 launch checklist:** code signing (avoid SmartScreen), multi-machine
  testing (clean Win10/11, light mode, non-admin, other locales/DPI), README +
  screenshots.

---

## 6. Planned / deferred

- **Convert hub:** AVIF encode (WIC HEIF encoder — codec-availability gating), EPS,
  more exotic targets via the bundled ImageMagick. *(Lossy WebP already shipped — `webp`
  0.3, lossless/lossy + quality in the Convert dialog.)*
- **Quick edits:** lossless JPEG rotate (jpegtran-style, no recompression),
  set-as-lock-screen, contact sheet / montage, copy-as-base64 data URI.
- **DjVu** rendered via the maintained pure-Rust `djvu-rs` crate (replaced our hand-rolled
  ZP/IW44/JB2 stack 2026-06-14) — multipage shared-dictionary (`INCL`→`Djbz`) pages now
  render fully (the old hand-roll degraded them to background-only).

### 6a. AI / agent integration — **CLI + MCP server shipped** (`st2k.exe`)

> Full design record: **[AI_INTEGRATION.md](AI_INTEGRATION.md)**. Phase 1 (the
> `st2k` command-line tool) **and** Phase 2 (`st2k --mcp`, an MCP server) are both
> built + installed. `st2k` verbs: `thumbnail · convert · batch · rotate · strip · ocr ·
> pdf · info [--json] · formats [--json]` — the bundled engine as an offline image
> toolbox for scripts and agents, zero extra installs. The core 8 verbs are also exposed
> as **MCP tools** so an AI client can discover and call them (`batch` is CLI-only).

**Idea:** because SageThumbs already bundles real image
capabilities (313-format decode incl. RAW/HEIC/ebook covers, ImageMagick, WIC, the
WinRT PDF + OCR engines, convert/resize/rotate/strip/PDF), expose those to AI
agents and scripts so users don't need to install a separate toolkit. **Do not
bundle anything new** — only surface existing functions.

**Status:**
1. ✅ **CLI shipped** as a standalone **`st2k.exe`** (console subsystem) — verbs
   `convert`, `rotate`, `strip`, `info` (JSON to stdout), `ocr` (text to stdout), `pdf`
   (combine), `thumbnail` (render any of the 313 types to PNG), **`batch`** (bulk
   thumbnail/convert over many files/folders in ONE process, fanned out across all CPU
   cores), `formats`. All logic lives in the `lib` (`verbs`, `strip`, `ocr`, `topdf`,
   `decode`, `parallel`); the CLI is a thin arg-parser over the same functions the menu
   uses. (Shipped as a separate binary, not a flag on the Options app.)
2. ✅ **MCP server mode** (`st2k --mcp`, stdio JSON-RPC 2.0) — exposes the same 8
   verbs as MCP tools (`tools/list` + `tools/call`) so an agent auto-discovers and
   calls them. Newline-delimited stdio, spawned on demand by the client (not a
   daemon); one small dep (`serde_json`, CLI-only). `src/mcp.rs`. To use: point an
   MCP client at `C:\Program Files\SageThumbs2K\st2k.exe` with arg `--mcp`.
3. **Net effect:** installing SageThumbs gives any local agent a free, offline
   image-processing toolbox — convert, OCR, metadata, PDF, thumbnail — over the
   formats Windows itself can't handle, with zero extra installs.

Constraints honored: reuse existing capabilities only; offline by default; file-writing
verbs require an explicit output argument (`rotate`/`resize` write an `(edited)`/
`(resized)` sibling; `strip` is in-place).
