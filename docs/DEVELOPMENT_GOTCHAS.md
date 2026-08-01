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
