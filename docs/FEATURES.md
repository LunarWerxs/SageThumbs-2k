# SageThumbs 2K: Features & Decisions Reference

A living, manual-ready catalog of everything SageThumbs 2K does and the key
decisions behind it. Source for the eventual README / user manual / store
listing. (This file is organized by feature area for end-user-facing
documentation.)

> **What it is:** a modern, crash-isolated Windows 11 shell extension (Rust) that
> rebuilds the abandoned SageThumbs (Explorer thumbnails for 316 file types plus
> a rich right-click image toolkit) and folds in XnShell/XnView-style conversion.
> Free for personal use (PolyForm Noncommercial 1.0.0). No personal data, no
> per-user tracking.

---

## 1. Thumbnails in Explorer

SageThumbs draws Explorer thumbnails for file types Windows can't, via a tiered
decoder (`image` crate → Windows WIC → a trimmed bundled ImageMagick → resvg for
SVG), with embedded-cover/first-page extraction for containers.

**316 registered extensions, in six categories** (also how the Options list is
grouped):

| Category | Examples | How |
|---|---|---|
| **Image** (186) | png, jpg, gif, bmp, tiff, webp, heic/heif/**heics/heifs/hif**, avif, psd, **psp/pspimage** (Paint Shop Pro), **iff/ilbm/lbm** (Amiga ILBM), **c4d** (Cinema 4D preview), **cdr/cdt/cmx** (CorelDRAW DISP preview), tga, dds, exr, ico, **icns** (Apple), **jxr/wdp/hdp/wmp** (JPEG XR / HD Photo), jp2/**jpf/jpx**, hdr/**rgbe/xyze**, svg/svgz, **wmf/emf/emz/wmz** (metafiles), **sketch/procreate/skp/3dm/dwg/max/c4d/xd/cdr/cdt** (design/CAD/3D), **blend/.blend1–32** (Blender + auto-saves), **ai** (Illustrator), **eps** (DOS-EPS preview), **f3d** (Autodesk Fusion 360), … | image crate / WIC / ImageMagick / resvg (SVG) |
| **Camera RAW** (34) | cr2/cr3, nef, arw, dng, raf, orf, rw2, pef, x3f, **bay/cap/dcs/drf/ori/ptx/pxn**, … | WIC (Raw Image Extension) / ImageMagick / embedded-JPEG preview |
| **Ebook & comics** (12) | epub, mobi/azw/azw3, **prc** (Mobipocket), fb2/fbz, cbz, cb7, **cbr**, **cbt**, **phz** (zip comic) | native-Rust cover extraction (zip/7z/tar/**rar** via the pure-Rust `rars` crate + hand-parsed MOBI) |
| **Document** (43) | **pdf** (page 1), **djv/djvu** (pure-Rust `djvu-rs` codec), **doc/docx/docm + dot/dotx** (Word), **xls/xlsx/xlsm/xlsb + xlt/xltx** (Excel), **ppt/pptx/pptm + pps/ppsx + pot/potx** (PowerPoint), **odt/ods/odp/odg/…** (OpenDocument), **key/pages/numbers** (Apple iWork), **indd** (InDesign), **vsd/vsdx/vsdm** (Visio), **pub** (Publisher), **ggb** (GeoGebra) | OS `Windows.Data.Pdf` (PDF); pure-Rust `djvu-rs` (DjVu); embedded preview extraction (Office OOXML `docProps/thumbnail` + legacy OLE `\x05SummaryInformation` / iWork / InDesign / Visio / Publisher) |
| **Audio** (18) | mp3, flac, ogg, opus, m4a, wma, ape, wavpack, musepack, **wav, aiff, aiff-c, dsf** (DSD) | embedded album art via `lofty`; **plus a drawn waveform for raw-PCM WAV/AIFF/AIFF-C with no cover art**, and a hand-rolled ASF parser for WMA (cover art + tags) which `lofty` can't read |
| **Video** (22) | **mkv** (Matroska), **webm**, mp4/m4v, mov, avi, wmv, …  | a representative frame (~30 % in, not the intro) via the OS **Media Foundation** codecs (no bundled bytes). MP4/MOV (`moov`) and Matroska/WebM (Cues) parse the container's own index to read just the one keyframe nearest 30 % (single-digit MB); AVI/WMV let MF's demuxer seek over a block-caching stream, never streaming the whole movie. (`.mpg/.mpeg/.flv` need MPEG-1/2 or FLV decoders Windows doesn't ship, so they keep the default icon.) |

*Counts sum to **316** (canonical source: `formats::FORMATS.len()`; `st2k formats` prints
it). DjVu (`.djv/.djvu`) thumbnails are decoded by the **maintained pure-Rust `djvu-rs`
crate** (MIT, no C, no GPL): the page's pre-rendered thumbnail when present, else the
rendered first page (IW44 background + anti-aliased JB2 text + foreground palette),
including multipage shared-dictionary pages. EPS thumbnails come from the
DOS-EPS embedded TIFF preview (no Ghostscript needed); plain-text EPS has no preview to
extract. **Autodesk Fusion 360** `.f3d` thumbnails come from the zstd-compressed preview
inside the archive's ZIP container, inflated by the pure-Rust `ruzstd` decoder (no C dep).
Photoshop files thumbnail from their embedded preview, and a **transparent** PSD
(e.g. a removed background) renders the real layered composite so it keeps its
transparency instead of flattening to white, while Convert / Resize / Image info
always use the REAL full-resolution composite.*

**Notable wins Windows itself can't do:** ebook/comic covers, Office/ODF previews
(no in-box handler), PDF first page (no in-box thumbnailer), Ogg/Opus/APE album
art, any RAW/HEIC the OS codec is missing, and a deep set of **art / CAD / 3D / design
project files** whose baked-in preview we extract directly (no rendering, no
codecs, works even on the ImageMagick-free compact install): **Photoshop**
`.psd/.psb`, **Affinity** `.afphoto/.afdesign/.afpub`, **Clip Studio** `.clip`,
**Krita** `.kra`, **OpenRaster** `.ora`, **Blender** `.blend`, **3MF** `.3mf`,
**FreeCAD** `.fcstd`, **Autodesk Fusion 360** `.f3d`, and 3D-printer **G-code**
`.gcode`. *No Windows tool
thumbnails most of these: PSD now works without ImageMagick, and `.clip`'s preview
is read straight out of its embedded SQLite database (no extra dependency), even for
canvases past the size limit, where only that small tail database is read, never the
multi-hundred-MB layer data.*

**Per-type toggle:** the Options dialog lists every format with a checkbox; turn
any on/off (multi-select with Shift/Ctrl, then Space or right-click → Check/
Uncheck/Toggle selected).

**Details for the formats Windows can't read:** a companion **property handler**
(`IPropertyStore`) surfaces each file's facts in Explorer the same way the built-in
ones do: for the 300+ formats Windows has no native reader for, it fills in image
**dimensions**, **colour depth** and **DPI**, **EXIF camera / date taken / GPS location**,
and for audio the **length, bitrate, artist, album, title, track, genre and year**. These
appear in the **bottom/side Details pane**, the **Properties ▸ Details** tab, the **hover
tooltip**, and as **sortable / groupable columns**; and the columns are *offered* in the
"Choose columns…" picker for those file types, not just reachable by search. (Camera RAW
even gets its GPS location, which Windows itself leaves blank.) It's **read-only** (it never
writes back to your files) and **crash-isolated** behind the same panic boundary as the
thumbnail provider, so a malformed file can't take down Explorer.

**Big preview in the reading pane:** a companion **preview handler** (`IPreviewHandler`)
renders the image LARGE in Explorer's preview/reading pane (and the file-open dialog's
preview) for the same 300+ formats: ebook/comic covers, RAW, HEIC, PSD/`.blend`, audio
album art, a video frame, and the rest. Like the thumbnail, it now handles **large files
without loading them whole**: it grabs a single frame from a multi-gigabyte video, seeks
straight to embedded album art in a long audiobook, or pulls the cover out of an oversized
comic/`.blend`/Photoshop file, so files that used to bog down the preview host or show a
blank pane now preview instantly, and your size limit is respected. It runs in Windows'
**out-of-process preview host** (never inside `explorer.exe`) and is crash-isolated behind
the same panic boundary, so a malformed file yields an empty pane, never a crash.

---

## 2. Right-click image toolkit (`SageThumbs 2K ▸`)

A nested submenu on both the classic and Windows-11 context menus. Appears only
when the selection contains a supported image.

**Multi-file jobs run in parallel.** When the selection has several files, Convert /
Resize / Rotate / Strip and Combine-to-PDF fan out across every CPU core via a tiny
dependency-free scoped thread pool (`src/parallel.rs`), 6–15× faster than the old
one-at-a-time pass, with no rayon weight added to the in-Explorer DLL. Each worker
initializes COM, which incidentally fixed HEIC/RAW silently failing in the Convert path.

- **Convert into ▸** PNG · JPG · WebP · WebP (lossless) · **AVIF** · BMP · GIF · TIFF ·
  Icon (.ico); one-click, writes a new file next to the original (never overwrites).
  *HEIC→JPG works here too (a `.heic` decodes via WIC).* *(AVIF is written via the bundled
  ImageMagick; on a compact no-magick install that one verb reports an error (ImageMagick not
  available) rather than silently doing nothing.)*
- **Convert…** (top-level): opens the **Convert dialog** (XnView-style): an
  **Output format** dropdown, native **JPG · PNG · WebP · BMP · GIF · TIFF · ICO ·
  TGA · QOI · PNM · PDF**, plus (on a full install with the bundled ImageMagick)
  **18 more: AVIF · JPEG XL · PSD · DDS · JP2 · PCX · SGI · EXR · HDR · Farbfeld · PAM · PFM · DPX ·
  FITS · XPM · PICT · RAS · PALM**; a per-format **Settings…** button (JPEG quality ·
  **WebP lossless/lossy + quality** · PNG compression · **AVIF / JPEG XL quality**), a **Resize** checkbox with
  presets *or* a custom **W × H**, an output-folder picker, and a progress bar. Batch:
  applies to the whole selection. On completion it offers to **open the output folder**.
  *(The magick-only formats are hidden on the compact install that ships without
  ImageMagick.)*
- **Combine into PDF**: selected images → one PDF (one image per page).
- **Combine into CBZ (comic)**: selected images → one `.cbz` comic archive (a ZIP,
  stored uncompressed), pages natural-sorted by name (page 2 before page 10).
- **Resize ▸** Fit 1920×1080 · 1280×720 · 800×600 · Scale 50% · 25%: quick
  presets that write a "(resized)" copy and never upscale.
- **Shrink for email ▸** Small (640 px) · Medium (1024 px) · Large (1600 px):
  caps the longest edge and writes a small "(email)" JPEG (q82, flattened onto
  white); never upscales, never touches the original.
- **Rotate / flip ▸** right 90° · left 90° · 180° · flip H · flip V (writes a
  "(edited)" copy, never touches the original). **JPEGs rotate/flip losslessly**:
  the DCT coefficients are rearranged directly (no re-compression, zero quality
  loss), for baseline JPEGs with block-aligned dimensions; other JPEGs and formats
  fall back to a normal re-encode.
- **Rename ▸** By date taken · By camera + date · **By artist - title** ·
  **By track - title**: batch-renames the selection from its metadata: photos from
  EXIF (`YYYY-MM-DD HH.MM.SS`, optional camera prefix), music from audio tags (via
  `lofty`: `Artist - Title`, or zero-padded `NN - Title`). Files missing the needed
  metadata are left untouched; name clashes get a `(2)`, `(3)`… suffix.
- **Files to folder**: create a folder and move the selected file(s) into it
  (works on any file type). One file → a folder named after it (no prompt); several
  → a name-prompt dialog. Always makes a *fresh* folder (never merges into an
  existing one); collisions get a `(n)` suffix.
- **Sort into folders ▸**
  - **By image size**: move each selected image into a `WIDTHxHEIGHT` subfolder of
    its own folder.
  - **By audio tag…**: sort selected music files into folders from their tags. A
    dialog takes a destination, a folder-name **template** (`$artist - $album`,
    tokens `$artist`/`$album`/`$title`/`$track`, `\` to nest), a "missing tag" text,
    and **copy-vs-move**. Tags read via `lofty`.
- **Copy text (OCR)**: extract text from the image to the clipboard.
- **Image info**: a verbose, **copyable** metadata window: file size & type, image
  format, colour depth/channels, dimensions, and **every EXIF tag** the file carries
  (camera, lens, exposure, software, date, GPS with a map link, …), scrollable, far
  more than a one-line popup.
- **Pick color (eyedropper)…**: a **system-wide screen color picker**: your
  mouse becomes an eyedropper anywhere on screen, with a 10× magnifier loupe that
  follows the cursor; **press Space (or click)** to copy the pixel's `#RRGGBB` to
  the clipboard, Esc to cancel. Picks from anywhere, not just the selected image.
  The **screenshot region editor** carries the same eyedropper as a tool (the
  **pipette button** on the toolbar, or the **`E`** key): the magnifier loupe lets
  you sample any pixel of the frozen capture; a click copies its `#RRGGBB`, flashes
  "Copied ✓", and sets it as the annotation colour, so you can grab a colour and Esc
  out, or keep drawing in it.
- **Strip metadata (EXIF/GPS)**: lossless removal of EXIF/IPTC/XMP/comments
  (keeps the ICC color profile).

  *(These four were a "Tools ▸" submenu; they're now individual top-level entries:
  show/hide + reorder each like any other menu item.)*
- **Copy to clipboard**: the image as a bitmap.
- **Upload (copy link)**: uploads the selected image(s) to a keyless, no-account
  host (**catbox.moe** by default; overridable via the `ScreenshotUploadUrl` registry
  value) and copies the resulting link(s) to the clipboard. Multi-select uploads every
  selected image and copies all the links. A small **"Uploading…" indicator** shows
  while the transfer runs, so a multi-second upload never looks like a dead click.
- **Set as folder icon**: makes the selected image the icon of its containing
  folder (writes a hidden square `.ico` + `desktop.ini`, marks the folder
  customized, and refreshes Explorer (the same mechanism as Explorer's own
  Customize ▸ Change Icon).
- **Set as wallpaper ▸** Stretched · Tiled · Centered.
- **Settings**: opens the Settings window (same as the Start-menu shortcut), so
  settings are reachable straight from the right-click menu.

**Context-menu preview** (the signature SageThumbs/XnShell touch): right-click a
single image and the menu itself shows a small **thumbnail + filename +
dimensions/size**, at the top of the `SageThumbs 2K ▸` submenu by default, or
directly on the main menu, or off (Options → "Menu preview:"). Clicking the
preview opens the image. Works for every format the fast in-process tiers cover
(HEIC, RAW, ebook covers, PSD/blend previews, **SVG/SVGZ**, …), but **not** video,
PDF, or the ImageMagick-only long tail (DPX, J2K, PCX, metafiles, …): those tiers are
deliberately skipped inside the menu (a right-click must never freeze Explorer
on a slow render), so such files show a caption-only tile with name + size while
still thumbnailing normally in the Explorer view itself. (SVG is the exception in
that group: resvg is pure-Rust and in-process, so it renders here too, bounded by
the same short menu-preview budget.) Transparent images sit
on a **subtle checkerboard** (on by default,
toggleable) so see-through areas don't vanish into the menu; it follows the
light/dark menu theme automatically. *Note: appears in the classic menu ("Show
more options" on stock Windows 11); the modern Win11 menu doesn't allow
custom-drawn items.*

---

## 3. Quick preview (press Space, see the file)

A QuickLook-style instant previewer, off by default (enable it in **Settings ▸ Quick preview**).
Tap **Space** in Explorer (or on the Desktop) and a borderless, dark/DPI-aware popup shows the
selected file at full size, without stealing focus from Explorer; tap Space again (or Esc) to
close, or hold Space and release to "peek". While it's open, arrow-clicking a different file in
Explorer follows the selection. A selected **`.lnk` shortcut** resolves to its target.

It previews **everything the thumbnailer can decode** (all the image/RAW/ebook/office/… formats),
plus these viewer-only extras:

- **Video and audio playback** via Media Foundation (the OS codecs, zero bundled bytes): the clip
  plays and loops with a transport strip: **play/pause**, an **`m:ss / m:ss`** time readout, a
  **click-and-drag seek bar**, and a **mute + volume slider**. Audio files (mp3/flac/ogg/…) play
  through the same transport.
- **Animated GIF / APNG / animated WebP** play frame-by-frame (respecting each frame's delay).
- **Font specimens** for `.ttf`/`.otf`/`.ttc`: the font's own name, a pangram at several sizes, and
  an A–Z / a–z / 0–9 glyph sheet, all rendered in the font itself (via the OS text stack).
- **Archive listings** for `.zip`/`.7z`/`.rar` (and .jar/.apk/…): a sorted file tree with sizes,
  read from the central directory / headers only — nothing is extracted.
- **Local HTML rendering** (opt-in, off by default) in an embedded **locked-down** WebView2:
  JavaScript disabled and **every non-`file://` request blocked**, so a page cannot phone home or
  load remote trackers. A separate opt-in can live-load a `.url` shortcut's real page in a throwaway
  session; left off, a `.url` shows its target address as text. Adds next to nothing to the app size
  and never touches the shell-extension DLL (the browser engine ships with Windows 11).
- **Rendered Markdown**, GitHub-style: headings, lists, block quotes, fenced code, and inline
  **bold**/*italic*/`code`/~~strike~~ plus **clickable links** (http/https/mailto only) — including
  **bare URLs** typed straight into the text (`https://…` / `www.…`), GitHub-style, without needing
  `[text](url)`. URLs inside code stay literal. README
  "hero" sections written in raw HTML (centered banner + title + tagline + badges) render properly,
  **images stored alongside the file display inline** (GitHub-style sizing, clickable when linked),
  and **tables draw the full grid with shaded alternating rows and auto-fitted columns**. The text
  column is capped and centered like a GitHub page. Web-hosted images (status badges and the like)
  show as labeled chips by default; an optional **"Download web images"** toggle (Settings ▸ Quick
  preview, off by default) fetches and displays them (HTTPS only, size-capped, in the background).
  Markdown with headings also gets a **collapsible outline sidebar** (a "Contents" panel): a
  clickable, indented list of the headings that jumps to a section on click (and selects it even
  when the page is already at the bottom) and highlights the one you are reading as you scroll.
  The panel slides open/closed via the outline button on the toolbar; the choice is remembered.
- **CSV/TSV column view**: spreadsheets-ish files render as a real gridded table (quoted fields,
  embedded commas/newlines, `;`-separated exports auto-detected), capped with a note for huge files.
- **Jupyter notebooks** (`.ipynb`): markdown cells render, code cells show syntax-highlighted with
  line numbers in the notebook's language, and text outputs (stream, results, cleaned error
  tracebacks) display beneath their cells.
- **Syntax-highlighted** text/code with **line numbers**, editor-style, for the common languages
  (Rust, Python, JS/TS, JSON, Java, Go, C/C++, C#, Ruby, PHP, Lua, Kotlin, Swift, shell, HTML/CSS,
  SQL, YAML/TOML/ini and more — comments, strings, numbers, keywords, and JSON/object keys in their
  own colour), a small pure-Rust lexer, in both standalone code files and Markdown code fences.
- **View source**: anything that RENDERS can be flipped to its raw text and back — a Markdown file,
  a CSV/TSV table, a Jupyter notebook, a rendered HTML page, an SVG. Hit the toolbar's **`{ }`**
  button or press **Ctrl+U** and you get the underlying file, syntax-highlighted with line numbers,
  fully selectable and copyable; press it again to go back. The mode sticks while the window is
  open, so **←/→** keeps showing source as you flip through a folder, and a fresh preview always
  opens rendered. The button only appears on files that actually have both views.
- **Multi-page PDF navigation**: PageUp/PageDown or the arrow keys (or on-screen ◀ ▶ buttons) page
  through the document, with a "current / total" indicator in the title bar.
- **Zoom + pan** on images (wheel to zoom at the cursor, drag to pan, double-click to toggle
  fit/100 %), and wheel-scroll for long text/Markdown.
- **Select & copy**: drag-select in text/code/log **and rendered Markdown** previews
  (double-click selects a word; **Shift+arrows**, Shift+Home/End/PgUp/PgDn and the Ctrl
  word-wise variants select from the keyboard; **Ctrl+A** selects all), then **Ctrl+C** copies
  the selection — or, with nothing selected, the whole document. Markdown copies as the text you
  SEE (tables come out tab-separated, so they paste into a spreadsheet as columns); **Ctrl+Shift+C**
  copies the raw Markdown source instead. Ctrl+C elsewhere copies the content too: the image
  itself (the exact PDF page / animation frame being shown), or the info card's text. The
  toolbar's copy button copies the file's path instead. Plain **Home/End** jump to the
  top/bottom of a text or Markdown document.
- **Folder browsing + full-screen**: **←/→** (or PgUp/PgDn) flip through the current folder's
  previewable files without closing the popup, QuickLook-style; **F11** toggles borderless
  full-screen (Esc restores).
- A slim caption **toolbar** (Segoe Fluent icons): keep-on-top, copy path, file info, upload &
  copy link, open with…, open, close.
- A calm **info card** (icon, name, size, date) for unsupported files or folders, never an error.

The viewer is a separate single-instance process, so a hostile-file decode can only take down a
throwaway window, never Explorer or the hotkey helper. It rides the same opt-in background helper
as the screenshot/custom-action hotkeys, so turning it on adds no extra resident process.

---

## 4. Options dialog

Reached from the Start-menu shortcut (`SageThumbs 2K`); the window is titled
**Settings**. Native Win32, dark-mode aware, 36 languages. **Redesigned in 0.7.0**: a
Windows 11-style **category rail** (General · File types · Ebook/comic · Right-click menu ·
Screenshots · Quick action · Quick preview · Advanced · Data & Backup) on the left with a content page on the right:
**toggle switches**, category icons, and a titled header per page. Everyday knobs up front;
Diagnostics / Updates / Backup tuck under **Advanced**. (Fixed-size window; the old single
long scroll is gone.)

- **Thumbnails:** enable thumbnails, prefer embedded (EXIF) thumbnails, enable the
  right-click menu, **show the menu on all file types** (so it's there everywhere; an
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
  to reorder them**, and **drag the divider rows** to regroup the menu; the right-click
  menu mirrors your arrangement exactly (group dividers included; an accent line shows the
  drop point as you drag, and adjacent/edge dividers tidy themselves). A **Reset order**
  button restores the default. Applies to both the classic and the modern Win11 menus.
- **Ebook & comic covers:** sort archive pages naturally, prefer a "cover" image,
  skip scanlation filler (credits/logos).
- **Screenshots:** enable the capture hotkey (default Ctrl+PrtScn; a plain PrtScn
  preset is offered) for the region editor, **plus an optional second "quick-save"
  hotkey** that grabs the whole screen straight to the clipboard + a timestamped PNG
  with no editor (Off by default); a **split-second screen flash confirms the capture**
  (like Win+Shift+S), and if the copy or the save failed, a small notification says
  exactly what went wrong instead of silence. As you drag out the region, a live **`width × height`
  pixel readout** follows the selection so you can size a capture precisely. In the editor,
  **Ctrl+C copies to the clipboard and Ctrl+S saves** (Enter copies too). **Save to a set
  folder on Ctrl+S** (a toggle): when on, Ctrl+S auto-saves a timestamped PNG to a folder
  you pick with **Set save folder…** (defaults to your Desktop); when off, Ctrl+S asks where
  to save each time. **Custom action hotkey:** assign ONE global
  hotkey to any of a curated set of actions: **pick a color** (the screen color picker),
  take a screenshot, Convert…, rotate right, move files into a new folder, strip metadata,
  **upload (copy link)**, **copy text (OCR)** straight to the clipboard, or open
  Settings. Screen-wide actions run instantly; the file actions operate on the
  current **Explorer selection** (or prompt with a file picker when nothing is selected).
  It rides the same opt-in helper, so binding one needs no extra background process.
- **Quick preview:** enable the Space-bar previewer (see §3) and tune it: **hold-Space
  peek**, **close when it loses focus**, **open in front** (bring the preview to the
  foreground when it opens, without pinning it always-on-top), and whether to preview
  **text/code** files, **Markdown** files, and **local HTML** pages. Off by default;
  enabling it starts the same opt-in background helper the hotkeys use.
- **Supported file types:** the full per-extension checklist (Extension / Category
  / Description columns), with bulk select and a **Defaults** button that re-ticks the
  recommended set; this resets *only* the file-type list (the whole-dialog factory reset
  is the **Reset all settings** button under Diagnostics).
- **Language:** system default or any of 36 translations.
- **Diagnostics:** a **Verbose logging** toggle and an **Open diagnostics log** button:
  the app writes a rotating log (version/OS header + a panic hook that records any crash
  by `file:line` before `panic = abort` aborts) to `%LOCALAPPDATA%\SageThumbs2K.log`, so a
  bug report can ship a real repro. A **Repair file associations** button re-registers
  SageThumbs for every enabled format when another app has hijacked the thumbnails, then
  clears the thumbnail cache so the fixed types redraw. Plus a **Reset all settings**
  button that restores every option to its factory default.
- **Hotkey service:** a live status line (Running / Stopped / Off) and a **Restart** button
  for the small background helper that powers the screenshot & custom-action hotkeys, plus
  the **Hide tray icon** toggle (the hotkeys still fire when it's hidden). The service
  **restarts itself automatically** if it ever stops, and simply opening Settings brings it
  back if it was down. The hotkeys also **survive the things Windows silently breaks them
  with**: sleep/resume, locking your PC, remote-desktop reconnects, Explorer restarts
  (the tray icon comes back too), and app updates all re-register them automatically. If
  **another app owns your chosen chord**, the status line says so ("hotkey in use by
  another app") instead of pretending everything works, and it clears itself within a
  minute of that app letting go. Quitting from the tray icon disables it for good (until
  you re-enable it here).
- **Updates:** a **Check for updates** button (plus an *Automatically check for updates*
  toggle) asks GitHub whether a newer release exists. When one is available, SageThumbs can
  **download and install it for you**: a progress bar shows the download, the file is
  integrity-checked, Windows asks once for permission, and the new version installs in the
  background and confirms with a quiet tray notification when it's done. You can still grab
  the installer from the releases page by hand if you prefer.
- **About:** a compact card: the eye logo, a **version pill that links to the GitHub
  repo** (with the GitHub mark), and a live **"Up to date" / update-available pill** that
  re-checks on click, plus the licence, copyright, and the clickable LunarWerx Studios
  wordmark. Opens **centered over the Settings window**. The Settings main view also carries
  a small author credit and a promo banner.

---

## 5. Design decisions (the "why")

- **Crash isolation:** runs in Explorer's isolated thumbnail host; `panic=abort` +
  `catch_unwind` at every COM boundary, bounded allocation, checked slicing; bad
  input yields the default icon, never a crash.
- **Use the OS, bundle nothing extra, where possible:** PDF rendering and OCR both
  use in-box WinRT APIs (`Windows.Data.Pdf`, `Windows.Media.Ocr`); zero added
  bytes. PDF *writing* (Combine-to-PDF) is a hand-rolled minimal `/DCTDecode` PDF;
  no PDF library. HEIC/AVIF decode via WIC: that means the OS codecs: on machines
  where they aren't preinstalled, install Microsoft's free **HEIF Image Extensions**
  (+ **HEVC Video Extensions** for iPhone HEIC) and **AV1 Video Extension** from the
  Store. There is deliberately NO bundled fallback for these two: the trimmed
  ImageMagick drops its HEIF stack (a 7 MB delegate DLL; decided 2026-07-08, size
  over edge-case coverage; revisit only if users actually report missing HEIC
  thumbnails on codec-less machines).
- **Trimmed ImageMagick** is bundled for the long tail of formats (RAW, DICOM, PCX,
  J2K, …); the installer's "compact" mode omits it, so nothing must-have depends on
  it. The measured magick-only set (things that DON'T thumbnail on a compact
  install): the JPEG-2000 family (j2c/j2k/jp2/jpc/jpf/jpm/jpx), film/print scans
  (cin/dpx/cal/cals/fits/fts/pcd), Windows metafiles (wmf/emf/emz/wmz), Visio
  (vsd/vsdx/vsdm), legacy-Office OLE previews (max), classic bitmaps
  (pcx/dcx/dib/ras/sun/sgi/xbm/xpm/xv/wpg/pdb), scientific floats
  (pfm/phm/fl32/mat/vicar/viff/vips/pgx/ph), miff/mng/tiff64, and **DWAA/DWAB-
  compressed OpenEXR** (uncompressed/ZIP/PIZ/RLE/B44 EXR decode pure-Rust; the
  DWA lossy codecs need the bundled magick (the standard install has it).
- **Colour-managed thumbnails:** images carrying an embedded ICC profile or a
  wide-gamut tag (Display P3 / Adobe RGB) are converted into sRGB before display, so
  they no longer look over-saturated next to ordinary photos. AVIF/HEIC read their
  profile from the ISOBMFF `colr` box, including the CICP `nclx` Display-P3 signal
  iPhone HEIC uses, and CMYK JPEGs are converted through their embedded CMYK profile.
  All pure-Rust (`zune-jpeg` for raw CMYK + `moxcms` for the transform), no C
  colour-engine dependency.
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

## 6. Packaging

- Inno Setup installer (`full` / `compact` / `custom`). The full build stays lean because the
  trimmed ImageMagick's text-shaping stack (glib / harfbuzz / freetype / fribidi / raqm) is
  stubbed out; we only ever decode raster, never render text.
- Registers the thumbnail provider + context-menu handlers under HKLM (admin);
  cleanly unregisters on uninstall.
- App/installer/shortcut icon embedded from the logo.
- **Pre-1.0 launch checklist:** code signing (avoid SmartScreen), multi-machine
  testing (clean Win10/11, light mode, non-admin, other locales/DPI), README +
  screenshots.

---

## 7. Planned / deferred

- **Convert hub:** EPS encode + more exotic targets via the bundled ImageMagick. *(AVIF and
  JPEG XL encode already SHIP via the bundled ImageMagick, with a quality slider in the Convert
  dialog; lossless/lossy WebP shipped too. A native WIC-backed HEIF/AVIF encoder is a possible
  future optimization to shrink the ImageMagick dependency for those targets.)*
- **Quick edits:** set-as-lock-screen, contact sheet / montage, copy-as-base64 data URI.
  *(Lossless JPEG rotate (jpegtran-style, no recompression) already shipped, see §2.)*

*(**DjVu** and the lossless-JPEG-rotate bullets used to live here as "planned"; both shipped
(DjVu via the pure-Rust `djvu-rs` crate, see §1) and were moved out so this list only shows
genuinely-outstanding work.)*

### 7a. AI / agent integration: **CLI + MCP server shipped** (`st2k.exe`)

> Phase 1 (the
> `st2k` command-line tool) **and** Phase 2 (`st2k --mcp`, an MCP server) are both
> built + installed. `st2k` verbs: `thumbnail · convert · batch · rotate · strip · ocr ·
> pdf · info [--json] · formats [--json]`: the bundled engine as an offline image
> toolbox for scripts and agents, zero extra installs. **Ten tools** are exposed over
> **MCP** so an AI client can discover and call them: the core verbs plus two agent-first
> tools, **`view`** (decode any file to a PNG image block the agent can actually *see*) and
> **`compress`** (`batch` is CLI-only).

**Idea:** because SageThumbs already bundles real image
capabilities (316-format decode incl. RAW/HEIC/ebook covers, ImageMagick, WIC, the
WinRT PDF + OCR engines, convert/resize/rotate/strip/PDF), expose those to AI
agents and scripts so users don't need to install a separate toolkit. **Do not
bundle anything new**; only surface existing functions.

**Status:**
1. ✅ **CLI shipped** as a standalone **`st2k.exe`** (console subsystem): verbs
   `convert`, `rotate`, `strip`, `info` (JSON to stdout), `ocr` (text to stdout), `pdf`
   (combine), `thumbnail` (render any of the 316 types to PNG), **`batch`** (bulk
   thumbnail/convert over many files/folders in ONE process, fanned out across all CPU
   cores), `formats`. All logic lives in the `lib` (`verbs`, `strip`, `ocr`, `topdf`,
   `decode`, `parallel`); the CLI is a thin arg-parser over the same functions the menu
   uses. (Shipped as a separate binary, not a flag on the Options app.)
2. ✅ **MCP server mode** (`st2k --mcp`, stdio JSON-RPC 2.0): exposes **10** MCP
   tools (`tools/list` + `tools/call`) so an agent auto-discovers and calls them: the
   core verbs plus **`view`** (which decodes any of the 316 formats to a PNG **image
   block**) so an AI agent can actually *see* the file, and **`compress`**. Newline-
   delimited stdio, spawned on demand by the client (not a daemon); one small dep
   (`serde_json`, CLI-only). `src/mcp.rs`. To use: point an MCP client at
   `C:\Program Files\SageThumbs2K\st2k.exe` with arg `--mcp`.
3. **Net effect:** installing SageThumbs gives any local agent a free, offline
   image-processing toolbox: convert, OCR, metadata, PDF, thumbnail, over the
   formats Windows itself can't handle, with zero extra installs.

Constraints honored: reuse existing capabilities only; offline by default; file-writing
verbs require an explicit output argument (`rotate`/`resize` write an `(edited)`/
`(resized)` sibling; `strip` is in-place).

### 7b. Settings sync: optional Connections account (opt-in)

> **New.** In **Settings ▸ Data & Backup**, a **"Sync settings…"** button lets you sign in
> with a Connections account and sync your SageThumbs preferences across your PCs. It's
> **opt-in and off by default**; no network happens unless you click it.

Sign-in opens your real browser (standard OAuth 2.0 + PKCE; SageThumbs never sees your
password), and from then on your portable settings (thumbnail limits/quality, menu layout
and toggles, hotkeys, language, container preferences) follow you to any machine you sign
into. Only an explicit **allowlist** of portable preferences syncs; **never** file paths,
secrets, or per-machine state, and never your images. Your settings always stay on your PC
too; the cloud is only a sync layer, so the app works fully offline / signed out; disconnect
anytime (it removes the cloud copy). **The shell-extension DLL never touches the network**:
all sign-in/sync code lives in the Settings app only, preserving the crash-isolation guarantee.
The refresh token is stored encrypted (Windows DPAPI); the store is a settings locker (≤64 KB,
no secrets).
