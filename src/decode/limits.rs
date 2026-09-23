/// Hard ceiling on either image edge (px). A 600-dpi A3 scan is ~14k px;
/// 16384 covers legitimate art/scans while keeping a single dimension
/// bounded. Shared by the `image` tier, the WIC tier, and the container
/// decoders (IW44/JB2) so "too tall/wide" means the same thing everywhere.
pub const MAX_DIM: u32 = 16_384;

/// Hard ceiling on total pixels (≈268 MP at MAX_DIM²). At 4 bytes/px that is
/// ~1 GiB of RGBA — the absolute worst case we'll let a decoder materialize.
/// Used as the WIC pixel cap and as the container area cap.
pub const MAX_PIXELS: u64 = (MAX_DIM as u64) * (MAX_DIM as u64);

/// Source-pixel ceiling for a WIC decode that SCALES on the way out — four times
/// [`MAX_PIXELS`], and the gap is not bravado. The two bound different things.
///
/// [`MAX_PIXELS`] answers "how much will we materialize", which is the right question
/// when the caller wants the whole image. Ask WIC for a 256 px thumbnail and the answer
/// stops depending on the source at all: the codec streams into `IWICBitmapScaler` and we
/// copy out `cx` squared. Measured on a 24000x14160 PNG (309 MB, 340 MP — a 4x upscale,
/// the kind of file this ceiling exists to have an opinion about): 2.1 s to a 256 px
/// thumbnail with NO measurable growth in the process working set. PNG has no
/// reduced-size mode, so that is the unfavourable case, not the flattering one.
///
/// What still needs a ceiling is a decompression bomb, whose cost tracks neither the file
/// size nor the output size — a few MB of nearly-incompressible-looking headers can declare
/// billions of pixels, and streaming them is cheap in MEMORY but not in TIME.
///
/// **The worst allowed case was measured, not estimated.** A hand-built 32000x32000 PNG
/// (1024 MP, just under this ceiling) costs 0.2 s when its rows are zeros, and **4.2 s**
/// when every row is Paeth-filtered over a non-trivial pattern — the adversarial shape,
/// since Paeth forces a per-byte predictor instead of a memcpy. 34000x34000 and 60000x60000
/// are refused at the header in under 0.1 s. Four seconds is well inside what this codebase
/// already tolerates from a hostile file (the ImageMagick tier carries a 20 s CPU budget),
/// and it buys real gigapixel panoramas rather than only the owner's 340 MP upscales.
///
/// **This ceiling is reachable ONLY from the isolated hosts.** It applies when a target edge
/// is supplied, and the in-process path that runs inside `explorer.exe` — the classic
/// context menu's preview tile, via `decode_menu_preview` -> `decode_cheap` -> `decode_any_with_wic_target` —
/// keeps the strict [`MAX_PIXELS`]/[`MAX_DIM`] guard and refuses these files at the header.
/// What withholds it is the `external` isolation flag, which decides the WIC target edge in
/// `cascade.rs` (`wic_cx = if external { wic_thumbnail_cx } else { None }`) and which
/// `decode_cheap` passes as `false`. That is the property that makes 4 s acceptable at all,
/// and it is pinned by `tests::the_in_process_menu_path_never_gets_the_widened_ceiling` rather
/// than left to the call graph's good behaviour.
pub const MAX_SCALED_SOURCE_PIXELS: u64 = 4 * MAX_PIXELS;

/// Per-decode allocation cap handed to the `image` crate's `Limits`. 512 MiB
/// bounds intermediate decode buffers well under MAX_PIXELS' ~1 GiB RGBA
/// surface.
///
/// RECONCILIATION NOTE (the documented WIC ~1 GiB vs image 512 MiB mismatch):
/// the `image` tier caps a single *allocation* at MAX_ALLOC = 512 MiB, while
/// the WIC tier caps *pixels* at MAX_PIXELS (~1 GiB of final RGBA). These are
/// deliberately different ceilings, not an oversight:
///   * `image` decodes in pure Rust inside OUR address space, may allocate
///     several transient buffers (palette expansion, row caches, the final
///     RGBA), and runs under `panic = "abort"` — so we keep its per-alloc
///     budget tight (512 MiB) to bound peak memory in the shell host.
///   * WIC hands back ONE already-decoded frame copied into a single RGBA
///     buffer we size ourselves (`stride * h`); the OS codec did its work in
///     its own memory. The meaningful guard there is "how many pixels will we
///     copy out", i.e. MAX_PIXELS. Its ~1 GiB worst case is a single, final,
///     short-lived buffer, not a multiplied transient, so the higher ceiling
///     is acceptable. We keep MAX_PIXELS (not 512 MiB) as the WIC ceiling so
///     huge OS-decodable formats (camera RAW, large HEIC) still thumbnail.
pub const MAX_ALLOC: u64 = 512 * 1024 * 1024;

/// Full-fidelity re-decode allocation cap, shared by the paths whose whole point
/// is keeping the real pixels: the PSD/PSB composite and the RAW re-read through
/// a name-selected coder (`decode_full_for_path`). The image is resized by
/// magick to FULL_FIDELITY_EDGE and re-decoded by the `image` tier; a near-
/// square image at that edge needs more than the default MAX_ALLOC, so this
/// OUR-own-resized-PNG case gets a matched, larger budget. See
/// `decode_psd_composite` for the agreement math.
pub const FULL_FIDELITY_MAX_ALLOC: u64 = 16_384 * 16_384 * 4 + (16 << 20);

/// ImageMagick `-resize` edge for full-fidelity decodes (shrink-only).
/// Kept at MAX_DIM so these paths and the bomb guard agree.
pub const FULL_FIDELITY_EDGE: &str = "16384x16384>";

/// Hard ceiling on the whole-file bytes we'll buffer in memory for ONE decode of a
/// file that ARRIVED AT US — an Explorer thumbnail, a preview pane, a CLI/MCP call
/// naming a path we did not choose. It is a DoS budget: the shell hands us whatever
/// the user happens to be browsing past, so the cost of the largest such file is a
/// cost we pay uninvited, and 256 MiB is comfortably more than any thumbnail needs.
pub const MAX_INPUT_BYTES: u64 = 256 * 1024 * 1024;

/// The same ceiling for a **user-initiated full-fidelity verb** — Convert, Resize,
/// Rotate, Strip, Combine — where the file is one the user picked and asked us to
/// process, and the answer they want is the whole picture.
///
/// Issue #34: this used to be [`MAX_INPUT_BYTES`], and a folder of Photoshop work
/// converted cleanly right up to 256 MiB and then stopped, with 502 MB documents
/// dropping out of a 60-file batch. The DoS reasoning above simply does not transfer.
/// Nobody browsed past a 502 MB PSD by accident; they selected it, chose a format, and
/// pressed Convert. A budget whose whole justification is "we did not ask for this
/// file" cannot be the one that refuses a file the user did ask for.
///
/// Why a ceiling at all, rather than none: the verb reads the document into one
/// contiguous buffer, and an allocation this crate cannot satisfy is an ABORT, not an
/// error — `panic = "abort"`, and the in-process fallback path can be inside
/// `explorer.exe`. `readers::read_full_fidelity` therefore reserves fallibly so a
/// machine that is merely short of memory reports it, and this number bounds what is
/// worth attempting in the first place.
///
/// 2 GiB because that is Photoshop's OWN limit: a `.psd` cannot exceed it, which is the
/// entire reason `.psb` exists. So every PSD ever written now converts, and the number
/// is one the format chose rather than one we invented. A genuinely larger `.psb` is
/// refused — with a message that says so, which is the half of this bug that was never
/// about the cap.
pub const MAX_FULL_FIDELITY_INPUT_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// ImageMagick subprocess resource caps. These are the SINGLE source for the
/// child's `-limit` CLI flags, the external kill-timeout ([`super::MAGICK_TIMEOUT`]),
/// and the shipped `scripts/packaging/imagemagick-policy.xml` (pinned by the
/// `magick_limits_match_policy_xml` test for policy.xml and `magick_time_limits_agree`
/// for the external timeout vs wall backstop). Tune here and all three stay in agreement.
/// CPU-TIME budget for one ImageMagick child — the real containment number. A decoder
/// stuck in a loop or grinding a decompression bomb burns CPU and is killed here.
///
/// This used to be a WALL-CLOCK budget, which conflated "this file will never finish"
/// with "this machine is busy". Measured on issue #9: the reporter's AVIF needs 0.34 s
/// of CPU, but while a batch AV1 encode saturated every core the same decode was still
/// unscheduled at 20 s of wall clock and got killed — dropping AVIF onto the WIC codec
/// we deliberately route around, so a busy machine produced wrong-coloured thumbnails
/// for some files and not others. Charging the budget to CPU keeps the guard strict for
/// hostile input (a spinning child hits 20 s of CPU sooner than it hits any wall clock)
/// while a starved-but-healthy child is left alone.
pub const MAGICK_CPU_SECS: u64 = 20;
/// Absolute WALL-CLOCK backstop, for a child that hangs without consuming CPU (blocked
/// on I/O rather than looping) — which [`MAGICK_CPU_SECS`] alone would never catch.
/// Deliberately generous: nothing legitimate approaches it, and every path that reaches
/// it is isolated in a throwaway host with its own caller-side budget on top.
pub const MAGICK_WALL_SECS: u64 = 120;
/// The same backstop for a user-chosen FULL-FIDELITY decode, which is a different job
/// with a different person waiting on it — see `magick::Fidelity` for the measurements.
pub const MAGICK_FULL_FIDELITY_WALL_SECS: u64 = 600;
/// `policy.xml`'s `time` ceiling, as a string.
///
/// ImageMagick's `-limit time` is ELAPSED seconds, and policy.xml is a CEILING the
/// command line cannot exceed — so this tracks the LONGEST wall backstop any caller
/// runs under ([`MAGICK_FULL_FIDELITY_WALL_SECS`]), while each child still passes its
/// own, tighter `-limit time` derived from that caller's budget (`add_magick_limits`).
/// Pinning the ceiling to the tile tier's 120 s instead would silently clamp every
/// full-fidelity decode back to it, which is the trap `magick_time_limits_agree`
/// now guards.
pub const MAGICK_POLICY_TIME_LIMIT: &str = "600";
pub const MAGICK_MEMORY_LIMIT: &str = "512MiB";
pub const MAGICK_MAP_LIMIT: &str = "1GiB";

/// The thumbnail-size setting's ceiling must stay under the decoders' own bomb guard, which is
/// the real technical limit; past it every raised request would be refused rather than honoured.
const _: () = assert!(crate::settings::THUMB_MAX < MAX_DIM);
