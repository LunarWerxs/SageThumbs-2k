//! The MCP tool catalog: every tool's name, description and JSON-Schema for its arguments,
//! the data table `tools/list` hands the client. Its own file since 2026-09-20 (`mcp.rs` had
//! crossed the 800-line mark); nothing here changed, it only moved.

use serde_json::{json, Value};
use st2k_base::formats;

/// The tool catalog (name + description + JSON-Schema for arguments).
pub(super) fn tool_defs() -> Value {
    let str_prop = |desc: &str| json!({ "type": "string", "description": desc });
    let thumbnail_desc = format!(
        "Render any supported image ({} formats Windows often can't, incl. HEIC/RAW/PSD/ebook covers) to an image file, capped to a max long-edge size.",
        formats::FORMATS.len()
    );
    let view_desc = format!(
        "Decode a file and RETURN IT AS AN IMAGE you can see directly — for any of the {} supported formats Windows often can't open (HEIC/RAW/PSD/ebook & comic covers/CAD previews/audio cover art/…). Use it to look at, describe, caption, OCR-by-eye, or analyze a file's visual content. Returns an image content block, not a file path.",
        formats::FORMATS.len()
    );
    json!([
        {
            "name": "thumbnail",
            "description": thumbnail_desc,
            "inputSchema": { "type": "object", "properties": {
                "input": str_prop("path to the source image"),
                "output": str_prop("path to write; output format is taken from this extension (.png/.jpg/…)"),
                "size": { "type": "integer", "description": "max long-edge in px (default 256; 0 = full size)" }
            }, "required": ["input", "output"] }
        },
        {
            "name": "view",
            "description": view_desc,
            "inputSchema": { "type": "object", "properties": {
                "input": str_prop("path to the source file"),
                "size": { "type": "integer", "description": "max long-edge in px (default 512)" }
            }, "required": ["input"] }
        },
        {
            "name": "convert",
            "description": "Convert an image to another format (format from the output extension), with optional quality and resize.",
            "inputSchema": { "type": "object", "properties": {
                "input": str_prop("source image path"),
                "output": str_prop("destination path; format from its extension"),
                "quality": { "type": "integer", "description": "encoder quality 1-100 (JPEG; default 90)" },
                "webp_quality": { "type": "integer", "description": "1-100 → lossy WebP at this quality (only for .webp output; omit for lossless WebP)" },
                "resize": str_prop("optional 'WxH' (fit, no upscale) or 'N%' (scale)")
            }, "required": ["input", "output"] }
        },
        {
            "name": "compress",
            "description": "Compress an image to a target file size → a '(compressed)' JPEG sibling at or under the limit (quality binary-search, then downscale if needed). Success means at or under the limit: a limit the search cannot meet fails, writes nothing, and the error names the smallest size reachable, so ask again with at least that.",
            "inputSchema": { "type": "object", "properties": {
                "input": str_prop("source image path"),
                "max_size": str_prop("target size, e.g. '1MB', '500KB', or a byte count")
            }, "required": ["input", "max_size"] }
        },
        {
            "name": "rotate",
            "description": "Rotate or flip an image, writing a new '(edited)' sibling file (never re-compresses the original in place).",
            "inputSchema": { "type": "object", "properties": {
                "input": str_prop("source image path"),
                "by": { "type": "string", "enum": ["right", "left", "180", "fliph", "flipv"], "description": "transform to apply" }
            }, "required": ["input", "by"] }
        },
        {
            "name": "strip",
            // Synced to strip.rs's real match arms (jpg/jpeg/jpe/jfif, png, webp, svg/svgz,
            // heic/heif/hif/avif) — this used to say "JPEG or PNG" only, understating what an
            // agent could actually call it on.
            "description": "Losslessly strip EXIF/IPTC/XMP metadata from a JPEG, PNG, WebP, SVG/SVGZ, or HEIC/HEIF/AVIF file in place (keeps the ICC color profile where present; no pixel re-encode).",
            "inputSchema": { "type": "object", "properties": {
                "input": str_prop("JPEG/PNG/WebP/SVG(Z)/HEIC/HEIF/AVIF path")
            }, "required": ["input"] }
        },
        {
            "name": "ocr",
            "description": "Recognize text in an image and return it (Windows OCR; needs a language pack installed).",
            "inputSchema": { "type": "object", "properties": {
                "input": str_prop("image path")
            }, "required": ["input"] }
        },
        {
            "name": "pdf",
            "description": "Combine one or more images into a single PDF (one image per page); with 'searchable' each page is OCR'd by the Windows engine and given an invisible text layer, so the PDF's text can be searched, selected and copied. 'output' must be a .pdf path and must not be one of the inputs (any spelling, case or hard link of an input is refused before anything is written). Returns JSON: {output, status: 'ok'|'partial', requested, combined, omitted: [{input, cause: 'unreadable'|'undecodable'|'unencodable', detail}]}; an input that cannot be used is left out and listed under 'omitted' unless 'strict' is true, in which case the call fails and writes nothing.",
            "inputSchema": { "type": "object", "properties": {
                "output": str_prop("destination .pdf path"),
                "inputs": { "type": "array", "items": { "type": "string" }, "description": "image paths, in page order" },
                "strict": { "type": "boolean", "description": "fail and write nothing if any input would be left out (default false = build from the usable inputs and list the rest)" },
                "searchable": { "type": "boolean", "description": "OCR every page and add an invisible, searchable text layer (default false); fails if no page could be recognised, e.g. no Windows OCR language installed" }
            }, "required": ["output", "inputs"] }
        },
        {
            "name": "cbz",
            "description": "Combine one or more images into a single CBZ (comic-book zip) archive, natural-sorted, with a ComicInfo.xml sidecar. 'output' must be a .cbz path and must not be one of the inputs (any spelling, case or hard link of an input is refused before anything is written). Returns the same JSON as 'pdf' ({output, status, requested, combined, omitted[]}); an input that cannot be read is left out and listed under 'omitted' unless 'strict' is true, in which case the call fails and writes nothing.",
            "inputSchema": { "type": "object", "properties": {
                "output": str_prop("destination .cbz path"),
                "inputs": { "type": "array", "items": { "type": "string" }, "description": "image paths, in page order" },
                "strict": { "type": "boolean", "description": "fail and write nothing if any input would be left out (default false = build from the usable inputs and list the rest)" }
            }, "required": ["output", "inputs"] }
        },
        {
            "name": "info",
            "description": "Read an image's dimensions, bit depth, DPI and EXIF camera/date/GPS — or, for an audio file (mp3/flac/wma/dsf/…), its artist/album/title/track/genre/year/duration/bitrate tags. Returns JSON.",
            "inputSchema": { "type": "object", "properties": {
                "input": str_prop("image or audio file path")
            }, "required": ["input"] }
        },
        {
            "name": "formats",
            "description": "List every supported input format (extension, category, description). Returns JSON.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "doctor",
            "description": "Read-only self-check: is the shell extension registered, loadable, and enabled? Diagnoses \"why aren't thumbnails showing\". Returns a paste-ready text report with a FIX per finding; optionally probes one file's decode as well.",
            "inputSchema": { "type": "object", "properties": {
                "file": str_prop("optional path to also probe (does this ONE file decode)")
            } }
        },
        {
            "name": "batch",
            "description": "Bulk-process many files/folders in one process: thumbnail, convert, or read info (dimensions/EXIF/audio tags, as one JSON array) for every supported file found. Each input directory is scanned ONE level deep unless 'recurse' is true. For thumbnail/convert the result is JSON: {status, requested, succeeded, failed, skipped_offline, results[]}, one entry per input carrying {input, output, status, cause, detail, elapsed_ms} - so a failed file is retryable by path and its 'cause' says whether it was unreadable, undecodable, unencodable or unwritable. Partial success is deliberate: a file that fails does not stop the rest. Every input failing is a tool error naming each one.",
            "inputSchema": { "type": "object", "properties": {
                "op": { "type": "string", "enum": ["thumbnail", "convert", "info"], "description": "operation to run on every input" },
                "inputs": { "type": "array", "items": { "type": "string" }, "description": "file and/or folder paths" },
                "recurse": { "type": "boolean", "description": "walk input directories recursively (default false = one level deep)" },
                "out": str_prop("output directory (default: alongside each source file; ignored for info)"),
                "size": { "type": "integer", "description": "thumbnail max long-edge in px (default 256; ignored for convert/info)" },
                "to": str_prop("output extension, required when op is \"convert\""),
                "quality": { "type": "integer", "description": "encoder quality 1-100 (default 90; ignored for info)" },
                "resize": str_prop("optional 'WxH' (fit, no upscale) or 'N%' (scale), convert only")
            }, "required": ["op", "inputs"] }
        },
        {
            "name": "prebuild",
            "description": "Pre-build Explorer's thumbnail cache for whole folders (so browsing them later is instant). Refuses to run elevated (the cache is per-user). Returns a built/cached/failed summary.",
            "inputSchema": { "type": "object", "properties": {
                "inputs": { "type": "array", "items": { "type": "string" }, "description": "file and/or folder paths" },
                "recurse": { "type": "boolean", "description": "walk input directories recursively (default false = one level deep)" },
                "sizes": { "type": "array", "items": { "type": "integer", "minimum": 1 }, "minItems": 1, "description": "edge sizes in px to build (omit for the default 96,256,768, Explorer's Medium/Large/Extra-large buckets). If given it must be a non-empty array of whole numbers above 0; every element is checked and one bad element fails the call, rather than being dropped" },
                "rebuild_all": { "type": "boolean", "description": "skip the already-cached probe and rebuild every file (default false)" },
                "jobs": { "type": "integer", "description": "worker threads (default 3)" }
            }, "required": ["inputs"] }
        },
        {
            "name": "register_status",
            "description": "Portable build only: report whether Explorer thumbnails are currently registered for this user.",
            "inputSchema": { "type": "object", "properties": {} }
        }
    ])
}
