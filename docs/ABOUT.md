# SageThumbs 2K

> Windows Explorer thumbnails, previews, and a right-click image toolkit for 349 formats Windows can't render.

<!-- odin:about HAND-OWNED above the GENERATED marker. Edit freely; `odin codex about --ingest` carries it back into Odin's Codex. -->

## What it is

A crash-isolated Rust shell extension for Windows 11 that draws File Explorer thumbnails, Details-pane columns, and a large reading-pane preview for 349 file types Windows cannot render natively (camera RAW, PSD, HEIC/AVIF, ebooks/comics, video, CAD/3D, DjVu, MS Office, audio). It adds a right-click image toolkit (convert, resize, lossless rotate, combine-to-PDF/CBZ, OCR, eyedropper) and an opt-in Space-bar QuickLook-style previewer with video/audio playback, syntax highlighting, PDF paging, SQLite/email/3D-model viewing and more. It also ships a CLI and MCP server (st2k.exe) so scripts and AI agents get the same decode/convert engine offline. A clean-room, from-scratch Rust revival of the decade-abandoned SageThumbs, free for personal use with a paid commercial license.

## Things not to forget

_The intricacies worth remembering: the gotchas, the half-built parts, the decisions whose
reason lives nowhere else. Odin never overwrites this section._

- The vendored jxl-oxide/jxl-render patch exists purely for a 1:8 LF-only render path that cuts a 12MP JPEG XL thumbnail decode to about 2 seconds; it is regenerated (never hand-edited) by scripts/vendor-jxl.ps1 and should be dropped once tirr-c/jxl-oxide#505 ships upstream. anchors: `Cargo.toml:23`
- FLV (VP6/Sorenson) and HDR VP9 decoding run in a short-lived, memory-capped child process that caps its own memory before reading any input and reads cap+1 bytes so an over-cap stream is detected instead of being silently truncated into a 'valid' but wrong file. anchors: `src/bin/vdec/mod.rs:95`
- Every other right-click verb (convert, rotate, strip, ocr, pdf, cbz, batch, prebuild) is wired through both the CLI and the MCP server via dispatch_tool, but rename-from-metadata, set-as-folder-icon and set-as-wallpaper are still right-click-only with no CLI/MCP entry point. anchors: `src/mcp.rs:721`
- A redeemed business seat key tolerates about a week of total network silence (GRACE_SECS = 7*24h) before the license degrades, so a dead network deliberately reads as still-licensed rather than instantly locking the user out; and the business-licence LOCK (7-day evaluation + 3-day notice, then the shell refuses) reaches only a copy that never held a key or whose key was revoked, never a once-licensed copy that merely went quiet. The mode read, the breadcrumb and the phase arithmetic live in the core crate so the thumbnail provider can decide inside explorer.exe with no app process alive. anchors: `crates/base/src/licence_state.rs`, `src/bin/app/license.rs`
- The preview handler is deliberately loaded inside Explorer's own out-of-process preview host so a malformed file decodes to an empty reading-pane rather than crashing Explorer - the crash isolation is load-bearing, not incidental. anchors: `src/previewhandler.rs:15`
- Settings > Diagnostics' repair-registration button waits at most 60 seconds on each regsvr32 call, a deliberately bounded timeout so a wedged re-registration can't hang the Settings window forever. anchors: `src/bin/app/settings_dlg/values.rs:1522`
- Batch rename-from-metadata treats a skip (a file missing the needed EXIF/audio tag, or a name clash) as expected rather than a failure - only a real rename error counts, so the reported 'attempted' total excludes skips. anchors: `crates/actions/src/verbs/actions/rename.rs:10`

<!-- odin:about GENERATED BEGIN - rewritten by `odin codex about --publish`; edit the Codex, not this -->

## What Odin knows about this project

Everything from here down is generated from this project's Codex dossier
(`codex/projects/sagethumbs-2k.md` in the Odin clone) and is **rewritten on every publish** -
edit the dossier, not this block. Everything ABOVE the marker is yours.

### At a glance

- **Remote:** https://github.com/LunarWerxs/SageThumbs-2k.git

### Architecture

- `src/` - Core decode pipeline, COM shell-extension handlers (thumbnail provider, preview handler, property store, context menu), the format registry, and library verbs shared by the CLI/MCP binaries.
- `src/bin/app/` - The SageThumbs2K.exe GUI: Settings dialog, Quick preview viewer, screenshot capture/annotate/OCR, eyedropper, licensing, auto-updater and Connections OAuth settings-sync.
- `src/bin/cli.rs` - st2k.exe entrypoint: CLI arg parsing over the shared verb library, plus --mcp to run the MCP server.
- `src/bin/vdec/` - Sandboxed, memory-capped child-process decoders for video codecs Windows' Media Foundation lacks (FLV VP6/Sorenson, HDR VP9).
- `crates/codecs/src/container/` - Per-format embedded-preview/cover extractors (PSD, Blender, Clip Studio, DWG, APK, audio album art, DjVu, and more) needing no external rendering engine.
- `crates/actions/src/verbs/` - Right-click toolkit actions (convert, resize, rotate, folder icon, wallpaper, rename) and the menu tree driving both classic and modern Explorer context menus.
- `crates/codecs/src/decode/` - Pure-Rust format decoders (e.g. JPEG 2000) and colour-accuracy decode test fixtures.
- `crates/dll/` - The thin cdylib wrapper exposing the COM class objects that regsvr32 registers as the shell extension.
- `crates/dlghook/` - A tiny DLL injected into other apps' Open/Save dialogs to read the selected file path for Quick preview, over a shared-memory handshake.
- `crates/vendor/` - Vendored pure-Rust decode crates carried in-tree (exr, jxl-oxide, jxl-render) for OpenEXR and JPEG XL support with no C dependency.
- `scripts/` - PowerShell/Python release, packaging and asset-generation scripts (build-release.ps1, the hero-collage generator).
- `tests/` - Integration tests: COM round-trip, render-regression, corpus decode across the vendored format fixtures.

### Features

25 recorded - 25 shipped, 0 partial, 0 planned. Each path is where the feature is DEFINED; the exact lines live in the Codex entry, which `odin codex check` re-verifies and repairs.

**Shipped**

- **Explorer thumbnails for 349 file types** _(free)_ - Draws real File Explorer thumbnails for 349 registered extensions (camera RAW, PSD, HEIC/AVIF, ebooks/comics, video, CAD/3D, DjVu, Office docs, audio) through a tiered decoder: the image crate, then Windows WIC, then a sandboxed bundled ImageMagick, then resvg for SVG. - `src/thumbprovider.rs`, `crates/base/src/formats.rs`
- **Right-click image toolkit** _(free)_ - A nested 'SageThumbs 2K' submenu on both classic and Windows 11 context menus: Convert, Resize, lossless Rotate/flip, Combine into PDF/CBZ, Strip metadata, batch Rename, Files-to-folder, upload-to-catbox and more, with multi-file jobs parallelised across every CPU core. - `src/command.rs`, `crates/actions/src/verbs/actions/helper.rs`
- **Set as folder icon / set as wallpaper** _(free)_ - Two right-click verbs: makes the selected image the icon of its containing folder (writes a hidden .ico + desktop.ini), or sets it as the desktop wallpaper (stretched/tiled/centered). - `crates/actions/src/verbs/actions/foldericon.rs`, `crates/actions/src/verbs/actions/wallpaper.rs`
- **Batch rename from metadata** _(free)_ - Right-click Rename batch-renames the selection from EXIF capture date/camera (photos) or audio tags via lofty (Artist - Title, zero-padded track number), skipping files missing the needed metadata. - `crates/actions/src/verbs/actions/rename.rs`
- **Drag-reorder right-click menu** _(free)_ - Settings ▸ Menu items lets a user show/hide and drag-reorder every SageThumbs right-click entry and its group dividers; the live context menu mirrors that arrangement exactly on both the classic and modern Win11 menus. - `crates/actions/src/verbs/menu.rs`, `src/bin/app/settings_dlg/ids.rs`
- **Details pane & sortable columns (property handler)** _(free)_ - An IPropertyStore handler surfaces image dimensions, EXIF camera/GPS info and audio tags in Explorer's Details pane, hover tooltips and sortable/groupable columns for the 300+ formats Windows itself cannot read; read-only and crash-isolated. - `src/propstore.rs`
- **Large reading-pane preview (preview handler)** _(free)_ - An IPreviewHandler renders the image large in Explorer's reading pane and the Open/Save dialog's preview for the same 300+ formats, running out-of-process and crash-isolated so a malformed file yields an empty pane rather than a crash. - `src/previewhandler.rs`
- **Quick preview (Space-bar QuickLook popup)** _(free)_ - Tap Space in Explorer, on the Desktop, in Everything search results, or inside any 64-bit app's Open/Save dialog for an instant full-size popup: video/audio playback, syntax-highlighted code, rendered Markdown, multi-page PDF paging, SQLite table browsing, .eml/.msg email viewing, 3D-model rendering and more. Off by default. - `src/bin/app/preview/window.rs`
- **Open/Save dialog selection preview (dlghook)** _(free)_ - A tiny helper DLL injected into any 64-bit app's Open/Save common dialog reads the currently-selected file path via a shared-memory handshake, so Quick preview can show what a file actually is before you commit to opening it. - `crates/dlghook/src/lib.rs`
- **CLI toolbox (st2k.exe)** _(free)_ - A standalone console binary exposing thumbnail/convert/batch/rotate/strip/ocr/pdf/info/formats verbs over the same decode/convert engine the shell extension uses, for scripts and automation with zero extra installs. - `src/bin/cli.rs`, `src/cli/convert.rs`
- **MCP server (st2k --mcp)** _(free)_ - st2k --mcp speaks stdio JSON-RPC 2.0 and exposes 10 tools (the CLI verbs plus agent-first `view`, which decodes any of the 349 formats to a PNG image block so an agent can see the file, and `compress`) so an AI client can discover and call an offline image toolbox. - `src/mcp.rs`
- **Screen OCR (Copy text)** _(free)_ - Copy text off any part of the screen (a right-click verb, Ctrl+T in the screenshot editor, a Quick preview toolbar button, or a global hotkey) via Windows' own OCR engine; recognized words land on the clipboard and in an editable window, with captured tables kept tab-separated. - `crates/actions/src/verbs/actions.rs`
- **System-wide eyedropper / color picker** _(free)_ - A screen-wide color picker with a 10x magnifier loupe: click or press Space to copy the pixel under the cursor as hex/rgb()/hsl()/hsv() (Tab switches format), keeping the last 10 picks a keypress away. - `src/bin/app/eyedropper.rs`
- **Screenshot capture + annotate** _(free)_ - A capture hotkey opens a region editor (draw/annotate, snap-to-45°, live width x height readout) with copy/save shortcuts, plus an optional quick-save hotkey that grabs the whole screen straight to clipboard + a timestamped PNG. - `src/bin/app/screenshot/overlay.rs`
- **Repair file associations** _(free)_ - One button in Settings ▸ Diagnostics re-registers SageThumbs for every enabled format when another app has hijacked the thumbnails, then clears the Explorer thumbnail cache. - `src/bin/app/settings_dlg/values.rs`
- **Auto-updater** _(free)_ - Checks GitHub for a newer release on a schedule (or on demand), and on approval downloads, integrity-checks and installs the update in the background with a quiet tray notification when done. - `src/bin/app/update.rs`
- **Commercial licensing (seat keys)** _(paid)_ - Free for personal use under PolyForm Noncommercial; a business copy gets a 7-day evaluation with every feature, then 3 days of reminders, then the shell stops serving thumbnails, previews, the Details pane and the menu until a seat key (esk_...) is redeemed under Settings ▸ Licence; a redeemed key is checked in the background and tolerant of about a week offline, and a copy that once held one is never stopped by silence, only by a revocation. A stored key can renew another 12 months of updates from a pre-filled checkout link, and a refused redemption (key already bound to another machine) now points the buyer at the Connections seat portal to re-bind it themselves instead of a dead end. - `crates/base/src/licence_state.rs`, `src/bin/app/license.rs`
- **Settings sync via Connections account** _(free)_ - Settings ▸ Data & Backup can sign in with a Connections account (OAuth 2.0 + PKCE, browser-based) to sync an allowlisted set of portable preferences across machines; off by default, never syncs images or secrets, and the app works fully offline when signed out. - `src/bin/app/oauth.rs`
- **Export / Import settings** _(free)_ - Save or apply the entire settings tree to a JSON file, from the Settings UI or headlessly via SageThumbs2K.exe --export-settings/--import-settings, for scripted deployment across a fleet of installs. - `src/bin/app/settings_io.rs`
- **Sandboxed video-codec decode (FLV / HDR VP9)** _(free)_ - Two codec families Windows' Media Foundation cannot decode (FLV's VP6/Sorenson Spark, and 10/12-bit HDR VP9 Profile 2/3) are decoded by SageThumbs itself in pure Rust, inside a short-lived, memory-capped child process, so a corrupt file costs one thumbnail rather than a crash. - `src/bin/vdec/mod.rs`, `src/bin/vdec/flv.rs`, `src/bin/vdec/vp9.rs`
- **Design, CAD, 3D and APK preview extraction** _(free)_ - Dozens of per-format extractors pull an embedded preview directly out of project files with no rendering needed: Photoshop, Affinity, Clip Studio (read straight from its embedded SQLite database), Blender, AutoCAD DWG, CorelDRAW, Cinema 4D, and Android APK launcher icons resolved through the compiled resource table. - `crates/codecs/src/container/clip.rs`, `crates/codecs/src/container/apk.rs`, `crates/codecs/src/container/dwg.rs`
- **DjVu and hand-rolled audio cover extraction** _(free)_ - Pure-Rust DjVu page rendering (djvu-rs) plus hand-rolled cover-art readers for formats lofty can't parse: ASF/WMA (WM/Picture) and DSD/.dsf (trailing ID3v2 APIC tag), alongside lofty-based extraction for MP3/FLAC/Ogg/APE. - `crates/codecs/src/container/djvu.rs`, `crates/codecs/src/container/audio/asf.rs`, `crates/codecs/src/container/audio/id3.rs`
- **Diagnostics report (`st2k doctor`)** _(free)_ - A read-only self-check that answers 'why do I have no thumbnails?', runs from Settings ▸ Diagnostics or the `st2k doctor` CLI command, checking registration, policy and codec state - including custom 'This PC' namespace entries a tweaker added and whether Explorer ever actually asked SageThumbs for the probed file - and showing the results in a dedicated 'Check for problems' window (or as a shareable report). - `src/doctor.rs`, `src/bin/app/doctor_report.rs`, `src/doctor/winchecks.rs`
- **Pre-build thumbnails for a folder tree** _(free)_ - A folder's right-click menu offers a 'Build thumbnails here' entry that walks the whole tree and generates Explorer thumbnails at every enabled size in advance, so opening a huge folder later shows thumbnails immediately instead of generating them on first browse. - `src/foldermenu.rs`, `src/prebuild.rs`, `src/bin/app/prebuild_dlg.rs`
- **Image info window** _(free)_ - A right-click 'Image info' verb opens a verbose, copyable metadata dump (dimensions, EXIF, DDS mip-level/compression facts, XMP) for a selected file, a separate window from Explorer's own Details-pane columns and from the other right-click toolkit actions. - `src/bin/app/image_info.rs`

### Where to add a new one

- **a new supported thumbnail/preview format** - register the extension + category in the FORMATS table, then add a decoder under crates/codecs/src/container/ (embedded-preview extraction) or crates/codecs/src/decode/ (full decode). anchors: `crates/base/src/formats.rs`
- **a new right-click verb** - add the action function under crates/actions/src/verbs/actions/ and wire it into the menu tree in crates/actions/src/verbs/menu.rs so both classic and modern menus pick it up. anchors: `crates/actions/src/verbs/menu.rs`
- **a new CLI/MCP tool** - add the verb to src/cli.rs, a thin arg branch in src/bin/cli.rs, and a dispatch_* handler in src/mcp.rs so it's exposed to both st2k and st2k --mcp. anchors: `src/mcp.rs`
- **a new Settings page or control** - add an ID in src/bin/app/settings_dlg/ids.rs, a page under settings_dlg/, and wire load/save in settings_dlg/values.rs. anchors: `src/bin/app/settings_dlg/ids.rs`
- **a new sandboxed video codec** - add a child verb under src/bin/vdec/ (mirroring flv.rs/vp9.rs) and dispatch it from run_child in mod.rs. anchors: `src/bin/vdec/mod.rs`

### Gaps and wants

_Withheld: this repository is public, and the gap list is not published outside the private index._
_Read it with `python odin.py codex brief sagethumbs-2k` in the Odin clone._

---

_Generated by `odin codex about --publish sagethumbs-2k` on 2026-09-18 from a Codex dossier stamped -. Regenerate after the product moves; `odin codex about` reports drift._
<!-- odin:about GENERATED END sha=8d8ef485fa63 -->
