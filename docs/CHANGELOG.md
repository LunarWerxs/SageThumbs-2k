# Changelog

All notable user-facing changes to **SageThumbs 2K**. Newest first.

## 0.8.0

- **The preview pane now handles big files like thumbnails do.** Explorer's reading/preview pane
  used to read a file whole before showing it — so a multi-gigabyte video, a long audiobook, or an
  oversized comic/`.blend`/Photoshop file would either bog down the preview host or just show a
  blank pane, even though its thumbnail worked fine. The pane now uses the exact same shortcuts the
  thumbnail does: it grabs a single video frame, seeks straight to embedded album art, or pulls the
  cover out of a huge archive without ever loading the whole file, and it respects your size limit.
- **Big Blender files finally show thumbnails.** `.blend` scenes over the size limit (100 MB by
  default) were silently skipped by Explorer even though the thumbnail sits in the first few
  kilobytes of the file — now we read just that small head slice, so a 2 GB scene thumbnails
  instantly. Same fix for huge Photoshop `.psd`/`.psb` files. (Thanks to GitHub issue #1.)
- **Compressed `.blend` files now show thumbnails at all.** Files saved with Blender's
  "Compress" option (gzip or zstd) previously never got a preview — now they do, at any size.
- **Big Clip Studio Paint canvases show thumbnails too.** A multi-layer `.clip` over the size
  limit was skipped even though its preview lives in a small database at the end of the file —
  we now jump straight to that database and read only it, so a 2 GB manga page thumbnails
  instantly. Works in the preview pane and the `st2k` CLI as well.
- **See-through EXR render passes show their content.** An OpenEXR whose alpha channel is
  entirely empty (emission/AOV/environment passes) used to show a blank default icon; it now
  renders its actual colors. Note: **DWAA/DWAB-compressed EXR needs the standard install** (the
  bundled ImageMagick decodes it); uncompressed/ZIP/PIZ/B44 EXR work everywhere. (GitHub issue #2.)
- **Old 32-bit TGA files no longer come out invisible.** Files whose header declares "no alpha"
  but still carry a (meaningless, all-zero) 4th channel used to decode fully transparent — in
  thumbnails, Convert, and the AI `view` tool. They now render opaque, as every image viewer does.
- **Apple `.icns` icons actually work now.** They were listed as supported but no decoder ever
  handled them; the embedded PNG (or JPEG-2000) icon is now extracted directly.
- **Huge Krita/OpenRaster/3MF/FreeCAD files show the right preview.** Oversized project files
  used to get an arbitrary internal layer image as their thumbnail (often blank); they now get
  the real composite preview, same as small ones.
- **Amiga IFF/ILBM images with a transparency mask render it correctly** (masked areas used to
  come out opaque).
- **DICOM `.dcm` medical scans finally thumbnail.** They were listed as supported but never
  actually rendered — the file's TIFF-like header fooled the decoder into treating it as a broken
  TIFF. CT/MR slices now show real, legible anatomy (the low-contrast medical data is auto-stretched
  so it isn't just a flat gray square).
- **`.jbig` removed from the supported-formats list.** It never actually decoded (no shipped
  decoder can read it) — the entry only cost a doomed 20-second attempt per file.
- **Release-readiness polish.** Licensing, the supported-format counts quoted across the docs, and
  some duplicated code were reconciled so everything lines up; file renames during Convert/Resize
  are now atomic (no half-written output if something goes wrong mid-write).
- **Sync your settings across your PCs — new, and completely optional.** Settings has a new
  **Data & Backup** section with a **"Sync settings…"** button: sign in with a Connections account
  (it opens your real browser — SageThumbs never sees your password) and your portable preferences
  follow you to every machine you sign into — thumbnail limits and quality, the right-click menu layout
  and toggles, hotkeys, language, and ebook/comic options. It's **off by default** (no network happens
  unless you turn it on), only an allowlist of portable settings syncs — **never your files, folder
  paths, or passwords, and never your images** — and your settings always stay on your PC too, so
  everything keeps working fully offline or signed out. Disconnect anytime and the cloud copy is
  removed. As always, the thumbnail shell extension itself never touches the network — all sign-in and
  sync code lives in the Settings app only.
- **Your sign-in is stored securely.** The token that keeps you signed in is encrypted on your machine
  with Windows' own DPAPI (only your account, on that PC, can read it) and is never part of the synced
  data — the cloud copy is a plain "settings locker," no secrets.

> Upgrading from 0.7.1? This release also rolls in everything under **0.7.2** below (hotkey resilience,
> capture/upload feedback, and the CLI fixes).

## 0.7.2

- **Hotkeys now survive sleep, lock, and updates.** Windows silently un-registers global hotkeys
  after sleep/resume, locking your PC, or a remote-desktop reconnect — the background helper now
  re-registers them the moment those happen (plus a once-a-minute safety net), so your screenshot
  hotkey keeps working instead of quietly dying until you reopened the app. App updates and
  reinstalls also restart the helper automatically — previously an update silently killed your
  hotkeys until the next sign-in.
- **The tray icon survives Explorer restarts.** When Windows Explorer crashes or restarts it wipes
  all tray icons; the helper now puts its icon back automatically (and retries at sign-in if the
  taskbar isn't ready yet).
- **Copying a screenshot is more reliable.** If another app was momentarily holding the clipboard
  (clipboard managers and Office do this constantly), your capture's copy could silently do
  nothing — it now retries briefly instead of giving up.
- **One capture at a time.** Pressing the screenshot hotkey twice no longer stacks a second frozen
  overlay on top of the first.
- **Bind OCR to your custom hotkey.** "Copy text (OCR)" joins the Quick Action list — press your
  hotkey over the selected image(s) and the recognized text lands straight on the clipboard.
- **"Sort into folders ▸ By image size" is much faster on big selections.** Reading each file's
  dimensions now runs in parallel like the other batch actions (exotic RAW/HEIC files used to be
  probed one at a time).
- **CLI: `st2k batch` now fails properly.** It exits with an error when every file failed
  (partial runs report how many failed) so scripts and automations can detect it. Also `st2k pdf`
  now honors your configured JPEG quality instead of a fixed 85, and OCR errors say what actually
  went wrong.
- **The quick-save hotkey now shows it worked.** A split-second screen flash confirms the capture
  (like Win+Shift+S); if the copy or the PNG save failed, a small notification tells you exactly
  what went wrong instead of total silence.
- **See when a hotkey is taken by another app.** If some other program owns your chosen chord,
  Settings ▸ Screenshots now says so ("hotkey in use by another app") instead of claiming
  everything is running — and it clears itself within a minute of the other app letting go.
- **Uploads show an "Uploading…" indicator.** Both the screenshot Upload button and right-click ▸
  Upload now show a small progress pill while the transfer runs — no more staring at nothing
  wondering if the click registered.

## 0.7.1

- **See a screenshot's exact size while you drag it.** When you drag out a region to capture, a small
  `width × height` readout now sits at the corner of the selection, so you can size things precisely (in
  real pixels).
- **The screenshot / action hotkeys stay working.** The small background helper that powers the global
  hotkeys now restarts itself automatically if it ever stops, and just opening Settings brings it back if
  it was down — so your hotkey won't quietly stop firing. Its live status shows under Settings ▸ Advanced
  ▸ "Hotkey service".
- **New: right-click ▸ Upload (copy link).** Right-click an image (or several) and upload straight to a
  free, no-account host (catbox.moe by default), with the link(s) copied to your clipboard — your original
  files are left untouched. The resulting links open in a small window you can select and copy from.
- **About box opens centered, with the proper GitHub icon.** The About window now appears centered over
  Settings instead of stuck in the top-left corner, and its GitHub badge shows the real GitHub logo.
- **"Hide tray icon" moved to Advanced.** That toggle now lives under Settings ▸ Advanced ▸ "Hotkey
  service", next to the Restart button.

## 0.7.0

- **Redesigned Settings window.** The old single long scroll is gone — Settings now opens with a
  Windows 11-style **category rail** down the left (General · File types · Ebook/comic · Right-click
  menu · Screenshots · Quick action · Advanced) and a clean content page on the right, with on/off
  **toggle switches**, category icons, and a titled header per page. Everyday options sit up front;
  diagnostics, updates and backup tuck under **Advanced**. Same settings, far less clutter. (The new
  labels are translatable; languages without the new strings yet fall back to English.)
- **Assign your own hotkey to a tool.** Pick an action (color picker, take a screenshot, Convert…,
  rotate, move-to-folder, strip metadata, or open Settings) and a keyboard shortcut, and that shortcut
  now works anywhere. The file actions run on whatever you've got selected in Explorer — or pop a file
  picker if nothing's selected. It reuses the existing screenshot helper, so there's no extra
  background program.
- **Cleaner right-click menu on music files.** Right-clicking an audio file (MP3, FLAC, …) no longer
  shows image-only actions like Resize, Rotate, or Set as wallpaper — just the ones that make sense
  (move to folder, rename by tag, sort by tag).
- **AVIF / JPEG XL quality slider.** The Convert… dialog now lets you set the quality for AVIF and
  JPEG XL output (it only had this for JPEG and WebP before).
- **+1 format: DSD audio (`.dsf`).** Album-art thumbnails for DSD audio files — now **316** supported
  file types.
- **Fixed: Photoshop files with a transparent background now preview correctly.** If you removed the
  background in Photoshop and saved, the thumbnail used to show a solid **white** background — because it
  came from Photoshop's built-in preview image, which can't store transparency. SageThumbs now renders the
  actual layered image (keeping the transparency) for transparent PSD/PSB files. This was never a
  refresh/cache problem — the thumbnail was always current, just flattened. (Needs the full install; the
  compact, ImageMagick-free build still falls back to the white preview.)
- **Fixed: dimensions now appear in the Explorer details pane.** The 0.6.0 update added image
  dimensions, camera info, and audio tags for the formats Windows can't read — but they only showed up
  in a file's Properties window and its hover tooltip, *not* in the details pane along the bottom (or
  side) of the Explorer window, where a PSD, camera RAW, EPUB, etc. still listed only its date and size.
  They now show there too.
- **A lot more file info in Explorer.** While fixing the above we found the handler was reading several
  useful facts and then throwing them away. Now, for the 300+ formats Windows can't read, Explorer's
  Details pane / Properties / columns also show: **date taken, GPS location, color depth and DPI** for
  photos and camera RAW; and **length (duration), bitrate, genre and year** for audio (OGG, Opus, AIFF,
  Musepack, …). Camera RAW even gets its GPS location where Windows itself shows nothing.
- **Those columns are now offered, not hidden.** You can right-click a column header (or "Choose
  columns…") in a folder of PSDs/RAWs/etc. and actually pick Dimensions, Date taken, Length, Artist, …
  as a **sortable/groupable column** — previously the data existed but Explorer never offered it for
  those file types. The files are also classified for `kind:` search (e.g. Krita/OpenRaster as pictures).
- **Fixed: "Show menu on all file types" now works on Windows 11's default menu.** The setting that adds
  a small file-utility menu (move to folder / sort / rename / pick color) to *unsupported* files only
  took effect on the old "Show more options" menu — on the modern Win11 right-click menu it did nothing.
  Now it works there too.
- **Fixed: more video formats get thumbnails.** **`.ts` / `.m2ts` / `.mts` (MPEG transport streams) and
  `.ogv` (Ogg video)** were registered but always showed a blank icon — they were being routed to the
  wrong decoder. They now use the OS video path like every other video. (`.flv` and raw `.mpg`/`.m2v`
  are routed correctly too, but only show a frame if Windows actually has that codec installed.)
- **Fixed: "Keep original file date" now applies to the Convert dialog.** The toggle worked for the
  quick one-click converts but was skipped by the **Convert…** dialog, so its output always got the
  current date. It's honored everywhere now.
- **Fixed: searching by the info we add now works.** The dimensions/camera/audio details showed in the
  Details pane but were stored in a form Windows Search wouldn't index — so "find by artist/camera/date"
  never matched our files. They're now stored in the canonical form the index and column-grouping expect.
- **Fixed: "Files to folder" tells you when it can't.** If creating the folder or moving the files failed
  (read-only, locked, different drive), the dialog used to just close as if it worked. It now shows a
  message and stays open so you can retry. The global-hotkey actions and the screenshot save now report
  failures too, instead of silently doing nothing.
- **+ audio length, bitrate, genre and year for WMA**, and a cleaner uninstall that no longer leaves
  stray registry entries behind (including from very old versions). Under the hood: a security hardening
  pass on the one-click updater (re-verifies the installer on disk right before it runs).

## 0.6.3

- **One-click updates.** When a new version is available, **Settings ▸ Check for updates** can now
  download and install it for you — a progress bar shows the download, Windows asks once for
  permission, and the update installs in the background and confirms when it's done. No more hunting
  down the installer by hand. (You can still grab it from the releases page if you prefer.)

## 0.6.2

A bug-fix release centered on a serious file-dialog problem, plus a sweep for anything like it.

- **Fixed: file dialogs could hang for up to ~2 minutes.** Opening a file picker — for example,
  attaching or uploading a file in your browser — could freeze for a long time as the dialog closed,
  and the preview pane could come up blank/white. The image preview now runs on its own
  message-pumping thread, so closing the dialog is instant and the preview paints reliably. *(This
  was the big one.)*
- **Preview pane now follows your theme.** The preview's background matches Windows dark/light mode
  instead of always being white — even when the host dialog hands the preview the wrong color.
- **Fixed: an unusual or corrupt file can no longer stall the shell.** A hardening pass put a strict
  time limit (and crash-safety guard) on *every* in-process decode path — PDF thumbnails, the
  right-click menu preview, OCR, the Details/property handler, and the SVG / video / camera-RAW
  helpers — so no single file can freeze Explorer, a file dialog, or the preview host. Earlier builds
  could stall on a malformed PDF or a very large image.
- **Fixed: a rare crash when closing a file dialog.** Background decode helpers now keep the
  extension loaded until they finish, so the shell can't unload it out from under a running decode.

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
