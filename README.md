# SageThumbs 2K

**A modern, crash-isolated Windows 11 thumbnail + context-menu shell extension — a clean-room Rust rewrite of the beloved (but decade-abandoned) [SageThumbs](https://sagethumbs.en.lo4d.com/).**

SageThumbs let Explorer show thumbnails for *hundreds* of image formats Windows can't read on its own. It hasn't been updated since ~2017 and relied on the proprietary, frozen **GFL** library. SageThumbs 2K rebuilds it from scratch in safe Rust, with a maintained decode pipeline, real crash isolation, and a native Windows 11 look — while keeping the thing that made the original great: **thumbnails for everything.**

> Status: **working and installed-on-machine.** Thumbnails, the context menu, and the Options dialog are all verified live. License: **MIT OR Apache-2.0**.

---

## Features

- **179 file types** thumbnailed in Explorer, across five categories (images, camera RAW, ebooks & comics, documents, audio) — RAW (Canon/Nikon/Sony/Fuji/Olympus/Pentax/…), DICOM, Photoshop PSD, GIMP XCF, DPX/Cineon, OpenEXR, FITS, HEIC/AVIF, JPEG-2000, PCX, Targa, Sun raster, SGI, WebP, JPEG XL, and the long tail Windows ignores. ([full list](#supported-formats))
- **Ebook & comic covers** (a native-Rust port of the abandoned [DarkThumbs](https://github.com/fire-eggs/DarkThumbs)): **EPUB**, **MOBI/AZW/AZW3** (Kindle), **FB2/FBZ** (FictionBook), and comic archives **CBZ / CB7 / CBR / CBT**. SageThumbs 2K sniffs the container, finds the cover image, and decodes it through the same pipeline — so your bookshelf and comics get real covers in Explorer. (CBR is behind the `rar` feature.)
- **Art / CAD / 3D-print project files** — we extract the preview image *baked inside* the file (no rendering, no codecs, works on the ImageMagick-free compact install): **Photoshop** `.psd/.psb`, **Affinity** `.afphoto/.afdesign/.afpub`, **Clip Studio** `.clip` (read straight out of its embedded SQLite DB), **Krita** `.kra`, **OpenRaster** `.ora`, **Blender** `.blend`, **3MF** `.3mf`, **FreeCAD** `.fcstd`, and 3D-printer **G-code** `.gcode`. No Windows tool thumbnails most of these.
- **DjVu — with a fully hand-rolled decoder.** No mainstream Windows thumbnailer renders `.djv/.djvu`; SageThumbs 2K ships a clean-room, zero-GPL implementation of the whole stack (ZP adaptive arithmetic coder → IW44 wavelet background → JB2 bilevel text mask, composited with the foreground palette) — scanned books show their text, photo pages show their photos, validated bit-exact against the reference decoder's transform.
- **Documents & audio** — **PDF** first-page (via in-box `Windows.Data.Pdf`, no bundled bytes), **OpenDocument / PowerPoint** previews, and embedded **album art** for MP3/FLAC/Ogg/Opus/M4A/WMA/APE/WavPack/Musepack.
- **`st2k.exe` CLI + MCP server** — the bundled engine as an offline image toolbox for scripts and AI agents: `thumbnail · convert · rotate · strip · ocr · pdf · info · formats`, plus **`st2k --mcp`** (stdio JSON-RPC) exposing the same verbs as MCP tools. OCR runs on the in-box `Windows.Media.Ocr`. See [`AI_INTEGRATION.md`](AI_INTEGRATION.md).
- **Tiered, maintained decode** — pure-Rust [`image`](https://github.com/image-rs/image) for the common/safe path → **Windows WIC** (free OS codecs: HEIC/AVIF/RAW) → **ImageMagick** (the obscure long tail) → a headerless-Targa fallback; **SVG** is detected up front and rasterized with **resvg**. No abandonware, no GFL.
- **Crash-isolated by design.** Runs out-of-process in Explorer's thumbnail host; every COM method is `catch_unwind`-guarded under `panic = "abort"`; the riskiest decodes run in a **sandboxed ImageMagick child process** with resource limits and a 20-second kill-timeout; decompression-bomb guards throughout. A malformed or malicious image can't take down Explorer.
- **True transparency** — returns a spec-correct premultiplied-ARGB DIB so Explorer composites real alpha over its own background (no more dated gray checkerboard behind transparent PNGs).
- **Right-click image toolkit** (modern Win11 `IExplorerCommand` flyout **and** a classic `IContextMenu` fallback for StartAllBack/ExplorerPatcher): Convert into PNG/JPG/WebP/BMP/GIF/TIFF/ICO (plus a **Convert… dialog** with 27 output formats, quality + resize), **Combine into PDF / CBZ**, Resize & **Shrink-for-email** presets, **lossless JPEG rotate/flip** (DCT rearrange, zero quality loss), batch **Rename from EXIF / audio tags**, **Files-to-folder** and **Sort-into-folders** (by image size or audio tag), a **system-wide eyedropper** color picker with magnifier loupe, **Set as folder icon**, OCR copy-text, Image info, Strip metadata, Copy to clipboard, Set as wallpaper — all non-destructive (atomic writes, never overwrites the source).
- **Native Win11 Options dialog** — two-column layout, Common-Controls v6 + Segoe UI, a per-format checklist, and **system-following dark mode**.
- **Configurable** like the original: enable/disable thumbnails, max file size, max thumbnail size, embedded-EXIF fast path, JPEG/PNG quality, per-format toggles.
- **36 languages** — the menu and Settings dialog follow your Windows display language (or pick one in Settings, with live preview): English, العربية, Български, Čeština, Dansk, Deutsch, Ελληνικά, Español, فارسی, Suomi, Filipino, Français, עברית, हिन्दी, Hrvatski, Magyar, Bahasa Indonesia, Italiano, 日本語, 한국어, Bahasa Melayu, Norsk, Nederlands, Polski, Português (Brasil), Română, Русский, Slovenčina, Slovenščina, Svenska, ไทย, Türkçe, Українська, Tiếng Việt, 简体中文, 繁體中文 — more than the original SageThumbs shipped. Translations live in `locales/*.toml` and are compiled into the binary by `build.rs` (no runtime dependency).

---

## Install

Download `SageThumbs2K-Setup-<version>.exe` from [Releases](https://github.com/LunarWerxs/SageThumbs-2k/releases) and run it.

- **Full** install (~9.8 MB) bundles the ImageMagick engine → all 179 file types.
- **Compact** install (~1–2 MB) skips ImageMagick → common formats only (PNG/JPEG/GIF/BMP/WebP/TIFF/ICO/HEIC/AVIF/RAW via the OS + SVG), plus the embedded-preview project files and ebook/comic covers (those don't need ImageMagick).

After installing, open a folder of images. Configure via **Start menu → "SageThumbs 2K Options"**. To uninstall, use *Apps & features* (it cleanly unregisters all formats).

> Not packaged as MSIX on purpose: SageThumbs 2K is a *classic* shell extension that registers via `regsvr32` and spawns ImageMagick as a subprocess — a model a traditional installer fits far better than MSIX's sandbox.

---

## Build from source

Requires the **MSVC** Rust toolchain (`rustup default stable-x86_64-pc-windows-msvc`), VS Build Tools (Desktop C++), and — for the installer — [Inno Setup](https://jrsoftware.org/isinfo.php) (`winget install JRSoftware.InnoSetup`).

```powershell
cargo build --release            # builds sagethumbs2k.dll + sagethumbs2k-app.exe
.\scripts\test.ps1               # build-first test runner (unit + COM round-trip)
.\scripts\install.ps1            # dev install (regsvr32 + Start-menu shortcut)
.\scripts\build-release.ps1      # full release pipeline -> dist\SageThumbs2K-Setup-<ver>.exe
```

`build-release.ps1` reads the version from `Cargo.toml`, builds, stages the DLL + Options EXE + a curated **hardened** ImageMagick (magick.exe + coder modules + a locked-down `policy.xml`), and compiles the Inno Setup installer. Pass `-NoImageMagick` for the compact build.

---

## How it works

```
IThumbnailProvider (IInitializeWithStream)        <- runs in Explorer's isolated dllhost surrogate
        |
        v  decode pipeline (stops at the first tier that decodes; SVG detected up front -> resvg)
   image crate  ->  WIC (OS codecs)  ->  ImageMagick (sandboxed child process)  ->  headerless-Targa
  (safe Rust)       HEIC/AVIF/RAW        the obscure long tail                      fallback
        |
        v
  premultiplied-BGRA top-down DIB section -> Explorer (WTSAT_ARGB)
```

The DLL exposes three COM coclasses from one `DllGetClassObject`: the thumbnail provider, the modern `IExplorerCommand` menu, and the classic `IContextMenu` menu. Settings live under `HKCU\Software\SageThumbs2K`.

---

## Supported formats

179 extensions, driven by `src/formats.rs` (Image 112 · Camera RAW 27 · Ebook/comics 10 · Document 14 · Audio 16). Highlights:

- **RAW** — 3fr, arw, cr2/cr3/crw, dcr, dng, erf, fff, iiq, k25/kdc, mef, mos, mrw, nef/nrw, orf, pef, raf, rw2/rwl, sr2/srf/srw, x3f
- **Pro / scientific** — dcm (DICOM), dpx, cin, exr, fits, hdr, fl32, pfm
- **Photoshop / paint** — psd/psb, xcf, pcx/dcx, miff, cut, mac, mat
- **Common + modern** — png/apng, jpg/jpeg, gif, bmp/dib, tiff, webp, heic/heif, avif, jp2/j2k/jpc, jxl, dds, ico, tga, qoi, svg
- …plus SGI, Sun raster, XBM/XPM, WPG, and more.

(PostScript/video/font coders are deliberately excluded for safety; PDF gets a first-page thumbnail via the in-box OS renderer.)

---

## How it compares to the original SageThumbs

**What SageThumbs 2K adds (that the original lacked):** the modern Win11 `IExplorerCommand` menu; real crash isolation (panic guards + a sandboxed subprocess + bomb limits) vs. the original's SEH-only protection; a maintained, permissively-licensed decode pipeline (no GFL); true premultiplied-ARGB transparency vs. the dated checkerboard flatten; higher-quality Lanczos3 resampling; non-destructive convert/wallpaper verbs (atomic, never clobber the source); full-resolution clipboard copy; a runtime image-only menu gate; and **system-following dark mode** + a Win11-native Options UI.

**What's still on the list:** exposing image dimensions/bit-depth via `IPropertyStore` so the long-tail formats are first-class in Explorer's Details pane (highest-value gap); an "Options…" context-menu verb; an About/version line in the dialog. Intentionally **dropped** (obsolete): the SQLite thumbnail cache + Disk-Cleanup integration, the XP-era Windows Image/Fax-Viewer toggle, `IExtractIcon` (superseded by `IThumbnailProvider`), and invasive ProgID/file-association rewriting (2K is deliberately non-invasive).

---

## License

Dual-licensed under **MIT OR Apache-2.0** (your choice). This is a clean-room rewrite, not a derivative of the GPLv2 C++ original. The bundled ImageMagick ships under its own permissive (Apache-2.0-derived) license. SageThumbs 2K does **not** use GFL.

## Credits

Inspired by Nikolay Raspopov's original **SageThumbs** (2004–2017). Built with [image-rs](https://github.com/image-rs/image), [resvg](https://github.com/linebender/resvg), [windows-rs](https://github.com/microsoft/windows-rs), and [ImageMagick](https://imagemagick.org/).
