//! SolidWorks `.sldprt` / `.sldasm` / `.slddrw`: the `PreviewPNG` stream of the OLE compound
//! file, the same preview the SolidWorks Document Manager API hands out and the one Linux
//! desktops extract with `gsf cat file.SLDPRT PreviewPNG`. Verified 2026-09-17 on assemblies
//! from GitHub, which are OLE and carry a PNG in that stream.
//!
//! ⛔ NEWER FILES ARE NOT OLE. Parts and drawings saved by recent SolidWorks releases (every
//! 2015+ file sampled) start with a random u32 and `00 00 00 04` and contain no compound-file
//! signature, no `PreviewPNG` name and no PNG anywhere: the whole document is wrapped in a
//! format only SolidWorks reads. Those return `None` here (the stock icon), which is honest; a
//! machine with SolidWorks installed has its thumbnail handler anyway. The corpus keeps one
//! such part under `sample-2015plus-wrapped.sldprt`, in `_expected-fail.txt`, so a gate never
//! reads "no thumbnail" for it as a regression.

use super::ole;

pub fn looks_like_solidworks(head: &[u8]) -> bool {
    ole::looks_like_ole(head)
}

/// The `PreviewPNG` stream as PNG bytes, or `None` when this is not an OLE file, has no such
/// stream, or the stream is not a raster the decode tiers accept.
pub fn extract(bytes: &[u8]) -> Option<Vec<u8>> {
    if !looks_like_solidworks(bytes) {
        return None;
    }
    super::util::decodable_image(ole::read_stream(bytes, "PreviewPNG")?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The corpus-driven assertion: a real assembly yields a decodable PNG, and the wrapped
    /// 2015+ part yields nothing rather than something wrong. Skips where the corpus is absent.
    #[test]
    fn a_real_assembly_yields_its_preview_and_a_wrapped_part_yields_none() {
        let dir = std::path::Path::new("../test-corpus");
        if let Ok(bytes) = std::fs::read(dir.join("sample.sldasm")) {
            let png = extract(&bytes).expect("assembly preview");
            let img = image::load_from_memory(&png).expect("png");
            assert!(
                img.width() >= 64 && img.height() >= 64,
                "{}x{}",
                img.width(),
                img.height()
            );
        }
        if let Ok(bytes) = std::fs::read(dir.join("sample-2015plus-wrapped.sldprt")) {
            assert!(
                !looks_like_solidworks(&bytes),
                "the wrapped format is not OLE"
            );
            assert!(extract(&bytes).is_none());
        }
    }

    /// The two refusals, WITHOUT the corpus, so CI exercises them too: the test above skips
    /// entirely on a runner (the corpus is a gitignored sibling directory), which left the
    /// only always-on coverage of this reader as the fuzz target's "does not panic".
    ///
    /// Both cases are ones a user really has: an OLE document from another application that
    /// happens to reach this reader, and a SolidWorks file whose `PreviewPNG` stream holds
    /// something that is not a raster. Neither may return bytes the decode tiers would then
    /// fail on - the stock icon is the honest answer.
    #[test]
    fn an_ole_file_without_a_usable_preview_stream_is_declined() {
        // A structurally valid OLE compound file with no `PreviewPNG` stream at all, taken
        // from the fuzz seed set so this test and the fuzzer share one definition of "OLE".
        let plain_ole = crate::container::fuzzseed::seeds()
            .into_iter()
            .find(|(name, _)| *name == "ole")
            .expect("the fuzz seed set must carry an `ole` entry")
            .1;
        assert!(
            looks_like_solidworks(&plain_ole),
            "the seed must be OLE, or this proves nothing"
        );
        assert!(
            extract(&plain_ole).is_none(),
            "an OLE file with no PreviewPNG stream must decline"
        );

        // Truncated and garbage inputs take the same route as a corrupt file on disk.
        assert!(extract(&[]).is_none());
        assert!(extract(b"not an ole file at all").is_none());
        for n in 0..plain_ole.len().min(2048) {
            assert!(extract(&plain_ole[..n]).is_none());
        }
    }
}
