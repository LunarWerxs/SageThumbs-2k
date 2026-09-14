//! Encoder input normalization policy (#694).
//!
//! Ingest converts archival image files into the internal RGBA [`Pixmap`] used
//! by segmentation and encoding. See `docs/encoder-ingestion.md` in the repo
//! root for the supported input matrix.

/// How semi-transparent pixels are handled when an ingest path must produce
/// opaque RGBA (CLI: `djvu encode --background <COLOR>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AlphaCompositing {
    /// Keep the alpha channel unchanged. Downstream segmentation/encoding sees
    /// partial transparency as-is; no background colour is applied silently.
    #[default]
    Preserve,
    /// Composite onto a solid background at decode time.
    CompositeOnBackground { red: u8, green: u8, blue: u8 },
}

impl AlphaCompositing {
    /// Apply this policy to decoded RGBA pixels in place.
    ///
    /// [`Preserve`](Self::Preserve) is a no-op. `CompositeOnBackground`
    /// blends every non-opaque pixel onto the solid background with
    /// deterministic integer rounding and sets its alpha to 255:
    /// `out = (c·a + bg·(255 − a) + 127) / 255`.
    pub fn apply(&self, rgba: &mut [u8]) {
        let Self::CompositeOnBackground { red, green, blue } = *self else {
            return;
        };
        for px in rgba.as_chunks_mut::<4>().0 {
            let a = u32::from(px[3]);
            if a == 255 {
                continue;
            }
            for (c, bg) in px.iter_mut().zip([red, green, blue]) {
                *c = ((u32::from(*c) * a + u32::from(bg) * (255 - a) + 127) / 255) as u8;
            }
            px[3] = 255;
        }
    }
}

/// What to do when an input file embeds an ICC colour profile
/// (CLI: `djvu encode --icc <MODE>`).
///
/// DjVu has no container for ICC profiles and ingest applies no colour
/// management, so a profile can never survive into the output. The policy
/// makes that explicit instead of silent: `Ignore` decodes the pixel bytes
/// as-is (the long-standing default), `Reject` refuses profiled input with
/// an error naming the profile. A future `Transform` mode would need a
/// colour-management engine and is out of scope (#694).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IccHandling {
    /// Decode without colour management; the embedded profile is dropped.
    #[default]
    Ignore,
    /// Fail with an explicit error when the input embeds an ICC profile.
    Reject,
}

/// Deterministic reduction of >8-bit samples to 8-bit channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DepthDownconversion {
    /// Keep the high 8 bits of each big-endian 16-bit sample (`sample >> 8`).
    /// No dithering; repeatable across platforms.
    #[default]
    TruncateHighByte,
}

/// Policy knobs applied while decoding raster inputs into [`Pixmap`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IngestPolicy {
    pub alpha: AlphaCompositing,
    pub depth_downconversion: DepthDownconversion,
    pub icc: IccHandling,
}

impl IngestPolicy {
    /// Reduce one big-endian 16-bit channel sample to 8 bits.
    #[inline]
    pub fn downsample_u16_be(&self, hi: u8, lo: u8) -> u8 {
        let _ = (self.depth_downconversion, lo);
        hi
    }

    /// Reduce one native-endian 16-bit channel sample to 8 bits.
    #[inline]
    pub fn downsample_u16(&self, sample: u16) -> u8 {
        let _ = self.depth_downconversion;
        (sample >> 8) as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_preserves_alpha_and_truncates_depth() {
        let policy = IngestPolicy::default();
        assert_eq!(policy.alpha, AlphaCompositing::Preserve);
        assert_eq!(policy.icc, IccHandling::Ignore);
        assert_eq!(
            policy.depth_downconversion,
            DepthDownconversion::TruncateHighByte
        );
        assert_eq!(policy.downsample_u16_be(0x12, 0x34), 0x12);
    }

    #[test]
    fn preserve_leaves_pixels_untouched() {
        let mut rgba = [200, 100, 0, 128, 10, 20, 30, 0];
        AlphaCompositing::Preserve.apply(&mut rgba);
        assert_eq!(rgba, [200, 100, 0, 128, 10, 20, 30, 0]);
    }

    #[test]
    fn composite_blends_and_makes_opaque() {
        let white = AlphaCompositing::CompositeOnBackground {
            red: 255,
            green: 255,
            blue: 255,
        };
        // (c·a + bg·(255 − a) + 127) / 255 for a = 128 on white.
        let mut rgba = [200, 100, 0, 128, 10, 20, 30, 255];
        white.apply(&mut rgba);
        assert_eq!(rgba, [227, 177, 127, 255, 10, 20, 30, 255]);

        // Fully transparent becomes the background colour exactly.
        let blue = AlphaCompositing::CompositeOnBackground {
            red: 0,
            green: 0,
            blue: 255,
        };
        let mut rgba = [200, 100, 0, 0];
        blue.apply(&mut rgba);
        assert_eq!(rgba, [0, 0, 255, 255]);
    }
}
