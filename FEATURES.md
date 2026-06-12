# SageThumbs 2K — Features & Decisions Reference

A living, manual-ready catalog of everything SageThumbs 2K does and the key
decisions behind it. Source for the eventual README / user manual / store
listing. (This file is organized by feature area for end-user-facing
documentation.)

> **What it is:** a modern, crash-isolated Windows 11 shell extension (Rust) that
> rebuilds the abandoned SageThumbs — Explorer thumbnails for ~179 file types plus
> a rich right-click image toolkit — and folds in XnShell/XnView-style conversion.
> Free, MIT OR Apache-2.0, no telemetry, no bundled adware.

---

## 1. Thumbnails in Explorer

SageThumbs draws Explorer thumbnails for file types Windows can't, via a tiered
decoder (`image` crate → Windows WIC → a trimmed bundled ImageMagick → resvg for
SVG), with embedded-cover/first-page extraction for containers.

**179 registered extensions, in five categories** (also how the Options list is
grouped):

| Category | Examples | How |
|---|---|---|
| **Image** (112) | png, jpg, gif, bmp, tiff, webp, heic/heif, avif, psd, tga, dds, exr, ico, svg, **ai** (Illustrator), **eps** (DOS-EPS preview), … | image crate / WIC / ImageMagick / resvg (SVG) |
| **Camera RAW** (27) | cr2/cr3, nef, arw, dng, raf, orf, rw2, pef, x3f, … | WIC (Raw Image Extension) / ImageMagick |
| **Ebook & comics** (10) | epub, mobi/azw/azw3, fb2/fbz, cbz, cb7, **cbr** (opt-in `rar` feature), **cbt** | native-Rust cover extraction (zip/7z/tar + hand-parsed MOBI) |
| **Document** (14) | **pdf** (page 1), **djv/djvu** (hand-rolled IW44 decoder), **odt/ods/odp/odg/…** (OpenDocument), **pptx/pptm/potx** (PowerPoint) | OS `Windows.Data.Pdf` (PDF); native IW44 wavelet decode (DjVu); embedded preview extraction (Office) |
| **Audio** (16) | mp3, flac, ogg, opus, m4a, wma, ape, wavpack, musepack, wav, aiff | embedded album art via `lofty` |

*Counts sum to **179** (canonical source: `formats::FORMATS.len()`; `st2k formats` prints
it). DjVu (`.djv/.djvu`) thumbnails are decoded by a **fully hand-rolled decoder**
(ZP arithmetic coder + IW44 wavelet background + JB2 bilevel text mask, composited
with the foreground palette — no GPL code): the page's pre-rendered thumbnail when
present, else background + anti-aliased text. Pure-bilevel pages render their text
mask on white. (Masks needing a shared multipage dictionary degrade to
background-only.) EPS thumbnails come from the
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

- **Convert into ▸** PNG · JPG · WebP · BMP · GIF · TIFF · Icon (.ico) — one-click,
  writes a new file next to the original (never overwrites). *HEIC→JPG works here
  too (a `.heic` decodes via WIC).*
- **Convert…** (top-level) — opens the **Convert dialog** (XnView-style): an
  **Output format** dropdown — native **JPG · PNG · WebP · BMP · GIF · TIFF · ICO ·
  TGA · QOI · PNM · PDF**, plus (on a full install with the bundled ImageMagick)
  **16 more: PSD · DDS · JP2 · PCX · SGI · EXR · HDR · Farbfeld · PAM · PFM · DPX ·
  FITS · XPM · PICT · RAS · PALM** — a per-format **Settings…** button (JPEG quality ·
  **WebP lossless/lossy + quality** · PNG compression), a **Resize** checkbox with
  presets *or* a custom **W × H**, an output-folder picker, and a progress bar. Batch:
  applies to the whole selection. *(The magick-only formats are hidden on the compact
  install that ships without ImageMagick.)*
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
- **Tools ▸**
  - **Copy text (OCR)** — extract text from the image to the clipboard.
  - **Image info** — dimensions, file size, camera make/model, date taken, GPS.
  - **Pick color (eyedropper)…** — a **system-wide screen color picker**: your
    mouse becomes an eyedropper anywhere on screen, with a 10× magnifier loupe that
    follows the cursor; **press Space (or click)** to copy the pixel's `#RRGGBB` to
    the clipboard, Esc to cancel. Picks from anywhere — not just the selected image.
  - **Strip metadata (EXIF/GPS)** — lossless removal of EXIF/IPTC/XMP/comments
    (keeps the ICC color profile).
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
covers, …). *Note: appears in the classic menu ("Show more options" on stock
Windows 11) — the modern Win11 menu doesn't allow custom-drawn items.*

---

## 3. Options dialog

Reached from the Start-menu shortcut (`SageThumbs 2K Options`). Native Win32,
dark-mode aware, 36 languages.

- **Thumbnails:** enable thumbnails, prefer embedded (EXIF) thumbnails, enable the
  right-click menu, menu-preview placement (off / submenu / main menu), and **show
  quick actions (Convert / Resize / Rotate) directly in the main right-click menu**.
- **Limits & quality:** max file size (MB), max thumbnail size (px), JPEG quality,
  PNG compression.
- **Ebook & comic covers:** sort archive pages naturally, prefer a "cover" image,
  skip scanlation filler (credits/logos).
- **Supported file types:** the full per-extension checklist (Extension / Category
  / Description columns), with bulk select.
- **Language:** system default or any of 28 translations.
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
- **Permissive, lean dependencies:** MIT/Apache/BSD only; no GPL/AGPL. MOBI is
  hand-parsed to avoid libmobi's LGPL. WebP encodes lossless via the pure-Rust
  `image` crate, or lossy via `libwebp` (the one bundled C dep, compiled with `cc`).
- **Dark mode** is honored throughout (title bar, controls, list header text,
  combo, owner-drawn menus) using the standard uxtheme app-mode ordinals.
- **Single source of truth** for the menu (a `MenuItem` tree) drives both the
  classic `IContextMenu` and modern `IExplorerCommand` surfaces.

---

## 5. Packaging

- Inno Setup installer (~9.8 MB full, with the trimmed ImageMagick), `full` / `compact` / `custom`.
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
- **DjVu shared-dictionary masks** (IW44 + JB2 + compositing all shipped 2026-06-11;
  remaining edge: multipage documents whose text mask references a shared `Djbz`
  dictionary via `INCL` render background-only — needs DIRM/INCL component resolution).

### 6a. AI / agent integration — **CLI + MCP server shipped** (`st2k.exe`)

> Full design record: **[AI_INTEGRATION.md](AI_INTEGRATION.md)**. Phase 1 (the
> `st2k` command-line tool) **and** Phase 2 (`st2k --mcp`, an MCP server) are both
> built + installed. `st2k` verbs: `thumbnail · convert · rotate · strip · ocr · pdf ·
> info [--json] · formats [--json]` — the bundled engine as an offline image
> toolbox for scripts and agents, zero extra installs. The same 8 verbs are exposed
> as **MCP tools** so an AI client can discover and call them.

**Idea:** because SageThumbs already bundles real image
capabilities (179-format decode incl. RAW/HEIC/ebook covers, ImageMagick, WIC, the
WinRT PDF + OCR engines, convert/resize/rotate/strip/PDF), expose those to AI
agents and scripts so users don't need to install a separate toolkit. **Do not
bundle anything new** — only surface existing functions.

**Status:**
1. ✅ **CLI shipped** as a standalone **`st2k.exe`** (console subsystem) — verbs
   `convert`, `rotate`, `strip`, `info` (JSON to stdout), `ocr` (text to stdout), `pdf`
   (combine), `thumbnail` (render any of the 179 types to PNG), `formats`. All logic
   lives in the `lib` (`verbs`, `strip`, `ocr`, `topdf`, `decode`); the CLI is a thin
   arg-parser over the same functions the menu uses. (Shipped as a separate binary, not
   a flag on the Options app.)
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
