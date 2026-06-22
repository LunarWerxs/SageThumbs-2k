# Bundled ImageMagick — what we ship and why it's small

SageThumbs 2K bundles a **trimmed, hardened ImageMagick** as the tier-3 long-tail decoder
(the obscure formats the pure-Rust `image` crate and Windows WIC can't read). It runs **only
as an isolated subprocess** (`decode.rs` — `magick - … PNG:-`), never linked in-process, so a
crash/OOM/CVE in magick's C code can't take down Explorer.

The bundle is produced by `scripts/build-release.ps1` from the **official ImageMagick Windows
install** in `C:\Program Files\ImageMagick*` (an MSVC build). Two things shrink it from the
stock ~25 MB to ~3 MB:

## 1. Dropped delegates + coders
We only decode **raster → PNG**, so the GUI/MFC runtime, the HEIF/AVIF/JPEG-XL/EXR/WebP coders
(handled by earlier tiers before magick is ever reached), and the cairo/pango/rsvg SVG stack
(we use `resvg`) are deleted. Regression-verified to lose zero decodable formats. See the
`$dropDll` / `$dropCoder` lists in `build-release.ps1`.

## 2. Stubbed text-shaping stack (~5 MB) — the interesting one
MagickCore **hard-links** the complex-text-shaping chain at load:

| DLL | stock size | why it exists |
|---|---|---|
| `CORE_RL_glib_` | ~2.65 MB | glib (used by harfbuzz/pango) |
| `CORE_RL_harfbuzz_` | ~1.48 MB | text shaping |
| `CORE_RL_freetype_` | ~0.72 MB | font rasterization |
| `CORE_RL_fribidi_` | ~0.13 MB | bidirectional text |
| `CORE_RL_raqm_` | ~0.04 MB | complex-script layout |

It exists purely for caption/label/annotate rendering — which **we never do** (we decode
images, we don't draw text). But you can't just delete the DLLs: they're in MagickCore's
*static import table*, so `magick.exe` won't even start without them (a runtime delete looks
fine in a one-off test, then fails on a cold process start).

**The fix: replace each with a tiny stub DLL** that exports the *same symbols* as no-ops
(`int name(void){return 0;}`, data exports as zeroed). The import table resolves at load, magick
starts, and the text functions are simply never called on the decode path. This drops ~5 MB raw
→ ~0.6 MB of stubs (**~4.4 MB saved**, ~1.5–2 MB off the compressed installer) with **zero
decode regression** — verified with the full corpus including the glib-stubbed build.

### How it stays permanent / no-hassle
`build-release.ps1` regenerates the stubs **on every build** straight from the installed
ImageMagick's own export tables (`gendef` → no-op `stub.c` + `build.def` → `gcc -shared
-nostdlib`). So an **ImageMagick upgrade just works** — the new export set is picked up
automatically; nothing to hand-maintain. It needs `gendef` + `gcc` from the same mingw toolchain
that already provides `windres` for the build; if they're missing, the build **warns and ships
the full text stack** rather than failing.

### If you ever need to verify after an IM upgrade
```powershell
pwsh scripts/build-release.ps1      # produces dist\SageThumbs2K-Setup-<ver>.exe with stubs
pwsh scripts/regression.ps1          # must stay PASS — all baseline extensions render
```
If `regression.ps1` ever drops a magick-tier format after a stub regen (e.g. a future IM build
that genuinely uses glib for raster I/O), narrow the stub list in `build-release.ps1` to exclude
`glib` (keep the 4 pure-text DLLs stubbed — still ~2.4 MB saved, zero risk).
