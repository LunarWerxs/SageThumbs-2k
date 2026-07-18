# Development Gotchas

Hard-won traps in this codebase. Each one cost real debugging time; none is
obvious from reading the code.

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
  complains about" pass reliably **misses two categories**: (1) statics/consts/thread_locals
  declared *inside a macro invocation* (the macro expansion hides them from a simple visibility
  grep), and (2) **struct fields** (a struct can be `pub(super)` while its individual fields are
  still private, and the compiler error for that is easy to skim past). Check both explicitly,
  don't assume "the struct is visible" means "the fields are too."
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
