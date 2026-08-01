# Bundled ImageMagick: pinned, trimmed, and dependency-closed

SageThumbs 2K uses ImageMagick as its tier-3 long-tail decoder and for the Convert
dialog's exotic output encoders. It runs out of process with resource limits and a kill
timeout; ImageMagick is never loaded into Explorer. The x64 Full installer maps the
contents of `packaging/stage/x64/magick` directly into the application directory, so
that staging directory is also the final flattened runtime layout. ARM64 is deliberately
Compact-only and must not stage ImageMagick until an independently pinned ARM64 bundle
exists.

Correctness on a clean Windows installation is more important than shaving a few bytes.
In particular, an installed developer copy of ImageMagick or the VC++ Redistributable
must never hide a missing release dependency.

## Pinned production input

Production builds accept exactly:

`ImageMagick 7.1.2-29 Q16-HDRI x64 (2026-07-27)`

Bumped from `7.1.2-25` on 2026-07-31 for the 2026 policy.xml bypass cluster, including
**CVE-2026-49219** (symlink read of a disallowed file) whose first fix was itself incomplete
(GHSA-56m6-8q75-f2rw), the PSD/DCM/MNG/APNG/concatenate/script bypasses, and
**CVE-2025-66628** (integer overflow on unchecked width/height in the PSX TIM coder).

[`imagemagick-source.json`](../packaging/imagemagick-source.json) pins its runtime identity and a
deterministic SHA-256 inventory of every source file eligible to enter the bundle:

- `magick.exe`
- every root DLL and XML file
- every file under `modules`
- `License.txt` and `NOTICE.txt`

[`check-magick-source.ps1`](../scripts/check-magick-source.ps1) checks all 195 files,
47,839,954 bytes, and the aggregate inventory digest before copying anything. Selecting
the first `C:\Program Files\ImageMagick*` directory is deliberately forbidden.

An ImageMagick upgrade is therefore an explicit source change: review the new upstream
package, update the pin, regenerate and inspect the stubs, run the full format regression
corpus, review the final dependency inventory, and run
`pwsh scripts/check-magick-dependency-freshness.ps1 -BundlePath packaging/stage/x64/magick`.
That last check is an advisory
maintenance check against the official zlib and libpng pages, not a CI or release-network
gate: an unavailable upstream site must not make a reproducible build fail.

## Exact root payload

The pinned build currently leaves these files at the flattened bundle root:

```text
colors.xml
configure.xml
CORE_RL_bzip2_.dll
CORE_RL_brotli_.dll
CORE_RL_freetype_.dll       (generated no-op stub)
CORE_RL_glib_.dll           (generated no-op stub)
CORE_RL_heif_.dll
CORE_RL_jpeg-turbo_.dll
CORE_RL_jpeg-xl_.dll
CORE_RL_lcms_.dll
CORE_RL_lqr_.dll
CORE_RL_lzma_.dll
CORE_RL_MagickCore_.dll
CORE_RL_MagickWand_.dll
CORE_RL_openjpeg_.dll
CORE_RL_png_.dll
CORE_RL_raqm_.dll           (generated no-op stub)
CORE_RL_raw_.dll
CORE_RL_tiff_.dll
CORE_RL_webp_.dll
CORE_RL_xml_.dll
CORE_RL_zip_.dll
CORE_RL_zlib_.dll
delegates.xml
english.xml
License.txt
locale.xml
log.xml
magick.exe
mime.xml
msvcp140.dll
NOTICE.txt
policy.xml                  (the SageThumbs hardened policy)
thresholds.xml
type-ghostscript.xml
type.xml
vcomp140.dll
vcruntime140.dll
vcruntime140_1.dll
```

All retained coder and filter modules remain below `modules`. `msvcp140.dll`,
`vcomp140.dll`, and both VCRuntime DLLs are load-bearing app-local dependencies.
Clean Windows does not promise the MSVC/OpenMP runtimes.
`CORE_RL_webp_.dll` is also load-bearing even
though the standalone WebP coder is omitted: the retained TIFF DLL hard-imports WebP
support and will not load without it.

HEIF/AVIF and JPEG XL decoding normally happens in earlier SageThumbs tiers, but
their ImageMagick delegates and coder modules remain load-bearing for the Convert
dialog's advertised AVIF and JXL **writers**. Removing them does not cause a clean
error in stock ImageMagick: it can exit successfully while writing input-format bytes
under the requested extension. The final output smoke prevents that regression. EXR,
HDR, Farbfeld, and PAM output use the already-shipped native Rust encoders instead.

The upstream `License.txt` and `NOTICE.txt` are required payload, not optional
documentation.

## Reviewed omissions

These source root DLLs are intentionally absent:

```text
CORE_RL_cairo_.dll
CORE_RL_croco_.dll
CORE_RL_exr_.dll
CORE_RL_fribidi_.dll
CORE_RL_gdk-pixbuf_.dll
CORE_RL_harfbuzz_.dll
CORE_RL_Magick++_.dll
CORE_RL_pango_.dll
CORE_RL_rsvg_.dll
mfc140u.dll
msvcp140_1.dll
msvcp140_2.dll
msvcp140_atomic_wait.dll
msvcp140_codecvt_ids.dll
vcruntime140_threads.dll
```

The SVG stack is handled by resvg; Magick++ and MFC are not used by the command-line
raster decoder. The following actual coder modules in the pinned package are omitted:

```text
IM_MOD_RL_clipboard_.dll
IM_MOD_RL_caption_.dll
IM_MOD_RL_ept_.dll
IM_MOD_RL_exr_.dll
IM_MOD_RL_farbfeld_.dll
IM_MOD_RL_hdr_.dll
IM_MOD_RL_html_.dll
IM_MOD_RL_inline_.dll
IM_MOD_RL_label_.dll
IM_MOD_RL_msl_.dll
IM_MOD_RL_mvg_.dll
IM_MOD_RL_pango_.dll
IM_MOD_RL_pdf_.dll
IM_MOD_RL_ps_.dll
IM_MOD_RL_ps2_.dll
IM_MOD_RL_ps3_.dll
IM_MOD_RL_screenshot_.dll
IM_MOD_RL_svg_.dll
IM_MOD_RL_ttf_.dll
IM_MOD_RL_txt_.dll
IM_MOD_RL_url_.dll
IM_MOD_RL_video_.dll
IM_MOD_RL_webp_.dll
IM_MOD_RL_xps_.dll
```

PANGO is a synthetic text-render input, not an advertised file extension, and the
hardened policy denies it. The build proves both facts before removing that module.
This lets the unused Cairo/Pango delegate DLLs stay out without leaving an unresolved
import. The other omitted coders are handled earlier or are network/interactive/external
inputs denied by the product's security model. Caption, EPT, label, MSL, MVG,
PostScript/PDF, HTML/data-URI, font/text, screenshot, and XPS modules expose only
aliases explicitly denied by the policy; the build refuses their removal if any
mapped alias is ever re-enabled.

No generic “delete every unreferenced DLL” sweep is allowed because ImageMagick discovers
coder modules dynamically. After the reviewed trim and stubbing,
[`prune-magick-unreferenced.ps1`](../scripts/prune-magick-unreferenced.ps1) removes only
six explicit candidates:

```text
msvcp140_1.dll
msvcp140_atomic_wait.dll
msvcp140_codecvt_ids.dll
vcruntime140_threads.dll
CORE_RL_fribidi_.dll
CORE_RL_harfbuzz_.dll
```

For each candidate it proves that no staged PE imports the basename and no staged file
contains an ASCII or UTF-16 `LoadLibrary`/configuration literal. Any reference aborts the
build. The last two become orphaned only because the retained RAQM DLL is itself stubbed.

## Deterministic text-stack stubs

MagickCore hard-imports GLib, FreeType, and RAQM even though SageThumbs never asks
ImageMagick to render text. The production build replaces those DLLs—and initially
generates the related HarfBuzz/Fribidi stubs for export verification—with tiny no-op
DLLs. Each generated DLL:

1. derives its export list from the pinned upstream DLL with `gendef`;
2. preserves function-versus-data export shape;
3. receives version metadata with `windres`;
4. is linked without a CRT by `gcc -nostdlib`; and
5. has its finished export inventory extracted and compared exactly with upstream.

`gendef`, `gcc`, `windres`, and `objdump` from WinLibs/MinGW are mandatory for a full
production bundle. There is no fallback that silently ships the stock text stack and
changes installer size/hash by build machine.

## Bundled dependency versions

Audited 2026-07-31 against the staged bundle, because a vendored image stack is exactly where
an ancient library hides: QuickLook shipped a **2017** zlib inside its exiv2 DLL for years
(their issue #1975). Ours are all current as of the `7.1.2-29` pin:

```text
brotli      1.2.0   (2025-10-27)     lcms        2.19.1  (2026-05-06)
bzip2       1.0.8   (2019-07-13)*    lzma        5.8.3   (2026-04-31)
heif        1.23.1  (2026-06-26)     openjpeg    2.5.4   (2025-09-20)
jpeg-turbo  3.2.0   (2025-06-30)     png         1.6.58  (2026-04-15)
jpeg-xl     0.12.0  (2026-07-01)     raw         0.22.2  (2026-07-16)
tiff        4.7.2   (2026-06-26)     webp        1.6.0   (2025-07-09)
xml         2.15.3  (2026-04-15)     zip         1.11.4  (2025-05-23)
zlib        1.3.2   (2026-02-17)     lqr         0.4.2   (2012-12-04)*
```

`*` bzip2 1.0.8 and liblqr 0.4.2 are the newest upstream releases that exist, not stale pins.
Regenerate this table after any x64 ImageMagick bump with
`Get-ChildItem packaging\stage\x64\magick\CORE_RL_*.dll`
and its `VersionInfo`. The three text-stack entries are our own no-op stubs and carry the
ImageMagick version instead, by design.

**Malformed-input spot check** (also 2026-07-31): a zero-dimension farbfeld, SGI and DDS, plus a
PSX TIM whose `width * height * 2` overflows 32 bits, all exit `1` with a clean error through the
staged engine. No crash, no access violation. `tim` therefore stays a registered extension.

## Final release gates

[`check-magick-bundle.ps1`](../scripts/check-magick-bundle.ps1) performs three independent
checks after every trim:

1. It recursively inspects every staged EXE/DLL import. An import must resolve to another
   bundled basename or a reviewed Windows-inbox DLL/API-set contract. MSVC runtime names
   are intentionally not on the Windows allowlist.
2. It runs the exact flattened staged `magick.exe` with bundle-local configure and module
   paths and a sanitized `PATH`. The smoke probe performs BMP→PNG and
   BMP→TIFF→PNG, exercising module discovery and the TIFF/WebP dependency.
3. It parses the Convert dialog's live `CV_MAGICK_FORMATS` list, produces every one of
   its 14 advertised Magick-backed outputs with the same explicit coder mapping as runtime,
   asks staged ImageMagick to identify the result, and independently validates each
   binary signature/header. Exit status and a nonempty file are not sufficient.

For the pinned build after reviewed orphan pruning, the gate reports 143 PE files and
845 import edges. The raw Magick payload is 30,057,721 bytes, including the required
writer delegates, upstream legal files, and clean-machine runtimes.

Focused fail-closed tests:

```powershell
pwsh scripts/test-magick-packaging.ps1 -BundlePath packaging/stage/x64/magick
pwsh scripts/test-staged-regression.ps1 -StagePath packaging/stage/x64
```

The tests prove that source-inventory drift, a missing `VCOMP140.dll`, a missing
`NOTICE.txt`, a missing dynamically loaded JXL writer, and an attempt to prune a
referenced runtime all stop the build. The staged regression wrapper additionally
recreates the installer's flattened directory in a disposable location and runs the
full real-sample corpus plus DICOM pixel checks against that exact runtime, so an
untrimmed ImageMagick in Program Files cannot hide a packaging regression.
