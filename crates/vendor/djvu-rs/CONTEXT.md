# djvu-rs

Pure-Rust tooling for the DjVu document format: reading, rendering, converting,
and creating DjVu files, delivered through several consumption channels.

## Language

### Documentation & audience

**Reader**:
The primary audience of the README — a person or an LLM agent who arrives with
a DjVu task (convert, extract text, render, create) and must decide within
seconds whether this project covers it.
_Avoid_: user, developer (too narrow — the Reader may not write Rust at all)

**Task**:
A user-level goal expressed in outcome terms (e.g. "DjVu → PDF", "extract
text", "show DjVu in a browser"), as opposed to a codec capability (e.g. "IW44
wavelet decoder") which is an implementation detail behind it.
_Avoid_: feature (ambiguous — collides with Cargo feature flags)

**Channel**:
A way to consume the project: Rust crate, CLI binary, WebAssembly bindings, or
Python bindings. All channels ship from this one repository and share the crate
version.
_Avoid_: binding, target, platform

**Format coverage**:
The chunk-level statement of which DjVu format elements the project decodes
and/or encodes (Sjbz, BG44, NAVM, …). Expert-facing detail, distinct from
Tasks; lives in `docs/format-coverage.md`, linked from the README.
_Avoid_: features, codec list

**Limitation**:
An explicit, verifiable statement of something the project cannot do yet,
collected in one README section rather than scattered through prose. As
valuable to a Reader (especially an LLM agent) as a capability claim.
_Avoid_: known issue, caveat (scattered fine-print style)

**Doc-sync test**:
A regular `#[test]` that fails when the docs drift from the code: compiled
README and `docs/guide.md` examples (doctest gate), a roll call of CLI
subcommands/flags in `docs/cli.md`, and a roll call of Cargo feature flags in
the README.
_Avoid_: pre-commit check (it runs in the ordinary test suite, not a separate
hook framework)

### Document structure

**Catalog**:
The `DIRM` directory of a multi-page `FORM:DJVM`: the ordered list of its
components (pages, shared components, thumbnails). One catalog assembler turns
it into pages for every sync loader; only the component source differs
(bundled bytes, a name resolver, a typed resolver).
_Avoid_: directory (ambiguous with a file-system directory), manifest

**Shared dictionary**:
The `Djbz` symbol dictionary of a shared `FORM:DJVI` component. A page names
the components it includes with `INCL` chunks; the first included component
that holds a `Djbz` is the page's shared dictionary.
_Avoid_: shared dict (in prose), DJVI (that is the form, not the dictionary)

### Rendering

**Native size**:
The page size stored in its INFO chunk, before the INFO rotation. The
compositor works in this orientation; `RenderOptions.width/height` are in it.
_Avoid_: page size (ambiguous for a rotated page)

**Display size**:
The page size after the INFO rotation (sides swapped for a quarter turn): the
size of the returned pixmap and the size a caller asks for (DPI, fit, exact).
The `render_size` module is the one place that turns a display-size request
into a native buffer size.
_Avoid_: output size, effective size
