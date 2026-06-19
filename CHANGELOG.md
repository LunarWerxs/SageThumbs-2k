# Changelog

All notable user-facing changes to **SageThumbs 2K**. Newest first.

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
