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
    "pdf", "djv", "djvu", "odt", "ods", "odp", "odg", "odf", "ott", "ots", "otp",
    "pptx", "pptm", "potx", "key", "pages", "numbers", "indd", "indt", "vsdx", "vsdm", "vsd", "pub", "ggb",
    // Microsoft Word / Excel / PowerPoint (OOXML packages + legacy OLE compound docs).
    "docx", "docm", "dotx", "dotm", "doc", "dot",
    "xlsx", "xlsm", "xlsb", "xltx", "xltm", "xls", "xlt",
    "ppsx", "ppsm", "potm", "ppt", "pps", "pot",
];
const AUDIO_EXTS: &[&str] = &[
    "mp3", "flac", "ogg", "oga", "opus", "spx", "m4a", "m4b", "aac", "wma", "ape", "wv",
    "mpc", "wav", "aiff", "aif", "aifc", "dsf",
];
const RAW_EXTS: &[&str] = &[
    "3fr", "arw", "cr2", "cr3", "crw", "dcr", "dng", "erf", "fff", "iiq", "k25",
    "kdc", "mdc", "mef", "mos", "mrw", "nef", "nrw", "orf", "pef", "raf", "rw2",
    "rwl", "sr2", "srf", "srw", "x3f",
    // MysticThumbs-parity additions (must mirror the Camera RAW block in FORMATS).
    "bay", "cap", "dcs", "drf", "ori", "ptx", "pxn",
];
// Video — a frame is grabbed via the OS Media Foundation codecs (no bundled bytes),
// streamed from disk. MF decodes what the OS has a codec for; the rest keep their
// default icon. Must mirror the Video block in FORMATS.
const VIDEO_EXTS: &[&str] = &[
    "mp4", "m4v", "mov", "qt", "mkv", "webm", "avi", "wmv", "asf", "flv", "f4v",
    "mpg", "mpeg", "m2v", "3gp", "3g2", "ts", "m2ts", "mts", "vob", "ogv", "divx",
];

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
    ("sketch", "Sketch design document"),
    ("procreate", "Procreate document"),
    ("skp", "SketchUp model"),
    ("dwg", "AutoCAD drawing"),
    ("3dm", "Rhino 3D model"),
    ("xd", "Adobe XD design"),
    ("max", "Autodesk 3ds Max scene"),
    ("c4d", "Cinema 4D scene"),
    ("pam", "Portable Arbitrary Map"),
    ("pbm", "Portable bitmap format"),
    ("pcd", "Photo CD"),
    ("pcx", "ZSoft IBM PC Paintbrush"),
    ("pdb", "Palm Database ImageViewer Format"),
    ("pes", "Embird Embroidery Format"),
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
    ("m2v", "MPEG-2 Video"),
    ("3gp", "3GPP Video"),
    ("3g2", "3GPP2 Video"),
    ("ts", "MPEG Transport Stream"),
    ("m2ts", "Blu-ray BDAV Video"),
    ("mts", "AVCHD Video"),
    ("vob", "DVD Video Object"),
    ("ogv", "Ogg Video"),
    ("divx", "DivX Video"),
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
pub const PREVIEW_TEXT_EXTS: &[&str] = &[
    "txt", "log", "json", "yaml", "yml", "toml", "xml", "ini", "cfg", "rs", "py", "js",
    "ts", "c", "cpp", "h", "cs", "java", "sh", "ps1", "bat", "html", "css", "sql",
];

/// Structured documents the viewer converts to markdown at load and renders through the
/// markdown pipeline (gated on `preview_markdown()`): CSV/TSV → a GitHub-grid table view,
/// Jupyter notebooks → rendered markdown + fenced code cells with outputs.
pub const PREVIEW_DOC_EXTS: &[&str] = &["csv", "tsv", "ipynb"];

/// Is `ext` (no dot) a convert-to-markdown document? ASCII-case-insensitive.
pub fn is_preview_doc(ext: &str) -> bool {
    PREVIEW_DOC_EXTS.iter().any(|&e| e.eq_ignore_ascii_case(ext))
}

/// Is `ext` (no dot) a markdown source the viewer renders? ASCII-case-insensitive.
pub fn is_preview_markdown(ext: &str) -> bool {
    PREVIEW_MD_EXTS.iter().any(|&e| e.eq_ignore_ascii_case(ext))
}

/// Is `ext` (no dot) a text/code file the viewer renders? ASCII-case-insensitive.
pub fn is_preview_text(ext: &str) -> bool {
    PREVIEW_TEXT_EXTS.iter().any(|&e| e.eq_ignore_ascii_case(ext))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The category lists under test are the REAL module-scoped consts that
    // `category()` uses — not a copy — so the data lives in exactly one place.
    const EBOOK: &[&str] = EBOOK_EXTS;
    const DOCUMENT: &[&str] = DOCUMENT_EXTS;
    const AUDIO: &[&str] = AUDIO_EXTS;
    const RAW: &[&str] = RAW_EXTS;
    const VIDEO: &[&str] = VIDEO_EXTS;

    fn in_formats(ext: &str) -> bool {
        FORMATS.iter().any(|(e, _)| *e == ext)
    }

    /// Every extension a category list names must actually be a registered
    /// `FORMATS` entry — otherwise `category()` classifies a phantom extension we
    /// never hook. Catches a typo or a `FORMATS` removal that leaves a stale list.
    #[test]
    fn category_lists_are_subset_of_formats() {
        for (name, list) in [
            ("EBOOK", EBOOK),
            ("DOCUMENT", DOCUMENT),
            ("AUDIO", AUDIO),
            ("RAW", RAW),
            ("VIDEO", VIDEO),
        ] {
            for &ext in list {
                assert!(
                    in_formats(ext),
                    "{name} list names `{ext}`, which is not in FORMATS",
                );
            }
        }
    }

    /// `category()` must agree with these lists for every real extension, and must
    /// never return a non-`Image` verdict for an extension none of the lists claim.
    /// The lists here ARE the consts `category()` reads, so the loop's value is the
    /// final guard: a known non-Image ext that fell out of its list would default to
    /// `Image`, and any list-less ext that somehow classified as non-Image trips the
    /// guard below — either way, drift between the two encodings fails the test.
    #[test]
    fn category_verdict_matches_lists() {
        for &(ext, _) in FORMATS {
            let expected = if EBOOK.contains(&ext) {
                Category::Ebook
            } else if DOCUMENT.contains(&ext) {
                Category::Document
            } else if AUDIO.contains(&ext) {
                Category::Audio
            } else if RAW.contains(&ext) {
                Category::Raw
            } else if VIDEO.contains(&ext) {
                Category::Video
            } else {
                Category::Image
            };
            assert!(
                category(ext) == expected,
                "category(\"{ext}\") disagrees with the category lists",
            );
        }
        // Guard: nothing the lists DON'T claim may classify as non-Image. A known
        // non-Image ext that drops out of a list would default to Image and be
        // caught above; this catches the reverse (a list-less ext wrongly typed).
        for &(ext, _) in FORMATS {
            let claimed = EBOOK.contains(&ext)
                || DOCUMENT.contains(&ext)
                || AUDIO.contains(&ext)
                || RAW.contains(&ext)
                || VIDEO.contains(&ext);
            if !claimed {
                assert!(
                    category(ext) == Category::Image,
                    "category(\"{ext}\") is non-Image but no list claims it",
                );
            }
        }
    }

    /// Per-category counts are DERIVED from `FORMATS` (not hardcoded), so adding a
    /// format updates the totals automatically. We only assert the partition is
    /// exhaustive — every `FORMATS` entry lands in exactly one bucket and the
    /// buckets sum back to `FORMATS.len()` — plus that each non-Image bucket equals
    /// its list length. No magic "179" lives here; it falls out of the table.
    #[test]
    fn category_counts_partition_formats() {
        let mut n = [0usize; 6];
        for &(ext, _) in FORMATS {
            n[category(ext) as usize] += 1;
        }
        let total: usize = n.iter().sum();
        assert_eq!(total, FORMATS.len(), "counts must partition FORMATS");

        // Each non-Image bucket equals exactly its list's length (no dupes, no gaps).
        assert_eq!(n[Category::Ebook as usize], EBOOK.len(), "Ebook");
        assert_eq!(n[Category::Document as usize], DOCUMENT.len(), "Document");
        assert_eq!(n[Category::Audio as usize], AUDIO.len(), "Audio");
        assert_eq!(n[Category::Raw as usize], RAW.len(), "Camera RAW");
        assert_eq!(n[Category::Video as usize], VIDEO.len(), "Video");
        // Image is whatever remains — derived, not asserted to a literal.
        let non_image = EBOOK.len() + DOCUMENT.len() + AUDIO.len() + RAW.len() + VIDEO.len();
        assert_eq!(
            n[Category::Image as usize],
            FORMATS.len() - non_image,
            "Image is the remainder of FORMATS",
        );
    }

    /// `REMOVED_EXTENSIONS` (the historically-dropped exts we sweep on register/unregister)
    /// MUST NOT overlap `FORMATS` — otherwise the cleanup would unhook a LIVE format.
    #[test]
    fn removed_extensions_disjoint_from_formats() {
        for &ext in REMOVED_EXTENSIONS {
            assert!(
                !FORMATS.iter().any(|&(e, _)| e == ext),
                "REMOVED_EXTENSIONS contains \"{ext}\" which is still a live FORMATS entry — \
                 the register/unregister cleanup sweep would unhook it",
            );
        }
    }

    /// `FORMATS` must have no duplicate extensions — a dupe would double-count in
    /// the partition and silently mis-size the Options list.
    #[test]
    fn formats_has_no_duplicate_extensions() {
        for (i, &(ext, _)) in FORMATS.iter().enumerate() {
            for &(other, _) in &FORMATS[i + 1..] {
                assert!(ext != other, "duplicate FORMATS extension `{ext}`");
            }
        }
    }
}
