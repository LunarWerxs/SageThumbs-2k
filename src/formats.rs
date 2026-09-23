//! The image formats SageThumbs 2K hooks — extension + friendly name.
//!
//! Curated from ImageMagick's readable raster formats plus the ones our safe
//! `image`/WIC/resvg tiers handle. This drives BOTH the per-extension
//! registration (`register.rs`) and the Options format checklist (`bin/app.rs`),
//! and the menu's `is_image` gate (`verbs.rs`).
//!
//! Decoding is content-sniffed and tiered (image → WIC → ImageMagick), so this
//! is simply the set of extensions Explorer will ask us to thumbnail; an
//! extension we can't actually read just falls back to the file's default icon.
//!
//! FORMATS is ordered by category (Images, then Camera RAW, then Ebooks &
//! comics) so the Options list groups naturally; `category()` classifies an
//! extension and `category_label()` names it for the list's Category column.

/// Coarse category for grouping the Options format list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Category {
    Image = 0,
    Raw = 1,
    Ebook = 2,
    Document = 3,
    Audio = 4,
    Video = 5,
    Archive = 6,
}

// The non-Image membership lists, the second copy of the category data that
// `FORMATS`'s section grouping also encodes. Module-scoped (not inlined in
// `category()`) so the test module can assert them against `FORMATS` directly —
// keeping this the single place the lists live, instead of a mirrored copy.
// Each MUST stay a subset of `FORMATS` (enforced by `category_lists_are_subset_of_formats`).
const EBOOK_EXTS: &[&str] = &[
    "azw", "azw3", "cb7", "cbr", "cbt", "cbz", "epub", "fb2", "fbz", "mobi", "phz", "prc",
];
const DOCUMENT_EXTS: &[&str] = &[
    "pdf", "djv", "djvu", "odt", "ods", "odp", "odg", "odf", "ott", "ots", "otp", "pptx", "pptm",
    "potx", "key", "pages", "numbers", "indd", "indt", "vsdx", "vsdm", "vsd", "pub", "ggb",
    // Microsoft Word / Excel / PowerPoint (OOXML packages + legacy OLE compound docs).
    "docx", "docm", "dotx", "dotm", "doc", "dot", "xlsx", "xlsm", "xlsb", "xltx", "xltm", "xls",
    "xlt", "ppsx", "ppsm", "potm", "ppt", "pps", "pot",
];
const AUDIO_EXTS: &[&str] = &[
    "mp3", "flac", "ogg", "oga", "opus", "spx", "m4a", "m4b", "aac", "wma", "ape", "wv", "mpc",
    "wav", "aiff", "aif", "aifc", "dsf",
];
const RAW_EXTS: &[&str] = &[
    "3fr", "arw", "cr2", "cr3", "crw", "dcr", "dng", "erf", "fff", "iiq", "k25", "kdc", "mdc",
    "mef", "mos", "mrw", "nef", "nrw", "orf", "pef", "raf", "rw2", "rwl", "sr2", "srf", "srw",
    "x3f",
    // MysticThumbs-parity additions (must mirror the Camera RAW block in FORMATS).
    "bay", "cap", "dcs", "drf", "ori", "ptx", "pxn",
];
// Video — a frame is grabbed via the OS Media Foundation codecs (no bundled bytes),
// streamed from disk. MF decodes what the OS has a codec for; the rest keep their
// default icon — except the codecs we decode ourselves out of process (FLV's VP6 /
// Sorenson, VP9 Profile 2/3, and MPEG-1/2 in program, elementary AND transport streams:
// `mpg`, `mpeg`, `m1v`, `m2v`, `vob`, `ts`, `m2ts`, `mts`). Must mirror the Video block in
// FORMATS.
const VIDEO_EXTS: &[&str] = &[
    "mp4", "m4v", "mov", "qt", "mkv", "webm", "avi", "wmv", "asf", "flv", "f4v", "mpg", "mpeg",
    "m1v", "m2v", "mpv", "mp2v", "m2p", "3gp", "3g2", "ts", "m2ts", "mts", "vob", "ogv", "divx",
];
/// The video extensions whose thumbnail does NOT depend on an OS codec: our own decoders
/// answer them. FLV's VP6 / Sorenson Spark (`st2k flv-frame`) and the MPEG family — MPEG-1
/// system streams and bare elementary streams have no Media Foundation source on any
/// Windows, and MPEG-2 program streams only with the Store extension, so all five decode
/// through `st2k mpeg-frame` when MF declines (2026-09-17). `capability()` answers at the
/// EXTENSION level (like every other field here): an H.264 FLV still rides Media Foundation
/// first, and `st2k doctor`'s per-file `video_codec_note` is the byte-accurate answer for
/// one file.
///
/// ⚠ `ts` / `m2ts` / `mts` are deliberately NOT here even though `mpeg12` learned transport
/// streams the same day. The overwhelmingly common content in those containers is H.264
/// (every AVCHD camcorder, every modern recorder), which Windows decodes itself and which
/// this tier declines on purpose — so claiming "needs no OS codec" for the whole extension
/// would be false for most files carrying it. MPEG-2 inside one now works without the Store
/// extension, and `doctor` says so per file; the blanket claim stays pessimistic, which is
/// the safe direction for a promise.
const SELF_DECODED_VIDEO_EXTS: &[&str] = &[
    "flv", "mpg", "mpeg", "m1v", "m2v", "mpv", "mp2v", "m2p", "vob",
];
// Generic archives — thumbnail = the contained images (first image, or the up-to-4
// contact sheet per Settings). Deliberately ONLY the big three: the zip-in-disguise
// long tail (jar/apk/appx/…) would mostly surface a random bundled icon as its
// "cover", which reads as noise, and the Quick preview already lists those. An
// archive with no readable image keeps its stock icon. Must mirror the Archives
// block in FORMATS. These also gate the context-menu OFF (`is_archive`): the image
// verbs (Convert/Rotate/…) would act on the extracted cover, not the archive —
// surprising, so v1 keeps archives thumbnail-only.
const ARCHIVE_EXTS: &[&str] = &["7z", "rar", "zip"];

/// Extensions SageThumbs hooked in PAST versions but dropped in the 2026-06-11 triage
/// (unrenderable). They are NOT in `FORMATS`, so the normal register/unregister
/// loops never touch their keys — an upgrade or uninstall would otherwise leave OUR stale
/// thumbnail/preview `shellex` hooks behind on any machine that ran an older build. `register()`
/// and `unregister()` sweep this list to clean those orphans. MUST stay disjoint from `FORMATS`
/// (enforced by `removed_extensions_disjoint_from_formats`). NOTE: `mpc` is NOT here — the
/// Magick-Pixel-Cache `.mpc` was dropped, but `.mpc` is now LIVE as Musepack audio.
pub const REMOVED_EXTENSIONS: &[&str] = &[
    "aai", "art", "avs", "cache", "hrz", "ipl", "mtv", "palm", "six", "jpt", "fax", "g3", "g4",
    "otb", "wbmp", "rgb", "pct", "pict",
    // jbig: removed 2026-07-08 (see the FORMATS comment above); listed here so
    // register()/unregister() still sweep the stale shellex hook on machines that
    // ran a build where it was registered.
    "jbig",
    // pes: removed 2026-09-17, and it never worked in a SHIPPED build. ImageMagick's PES
    // coder draws embroidery stitches by generating SVG and rendering it through RSVG, and
    // this product's ImageMagick bundle deliberately omits that whole stack (rsvg, cairo,
    // pango, harfbuzz - see docs/MAGICK.md "Reviewed omissions": "The SVG stack is handled
    // by resvg"). Without it magick falls back to an external `rsvg-convert` delegate that
    // is not shipped either, so every install and every portable zip answered `.pes` with
    // the stock icon. It looked fine on a developer box only because a FULL ImageMagick is
    // installed there and the decode tier falls back to it - exactly the masking that
    // `test-staged-regression.ps1` exists to catch, and it did, the day the corpus gained
    // its first real `.pes`. Re-registering it means shipping the SVG stack (megabytes, for
    // one embroidery format) or writing a reader for the PEC thumbnail Brother embeds.
    "pes",
];

/// Classify an extension into a display category.
pub fn category(ext: &str) -> Category {
    if EBOOK_EXTS.contains(&ext) {
        Category::Ebook
    } else if DOCUMENT_EXTS.contains(&ext) {
        Category::Document
    } else if AUDIO_EXTS.contains(&ext) {
        Category::Audio
    } else if RAW_EXTS.contains(&ext) {
        Category::Raw
    } else if VIDEO_EXTS.contains(&ext) {
        Category::Video
    } else if ARCHIVE_EXTS.contains(&ext) {
        Category::Archive
    } else {
        Category::Image
    }
}

/// Short label for the Options list's Category column.
pub fn category_label(cat: Category) -> &'static str {
    match cat {
        Category::Image => "Image",
        Category::Raw => "Camera RAW",
        Category::Ebook => "Ebook",
        Category::Document => "Document",
        Category::Audio => "Audio",
        Category::Video => "Video",
        Category::Archive => "Archive",
    }
}

/// Is `ext` a generic archive (.zip/.rar/.7z)? ASCII-case-insensitive, no
/// allocation (the menu gate runs per selected path on every right-click).
/// Archives are thumbnail/preview-only: `verbs::is_image` excludes them so the
/// image verbs never offer to Convert/Rotate an archive's extracted cover.
pub fn is_archive(ext: &str) -> bool {
    ARCHIVE_EXTS.iter().any(|a| a.eq_ignore_ascii_case(ext))
}

// ---- Capability: what "we hook this extension" actually MEANS (audit E03) ----------------
//
// `is_known`/`category` answer "did we register this extension" - they say nothing about
// HOW the picture Explorer shows for it is actually produced, which is a different question
// with real user-visible consequences (a RAW file's thumbnail is its embedded JPEG, not a
// full demosaic; an archive shows contents, not a "photo"; some formats need an OS codec that
// may not be installed). `Capability` formalizes the per-category truth already written in
// prose in `docs/FEATURES.md` and the FORMATS comments above, DERIVED from the same category
// lists rather than a second hand-maintained table - see `capability()`.

/// An OS-provided codec a format's decode route depends on (as opposed to bundled/pure-Rust
/// decoding). `st2k doctor` checks for these specifically and warns (never errors) when one
/// is missing - the consequence is always "this format keeps its default icon", not a crash.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OsCodec {
    /// Video frames are grabbed via the OS Media Foundation codecs (`src/video.rs`); missing
    /// on Windows "N"/"KN" editions without the Media Feature Pack.
    MediaFoundation,
    /// JPEG XR / HD Photo (jxr/wdp/hdp/wmp) - one codec, decoded via WIC's built-in WMPhoto
    /// codec (`src/decode/wic.rs`). No bundled decoder backs this up.
    WmPhoto,
    /// HEIC/HEIF and the AVC-coded sibling AVCI - decoded via the OS's HEIF WIC codec (the
    /// "HEIF Image Extensions" / "HEVC Video Extensions" Store package on a clean Windows
    /// install). Confirmed from `src/decode/wic.rs`'s own module doc, which lists "HEIC/HEIF,
    /// AVIF, camera RAW, JPEG 2000, JPEG XR" as the formats WIC (not a bundled crate) decodes;
    /// there is no in-process HEIF decoder anywhere in this tree, so a HEIC/HEIF file decodes
    /// only if that OS codec is present - exactly the same shape as the WMPhoto trio, just a
    /// different Store package underneath. `st2k doctor` probes for it the same way, via WIC's
    /// own `IWICImagingFactory::CreateDecoder(GUID_ContainerFormatHeif, ...)` component lookup.
    Heif,
    /// AVIF - decoded via WIC's AV1 image codec (the "AV1 Video Extension" Store package),
    /// then Media Foundation, then external `magick`, per `Cargo.toml`'s `image` crate feature
    /// list (no `avif`/`av1` feature enabled - there is no in-process AV1 decoder here at all).
    /// UNLIKE `Heif`/`WmPhoto`, there is no `GUID_ContainerFormat*` for AVIF/AV1 in the
    /// `windows` crate (checked against `windows` 0.62.2's
    /// `Win32::Graphics::Imaging` module, which defines Heif/Wmp/… but nothing AV1-shaped) -
    /// so `st2k doctor` cannot do the same `CreateDecoder` component lookup it does for the
    /// other two, and reports this one honestly as unverified rather than guessing.
    Av1,
}

impl OsCodec {
    /// Stable lowercase wire value for `st2k formats --json` and the website generator.
    pub fn as_str(self) -> &'static str {
        match self {
            OsCodec::MediaFoundation => "media_foundation",
            OsCodec::WmPhoto => "wmphoto",
            OsCodec::Heif => "heif",
            OsCodec::Av1 => "av1",
        }
    }
}

/// How the pixels a thumbnail/preview shows are actually produced.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    /// The whole image is decoded (image crate / WIC / ImageMagick / resvg / mesh render).
    FullDecode,
    /// RAW rides its embedded JPEG preview first, with a full demosaic (WIC / magick-libraw)
    /// as the backstop when no usable preview is embedded - never a from-scratch demosaic by
    /// default. See the Camera RAW comment on `FORMATS` above.
    EmbeddedPreview,
    /// Audio: the embedded album/cover art tag. The PCM formats (wav/aiff/aif/aifc) fall back
    /// to a rendered waveform when the file carries no embedded art at all.
    CoverArt,
    /// Ebooks/comics and documents: the format's own cover image, or its first page rendered
    /// (PDF page 1, an Office/OpenDocument embedded preview, …).
    CoverOrFirstPage,
    /// A representative frame grabbed from the video stream.
    VideoFrame,
    /// Archives: the contained image(s) - first image, or the contact-sheet of up to four.
    ContainedImages,
}

impl Source {
    /// Stable lowercase wire value for `st2k formats --json` and the website generator.
    pub fn as_str(self) -> &'static str {
        match self {
            Source::FullDecode => "full_decode",
            Source::EmbeddedPreview => "embedded_preview",
            Source::CoverArt => "cover_art",
            Source::CoverOrFirstPage => "cover_or_first_page",
            Source::VideoFrame => "video_frame",
            Source::ContainedImages => "contained_images",
        }
    }
}

/// What a hooked extension can actually do - answerable for any `is_known` extension via
/// [`capability`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Capability {
    /// How the picture is produced.
    pub source: Source,
    /// Does this extension get the image verbs (Convert/Rotate/…)? Mirrors
    /// `verbs::actions::is_image`'s rule exactly: every known extension except archives.
    pub convertible: bool,
    /// Does Quick preview list this file's CONTENTS rather than show one picture?
    /// True only for archives.
    pub preview_listing: bool,
    /// The OS codec this format's decode route depends on, if any.
    pub os_codec: Option<OsCodec>,
}

/// JPEG XR / HD Photo - one codec, several extensions (see the `FORMATS` comment above).
/// Must stay a subset of `FORMATS` (enforced by `capability_lists_are_subset_of_formats`).
const WMPHOTO_EXTS: &[&str] = &["jxr", "wdp", "hdp", "wmp"];

/// HEIC/HEIF/AVCI family - decoded via the OS's HEIF WIC codec (see [`OsCodec::Heif`]).
/// Must stay a subset of `FORMATS` (enforced by `capability_lists_are_subset_of_formats`).
const HEIF_OS_CODEC_EXTS: &[&str] = &["heic", "heif", "heics", "heifs", "hif", "avci"];

/// AVIF (AV1 Image File Format) - see [`OsCodec::Av1`] for why this can't be probed the
/// way `WMPHOTO_EXTS`/`HEIF_OS_CODEC_EXTS` are. One extension today; a future `avifs`-style
/// sibling belongs here, not hand-added at the call site.
/// Must stay a subset of `FORMATS` (enforced by `capability_lists_are_subset_of_formats`).
const AV1_OS_CODEC_EXTS: &[&str] = &["avif"];

/// `Category::Image` extensions whose cover is an EMBEDDED preview the container already
/// carries (`container::extract_cover`), never a full raster decode of the image itself -
/// audit E03 finding #1: `capability()` used to give every Image-category extension
/// `Source::FullDecode` from `category()` alone, which was FALSE for this whole list (PSD's
/// baked resource-1036 thumbnail, an Illustrator/EPS file's already-embedded raster preview,
/// an APK's declared launcher icon, a Blender/Krita/OpenRaster/3MF/FreeCAD/Fusion-360/
/// Sketch/Procreate/Adobe-XD/CorelDRAW baked-in thumbnail, and so on - none of these decode
/// the file's actual image/scene/document content).
///
/// Derived by READING `container/mod.rs`'s `extract_cover` dispatch, not from memory:
/// `try_creative_app_cover` (eps/ai, psd/psb, icns, blend, affinity, psp family, ilbm
/// family, c4d, cdr/cdt/cmx, clip), `try_ebook_and_cad_cover` (skp, dwg, 3dm, max - mobi/
/// tarfmt/fb2/indd/indt are Ebook/Document category already and need no override),
/// `try_misc_cover` (gcode/gco - audio is Audio category already), PLUS the ZIP-family
/// cascade `extract_cover` reaches through `try_generic_archive_cover` ->
/// `zipfmt::extract_from_archive` -> `dedicated_preview` -> `container::project::extract`
/// (kra, ora, 3mf, fcstd, f3d, sketch, procreate, xd - key/pages/numbers/vsdx/vsdm/ggb from
/// that same cascade are Document category already) and the APK branch ahead of it
/// (apk/apks/xapk/apkm - deliberately NOT `Category::Archive`, see the `FORMATS` comment).
/// Must stay a subset of `FORMATS` (enforced by `capability_lists_are_subset_of_formats`).
const EMBEDDED_PREVIEW_EXTS: &[&str] = &[
    // EPS / Illustrator: an already-embedded raster preview only (DOS-EPS TIFF, EPSI, or a
    // Photoshop-resource JPEG), sniffed by content in `container::eps` - never a from-scratch
    // PostScript render. `ai` is PDF/EPS-compatible and rides the same content-sniffed cascade.
    "eps",
    "ai",
    // Photoshop PSD/PSB: the baked resource-1036 JPEG thumbnail (`container::psd`) unless the
    // file has no usable alpha-free preview stored, in which case `decode_full`'s magick tier
    // composites the real layers - but the THUMBNAIL path (what this claims) is preview-first.
    "psd",
    "psb",
    // Apple Icon Image: the largest embedded PNG/JPEG-2000 member (`container::icns`).
    "icns",
    // Blender: the RGBA thumbnail baked into the TEST file-block (`container::blend`), incl.
    // the gzip/zstd-"Compressed" save variant.
    "blend",
    // Affinity Photo/Designer/Publisher: an embedded PNG preview (`container::affinity`).
    "afphoto",
    "afdesign",
    "afpub",
    "af",
    // Paint Shop Pro family: the Composite Image Bank JPEG, or a bounded whole-file JPEG
    // carve (`container::psp`) - not guaranteed present, but never a from-scratch render.
    "psp",
    "pspimage",
    "pspbrush",
    "pspframe",
    "psptube",
    "pspshape",
    "pspselection",
    "pspmask",
    "tub",
    // Amiga/Deluxe Paint ILBM: a real planar decode of the file's OWN embedded bitmap
    // (`container::ilbm`) - not a codec render of a different representation.
    "iff",
    "ilbm",
    "lbm",
    // Cinema 4D: the document/scene preview JPEG carved from the header slot (`container::c4d`).
    "c4d",
    // CorelDRAW/Corel Presentation Exchange: the RIFF `DISP` preview DIB (`container::cdr`),
    // or - for the newer ZIP/OPC `.cdr` - the packaged thumbnail bitmap (`container::project`).
    "cdr",
    "cdt",
    "cmx",
    // Paint.NET: the base64 PNG preview in the XML preamble (`container::pdn`).
    "pdn",
    // Clip Studio Paint: the preview PNG inside the embedded SQLite database (`container::clip`).
    "clip",
    // SketchUp / AutoCAD / Rhino: a thumbnail carved from the model file (`container::skp`/
    // `dwg`/`rhino`) - never a 3-D render (unlike STL/OBJ/PLY, which `decode/mesh.rs` DOES
    // render, so those correctly keep `Source::FullDecode`).
    "skp",
    "dwg",
    "3dm",
    // Autodesk 3ds Max: the OLE2 `\x05SummaryInformation` thumbnail (`container::max`).
    "max",
    // 3D-printer G-code: an embedded base64 PNG preview some slicers bake into the header
    // comments (`container::gcode`) - not a render of the sliced print.
    "gcode",
    "gco",
    // PrusaSlicer binary G-code: the slicer's preview as whole image blocks (`container::bgcode`),
    // rather than the base64-in-comments of the text format above.
    "bgcode",
    // ZIP-packaged project/design files: a ready-made preview baked into the package
    // (`container::project`, reached through the same ZIP dispatch as EPUB/CBZ).
    "kra",
    "ora",
    "3mf",
    "fcstd",
    "f3d",
    "sketch",
    "procreate",
    "xd",
    // Pixelorama `.pxo` (1.0+): the root `preview.png` the app bakes in for file managers.
    "pxo",
    // Aseprite sprites: RENDERED - frame 0 composited from the file's own layers and cels
    // (`container::aseprite`), since the format bakes in no preview. `.ase` is shared with
    // 3DS ASCII scenes and Adobe swatches, so the dispatch keys on the magic word, not this.
    "aseprite",
    "ase",
    // SolidWorks (OLE-era files): the `PreviewPNG` stream (`container::solidworks`). Files
    // saved by 2015+ releases use a wrapper that is not OLE and keep the stock icon.
    "sldprt",
    "sldasm",
    "slddrw",
    // Minecraft Bedrock packages: the game's own `world_icon.jpeg` / `pack_icon.png`
    // (`container::project`, through the same ZIP dispatch as Krita and EPUB).
    "mcworld",
    "mctemplate",
    "mcpack",
    "mcaddon",
    // SpriteLoop `.spla` animation packages: RENDERED - frame 0 of the rig composited from
    // the part PNGs and the manifest's transforms (`container::spla`), since the package
    // carries no preview of its own.
    "spla",
    // Android packages: the manifest-declared launcher icon (`container::apk`), never a
    // decode/render of the app's actual UI.
    "apk",
    "apks",
    "xapk",
    "apkm",
];

/// The capability of a hooked extension - derived from `category()` plus the small explicit
/// lists above for the handful of extensions whose route is genuinely per-extension rather
/// than per-category (the OS-codec dependencies). Not a second hand-maintained table: every
/// field either falls out of `category()`, or reuses `is_known`/`is_archive` exactly the way
/// their existing callers (`verbs::actions::is_image`, the Quick preview archive listing) do.
pub fn capability(ext: &str) -> Capability {
    let convertible = is_known(ext) && !is_archive(ext);
    let source = match category(ext) {
        // Most Image-category formats really are a full decode, but a fixed subset carve an
        // already-embedded preview instead (see `EMBEDDED_PREVIEW_EXTS`'s doc) - audit E03 #1.
        Category::Image if EMBEDDED_PREVIEW_EXTS.contains(&ext) => Source::EmbeddedPreview,
        Category::Image => Source::FullDecode,
        Category::Raw => Source::EmbeddedPreview,
        Category::Ebook | Category::Document => Source::CoverOrFirstPage,
        Category::Audio => Source::CoverArt,
        Category::Video => Source::VideoFrame,
        Category::Archive => Source::ContainedImages,
    };
    let os_codec = if AV1_OS_CODEC_EXTS.contains(&ext) {
        Some(OsCodec::Av1)
    // The extensions whose own codecs are decoded by `st2k flv-frame` / `st2k mpeg-frame`
    // - never Media Foundation - are excluded from the blanket video->MediaFoundation rule
    // below (audit E03 #5; see `SELF_DECODED_VIDEO_EXTS` for why the answer is per
    // extension, not per file).
    } else if VIDEO_EXTS.contains(&ext) && !SELF_DECODED_VIDEO_EXTS.contains(&ext) {
        Some(OsCodec::MediaFoundation)
    } else if WMPHOTO_EXTS.contains(&ext) {
        Some(OsCodec::WmPhoto)
    } else if HEIF_OS_CODEC_EXTS.contains(&ext) {
        Some(OsCodec::Heif)
    } else {
        None
    };
    Capability {
        source,
        convertible,
        preview_listing: is_archive(ext),
        os_codec,
    }
}

/// (extension without dot, friendly description), grouped by category.
pub const FORMATS: &[(&str, &str)] = &[
    // --- Images ---
    ("ai", "Adobe Illustrator (PDF-compatible)"),
    ("apng", "Animated Portable Network Graphics"),
    ("avci", "AVC Image File Format"),
    ("avif", "AV1 Image File Format"),
    ("bmp", "Microsoft Windows bitmap image"),
    ("bw", "Silicon Graphics (B&W)"),
    ("cal", "Continuous Acquisition and Life-cycle Support"),
    ("cals", "Continuous Acquisition and Life-cycle Support"),
    ("cdr", "CorelDRAW drawing"),
    ("cdt", "CorelDRAW template"),
    ("cmx", "Corel Presentation Exchange"),
    ("cin", "Cineon Image File"),
    ("cur", "Microsoft icon"),
    ("cut", "DR Halo"),
    ("dcm", "DICOM medical image"),
    ("dcx", "ZSoft IBM PC multi-page Paintbrush"),
    ("dds", "Microsoft DirectDraw Surface"),
    ("dib", "Windows DIB"),
    // `.dicom` is the same content as `.dcm` (decode is content-sniffed on bytes, not the
    // extension), so this alias rides the existing DICOM path (QuickLook parity, 2026-07-11).
    ("dicom", "DICOM medical image"),
    ("dpx", "SMPTE 268M-2003"),
    ("dxt1", "Microsoft DirectDraw Surface"),
    ("dxt5", "Microsoft DirectDraw Surface"),
    ("emf", "Windows Enhanced Metafile"),
    ("emz", "Compressed Windows Enhanced Metafile"),
    ("eps", "Encapsulated PostScript (embedded preview)"),
    ("exr", "High Dynamic-range (OpenEXR)"),
    ("farbfeld", "Farbfeld"),
    ("ff", "Farbfeld"),
    ("fits", "Flexible Image Transport System"),
    ("fl32", "FilmLight"),
    ("fts", "Flexible Image Transport System"),
    ("gif", "CompuServe graphics interchange format"),
    ("hdr", "Radiance RGBE image format"),
    ("heic", "High Efficiency Image Format"),
    ("heif", "High Efficiency Image Format"),
    ("livp", "Apple Live Photo"),
    ("icb", "Truevision Targa image"),
    ("ico", "Microsoft icon"),
    ("icon", "Microsoft icon"),
    ("icns", "Apple Icon Image"),
    ("j2c", "JPEG-2000 Code Stream Syntax"),
    ("j2k", "JPEG-2000 Code Stream Syntax"),
    // NOTE: `jbig` was REMOVED (2026-07-08) — a registered dead hook. No tier can
    // decode it: no image-crate/WIC support, no container path, and ImageMagick's
    // own format table reports JBIG as `---` (the delegate isn't compiled in), so
    // the hook only ever produced a doomed 20s magick attempt. Don't re-add
    // without an actual decoder.
    ("jfif", "JPEG/JFIF"),
    ("jng", "JPEG Network Graphics"),
    ("jnx", "Garmin tile format"),
    ("jp2", "JPEG-2000 File Format Syntax"),
    ("jpc", "JPEG-2000 Code Stream Syntax"),
    ("jpe", "JPEG (JFIF)"),
    ("jpeg", "JPEG (JFIF)"),
    ("jpg", "JPEG (JFIF)"),
    ("jpm", "JPEG-2000 File Format Syntax"),
    ("jps", "Stereo JPEG"),
    ("jxl", "JPEG XL (ISO/IEC 18181)"),
    // JPEG XR / HD Photo (a.k.a. Windows Media Photo) — one codec, three extensions;
    // decoded by the OS via WIC's built-in WMPhoto codec (no bundled decoder).
    ("jxr", "JPEG XR (ISO/IEC 29199-2)"),
    ("wdp", "HD Photo / Windows Media Photo (JPEG XR)"),
    ("hdp", "HD Photo (JPEG XR)"),
    ("mac", "MacPaint"),
    ("iff", "Amiga IFF ILBM image"),
    ("ilbm", "Amiga IFF ILBM image"),
    ("lbm", "Deluxe Paint ILBM image"),
    ("mat", "MATLAB level 5 image format"),
    ("miff", "Magick Image File Format"),
    ("mng", "Multiple-image Network Graphics"),
    ("mpo", "Multi-Picture (3D) JPEG"),
    ("ora", "OpenRaster format"),
    // Art / CAD / 3D-print project files — we extract their embedded preview.
    ("kra", "Krita document"),
    ("3mf", "3D Manufacturing Format"),
    ("stl", "Stereolithography 3D model"),
    ("obj", "Wavefront 3D model"),
    ("ply", "Polygon File Format 3D model"),
    ("fcstd", "FreeCAD document"),
    ("f3d", "Autodesk Fusion 360 archive"),
    ("gcode", "3D-printer G-code (sliced)"),
    ("gco", "3D-printer G-code (sliced)"),
    ("afphoto", "Affinity Photo document"),
    ("afdesign", "Affinity Designer document"),
    ("afpub", "Affinity Publisher document"),
    ("af", "Affinity document"),
    ("blend", "Blender scene"),
    ("clip", "Clip Studio Paint document"),
    ("pxo", "Pixelorama project"),
    ("aseprite", "Aseprite sprite"),
    ("ase", "Aseprite sprite"),
    ("bgcode", "3D-printer G-code (binary)"),
    ("sldprt", "SolidWorks part"),
    ("sldasm", "SolidWorks assembly"),
    ("slddrw", "SolidWorks drawing"),
    ("mcworld", "Minecraft Bedrock world"),
    ("mctemplate", "Minecraft Bedrock world template"),
    ("mcpack", "Minecraft Bedrock pack"),
    ("mcaddon", "Minecraft Bedrock add-on"),
    ("spla", "SpriteLoop animation package"),
    ("pspimage", "Paint Shop Pro image"),
    ("psp", "Paint Shop Pro image"),
    // The rest of the Paint Shop Pro family: same "~BK\0" block container as .pspimage,
    // so `container::psp` reads them unchanged (dispatch is by CONTENT magic — see
    // `container::extract_cover` — the extension only decides what we hook in Explorer).
    // A preview is NOT guaranteed in these: PSP writes the Composite Image Bank when it
    // has a flattened preview to store, and `psp::extract` additionally falls back to a
    // bounded whole-file JPEG carve. When neither finds one we return None and Explorer
    // shows its default icon — exactly the pre-registration behaviour, so this is upside-
    // only. `.pspmask` is the odd one: it can be a plain Windows BMP instead of a PSP
    // container, which needs no special case — the PSP sniff simply fails and it falls
    // through to the normal `image`-crate tier that already decodes BMP.
    ("pspbrush", "Paint Shop Pro brush"),
    ("pspframe", "Paint Shop Pro picture frame"),
    ("psptube", "Paint Shop Pro picture tube"),
    ("pspshape", "Paint Shop Pro preset shape"),
    ("pspselection", "Paint Shop Pro selection"),
    ("pspmask", "Paint Shop Pro mask"),
    // Legacy picture tube (pre-PSP 11/12, ~20 years old). Same PSPImage container; confirmed
    // by the reporter of issue #4. Costs one table row and content dispatch does the rest.
    ("tub", "Paint Shop Pro picture tube (legacy)"),
    ("sketch", "Sketch design document"),
    ("procreate", "Procreate document"),
    ("skp", "SketchUp model"),
    ("dwg", "AutoCAD drawing"),
    ("3dm", "Rhino 3D model"),
    ("xd", "Adobe XD design"),
    ("max", "Autodesk 3ds Max scene"),
    ("c4d", "Cinema 4D scene"),
    // Android packages — the manifest-declared launcher icon (container/apk.rs); the
    // split-bundle wrappers carry a base.apk inside another zip. Deliberately NOT in
    // ARCHIVE_EXTS: these get a real single-icon cover, not the archive contact sheet.
    ("apk", "Android application package"),
    ("apks", "Android split-APK bundle"),
    ("xapk", "Android split-APK bundle (XAPK)"),
    ("apkm", "Android split-APK bundle (APKM)"),
    ("pam", "Portable Arbitrary Map"),
    ("pbm", "Portable bitmap format"),
    ("pcd", "Photo CD"),
    ("pcx", "ZSoft IBM PC Paintbrush"),
    ("pdb", "Palm Database ImageViewer Format"),
    ("pdn", "Paint.NET image"),
    ("pfm", "Portable float format"),
    ("pgm", "Portable graymap format"),
    ("pgx", "JPEG 2000 uncompressed format"),
    ("phm", "Portable half float format"),
    ("pix", "Alias/Wavefront RLE image format"),
    ("png", "Portable Network Graphics"),
    ("pnm", "Portable anymap"),
    ("ppm", "Portable pixmap format"),
    ("psb", "Adobe Large Document Format"),
    ("psd", "Adobe Photoshop bitmap"),
    ("ptif", "Pyramid encoded TIFF"),
    ("pwp", "Seattle Film Works"),
    ("qoi", "Quite OK image format"),
    ("ras", "SUN Rasterfile"),
    ("rla", "Alias/Wavefront image"),
    ("rle", "Utah Run length encoded image"),
    ("rmf", "Raw Media Format"),
    ("scr", "ZX-Spectrum SCREEN$"),
    ("sct", "Scitex HandShake"),
    ("sf3", "Simple File Format Family Images"),
    ("sfw", "Seattle Film Works"),
    ("sgi", "Silicon Graphics RGB"),
    ("sti", "Sinar CaptureShop Raw Format"),
    ("sun", "SUN Rasterfile"),
    ("svg", "Scalable Vector Graphics"),
    ("svgz", "Compressed Scalable Vector Graphics"),
    ("tga", "Truevision Targa image"),
    ("tif", "Tagged Image File Format"),
    ("tiff", "Tagged Image File Format"),
    ("tiff64", "Tagged Image File Format (64-bit)"),
    ("tim", "PSX TIM"),
    ("tm2", "PS2 TIM2"),
    ("vda", "Truevision Targa image"),
    ("vicar", "Video Image Communication And Retrieval"),
    ("viff", "Khoros Visualization image"),
    ("vips", "VIPS image"),
    ("vst", "Truevision Targa image"),
    ("webp", "Google WebP"),
    ("wmf", "Windows Metafile"),
    ("wpg", "Word Perfect Graphics"),
    ("xbm", "X Windows system bitmap"),
    ("xcf", "GIMP image"),
    ("xpm", "X Windows system pixmap"),
    ("xv", "Khoros Visualization image"),
    // --- MysticThumbs-parity aliases (Tier A) ---
    // Extra extensions for formats we ALREADY decode. Decoding is content-sniffed,
    // so each rides the same tier as its cousin — registering the extension is all
    // that's needed.
    ("heics", "HEIF image sequence"),
    ("heifs", "HEIF image sequence"),
    ("hif", "High Efficiency Image Format"),
    ("jpf", "JPEG-2000 File Format Syntax"),
    ("jpx", "JPEG-2000 Part-2 (extended)"),
    ("rgbe", "Radiance RGBE image format"),
    ("xyze", "Radiance XYZE image format"),
    ("hdri", "Radiance HDR image format"),
    ("cxr", "OpenEXR image"),
    ("wmp", "HD Photo / Windows Media Photo (JPEG XR)"),
    ("wmz", "Compressed Windows Metafile"),
    ("emg", "Windows Enhanced Metafile"),
    ("tpic", "Truevision Targa image"),
    ("pdd", "Adobe Photoshop bitmap"),
    ("psdt", "Adobe Photoshop template"),
    ("indt", "Adobe InDesign template"),
    ("aftemplate", "Affinity template document"),
    ("skb", "SketchUp backup model"),
    ("ph", "Photo CD"),
    // Blender keeps rolling auto-save backups (.blend1 … .blend32) — same container
    // as .blend, so the Blender cover extractor reads them all.
    ("blend1", "Blender auto-save backup"),
    ("blend2", "Blender auto-save backup"),
    ("blend3", "Blender auto-save backup"),
    ("blend4", "Blender auto-save backup"),
    ("blend5", "Blender auto-save backup"),
    ("blend6", "Blender auto-save backup"),
    ("blend7", "Blender auto-save backup"),
    ("blend8", "Blender auto-save backup"),
    ("blend9", "Blender auto-save backup"),
    ("blend10", "Blender auto-save backup"),
    ("blend11", "Blender auto-save backup"),
    ("blend12", "Blender auto-save backup"),
    ("blend13", "Blender auto-save backup"),
    ("blend14", "Blender auto-save backup"),
    ("blend15", "Blender auto-save backup"),
    ("blend16", "Blender auto-save backup"),
    ("blend17", "Blender auto-save backup"),
    ("blend18", "Blender auto-save backup"),
    ("blend19", "Blender auto-save backup"),
    ("blend20", "Blender auto-save backup"),
    ("blend21", "Blender auto-save backup"),
    ("blend22", "Blender auto-save backup"),
    ("blend23", "Blender auto-save backup"),
    ("blend24", "Blender auto-save backup"),
    ("blend25", "Blender auto-save backup"),
    ("blend26", "Blender auto-save backup"),
    ("blend27", "Blender auto-save backup"),
    ("blend28", "Blender auto-save backup"),
    ("blend29", "Blender auto-save backup"),
    ("blend30", "Blender auto-save backup"),
    ("blend31", "Blender auto-save backup"),
    ("blend32", "Blender auto-save backup"),
    // --- Camera RAW ---
    ("3fr", "Hasselblad CFV/H3D39II Raw Format"),
    ("arw", "Sony Alpha Raw Format"),
    ("cr2", "Canon Digital Camera Raw Format"),
    ("cr3", "Canon Digital Camera Raw Format"),
    ("crw", "Canon Digital Camera Raw Format"),
    ("dcr", "Kodak Digital Camera Raw Format"),
    ("dng", "Digital Negative Raw Format"),
    ("erf", "Epson Raw Format"),
    ("fff", "Hasselblad CFV/H3D39II Raw Format"),
    ("iiq", "Phase One Raw Format"),
    ("k25", "Kodak Digital Camera Raw Format"),
    ("kdc", "Kodak Digital Camera Raw Format"),
    ("mdc", "Minolta Digital Camera Raw Format"),
    ("mef", "Mamiya Raw Format"),
    ("mos", "Aptus Leaf Raw Format"),
    ("mrw", "Sony (Minolta) Raw Format"),
    ("nef", "Nikon Digital SLR Camera Raw Format"),
    ("nrw", "Nikon Digital SLR Camera Raw Format"),
    ("orf", "Olympus Digital Camera Raw Format"),
    ("pef", "Pentax Electronic Raw Format"),
    ("raf", "Fuji CCD-RAW Graphic Raw Format"),
    ("rw2", "Panasonic Lumix Raw Format"),
    ("rwl", "Leica Raw Format"),
    ("sr2", "Sony Raw Format 2"),
    ("srf", "Sony Raw Format"),
    ("srw", "Samsung Raw Format"),
    ("x3f", "Sigma Camera RAW Format"),
    // MysticThumbs-parity (Tier B): more camera-RAW extensions. They ride the same
    // embedded-JPEG preview path (`decode::decode_raw_preview`) + WIC/magick-libraw
    // backstops as the formats above, so it's the same decode, just more vendors.
    ("bay", "Casio / Phase One Raw Format"),
    ("cap", "Phase One Raw Format"),
    ("dcs", "Kodak DCS Raw Format"),
    ("drf", "Kodak Raw Format"),
    ("ori", "Olympus Raw Format"),
    ("ptx", "Pentax Raw Format"),
    ("pxn", "Logitech Fotoman Raw Format"),
    // --- Ebooks & comics (cover thumbnails; the DarkThumbs port) ---
    ("azw", "Amazon Kindle ebook"),
    ("azw3", "Amazon Kindle ebook (KF8)"),
    ("cb7", "Comic book archive (7-Zip)"),
    ("cbr", "Comic book archive (RAR)"),
    ("cbt", "Comic book archive (TAR)"),
    ("cbz", "Comic book archive (ZIP)"),
    ("epub", "EPUB ebook"),
    ("fb2", "FictionBook 2 ebook"),
    ("fbz", "FictionBook 2 ebook (zipped)"),
    ("mobi", "Mobipocket / Kindle ebook"),
    ("phz", "Comic / image archive (ZIP)"),
    ("prc", "Mobipocket / Palm ebook"),
    // --- Documents (page 1 render or embedded preview) ---
    ("pdf", "Portable Document Format (page 1)"),
    ("djv", "DjVu document"),
    ("djvu", "DjVu document"),
    ("ggb", "GeoGebra worksheet"),
    ("odt", "OpenDocument Text"),
    ("ods", "OpenDocument Spreadsheet"),
    ("odp", "OpenDocument Presentation"),
    ("odg", "OpenDocument Graphics"),
    ("odf", "OpenDocument Formula"),
    ("ott", "OpenDocument Text Template"),
    ("ots", "OpenDocument Spreadsheet Template"),
    ("otp", "OpenDocument Presentation Template"),
    ("pptx", "PowerPoint presentation"),
    ("pptm", "PowerPoint macro-enabled presentation"),
    ("potx", "PowerPoint template"),
    ("key", "Apple Keynote presentation"),
    ("pages", "Apple Pages document"),
    ("numbers", "Apple Numbers spreadsheet"),
    ("indd", "Adobe InDesign document"),
    ("vsdx", "Visio drawing"),
    ("vsdm", "Visio macro-enabled drawing"),
    ("vsd", "Visio drawing (legacy)"),
    ("pub", "Microsoft Publisher document"),
    // Microsoft Word / Excel / PowerPoint. OOXML packages carry a docProps/thumbnail
    // (present when the author saved a preview) handled by the generic `office.rs`
    // path; legacy 97-2003 docs are OLE compound files whose \x05SummaryInformation
    // holds a CF_DIB preview, handled by `max.rs`. No new decode code — both ride the
    // existing container extractors, so an absent preview just falls back to the icon.
    ("docx", "Word document"),
    ("docm", "Word macro-enabled document"),
    ("dotx", "Word template"),
    ("dotm", "Word macro-enabled template"),
    ("doc", "Word 97-2003 document"),
    ("dot", "Word 97-2003 template"),
    ("xlsx", "Excel workbook"),
    ("xlsm", "Excel macro-enabled workbook"),
    ("xlsb", "Excel binary workbook"),
    ("xltx", "Excel template"),
    ("xltm", "Excel macro-enabled template"),
    ("xls", "Excel 97-2003 workbook"),
    ("xlt", "Excel 97-2003 template"),
    ("ppsx", "PowerPoint slideshow"),
    ("ppsm", "PowerPoint macro-enabled slideshow"),
    ("potm", "PowerPoint macro-enabled template"),
    ("ppt", "PowerPoint 97-2003 presentation"),
    ("pps", "PowerPoint 97-2003 slideshow"),
    ("pot", "PowerPoint 97-2003 template"),
    // --- Audio (embedded album / cover art) ---
    ("mp3", "MP3 audio (album art)"),
    ("flac", "FLAC audio (album art)"),
    ("ogg", "Ogg Vorbis audio (album art)"),
    ("oga", "Ogg audio (album art)"),
    ("opus", "Opus audio (album art)"),
    ("spx", "Speex audio (album art)"),
    ("m4a", "MPEG-4 audio (album art)"),
    ("m4b", "MPEG-4 audiobook (album art)"),
    ("aac", "AAC audio (album art)"),
    ("wma", "Windows Media Audio (album art)"),
    ("ape", "Monkey's Audio (album art)"),
    ("wv", "WavPack audio (album art)"),
    ("mpc", "Musepack audio (album art)"),
    ("wav", "WAV audio (waveform / album art)"),
    ("aiff", "AIFF audio (waveform / album art)"),
    ("aif", "AIFF audio (waveform / album art)"),
    ("aifc", "AIFF-C audio (waveform / album art)"),
    ("dsf", "DSD audio (album art)"),
    // ---- Video (a representative frame, grabbed via OS Media Foundation codecs) ----
    ("mp4", "MPEG-4 Video"),
    ("m4v", "MPEG-4 Video (iTunes)"),
    ("mov", "QuickTime Movie"),
    ("qt", "QuickTime Movie"),
    ("mkv", "Matroska Video"),
    ("webm", "WebM Video"),
    ("avi", "Audio Video Interleave"),
    ("wmv", "Windows Media Video"),
    ("asf", "Advanced Systems Format Video"),
    ("flv", "Flash Video"),
    ("f4v", "Flash MP4 Video"),
    ("mpg", "MPEG Video"),
    ("mpeg", "MPEG Video"),
    ("m1v", "MPEG-1 Video"),
    ("m2v", "MPEG-2 Video"),
    // The same two shapes our own decoder already reads, under the names DVD-authoring and
    // capture tools write them with (2026-09-17): `mpv`/`mp2v` are a bare MPEG video
    // elementary stream (what `m2v` is), `m2p` is an MPEG-2 program stream (what `vob` and a
    // PS-shaped `mpg` are). Nothing new had to be decoded for these - they were rendering
    // through `mpeg12` already for anyone who renamed the file, and only the registration was
    // missing. `.mod` (JVC camcorder MPEG-2 PS) and `.dat` (VideoCD) are deliberately NOT
    // here: both collide with far commoner non-video files (tracker music, every other
    // `.dat`), which is the standing rule in ROADMAP's rejected appendix.
    ("mpv", "MPEG Video Elementary Stream"),
    ("mp2v", "MPEG-2 Video Elementary Stream"),
    ("m2p", "MPEG-2 Program Stream"),
    ("3gp", "3GPP Video"),
    ("3g2", "3GPP2 Video"),
    ("ts", "MPEG Transport Stream"),
    ("m2ts", "Blu-ray BDAV Video"),
    ("mts", "AVCHD Video"),
    ("vob", "DVD Video Object"),
    ("ogv", "Ogg Video"),
    ("divx", "DivX Video"),
    // --- Archives (thumbnail = the contained images; must mirror ARCHIVE_EXTS) ---
    ("7z", "7-Zip archive"),
    ("rar", "RAR archive"),
    ("zip", "ZIP archive"),
];

/// Is `ext` (no dot) one we hook? ASCII-case-insensitive, so callers need not
/// pre-lowercase (and allocate). Backed by a one-time sorted index of the FORMATS
/// extensions so the lookup is a binary search rather than a linear scan over all
/// ~280 entries — it's on the menu-build / selection-gating hot path.
pub fn is_known(ext: &str) -> bool {
    use std::sync::OnceLock;
    // FORMATS is ordered by category (the Settings list relies on that), so it is
    // NOT sorted by extension — keep a separate sorted slice for the search. Built
    // once; the FORMATS extensions are already lowercase ASCII.
    static SORTED: OnceLock<Vec<&'static str>> = OnceLock::new();
    let sorted = SORTED.get_or_init(|| {
        let mut v: Vec<&'static str> = FORMATS.iter().map(|&(e, _)| e).collect();
        v.sort_unstable();
        v
    });
    // Compare against the (lowercase) table entries by lowercasing `ext`'s bytes on
    // the fly — no allocation, and matches a mixed-case ".PNG" against "png".
    sorted
        .binary_search_by(|&e| e.bytes().cmp(ext.bytes().map(|b| b.to_ascii_lowercase())))
        .is_ok()
}

// ---- Quick preview: text/markdown lists (VIEWER-ONLY — Phase 3) --------------------------
// Consulted ONLY by the Quick preview viewer to decide "render this as markdown / as
// syntax-highlighted text". DELIBERATELY NOT in FORMATS: adding them there would register
// thumbnail/property/preview-pane handlers + enable the image verbs on .md/.txt files. So
// `is_known()` above stays unaffected; only the viewer's content dispatch reads these.

/// Markdown source extensions the viewer renders GitHub-style (gated on `preview_markdown()`).
pub const PREVIEW_MD_EXTS: &[&str] = &["md", "markdown", "mdown", "mkd", "mdwn", "mdtxt", "mdtext"];

/// Text/code extensions the viewer renders as text (gated on `preview_text()`). A CURATED set,
/// not "every text file" — the viewer's content sniff catches unknown-but-textual files too.
/// (`csv` moved to [`PREVIEW_DOC_EXTS`] — it renders as a real table now.)
/// `srt`/`vtt` subtitles are here because they ARE plain text and people do want to glance
/// at one next to the video, which is the same reason PowerToys' Peek added them.
pub const PREVIEW_TEXT_EXTS: &[&str] = &[
    "txt", "log", "json", "yaml", "yml", "toml", "xml", "ini", "cfg", "rs", "py", "js", "ts", "c",
    "cpp", "h", "cs", "java", "sh", "ps1", "bat", "html", "css", "sql", "srt", "vtt",
];

/// Structured documents the viewer converts to markdown at load and renders through the
/// markdown pipeline (gated on `preview_markdown()`): CSV/TSV/PSV → a GitHub-grid table view,
/// Jupyter notebooks → rendered markdown + fenced code cells with outputs.
pub const PREVIEW_DOC_EXTS: &[&str] = &["csv", "tsv", "psv", "ipynb"];

/// Is `ext` (no dot) a convert-to-markdown document? ASCII-case-insensitive.
pub fn is_preview_doc(ext: &str) -> bool {
    PREVIEW_DOC_EXTS
        .iter()
        .any(|&e| e.eq_ignore_ascii_case(ext))
}

/// Is `ext` (no dot) a markdown source the viewer renders? ASCII-case-insensitive.
pub fn is_preview_markdown(ext: &str) -> bool {
    PREVIEW_MD_EXTS.iter().any(|&e| e.eq_ignore_ascii_case(ext))
}

/// Is `ext` (no dot) a text/code file the viewer renders? ASCII-case-insensitive.
pub fn is_preview_text(ext: &str) -> bool {
    PREVIEW_TEXT_EXTS
        .iter()
        .any(|&e| e.eq_ignore_ascii_case(ext))
}

#[cfg(test)]
mod tests;
