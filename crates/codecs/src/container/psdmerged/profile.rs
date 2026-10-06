//! The document's own colour profile, carrying what it stores to sRGB.
//!
//! Photoshop keeps the document's profile in resource 1039 and draws everything it shows,
//! its baked preview included, through it. Read without it, a CMYK document's inks became RGB
//! by plain arithmetic and a grey one showed its stored levels: the corpus's red came out
//! (234, 5, 3) where Photoshop shows (220, 42, 40), its grey 94 where Photoshop shows 115
//! (the Dot Gain 20% curve). Both are what a document with no baked preview thumbnailed as,
//! and what the Quick preview showed of every CMYK and grey document. An RGB document in a
//! wide space (Adobe RGB, Display P3) came out over-saturated the same way.
//!
//! A document with no profile, an unreadable one, or one in a space its mode does not use
//! keeps the old reading, so a profile can only ever correct a picture, never lose one.

use std::fmt;
use std::sync::Arc;

use image::DynamicImage;
use moxcms::{ColorProfile, DataColorSpace, Layout, Transform8BitExecutor, TransformOptions};

use super::{Depth, Mode};

/// Image resource 1039: the document's ICC profile.
pub(super) const ICC_PROFILE: u16 = 1039;

/// How a document's samples reach sRGB.
#[derive(Clone)]
pub(super) enum Profile {
    /// No profile this reads: the samples are shown as they are stored.
    None,
    /// An RGB profile, for the RGB the finished picture holds (RGB and Indexed documents).
    Rgb(Vec<u8>),
    /// The sRGB each of a grey document's 256 levels is.
    Grey(Box<[[u8; 3]; 256]>),
    /// A CMYK profile, for the inks.
    Cmyk(Arc<Transform8BitExecutor>),
}

impl fmt::Debug for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::None => "None",
            Self::Rgb(_) => "Rgb",
            Self::Grey(_) => "Grey",
            Self::Cmyk(_) => "Cmyk",
        })
    }
}

impl Profile {
    /// The profile `icc` for a `mode` document at `depth`. A 32-bit document's samples are
    /// linear light that `display_row` already carries through the sRGB curve, so its profile
    /// (a linear one) is not applied again.
    pub(super) fn of(mode: Mode, depth: Depth, icc: Option<&[u8]>) -> Self {
        let Some((icc, src)) = icc.and_then(|b| Some((b, ColorProfile::new_from_slice(b).ok()?)))
        else {
            return Self::None;
        };
        let to_srgb = |src_layout, dst_layout| {
            src.create_transform_8bit(
                src_layout,
                &ColorProfile::new_srgb(),
                dst_layout,
                TransformOptions::default(),
            )
            .ok()
        };
        match (mode, src.color_space) {
            _ if depth == Depth::Float => Self::None,
            (Mode::Rgb | Mode::Indexed, DataColorSpace::Rgb) => Self::Rgb(icc.to_vec()),
            (Mode::Grey, DataColorSpace::Gray) => to_srgb(Layout::Gray, Layout::Rgb)
                .and_then(|t| grey_levels(&*t))
                .map_or(Self::None, Self::Grey),
            // Four inks in, RGB out (moxcms lays CMYK out as `Rgba`): its `Cmyka` layout is
            // refused for the lookup-table profiles every real CMYK document carries
            // (`InvalidLayout`, moxcms 0.8.1), so transparency goes round the transform.
            (Mode::Cmyk, DataColorSpace::Cmyk) => {
                to_srgb(Layout::Rgba, Layout::Rgb).map_or(Self::None, Self::Cmyk)
            }
            _ => Self::None,
        }
    }

    /// A picture drawn in the document's RGB or grey, in sRGB. CMYK is carried over earlier,
    /// as its inks become RGB ([`Self::cmyka_to_rgba`]).
    pub(super) fn finish(&self, img: DynamicImage) -> DynamicImage {
        match self {
            Self::Rgb(icc) => crate::decode::apply_icc_to_srgb(img, Some(icc.clone())),
            Self::Grey(levels) => {
                let mut rgba = img.into_rgba8();
                for p in rgba.as_chunks_mut::<4>().0 {
                    let srgb = levels[usize::from(p[0])];
                    p[..3].copy_from_slice(&srgb);
                }
                DynamicImage::ImageRgba8(rgba)
            }
            Self::None | Self::Cmyk(_) => img,
        }
    }

    /// CMYK and transparency as Photoshop stores them, five bytes a pixel, each ink inverted
    /// (255 is none), as RGBA: through the profile, or by arithmetic when there is none (the
    /// black scaling what the other inks leave).
    pub(super) fn cmyka_to_rgba(&self, cmyka: &[u8]) -> Option<Vec<u8>> {
        let px = cmyka.as_chunks::<5>().0;
        let mut rgba = Vec::new();
        rgba.try_reserve_exact(px.len().checked_mul(4)?).ok()?;
        rgba.resize(px.len() * 4, 0);
        let out = rgba.as_chunks_mut::<4>().0;
        // A run at a time, so the transform's buffers stay small whatever the picture's size.
        for (px, out) in px.chunks(CMYK_RUN).zip(out.chunks_mut(CMYK_RUN)) {
            let rgb = match self {
                Self::Cmyk(t) => inks_to_rgb(&**t, px),
                _ => None,
            };
            for (i, (o, p)) in out.iter_mut().zip(px).enumerate() {
                o[..3].copy_from_slice(&match &rgb {
                    Some(rgb) => rgb[i],
                    None => stored_cmyk_to_rgb(p),
                });
                o[3] = p[4];
            }
        }
        Some(rgba)
    }
}

/// Pixels a CMYK transform takes at once.
const CMYK_RUN: usize = 4096;

/// Photoshop's inverted inks through `t`, which counts ink up from none.
fn inks_to_rgb(t: &Transform8BitExecutor, px: &[[u8; 5]]) -> Option<Vec<[u8; 3]>> {
    let inks: Vec<u8> = px
        .iter()
        .flat_map(|p| [0, 1, 2, 3].map(|c| 255 - p[c]))
        .collect();
    let mut rgb = vec![[0u8; 3]; px.len()];
    t.transform(&inks, rgb.as_flattened_mut()).ok()?;
    Some(rgb)
}

/// The arithmetic reading of one stored CMYK pixel: the black scales what the other inks leave.
fn stored_cmyk_to_rgb(p: &[u8; 5]) -> [u8; 3] {
    [0, 1, 2].map(|c| (u16::from(p[c]) * u16::from(p[3]) / 255) as u8)
}

/// The 256 grey levels through `t`, a grey-to-sRGB transform.
fn grey_levels(t: &Transform8BitExecutor) -> Option<Box<[[u8; 3]; 256]>> {
    let levels: Vec<u8> = (0..=255).collect();
    let mut rgb = [0u8; 256 * 3];
    t.transform(&levels, &mut rgb).ok()?;
    let mut out = Box::new([[0u8; 3]; 256]);
    for (o, px) in out.iter_mut().zip(rgb.as_chunks::<3>().0) {
        *o = *px;
    }
    Some(out)
}
