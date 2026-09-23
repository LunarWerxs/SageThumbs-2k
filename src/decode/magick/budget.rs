//! The CPU, memory and time budgets a magick child runs under, by fidelity.

use super::*;

/// Apply our shared ImageMagick resource caps (memory / map / time) to `cmd`. One
/// place so the decode and encode subprocess paths can't drift, and so the values
/// stay tied to [`limits`] (and, via the tests, to `policy.xml`).
///
/// `wall` is the caller's own wall-clock backstop, and `-limit time` is DERIVED from it
/// rather than pinned beside it. ImageMagick's limit is documented as elapsed seconds, so a
/// fixed string lower than the caller's backstop would let the child self-abort a decode the
/// caller was still happy to wait for - which is precisely the drift that shipped when the
/// full-fidelity paths raised their memory and hand-back caps and left everything else at
/// the tile tier's values.
pub(in super::super) fn add_magick_limits(cmd: &mut Command, wall: Duration) {
    let time_limit = wall.as_secs().to_string();
    cmd.args([
        "-limit",
        "memory",
        limits::MAGICK_MEMORY_LIMIT,
        "-limit",
        "map",
        limits::MAGICK_MAP_LIMIT,
        "-limit",
        "time",
        &time_limit,
    ]);
}

/// Metafiles are untrusted vector programs rather than ordinary raster input.
/// Keep their ImageMagick child especially small: a normal Office/Visio preview
/// renders in a fraction of a second, while malformed or enormously complex WMF
/// and EMF content can otherwise consume the general-purpose 512 MiB / 20 s
/// budget merely to produce a useless frame. These are deliberately command-line
/// overrides, after [`add_magick_limits`], so they constrain only this decode
/// invocation and do not weaken the broader Magick policy or raster/PSD support.
///
/// 192 MiB, not the 96 it was: an ordinary Excel file's thumbnail WMF (the corpus's
/// `real.xls`) renders at 3832x2153 before it is shrunk, and at the preview pane's 1024 px the
/// resize no longer fit in 96, spilled the pixel cache to disk and took 11.5 s, past the 3 s
/// CPU budget, so the pane stayed blank; in 128 MiB it takes 0.4 s (measured 2026-09-23). The
/// CPU budget below is what stops a hostile program, not this.
pub(super) const METAFILE_MAGICK_MEMORY_LIMIT: &str = "192MiB";

pub(super) const METAFILE_MAGICK_MAP_LIMIT: &str = "192MiB";

/// Metafile CPU budget, and the elapsed-time backstop that goes with it. Same split as the
/// general-purpose pair (see [`limits::MAGICK_CPU_SECS`]): 3 s of CPU still kills a complex
/// or malformed WMF/EMF exactly as before, while the wider elapsed allowance keeps a busy
/// machine from failing a metafile that only needed a fraction of a second of real work.
pub(super) const METAFILE_MAGICK_TIME_LIMIT: &str = "18";

pub(super) const METAFILE_MAGICK_TIMEOUT: Duration = Duration::from_secs(18);

pub(super) const METAFILE_MAGICK_CPU_BUDGET: Duration = Duration::from_secs(3);

/// How often the watchdog wakes to re-check the child while waiting for its output.
pub(super) const WATCHDOG_SLICE: Duration = Duration::from_millis(250);

/// The two limits one magick child runs under: CPU time is the real budget, elapsed time
/// only the backstop for a child that hangs without burning any.
#[derive(Clone, Copy)]
pub(super) struct MagickBudget {
    pub(super) cpu: Duration,
    pub(super) wall: Duration,
}

/// Ordinary raster decodes.
pub(super) const RASTER_BUDGET: MagickBudget = MagickBudget {
    cpu: MAGICK_CPU_BUDGET,
    wall: MAGICK_TIMEOUT,
};

/// Metafiles, which get a much tighter CPU budget (see [`METAFILE_MAGICK_CPU_BUDGET`]);
/// [`add_metafile_magick_limits`] sets their memory/map/elapsed caps.
pub(super) const METAFILE_BUDGET: MagickBudget = MagickBudget {
    cpu: METAFILE_MAGICK_CPU_BUDGET,
    wall: METAFILE_MAGICK_TIMEOUT,
};

/// Who is waiting for this magick child. The work is identical; the BUDGET is not, because
/// the two callers are not in the same situation.
///
/// [`Fidelity::Tile`] is Explorer browsing past a file nobody asked about, in a host that
/// must stay responsive - 20 s of CPU is already generous there.
///
/// [`Fidelity::Full`] is the user having picked this exact file for Convert, Resize or Image
/// info and watching a progress bar with a Cancel button on it. Issue #41 is what the tile
/// budget did to that case: a folder of 5464x8192 photographs "converted, resized to fit
/// 1920x1080" into 107x160 files, because the composite was killed at 20 s of CPU and the
/// last-resort tier then carved out the file's own embedded ~160 px preview. Measured here
/// on flat PSDs, one thread-summed CPU figure per document (the whole reason the cliff moves
/// from machine to machine: the same work costs more CPU-seconds on a slower core, and more
/// threads mean more of them per second):
///
/// ```text
///   4433x5906    26 MP   10.5 s   <- converts
///   5147x6737    35 MP   13.5 s   <- converts here; the reporter's machine fails HERE
///   5464x8192    45 MP   21.6 s   <- over the 20 s tile budget
///   9000x9000    81 MP   38.8 s
///   12000x12000 144 MP   64.5 s
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Fidelity {
    Tile,
    Full,
}

/// CPU budget for a user-chosen full-fidelity decode. ~0.45 s of CPU per megapixel above,
/// so [`limits::MAX_DIM`] squared (268 MP, the largest picture this product will materialize
/// at all) lands near 120 s on this machine; 180 s leaves the same headroom for a core half
/// as fast. A hostile file is still bounded - by this, by the memory and hand-back caps, and
/// by the caller's own progress dialog.
pub(super) const FULL_FIDELITY_MAGICK_CPU_BUDGET: Duration = Duration::from_secs(180);

/// Wall backstop for the same child, for one that hangs without burning CPU. It has to sit
/// above the wall time that budget can legitimately take (144 MP measured at 67.8 s of wall
/// here, and a slow disk reading a 750 MB document adds to it), or the backstop would kill
/// exactly the decode the CPU budget was raised to allow.
pub(super) const FULL_FIDELITY_MAGICK_TIMEOUT: Duration =
    Duration::from_secs(limits::MAGICK_FULL_FIDELITY_WALL_SECS);

/// The full-fidelity pairing (see [`Fidelity`]).
pub(super) const FULL_FIDELITY_BUDGET: MagickBudget = MagickBudget {
    cpu: FULL_FIDELITY_MAGICK_CPU_BUDGET,
    wall: FULL_FIDELITY_MAGICK_TIMEOUT,
};

/// The budget one child runs under: the metafile clamp first (an untrusted vector program is
/// tight whoever asked for it), then the caller's fidelity.
pub(super) fn budget_for(fidelity: Fidelity, is_meta: bool) -> MagickBudget {
    match (is_meta, fidelity) {
        (true, _) => METAFILE_BUDGET,
        (false, Fidelity::Tile) => RASTER_BUDGET,
        (false, Fidelity::Full) => FULL_FIDELITY_BUDGET,
    }
}

pub(super) fn add_metafile_magick_limits(cmd: &mut Command) {
    cmd.args([
        "-limit",
        "memory",
        METAFILE_MAGICK_MEMORY_LIMIT,
        "-limit",
        "map",
        METAFILE_MAGICK_MAP_LIMIT,
        "-limit",
        "time",
        METAFILE_MAGICK_TIME_LIMIT,
    ]);
}
