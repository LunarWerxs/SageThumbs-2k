#![cfg(test)]

//! Seeds for the design, CAD and paint application formats.

use super::*;

/// Affinity Designer/Photo: the four-byte signature, then an embedded PNG the scanner carves
/// by hunting for the signature and the IEND terminator.
pub(super) fn synthetic_affinity() -> Vec<u8> {
    let mut out = vec![0x00, 0xFF, 0x4B, 0x41];
    out.extend_from_slice(&[0u8; 64]);
    out.extend_from_slice(&png(64, 64)); // under the 512 px preference threshold
    out.extend_from_slice(&[0u8; 32]);
    out
}

/// InDesign: the 16-byte document GUID, then an XMP `<xmpGImg:image>` element holding a
/// base64 JPEG.
pub(super) fn synthetic_indd() -> Vec<u8> {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(jpeg(32, 24));
    let mut out = vec![
        0x06, 0x06, 0xED, 0xF5, 0xD8, 0x1D, 0x46, 0xE5, 0xBD, 0x31, 0xEF, 0xE7, 0xFE, 0x74, 0xB7,
        0x1D,
    ];
    out.extend_from_slice(b"<x:xmpmeta><xmpGImg:image>");
    // Real files break the payload with XML newline entities, which the decoder has to skip —
    // so put one in, or that branch is never taken.
    out.extend_from_slice(b64.as_bytes());
    out.extend_from_slice(b"&#xA;");
    out.extend_from_slice(b"</xmpGImg:image></x:xmpmeta>");
    out
}

/// Blender: the legacy 4-byte-pointer header and a `TEST` block, which is where Blender stores
/// its bottom-up RGBA preview.
pub(super) fn synthetic_blend() -> Vec<u8> {
    const W: u32 = 12;
    const H: u32 = 10;
    let px: Vec<u8> = (0..(W * H * 4)).map(|i| (i % 255) as u8).collect();

    let mut out = b"BLENDER_v300".to_vec(); // '_' => 4-byte pointers, blocks start at 12
    out.extend_from_slice(b"TEST");
    // The length must be EXACTLY 8 + the pixels, or the extractor declines — which is the
    // arithmetic worth mutating.
    out.extend_from_slice(&((8 + px.len()) as i32).to_le_bytes());
    out.extend_from_slice(&[0u8; 12]); // rest of the 20-byte BHead4
    out.extend_from_slice(&(W as i32).to_le_bytes());
    out.extend_from_slice(&(H as i32).to_le_bytes());
    out.extend_from_slice(&px);
    out.extend_from_slice(b"ENDB");
    out.extend_from_slice(&[0u8; 16]);
    out
}

/// AutoCAD: the version tag, a pointer at 0x0D to the preview section, the 16-byte sentinel,
/// and a one-entry image table whose type-6 record points at a PNG.
pub(super) fn synthetic_dwg() -> Vec<u8> {
    const SENTINEL: [u8; 16] = [
        0x1F, 0x25, 0x6D, 0x07, 0xD4, 0x36, 0x28, 0x28, 0x9D, 0x57, 0xCA, 0x3F, 0x9D, 0x44, 0x10,
        0x2B,
    ];
    let p = png(48, 48);
    let imgptr = 0x20u32;
    let png_off = imgptr + 16 + 4 + 1 + 9; // sentinel + size + count + one 9-byte record

    let mut out = b"AC1015".to_vec();
    out.resize(0x0D, 0);
    out.extend_from_slice(&imgptr.to_le_bytes());
    out.resize(imgptr as usize, 0);
    out.extend_from_slice(&SENTINEL);
    out.extend_from_slice(&((p.len() + 14) as u32).to_le_bytes()); // overall image-data size
    out.push(1); // one table entry
    out.push(6); // code 6 = PNG
    out.extend_from_slice(&png_off.to_le_bytes());
    out.extend_from_slice(&(p.len() as u32).to_le_bytes());
    debug_assert_eq!(out.len(), png_off as usize);
    out.extend_from_slice(&p);
    out
}

/// Clip Studio Paint: the CSFCHUNK wrapper, a `CHNKHead` pointing at a `CHNKSQLi`, and a REAL
/// SQLite database inside it.
///
/// The database is the committed `tests/fixtures/sqlite/sample.db` rather than a hand-built
/// one, deliberately: it was written by a real SQLite, so mutating it exercises the b-tree
/// walk against structures this code did not invent. That walk resolves page pointers out of
/// the file's own bytes, so a crafted file can turn the tree into a cycle — the exact thing a
/// hand-assembled fixture would be least likely to model.
pub(super) fn synthetic_clip() -> Vec<u8> {
    let db: &[u8] = include_bytes!("../../../../../tests/fixtures/sqlite/sample.db");
    let mut out = b"CSFCHUNK".to_vec();
    out.extend_from_slice(&[0u8; 8]);
    out.extend_from_slice(&24u64.to_be_bytes()); // pointer to the first chunk
    out.extend_from_slice(b"CHNKHead");
    out.extend_from_slice(&16u64.to_be_bytes()); // header chunk length
    out.extend_from_slice(&[0u8; 8]);
    out.extend_from_slice(&56u64.to_be_bytes()); // pointer to the SQLite chunk
    out.extend_from_slice(b"CHNKSQLi");
    out.extend_from_slice(&(db.len() as u64).to_be_bytes());
    out.extend_from_slice(db);
    out
}

/// Paint Shop Pro: 32-byte signature, version, a decoy block, then the Composite Image Bank
/// (id 16) whose content is an 8-byte info chunk followed by the JPEG.
///
/// Rebuilt here rather than reached from `psp::tests`: that builder lives inside a private
/// `mod tests`, and promoting it would mean editing a module this change has no other business
/// in. The duplication is safe because `every_seed_reaches_its_parser` fails the moment this
/// drifts from what the real parser accepts.
pub(super) fn synthetic_psp(jpeg_bytes: &[u8]) -> Vec<u8> {
    const SIG: &[u8] = b"Paint Shop Pro Image File\n\x1a";
    const BK: [u8; 4] = [0x7E, 0x42, 0x4B, 0x00]; // "~BK\0"
    let mut f = Vec::new();
    f.extend_from_slice(SIG);
    f.extend_from_slice(&vec![0u8; 32 - SIG.len()]); // pad the signature to 32
    f.extend_from_slice(&8u16.to_le_bytes()); // major version
    f.extend_from_slice(&0u16.to_le_bytes()); // minor version
                                              // A decoy block first, so the block walk has more than one hop to get wrong.
    f.extend_from_slice(&BK);
    f.extend_from_slice(&0u16.to_le_bytes()); // block id 0: general image attributes
    f.extend_from_slice(&4u32.to_le_bytes());
    f.extend_from_slice(&[1, 2, 3, 4]);
    let mut content = Vec::new();
    content.extend_from_slice(&8u32.to_le_bytes()); // info chunk size
    content.extend_from_slice(&1u32.to_le_bytes()); // composite image count
    content.extend_from_slice(jpeg_bytes);
    f.extend_from_slice(&BK);
    f.extend_from_slice(&16u16.to_le_bytes()); // Composite Image Bank
    f.extend_from_slice(&(content.len() as u32).to_le_bytes());
    f.extend_from_slice(&content);
    f
}

/// Cinema 4D: the header byte and tag, filler, then the scene-preview JPEG, then a far-away
/// material swatch (the case the real extractor has to NOT pick).
pub(super) fn synthetic_c4d(gap: usize, preview: &[u8], swatch: &[u8]) -> Vec<u8> {
    let mut f = vec![0x36];
    f.extend_from_slice(b"C4DC4D6");
    f.extend_from_slice(&[0u8; 4]);
    f.resize(8 + gap, 0);
    f.extend_from_slice(preview);
    f.resize(f.len() + 4000, 0);
    f.extend_from_slice(swatch);
    f
}

/// Deflate `data` and return the compressed bytes, or an empty `Vec` on the (practically
/// unreachable) encoder-write failure — avoids `unwrap` the way every other builder in this
/// file does (`write_to(...)` ignored, `unwrap_or_default()`), keeping the `unwrap_used` lint
/// happy even though this `#![cfg(test)]` module would not require it.
pub(super) fn zlib_compress(data: &[u8]) -> Vec<u8> {
    use std::io::Write as _;
    let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    let _ = enc.write_all(data);
    enc.finish().unwrap_or_default()
}

/// GIMP XCF: a v0 header (no precision word — the simplest of the version-gated shapes),
/// one RGBA layer with an uncompressed 8×8 tile. Exercises the full header -> properties ->
/// layer -> hierarchy -> level -> tile walk `xcf::extract` does, with opaque pixel data so
/// the flattened result is not all-transparent (which `extract` treats as "nothing decoded"
/// and declines).
pub(super) fn synthetic_xcf() -> Vec<u8> {
    const W: u32 = 8;
    const H: u32 = 8;

    let mut out = b"gimp xcf file\0".to_vec(); // v0: "file", no precision word follows
    debug_assert_eq!(out.len(), 14);
    out.extend_from_slice(&W.to_be_bytes());
    out.extend_from_slice(&H.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes()); // base_type — unread by decode_layer

    // Property list: one PROP_COMPRESSION (0 = none, so the tile below is a raw copy), then
    // PROP_END.
    out.extend_from_slice(&17u32.to_be_bytes());
    out.extend_from_slice(&1u32.to_be_bytes());
    out.push(0);
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());

    // Layer pointer list: one layer, then the 0 terminator.
    let layer_off = out.len() as u32 + 8;
    out.extend_from_slice(&layer_off.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    debug_assert_eq!(out.len(), layer_off as usize);

    // Layer: lw, lh, ltype=1 (RGBA), an empty name, PROP_END, the hierarchy pointer, then a
    // null mask pointer (no layer mask).
    out.extend_from_slice(&W.to_be_bytes());
    out.extend_from_slice(&H.to_be_bytes());
    out.extend_from_slice(&1u32.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes()); // name_len = 0
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    let hier_off = out.len() as u32 + 8;
    out.extend_from_slice(&hier_off.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    debug_assert_eq!(out.len(), hier_off as usize);

    // Hierarchy: hw/hh (present but unread beyond consuming the bytes), bpp=4 (RGBA at one
    // byte per sample), then the first (only) level pointer.
    out.extend_from_slice(&W.to_be_bytes());
    out.extend_from_slice(&H.to_be_bytes());
    out.extend_from_slice(&4u32.to_be_bytes());
    let level_off = out.len() as u32 + 4;
    out.extend_from_slice(&level_off.to_be_bytes());
    debug_assert_eq!(out.len(), level_off as usize);

    // Level: must restate lw/lh exactly, then one tile pointer — W×H fits inside a single
    // 64×64 tile, so the grid is 1×1.
    out.extend_from_slice(&W.to_be_bytes());
    out.extend_from_slice(&H.to_be_bytes());
    let tile_off = out.len() as u32 + 4;
    out.extend_from_slice(&tile_off.to_be_bytes());
    debug_assert_eq!(out.len(), tile_off as usize);

    // The tile itself, COMPRESS_NONE: raw interleaved RGBA bytes, fully opaque.
    for _ in 0..(W * H) {
        out.extend_from_slice(&[200, 50, 50, 255]);
    }
    out
}

/// SketchUp `.skp`: the ASCII header the older-format sniff matches, then an embedded PNG
/// the way a GUI save bakes one in (see `skp::tests::carves_first_embedded_png`, which this
/// mirrors).
pub(super) fn synthetic_skp() -> Vec<u8> {
    let mut f = b"SketchUp Model".to_vec();
    f.extend_from_slice(&[0u8; 16]);
    f.extend_from_slice(&png(24, 24));
    f.extend_from_slice(&[0xAB; 64]); // trailing model data
    f
}

/// Rhino `.3dm`: the header string, the `TCODE_PROPERTIES_COMPRESSED_PREVIEWIMAGE` chunk
/// (a 40-byte BITMAPINFOHEADER + the ON compressed-buffer framing `rhino::extract` scans
/// past), and a zlib-deflated 24bpp DIB pixel buffer — the same construction as
/// `rhino::tests::inflates_and_wraps_a_dib`.
pub(super) fn synthetic_rhino() -> Vec<u8> {
    const PREVIEW_TYPECODE: [u8; 4] = [0x25, 0x80, 0x00, 0x20];
    let (w, h) = (2i32, 2i32);
    let stride = 2 * 3 + 2; // 6 bytes/row padded to 8 (24bpp, no BI_BITFIELDS)
    let pixels = vec![0u8; stride * h as usize];

    let mut bmih = Vec::new();
    bmih.extend_from_slice(&40u32.to_le_bytes()); // biSize
    bmih.extend_from_slice(&w.to_le_bytes());
    bmih.extend_from_slice(&h.to_le_bytes());
    bmih.extend_from_slice(&1u16.to_le_bytes()); // planes
    bmih.extend_from_slice(&24u16.to_le_bytes()); // bitcount
    bmih.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    bmih.extend_from_slice(&(pixels.len() as u32).to_le_bytes());
    bmih.extend_from_slice(&[0u8; 16]); // xppm/yppm/clrused/clrimportant

    let zbytes = zlib_compress(&pixels);

    let mut chunk_content = Vec::new();
    chunk_content.extend_from_slice(&bmih);
    chunk_content.extend_from_slice(&(pixels.len() as u32).to_le_bytes()); // uncompressedSize
    chunk_content.extend_from_slice(&0u32.to_le_bytes()); // crc (unchecked)
    chunk_content.push(1); // method = deflate
    chunk_content.extend_from_slice(&0u32.to_le_bytes()); // nested typecode
    chunk_content.extend_from_slice(&(zbytes.len() as u64).to_le_bytes()); // nested len
    chunk_content.extend_from_slice(&zbytes);

    let mut f = b"3D Geometry File Format 7\0".to_vec();
    f.extend_from_slice(&PREVIEW_TYPECODE);
    f.extend_from_slice(&(chunk_content.len() as u64).to_le_bytes());
    f.extend_from_slice(&chunk_content);
    f
}

/// Krita `.kra`: the `mimetype`-keyed project-preview path (see `project::tests`), a stored
/// zip so mutations land on the mimetype string / preview PNG rather than a DEFLATE checksum.
pub(super) fn synthetic_project() -> Vec<u8> {
    stored_zip(&[
        ("mimetype", b"application/x-krita"),
        ("mergedimage.png", &png(16, 16)),
    ])
}

/// Pixelorama 1.0+ `.pxo`: the mimetype the project branch keys off, the root `preview.png`
/// it extracts, and a `data.json` beside them as the real files carry.
pub(super) fn synthetic_pxo() -> Vec<u8> {
    stored_zip(&[
        ("mimetype", b"application/x-pixelorama"),
        ("data.json", br#"{"size_x":16,"size_y":16,"frames":[]}"#),
        ("preview.png", &png(16, 16)),
    ])
}

/// An Aseprite sprite built by the module's OWN builder, so the bytes the fuzzer mutates and
/// the bytes its tests prove the parser on cannot drift apart: an RGBA layer under a
/// half-opacity zlib-compressed one, offset so the composite blends.
pub(super) fn synthetic_aseprite() -> Vec<u8> {
    use crate::container::aseprite::synth::{build, CelSpec, LayerSpec};
    let red: Vec<u8> = [255u8, 0, 0, 255].repeat(16);
    let blue: Vec<u8> = [0u8, 0, 255, 255].repeat(16);
    let layer = |visible, opacity| LayerSpec {
        visible,
        opacity,
        is_group: false,
        child_level: 0,
        background: false,
    };
    // Struct literals rather than a helper closure: a closure returning `CelSpec<'_>` would tie
    // both cels to ONE inferred lifetime, which two separate locals cannot satisfy.
    let bottom = CelSpec {
        layer: 0,
        x: 0,
        y: 0,
        opacity: 255,
        z: 0,
        w: 4,
        h: 4,
        pixels: &red,
        zlib: false,
    };
    let top = CelSpec {
        layer: 1,
        x: 2,
        y: 2,
        opacity: 255,
        z: 0,
        w: 4,
        h: 4,
        pixels: &blue,
        zlib: true,
    };
    build(
        8,
        8,
        32,
        0,
        &[],
        &[layer(true, 255), layer(true, 128)],
        &[bottom, top],
    )
}

/// Seattle FilmWorks: a small two-colour picture wrapped by the module's own builder, so the
/// marker walk and the Huffman splice are what gets mutated.
pub(super) fn synthetic_sfw() -> Vec<u8> {
    let picture = image::RgbImage::from_fn(16, 16, |_, y| {
        if y < 8 {
            image::Rgb([220, 30, 30])
        } else {
            image::Rgb([30, 30, 220])
        }
    });
    sfw::synth(&picture, false)
}

/// Alias PIX: three colour rows, the first wide enough to need more than one run.
pub(super) fn synthetic_pix() -> Vec<u8> {
    pix::synth(300, &[[200, 10, 20], [10, 200, 20], [10, 20, 200]], false)
}

/// SolidWorks: the same OLE container the other compound-file seeds use, with a PNG in a
/// stream named `PreviewPNG` rather than `SummaryInformation`.
pub(super) fn synthetic_solidworks() -> Vec<u8> {
    synthetic_ole_named("PreviewPNG", &png(16, 16))
}

/// SpriteLoop `.spla`: a two-part rig whose frame 0 places both parts on a 32x32 canvas, one
/// of them rotated, skewed, scaled, faded and tinted, so the whole affine path is on the fuzz
/// surface and not only the identity placement.
pub(super) fn synthetic_spla() -> Vec<u8> {
    stored_zip(&[
        (
            "manifest.json",
            br#"{"format":"spla","version":1,"name":"seed","canvas":{"width":32,"height":32},"parts":[{"id":"a","name":"a","asset":"assets/asset_0001.png","width":16,"height":16,"pivot":{"x":8,"y":8},"drawOrder":0},{"id":"b","name":"b","asset":"assets/asset_0002.png","width":16,"height":16,"pivot":{"x":0,"y":0},"drawOrder":1}],"animations":[{"id":"idle","name":"idle","fps":24,"loop":true,"frameCount":1,"frames":[{"index":0,"sourceFrame":0,"parts":[{"part":"a","x":16,"y":16,"rotation":30,"skewX":10,"skewY":0,"scaleX":0.75,"scaleY":1.25,"opacity":0.9},{"part":"b","x":4,"y":4,"rotation":0,"scaleX":1,"scaleY":1,"opacity":1,"tint":[1,0.5,0.5]}]}]}]}"#,
        ),
        ("assets/asset_0001.png", &png(16, 16)),
        ("assets/asset_0002.png", &png(16, 16)),
    ])
}
