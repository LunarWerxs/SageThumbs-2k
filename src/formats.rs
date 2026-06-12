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
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Image = 0,
    Raw = 1,
    Ebook = 2,
    Document = 3,
    Audio = 4,
}

/// Classify an extension into a display category.
pub fn category(ext: &str) -> Category {
    const EBOOK: &[&str] = &[
        "azw", "azw3", "cb7", "cbr", "cbt", "cbz", "epub", "fb2", "fbz", "mobi",
    ];
    const DOCUMENT: &[&str] = &[
        "pdf", "djv", "djvu", "odt", "ods", "odp", "odg", "odf", "ott", "ots", "otp",
        "pptx", "pptm", "potx",
    ];
    const AUDIO: &[&str] = &[
        "mp3", "flac", "ogg", "oga", "opus", "spx", "m4a", "m4b", "aac", "wma", "ape", "wv",
        "mpc", "wav", "aiff", "aif",
    ];
    const RAW: &[&str] = &[
        "3fr", "arw", "cr2", "cr3", "crw", "dcr", "dcs", "dng", "erf", "fff", "iiq", "k25",
        "kdc", "mdc", "mef", "mos", "mrw", "nef", "nrw", "orf", "pef", "raf", "raw", "rw2",
        "rwl", "sr2", "srf", "srw", "x3f",
    ];
    if EBOOK.contains(&ext) {
        Category::Ebook
    } else if DOCUMENT.contains(&ext) {
        Category::Document
    } else if AUDIO.contains(&ext) {
        Category::Audio
    } else if RAW.contains(&ext) {
        Category::Raw
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
    ("cin", "Cineon Image File"),
    ("cur", "Microsoft icon"),
    ("cut", "DR Halo"),
    ("dcm", "DICOM medical image"),
    ("dcx", "ZSoft IBM PC multi-page Paintbrush"),
    ("dds", "Microsoft DirectDraw Surface"),
    ("dib", "Windows DIB"),
    ("dpx", "SMPTE 268M-2003"),
    ("dxt1", "Microsoft DirectDraw Surface"),
    ("dxt5", "Microsoft DirectDraw Surface"),
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
    ("icb", "Truevision Targa image"),
    ("ico", "Microsoft icon"),
    ("icon", "Microsoft icon"),
    ("j2c", "JPEG-2000 Code Stream Syntax"),
    ("j2k", "JPEG-2000 Code Stream Syntax"),
    ("jbig", "Joint Bi-level Image"),
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
    ("mac", "MacPaint"),
    ("mat", "MATLAB level 5 image format"),
    ("miff", "Magick Image File Format"),
    ("mng", "Multiple-image Network Graphics"),
    ("mpo", "Multi-Picture (3D) JPEG"),
    ("ora", "OpenRaster format"),
    // Art / CAD / 3D-print project files — we extract their embedded preview.
    ("kra", "Krita document"),
    ("3mf", "3D Manufacturing Format"),
    ("fcstd", "FreeCAD document"),
    ("gcode", "3D-printer G-code (sliced)"),
    ("gco", "3D-printer G-code (sliced)"),
    ("afphoto", "Affinity Photo document"),
    ("afdesign", "Affinity Designer document"),
    ("afpub", "Affinity Publisher document"),
    ("af", "Affinity document"),
    ("blend", "Blender scene"),
    ("clip", "Clip Studio Paint document"),
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
    ("wpg", "Word Perfect Graphics"),
    ("xbm", "X Windows system bitmap"),
    ("xcf", "GIMP image"),
    ("xpm", "X Windows system pixmap"),
    ("xv", "Khoros Visualization image"),
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
    // --- Documents (page 1 render or embedded preview) ---
    ("pdf", "Portable Document Format (page 1)"),
    ("djv", "DjVu document"),
    ("djvu", "DjVu document"),
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
    ("wav", "WAV audio (album art)"),
    ("aiff", "AIFF audio (album art)"),
    ("aif", "AIFF audio (album art)"),
];

/// Is `ext` (lowercase, no dot) one we hook?
pub fn is_known(ext: &str) -> bool {
    FORMATS.iter().any(|(e, _)| *e == ext)
}
