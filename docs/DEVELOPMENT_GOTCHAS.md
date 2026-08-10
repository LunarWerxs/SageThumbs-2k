# Development Gotchas

Hard-won traps in this codebase. Each one cost real debugging time; none is
obvious from reading the code.

## The menu preview MUST be an owner-drawn item (regression 1.3.2 → 1.3.6)

**Symptom.** The right-click preview (thumbnail + filename + `3000 x 2000 px - 600 KB`)
renders as a ~6 px horizontal strip of squashed image instead of the ~144 x 136 tile.
Reported 2026-07-26 against 1.3.5 ("since the last update the preview is fucked up, the
image is a tall rectangle"). Present in every build from **1.3.2** onward; 1.3.1 and
earlier were correct.

**Root cause.** Not the compositing, not the decode, not a stale GDI handle: those were
all verified healthy (`cargo run --example previewshot` produced a perfect 144 × 136 PNG
throughout). The item is fine; the *menu host* refuses to give it a row.

A **third-party menu skin** draws the popup itself: StartAllBack, whose `StartAllBackX64.dll`
and `DarkMagicX64.dll` are injected into `explorer.exe` to give Windows 11 a
Windows-10-style dark classic context menu. Its measurement pass treats **every bitmap
menu item as an icon**: it takes the bitmap's width but clamps the row to icon height and
clips everything below it. ExplorerPatcher and similar shell skins are the same class of
host. This is *not* Windows: the identical 144 × 136 `MF_BITMAP` item in a plain
non-injected process renders at full size (verified).

**Why the 1.3.2 change looked right and wasn't.** 1.3.2 ("fix menu theming") moved the
tile from owner-draw to `hbmpItem` on the stated premise that *"bitmap items are drawn
natively, so the popup keeps its own dark/light theme"*. The theming half is true. The
premise that the tile survives is false, and nobody re-checked the tile afterwards. The
change was verified against the bug it fixed, not against the feature it moved.

**What was tried, live in Explorer, before concluding** (each one screenshotted; the whole
point is that none of this is deducible from the docs):

| mechanism | result |
| --- | --- |
| `hbmpItem` on an empty `MF_STRING` item (1.3.2–1.3.6) | ~6 px sliver |
| `InsertMenuItemW` with `MIIM_BITMAP \| MIIM_ID` | ~6 px sliver |
| `MF_BITMAP` item, 32-bpp DIB section | ~6 px sliver |
| `MF_BITMAP` item, screen-compatible DDB | ~6 px sliver |
| `MF_BITMAP` item, 24-bpp DDB | ~6 px sliver |
| `hbmpItem = HBMMENU_CALLBACK` | **no** `WM_MEASUREITEM`/`WM_DRAWITEM` is ever delivered |
| `MF_OWNERDRAW`, id **outside** the range `QueryContextMenu` claimed | item present, zero-sized, never measured |
| `MF_OWNERDRAW`, id **inside** the claimed range | **full 144 × 136 tile** |

So the bitmap *format* never mattered, and `HBMMENU_CALLBACK` is a dead end here: the
shell forwards owner-draw messages to `IContextMenu2`/`IContextMenu3` only for items it
recognises as owner-drawn and whose command id it can map back to this handler.

**Rules that follow.**

- The preview item is `MF_OWNERDRAW` with `WM_MEASUREITEM`/`WM_DRAWITEM` handled in
  `contextmenu.rs`. Do not "improve" it into a bitmap item again: `preview_item_is_owner_drawn`
  and the flyout assertion in `tests/context_menu_latency.rs` exist to stop exactly that.
- **Its command id must be inside the range `QueryContextMenu` returns.** An id one past
  the range gets no messages at all, so the item silently measures to nothing. This is the
  single easiest way to "fix" the preview into invisibility.
- The known cost, unchanged since 1.3.1: **one owner-drawn item drops the entire popup
  onto the classic (light) drawing path**, including every other handler's items. That is
  the real trade, and it is why the preview is opt-out: `MenuPreview = 0` inserts no
  owner-drawn item and the menu renders natively. `MenuPreview = 1` (the default, with the
  preview inside the SageThumbs flyout) confines the classic look to our own flyout and
  leaves the main menu themed; `MenuPreview = 2` puts the preview, and therefore the
  classic look, on the main menu.
- **Verify menu changes by looking at a real menu, not at a test.** Every unit test here
  passed while the preview was a sliver, because they assert what we hand to the menu and
  the menu is what mangles it. Drive Explorer, screenshot the popup, look at it.

### Extension-created submenu rows cannot wait for `WM_INITMENUPOPUP`

Explorer can omit `WM_INITMENUPOPUP` for the child `HMENU` created by a shell extension.
Deferring the submenu preview row to that notification therefore makes `MenuPreview = 1`
intermittently produce a flyout with commands but no preview. Insert the preview and its
separator while `QueryContextMenu` builds the child menu; keep `IContextMenu2`/`IContextMenu3`
handling only for owner-draw measure and paint. The integration regression must inspect the
submenu immediately after `QueryContextMenu`, before sending any synthetic lifecycle message.

### Fixed Settings pages must leave footer clearance

The Settings footer is anchored independently from each category's rows. A fixed page can
therefore grow into Save/Close without any layout error: General once ended exactly at that
boundary. Keep every fixed category at least 12 design pixels above `footer_y`. File Types is
the deliberate `ListFill` exception, and Right-click Menu is the deliberate `ListAuto`
exception whose checklist scrolls internally while Reset order stays visible. The debug
assertion in `settings_dlg/navrail.rs` guards the fixed pages; verify changed pages through
the light and dark `--shot --window settings --tab N` captures too.

### Two different symptoms, two different culprits

The earlier open question is now measured, in a process with no skin injected, forced into
dark mode with the `uxtheme` ordinals (`SetPreferredAppMode(2)` + `FlushMenuThemes`) so a
*dark themed* popup could be sampled without touching the reporting machine's shell:

| host | `hbmpItem` (what 1.3.2-1.3.6 shipped) | bitmap item (`MF_BITMAP`) | owner-drawn item |
| --- | --- | --- | --- |
| No menu skin, dark theme | **full tile, popup stays dark** | **full tile, popup stays dark** | full tile, **popup turns light** |
| StartAllBack skin (`explorer.exe`) | ~6 px sliver | ~6 px sliver | full tile, popup turns light |

Note the top-left cell: **on an unskinned machine the 1.3.2 code was never broken.** The
regression only ever existed under a menu skin, and the 1.3.7 owner-draw fix therefore
*costs* unskinned users their themed popup to repair skinned ones. That is the argument for
picking per host, and it also inverts the risk of doing so: with the **bitmap item as the
default** and owner-draw only on a positive skin match, an unrecognised skin falls back to
exactly today's behaviour rather than to something worse. A name list that can only add
fixes is safe; the earlier warning below assumed the opposite default.

So the two complaints have unrelated causes, and it is worth keeping them apart:

- **The sliver is the skin.** Windows sizes bitmap menu items from the bitmap. StartAllBack's
  measurement pass does not.
- **The white popup is Windows.** One owner-drawn item makes USER32 abandon the themed
  drawing path for the *entire* popup, including every other handler's items, and fall back
  to classic system colours. Reproduced with zero skin DLLs in the process: a menu of plain
  text items renders dark, and adding a single owner-drawn item to that same menu turns the
  whole thing light. The v1.3.1-era comment claiming this was right; it was only the *other*
  half of the 1.3.2 rationale that was wrong.

**Therefore the ideal build picks per host**, and this is now evidence-backed rather than
inferred: bitmap item when no menu skin is injected (tile **and** a dark themed popup, which
is better than any version has ever shipped), owner-draw when one is. Detection would be an
in-process `GetModuleHandleW` for `StartAllBackX64.dll`, `DarkMagicX64.dll`, ExplorerPatcher's
`ep_*.dll` and equivalents.

**The catch, before anyone builds it:** that detection is a name list, and a name list fails
silently in the wrong direction. A skin we have not heard of reads as "no skin", we insert a
bitmap item, and the user is back to the sliver with nothing in the logs. Whoever implements
this should decide deliberately how to handle the unknown-skin case rather than inherit this
paragraph's assumption.

Practical traps hit while splitting a monolith file into a directory module (the pattern
used for `settings_dlg/` and `preview/`, see §4) and while diagnosing preview-pane rendering.
Read this before doing either again.

**Splitting a file into `mod.rs` + siblings ("parent-hub" import model):**

- Import shape: each sibling file does `use super::*`; the parent `mod.rs` does a **private**
  `use child::*` re-import for each child (NOT `pub use`: a `pub use` of items that aren't
  themselves `pub` enough trips an "doesn't reexport anything public enough" lint). This avoids
  the `use super::*` glob-reexport `E0603` ("item is private") tangle that a naive split falls
  into.
- **The `pub(super)` widening trap:** when extracting a leaf module, everything it needs from
  the parent has to be widened to at least `pub(super)`. A blanket "widen anything the compiler
  complains about" pass reliably **misses three categories**: (1) statics/consts/thread_locals
  declared *inside a macro invocation* (the macro expansion hides them from a simple visibility
  grep), (2) **struct fields** (a struct can be `pub(super)` while its individual fields are
  still private, and the compiler error for that is easy to skim past), and (3) **inherent
  `impl` block methods** (hit 2026-07-31 splitting `preview/markdown.rs`: widening `struct Fonts`
  left `Fonts::new` and `Fonts::free` private, `E0624`). The rule that covers all three: widen
  the type AND its fields AND its inherent methods, as three separate passes.
  **But do NOT widen enum variants** -- a variant inherits its enum's visibility and a
  `pub(super)` qualifier on one is `E0449` ("visibility qualifiers are not permitted here"), a
  hard parse-adjacent error. A regex that widens "every indented item in the block" will happily
  corrupt an `enum` body; scope any such sweep to `struct` and `impl` blocks only.
- **The `super::` re-anchoring trap (hit again 2026-07-31, splitting `screenshot/overlay.rs`):**
  when the file you are splitting was ITSELF a child of something else, every `super::foo` in it
  meant *the grandparent* while the code lived in one file. Move that same line into a new leaf
  and `super::` now means the file you just split, so it silently stops resolving. The compiler
  does catch it (`cannot find X in super`), but the fix is not "widen a visibility": rewrite the
  path to `crate::<grandparent>::foo` (or re-import the name in the hub). Grep the extracted
  children for `super::` and check each one is still pointing where it was.
  **This is the reason to prefer `foo.rs` + `foo/` over converting to `foo/mod.rs`:** keeping the
  parent file where it is means `super::` inside IT stays correct and only the new leaves need
  auditing. It also sidesteps the `include_bytes!` depth change below entirely.
- **`pub(super)` re-anchors too - the VISIBILITY twin of the trap above (hit 2026-07-31 splitting
  `preview/highlight.rs` and `preview/window.rs`).** The bullet above is about `super::` in a
  *path*; this is the same word in a *visibility*, and it fails far more confusingly. An item
  already marked `pub(super)` in `preview/window.rs` means "visible to `preview`". Move it
  verbatim into `preview/window/zoom.rs` and it silently means "visible to `window`", so
  `preview::mod`'s existing callers stop seeing it. The error is **`E0603 "... is private"` at the
  CALLER**, naming an *import* rather than the item, which reads like a missing `pub use` and
  sends you editing the hub instead of the child. Fix: rewrite `pub(super)` to the absolute
  `pub(in crate::<parent>)` in every extracted item, and make the hub's re-export match it
  (`pub(in crate::preview) use zoom::*;`). The pre-existing `window/scroll.rs` was already written
  this way: copy a working sibling's visibility form rather than the parent's.
- **`include_bytes!` path breakage:** paths in `include_bytes!`/`include_str!` are relative to
  the *source file*, not the crate root. Moving a file one level deeper into a new subdirectory
  (e.g. `foo.rs` → `foo/bar.rs`) silently breaks any `include_bytes!("../asset.bin")`-style path
  in it; add the extra `../` the new depth requires. This fails at compile time with a missing-
  file error, but it's easy to miss in a large diff.
- **The const-shadowing-a-glob trap:** a local `const` in the original file that happened to
  shadow a name from a `windows::*` (or other) glob import stops being unambiguous once that
  file is split and the const gets re-exported through the parent-hub `use child::*`. The name
  now resolves to two candidates and becomes an ambiguity error at the *use site*, not at the
  definition site, which makes it confusing to trace. Keep any such shadowing workaround const
  in the core/parent file rather than moving it out to an extracted leaf.
- Verify a pure-move split by: a clean build (0 warnings), `cargo fix` to prune now-unused
  imports, the test suite, and a headless `--shot` capture compared byte-for-byte against a
  pre-split capture (identical bytes prove no behavior changed, not just "it compiles").
- Do this kind of refactor as one linear pass of deterministic edits, not as multiple
  concurrent automated edits to the same files: two independent editors racing on one crate's
  imports produces interleaved, half-applied edits that are hard to untangle.

**Reading rendered preview-pane pixels:**

- **ClearType subpixel fringing looks like syntax-highlight color and isn't.** Gray anti-aliased
  text rendered with ClearType shows faint orange/blue fringing at the subpixel level. A naive
  pixel sampler picking up that fringing can misread it as a syntax-highlight color and wrongly
  conclude a plain-text file is being colorized. Before trusting a pixel-sampled color as
  "highlighting," confirm the file's detected language/highlight mode independently (a `Plain`-
  classified file has no highlighter running at all, whatever a color sampler reports).

**Testing the full-screen screenshot editor with Windows UI automation:**

- **`WS_EX_TOOLWINDOW` makes the editor undiscoverable.** The Windows automation bridge first
  requires a visible, uncloaked top-level window, then rejects `WS_EX_TOOLWINDOW`; an ownerless
  window is otherwise accepted. The main capture editor therefore uses
  `WS_EX_TOPMOST | WS_EX_NOACTIVATE`, with its existing explicit foreground activation when
  launched. `WS_EX_NOACTIVATE` keeps the popup out of the taskbar by default without hiding it
  from automation. Do not apply this rule to the separate white-flash window: that click-through
  helper should remain a tool window.
- **Automated UI tests must use the exact hidden `--screenshot-automation` route.** It creates
  the class `SageThumbs2KShotAutomation` with a title beginning
  `SageThumbs 2K Screenshot Automation`, covering the complete virtual desktop with an opaque,
  synthetic canvas. It must never copy pixels from the live desktop. Keep it isolated from the
  normal `--screenshot` route and make both classes participate in the one-overlay-at-a-time
  guard.
- **The synthetic route is a privacy and side-effect boundary, not a demo switch.** Clipboard
  writes (including eyedropper hex), save/save-as, uploads/network access, persisted custom
  colours, and native colour/font dialogs must remain disabled there. Its optional test-only
  controls may expose deterministic state through the window title, but must not alter normal
  capture behavior.
- **The toolbar is owner-drawn, so automation has no semantic child buttons to query.** Drive the
  synthetic editor with its keyboard shortcuts and client coordinates after selecting exactly
  one matching class/title/process. Run the window-contract smoke test explicitly with
  `cargo test --test screenshot_automation -- --ignored --test-threads=1`; it is ignored during
  ordinary test runs because it intentionally opens an opaque topmost window over the virtual
  desktop.

## Decode the size you are going to SHOW, not the size the file happens to be

Three separate bugs in 1.7.3-1.7.4 were the same mistake: decoding at full size and
throwing most of it away. If you touch a decode tier, the question to ask is "what is the
smallest thing that satisfies this request", not "how do I decode this file".

Audited 2026-08-04 by thumbnailing ONE 76 MP image in every major format at 256 px, so the
numbers are directly comparable. Timings are best-of-three on an idle machine:

| format | time | reduced representation | using it? |
|---|---|---|---|
| exr | 0.11 s | scaled streaming read | yes, `exrscale.rs` |
| psd | 0.53 s | embedded composite preview | yes |
| tif | 0.95 s | pyramidal sub-IFD overviews | **no** (not urgent at this speed) |
| png | 1.45 s | none (interlacing is not usable here) | n/a |
| bmp | 1.65 s | none | n/a |
| jpg | 3.01 s | DCT scaling (1/2, 1/4, 1/8) | **no**, see below |
| jp2 | 0.25 s | wavelet resolution levels | yes, native decoder since 1.7.5 |
| webp | 4.08 s | none | n/a |
| dds | n/a | mipmap chain | yes, since 1.7.4 |

Camera RAW is already fine: it uses the embedded JPEG preview.

**JPEG is the open one, and it is deliberately open.** `zune-jpeg` (what `image` uses, and
what we ship) exposes no scaled decode. `jpeg-decoder` does, but adding it means shipping a
SECOND JPEG decoder purely for large files, against installer size budgets. The measured
pain is mild at real sizes: 0.37 s for a 12 MP phone photo, 0.71 s for a 24 MP camera
photo, and Explorer caches the result, so this is a size-versus-speed decision for a human,
not a quiet dependency addition. The 3.0 s figure above is a 76 MP outlier.

The embedded EXIF thumbnail path does NOT cover this: `EMBEDDED_MAX_REQUEST` is 96 px,
because an EXIF thumbnail is typically 160x120 and would be upscaled into anything larger.
That gate is correct; it just means requests above 96 px always do real work.

**Never trust a decoder's "reduced" flag without looking at the pixels.** Two independent
implementations of exactly this feature return correctly-SIZED output containing the WRONG
IMAGE. ImageMagick's `jp2:reduce-factor` and `oxigdal-jpeg2000`'s
`decode_region_at_resolution` both hand back a crop instead of a downscale, and both look
like a spectacular speedup until you render them. Compare against a full decode, visually,
every time.

## A WIC component may not return the pixel format you gave it

The direct cost of the optimisation above, and it shipped in 1.3.6 and was not caught until
2026-08-07. `wic.rs` converted the frame to `32bppRGBA`, then put an `IWICBitmapScaler` on the
end of that chain. `IWICBitmapScaler` makes no promise to preserve its source's pixel format,
and with `WICBitmapInterpolationModeFant` it does not: it hands back WIC's native **BGRA**.
Those bytes went straight into `RgbaImage::from_raw`, so **every scaled WIC decode had red and
blue transposed**: HEIC, AVIF, JPEG XR, which is to say every Explorer tile smaller than its
source. Nothing failed, nothing logged; skies just came out orange.

Two properties made it survive:

- **Only the thumbnail path took the scaler.** Full-fidelity callers pass `thumbnail_cx = None`,
  so Convert, Resize, image info and the preview pane were always right. "The folder view is
  wrong but everything else is fine" is the signature of a thumbnail-only branch.
- **The corpus regression could not see it.** The baseline was captured after the regression
  landed, so the swapped tiles WERE the baseline. A contact sheet proves "still renders", never
  "renders correctly". Colour needs a reference decoder, which is why the DICOM content checks
  exist and why `wic_thumbnail_scaling_keeps_rgba_channel_order` asserts exact channel values.

The rule: **re-assert the pixel format on whatever the chain actually produced**, do not infer it
from what you fed in. `ensure_rgba32` does this and is a no-op when the format is already right.
The same caution applies to any WIC component you bolt on later (rotator, clipper, colour
transform), not just the scaler.

Finding it took a boundary, not a stare: render one source at `--size 359` and `--size 360`
against a 360 px-wide file. Below the source size it swapped, at or above it did not. When a bug
appears "sometimes", look for the branch it correlates with before looking for a race.

## The magick watchdog must charge CPU time, not wall clock

Same investigation. The ImageMagick child had a 20 s **elapsed** kill-timeout, which quietly
conflates "this file will never finish" with "this machine is busy". Measured: an AVIF that
needs **0.34 s of CPU** was being killed at 20 s of wall clock while a batch AV1 encode kept it
unscheduled, a 60x margin. The log said `decode timed out (status Some(ExitStatus(0)))`, exit
code **0**, the child had already succeeded and we discarded its output, then fell through to
the next tier, which for AVIF is the WIC codec we deliberately route around.

So the budget is CPU time (`GetProcessTimes`, kernel + user) with a generous elapsed backstop for
a child that hangs without burning any. This is stricter, not looser: a looping child reaches
20 s of CPU sooner than it ever reached 20 s of wall clock. ImageMagick's own `-limit time` is
elapsed seconds, so it has to track the **backstop**; pinning it to the CPU budget lets magick
self-abort a starved decode from inside the child and the fix does nothing.

If you benchmark this, note that the obvious saturator lies: `ffmpeg -c:v libaom-av1 -cpu-used 0`
holds **1.4 GB per encoder**, so one per core exhausts a 63 GB machine and ImageMagick then fails
with `Memory allocation error @ error/heic.c`, which looks exactly like a decode regression and is
not one. Saturate with something memory-light.

## Do not cache the portable settings file on `(mtime, len)`

`settings.rs`'s `store` reads the portable `SageThumbs2K.ini` on every access, and that is
deliberate. The obvious optimisation is to stat the file and re-parse only when it changed,
keyed on modification time plus length. That key is broken for exactly the edits people make:
flipping `1` to `0`, or `512` to `256`, changes neither half. Combine that with a filesystem
timestamp granularity coarser than the gap between two quick writes and a real edit becomes
invisible, so the app keeps serving the old value with no way for the user to tell why.

It fails the way bad caches always do. `tests/portable_settings.rs` caught it only under the
full suite and passed in isolation, because the timing has to line up. That test now pins the
nasty case on purpose: the replacement is the same byte length as what it replaces and lands
immediately after the store's own write. If you are tempted to add the cache back, that test
is why you should not. There is no hot path to protect either: `thumb_settings` and
`menu_visibility` already take one snapshot per operation rather than reading per item.

## `preflight.ps1` must mirror EVERY step of the CI job it claims to mirror

The pre-push hook exists so a push cannot fail CI on something checkable locally, and it says
so: "Mirrors the GitHub CI `build-test` + `deny` jobs". On 2026-08-05 it printed
`PREFLIGHT PASSED - safe to push` on a commit CI then rejected, because it ran build, tests,
clippy and cargo-deny but not that job's last step, `cargo fmt --all --check`.

A gate that covers most of a job is worse than no gate, because it is trusted. If you add a
step to `build-test` in `ci.yml`, add it to `scripts/preflight.ps1` in the same position, or
change the comment to stop claiming a mirror it no longer is.

## Explorer's own settings lie to the registry, and the performance profile lies twice

Two traps that cost hours on a real machine (2026-08-05), both of which make a registry read
say "fine" while Explorer behaves as though it were not.

**Explorer keeps its own copy of `IconsOnly` in memory and writes it back on exit.** So the
obvious repair sequence silently fails: write `IconsOnly = 0`, restart Explorer, and the dying
process overwrites your `0` with its in-memory `1` before the new one starts and reads it back.
Every check you run afterwards reads `0` and looks correct. For Explorer's own view settings the
UI checkbox is the source of truth, not the key. Change it in Folder Options, or write the value
with Explorer stopped. The same write-back applies to shell bags (per-folder view state).

**"Adjust for best performance" owns that switch and keeps re-applying it.** Performance Options
turns off the "Show thumbnails instead of icons" visual effect, which IS `IconsOnly`. A machine
in that profile reverts the setting behind you, so the contradiction persists no matter how many
times it is "fixed". `doctor` reports `VisualFXSetting == 2` for this reason, and reports it even
when `IconsOnly` currently reads `0`, because that combination is the confusing one.

**Do NOT report on `VisualEffects\ThumbnailsOrIcon\DefaultApplied`.** It reads `1` on healthy
machines where thumbnails work fine; it means "this effect is at its profile default", not
"off". Flagging it would fail every working install, which is the same false-alarm mistake
issue #11 already cost us with `DisableThumbnailCache`.

Two more things `doctor` cannot see, so it prints them as guidance instead: a folder in Details,
List or Small icons view never draws thumbnails at all (Windows auto-classifies a folder of
documents that way), and the thumbnail cache keeps serving "this file has no thumbnail" for
every file it saw while the switch was off, so it needs clearing after any of these fixes.

## Cloud placeholders: the file is there, the bytes are not

A OneDrive Files-On-Demand placeholder has local metadata and no local content. The first read
pulls the whole file over the network, and for a thumbnail that read happens inside Explorer's
thumbnail host, where slow is indistinguishable from broken.

How bad it is depends entirely on how much of the file we need. Formats whose preview is baked
into the first bytes stay cheap, because `stream_source` reads a bounded prefix or seeks to a
cover. Formats with no embedded preview fall through to the whole-file read, so the entire image
has to come down before anything can be drawn. `.xcf` is the sharp edge and the one that got
reported: GIMP writes no embedded thumbnail, so there is nothing to read but the whole file.

`doctor <file>` reports the placeholder via `FILE_ATTRIBUTE_OFFLINE` /
`FILE_ATTRIBUTE_RECALL_ON_*` and points at "Always keep on this device". It deliberately does
NOT sniff the header to decide whether this particular format could be served from a prefix:
reading even the first bytes of a placeholder triggers the recall the check exists to warn
about. Behaviour is unchanged on purpose too: refusing to hydrate would take thumbnails away
from everyone whose cloud files are downloaded and working today.

## The release gate reads your changelog prose, and rejects four ordinary words

`Get-ReleaseChangelogSection` (scripts/release-manifest-lib.ps1) refuses to release when the
section for this version matches `TODO|TBD|PLACEHOLDER|CHANGEME`, case-insensitive, anywhere in
its text. The intent is to catch an unfinished section; the check cannot tell an unfinished
section from an ordinary sentence that happens to use one of those words.

1.8.5 tripped it describing an mp4 "placeholder track", which is simply what that kind of track
is called. The failure arrives at step [1/6], before anything is pushed or built, so it costs a
re-run and nothing worse. Reword the prose rather than loosening the guard: the guard is right
far more often than it is wrong, and every synonym is free.

## A size reference must name the artifact that SHIPPED, and each architecture drifts alone

Two separate traps in `packaging/size-budget.json`.

**Installers are not byte-reproducible.** Inno Setup output varies between builds of identical
source, so the reference recorded from a local build will not match the asset the release
pipeline uploads. 1.8.5's arm64 installer came out 14,793 bytes smaller on the release rebuild
than on the local one, with a different SHA-256, which left the policy naming a build nobody
could download. Record the PUBLISHED asset (download it and hash it), not your local `dist/`
copy. The Rust payload figure does not have this problem: it is reproducible, and both builds
reported it byte-identical.

**References drift per architecture.** x64 and arm64 are rebaselined independently, so one can
be several releases stale while the other is current. When 1.8.5 failed the arm64 installer gate
by 6,340 bytes, arm64 was still calibrated to 1.8.0 and x64 to 1.8.4: five releases of
already-reviewed growth measured against a stale point, not a fat release. The diagnostic that
settles it: check what the CURRENT-reference architecture did in the same release. x64 grew
5,150 bytes of installer and passed with 125,922 to spare, which is what proved the release was
innocent. Rebaseline the payload in the same edit when it is also near its allowance, or the
next release inherits a failure that has nothing to do with it either.

## Installing a dev build over a running Explorer: rename the DLL, do not kill Explorer

`sagethumbs2k.dll` is memory-mapped by `explorer.exe` the moment Explorer draws one
thumbnail, and a mapped PE cannot be overwritten: the copy fails with a sharing violation.

The obvious fix, kill Explorer then copy, is a race you lose about half the time. Windows
restarts Explorer within about a second, and the new instance re-maps the DLL before a
multi-file copy finishes, so the install fails partway through and leaves a mixed set of
files behind.

Do what the shipped installer does instead: `MoveFile` the in-use DLL to a throwaway name
beside itself, then copy the new one into the freed path. A rename of a mapped file is
allowed, unlike an overwrite; the old file stays on disk until its last mapping goes away
and can be swept on the next install. Nothing has to be killed, so there is no race to lose.
The stranded copies are real, not theoretical: one sweep removed 67 MB of them from
Program Files, left there by earlier kill-then-copy attempts.

## Running `cargo test` on a clean tree fails eight COM tests that are not broken

`tests/com_roundtrip.rs` loads the built `sagethumbs2k.dll` through `LoadLibrary` rather than
linking the crate, because the point is to exercise the real COM surface an Explorer would
call. `cargo test` builds test binaries; it does not build the cdylib, so on a fresh checkout,
after `cargo clean`, or after a version bump invalidates the artifact, all eight fail at once
with `cdylib not built` (or `LoadLibrary: 0x8007007E`). Run `cargo build` first. The suite is
green immediately afterwards, with no source change in between.

## Do not touch the working tree while `release.ps1` is running

The release builds its installers from the LOCAL working tree and records a provenance
manifest of what went into them. `check-release-manifest.ps1` then refuses to publish if that
manifest says the tree was dirty at build time, which is correct: an artifact built from
files that were being edited underneath it is not the commit it claims to be.

The clean-main guard runs at step [1/6], so a tree that is clean when the release STARTS
passes it, and then any edit during the ten-plus minutes of building both architectures
turns the artifacts unpublishable. 1.9.0's first attempt died exactly this way, at the
provenance gate, after both installers had already been built and size-checked.

Nothing is left half-done when it fails, which is the saving grace: no tag, no draft, no
release, because the gate sits BEFORE the draft is created. The cost is only the rebuild.
Start the release, then leave the tree alone until it prints `DONE`.

## SourceForge's default-download API lies about having taken effect

`set-sourceforge-default.ps1` PUTs the default installer for the green Download button, and
the API returns success while `best_release.json` keeps serving the PREVIOUS version for a
few minutes. The release script retries, reports "API reported success but best_release.json
still says …", and tells you to set it by hand. It usually does not need setting by hand:
re-run the script a few minutes later and it reports "already correct". Believe the second
reading, not the first.
