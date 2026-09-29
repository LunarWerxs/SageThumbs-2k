//! AVIF: our own decoder, Rust end to end. The container is read here (`container`); the AV1
//! picture is decoded by `rav1d` and converted by our own code (`native`); what the file's colour
//! means for the screen then goes through the same ICC and HDR code every other format uses.
//!
//! This replaced Windows' AV1 codec (a Store extension behind WIC and Media Foundation) on
//! 2026-09-29. That codec needed three workarounds for colour it got wrong (issues #9, #39),
//! a probe of eight test pictures per process to decide which to apply, and it could not be
//! trusted with two requests at once: eight AVIF thumbnails asked for together deadlocked the
//! shell's thumbnail host, and with it every thumbnail and taskbar icon on the machine (a
//! user's report; reproduced through the real shell the same day). No AVIF reaches
//! WIC any more: a file this cannot decode goes to ImageMagick or the embedded-preview scan.
//!
//! WHERE THE DECODE RUNS. `rav1d` panics on some malformed input (the fuzz gate found one in
//! its first run: an `unwrap` in its own error path, still there upstream), and under
//! `panic = "abort"` a panic inside Explorer kills Explorer. So it gets the containment every
//! third-party video decoder here gets (`src/bin/vdec/`): the shell extension never links it and
//! asks the throwaway `st2k avif-frame` child instead, under the child's memory cap. The EXEs
//! (feature `av1`, via the root package's `avif-video`) and this crate's tests decode in
//! process, where a panic ends only that program.

use super::*;

mod container;
#[cfg(any(test, feature = "av1"))]
mod native;

pub(super) use container::is_avif;
use container::{Avif, Nclx};

/// Whether this build decodes AVIF in process (an EXE or the tests) rather than in the child.
pub(super) const DECODES_HERE: bool = cfg!(any(test, feature = "av1"));
/// Threads for one decode off Explorer's own process: AV1 decodes a whole picture, and four
/// threads took a 12 MP one from 336 ms to 103 ms (measured 2026-09-29).
pub(super) const THREADS_ISOLATED: u32 = 4;
/// The longest side the `st2k avif-frame` child hands back, whatever it is asked for: it
/// answers the shell's thumbnails and preview pane, never a full-fidelity conversion (those
/// run in the EXEs, which decode in process).
pub const AVIF_CHILD_MAX_EDGE: u32 = 4096;
/// Most AVIF bytes handed to the child (and what it will read from stdin).
pub const AVIF_CHILD_INPUT_CAP: usize = limits::MAX_INPUT_BYTES as usize;
/// CPU the child may spend: a 12 MP picture takes ~0.4 s of it on four threads.
#[cfg(not(any(test, feature = "av1")))]
const AVIF_CPU_BUDGET: Duration = Duration::from_secs(10);
/// Wall-clock backstop for a child that stops using CPU without finishing.
#[cfg(not(any(test, feature = "av1")))]
const AVIF_WALL_CEILING: Duration = Duration::from_secs(30);
/// Largest PNG accepted back: a 4096-px RGBA picture that did not compress at all.
#[cfg(not(any(test, feature = "av1")))]
const AVIF_PNG_CAP: usize = (AVIF_CHILD_MAX_EDGE as usize).pow(2) * 4 + (1 << 20);

fn fail(why: &str) -> Error {
    Error::new(E_FAIL, format!("avif: {why}"))
}

/// Decode an AVIF for display, fitted within `edge` px when one is given: in process where this
/// build carries the decoder, otherwise through the `st2k avif-frame` child.
pub(super) fn decode_avif(bytes: &[u8], threads: u32, edge: Option<u32>) -> Result<DynamicImage> {
    #[cfg(any(test, feature = "av1"))]
    {
        native::decode_avif(bytes, threads, edge)
    }
    #[cfg(not(any(test, feature = "av1")))]
    {
        let _ = threads;
        via_child(bytes, edge)
    }
}

/// [`decode_avif`] in THIS process, for the `st2k avif-frame` child: reduced toward `max_edge`
/// when one is given (by a power of two, never below the edge).
#[cfg(feature = "av1")]
pub fn decode_avif_here(bytes: &[u8], threads: u32, max_edge: Option<u32>) -> Result<DynamicImage> {
    native::decode_avif(bytes, threads, max_edge)
}

/// Ask the `st2k avif-frame` child for the picture (see the module doc for why it is a child).
#[cfg(not(any(test, feature = "av1")))]
fn via_child(bytes: &[u8], edge: Option<u32>) -> Result<DynamicImage> {
    if bytes.len() > AVIF_CHILD_INPUT_CAP {
        return Err(fail("the file is larger than the child reads"));
    }
    let edge = edge
        .unwrap_or(AVIF_CHILD_MAX_EDGE)
        .clamp(1, AVIF_CHILD_MAX_EDGE);
    let png = crate::flv::child_frame_png(
        "avif-frame",
        bytes,
        &[edge.to_string()],
        AVIF_CPU_BUDGET,
        AVIF_WALL_CEILING,
        AVIF_PNG_CAP,
    )
    .ok_or_else(|| fail("the st2k avif-frame child produced no picture"))?;
    // Our own child's output, but still checked before the pixels are decoded, and after.
    let (w, h) =
        image::ImageReader::with_format(std::io::Cursor::new(&png), image::ImageFormat::Png)
            .into_dimensions()
            .map_err(|e| fail(&e.to_string()))?;
    if w == 0 || h == 0 || w.max(h) > AVIF_CHILD_MAX_EDGE {
        return Err(fail("the child declared out-of-bounds dimensions"));
    }
    image::load_from_memory_with_format(&png, image::ImageFormat::Png)
        .map_err(|e| fail(&e.to_string()))
}

fn is_hdr(colour: Nclx) -> bool {
    cicp::is_hdr_transfer(colour.transfer)
}

/// The HDR signal of an ISOBMFF picture (AVIF or HEIC): its primary item's `nclx` (or its first
/// tile's, for a grid) when that names a PQ or HLG transfer, in the shape the PNG `cICP`
/// conversion takes. For what ImageMagick hands back: it decodes an HDR AVIF/HEIC to its raw
/// transfer-encoded signal, which shown as sRGB is a dark, flat picture (issue #39).
pub(super) fn isobmff_hdr_cicp(bytes: &[u8]) -> Option<cicp::PngCicp> {
    let file = Avif::parse(bytes)?;
    let id = file.primary;
    let colour = file
        .nclx(id)
        .or_else(|| file.grid(id).and_then(|g| file.nclx(*g.tiles.first()?)))?;
    is_hdr(colour).then_some(cicp::PngCicp {
        primaries: u8::try_from(colour.primaries).ok()?,
        transfer: colour.transfer as u8,
        full_range: colour.full_range,
    })
}

/// What the always-on fuzz harness drives: the container walk (the part of this that runs in
/// the shell extension). The AV1 decode is contained by the child instead; see the module doc.
#[cfg(test)]
pub(crate) mod fuzzapi {
    use super::*;

    pub(crate) fn container(b: &[u8]) {
        if let Some(file) = Avif::parse(b) {
            let id = file.primary;
            let _ = (
                file.kind(id),
                file.nclx(id),
                file.icc(id),
                file.grid(id),
                file.data(id),
            );
            if let Some(alpha) = file.alpha_for(id) {
                let _ = (file.premultiplied(id, alpha), file.data(alpha));
            }
        }
    }
}
