//! `djvu ocr` and OCR backend selection. Compiled only with an `ocr-*` feature.

use super::*;

pub(super) fn cmd_ocr(
    path: &Path,
    backend: OcrBackendChoice,
    lang: &str,
    model_path: Option<&Path>,
    output: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    use djvu_rs::ocr::OcrOptions;

    // Fail early on a misconfigured backend before any per-page work.
    let ocr_backend = build_ocr_backend(backend.clone(), model_path)?;

    let data = std::fs::read(path)?;
    let mut doc_mut = djvu_rs::djvu_mut::DjVuDocumentMut::from_bytes(&data)?;
    let _ = doc_mut.page_mut(0)?;

    let doc = djvu_rs::djvu_document::DjVuDocument::parse(&data)?;

    // OCR each page and inject the recognized text layer. Pages are
    // independent and OCR dominates wall-clock, so with the `parallel`
    // feature the render+recognize fan out over rayon (#573) — one backend
    // instance per task (`recognize` builds a fresh Tesseract per call, so
    // instances never cross threads; the onnx backend re-optimizes its tract
    // plans per page here — cross-page plan reuse is a known follow-up); text
    // layers are injected sequentially
    // in page order afterwards, keeping the output bytes identical to the
    // sequential path.
    let count = doc.page_count();
    let ocr_one = |i: usize,
                   be: &dyn djvu_rs::ocr::OcrBackend|
     -> Result<djvu_rs::text::TextLayer, String> {
        let page = doc.page(i).map_err(|e| e.to_string())?;
        let w = page.width() as u32;
        let h = page.height() as u32;
        let opts = djvu_rs::djvu_render::RenderOptions {
            width: w,
            height: h,
            ..Default::default()
        };
        let pixmap = djvu_rs::djvu_render::render_pixmap(page, &opts).map_err(|e| e.to_string())?;
        // The render above is at the page's native resolution — tell the
        // recognizer the true dpi (#603: a hard-coded 300 mis-scaled OCR on
        // 400/600-dpi scans; Tesseract's segmentation is dpi-sensitive).
        let options = OcrOptions {
            languages: lang.to_string(),
            dpi: page.dpi() as u32,
        };
        be.recognize(&pixmap, &options).map_err(|e| e.to_string())
    };

    #[cfg(feature = "parallel")]
    let layers: Vec<djvu_rs::text::TextLayer> = {
        use rayon::prelude::*;
        drop(ocr_backend);
        let model_path = model_path.map(Path::to_path_buf);
        (0..count)
            .into_par_iter()
            .map(|i| {
                let be = build_ocr_backend(backend.clone(), model_path.as_deref())
                    .map_err(|e| e.to_string())?;
                ocr_one(i, be.as_ref())
            })
            // Collect first, then take the first error in page order: rayon's
            // own `Result` collect returns whichever error comes first in time.
            .collect::<Vec<_>>()
            .into_iter()
            .collect::<Result<Vec<_>, String>>()?
    };
    #[cfg(not(feature = "parallel"))]
    let layers: Vec<djvu_rs::text::TextLayer> = (0..count)
        .map(|i| ocr_one(i, ocr_backend.as_ref()))
        .collect::<Result<Vec<_>, String>>()?;

    for (i, text_layer) in layers.iter().enumerate() {
        eprintln!(
            "Page {}: {} chars, {} zones",
            i + 1,
            text_layer.text.len(),
            text_layer.zones.len()
        );
        doc_mut.page_mut(i)?.set_text_layer(text_layer)?;
    }

    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }

    std::fs::write(output, doc_mut.try_into_bytes()?)?;
    eprintln!(
        "OCR complete. Embedded text layers for {count} page(s) into {}",
        output.display()
    );

    Ok(())
}

pub(super) fn build_ocr_backend(
    backend: OcrBackendChoice,
    model_path: Option<&Path>,
) -> Result<Box<dyn djvu_rs::ocr::OcrBackend>, Box<dyn std::error::Error>> {
    match backend {
        OcrBackendChoice::Tesseract => {
            let _ = model_path;
            #[cfg(feature = "ocr-tesseract")]
            {
                Ok(Box::new(djvu_rs::ocr_tesseract::TesseractBackend::new()))
            }
            #[cfg(not(feature = "ocr-tesseract"))]
            {
                Err(
                    "Tesseract OCR backend is not enabled; rebuild with --features ocr-tesseract"
                        .into(),
                )
            }
        }
        OcrBackendChoice::Onnx => {
            #[cfg(feature = "ocr-onnx")]
            {
                // Models come only from the pinned manifest (SHA-256 verified);
                // an ad-hoc --model path would bypass that verification.
                if model_path.is_some() {
                    return Err(
                        "--backend onnx does not take --model: models are pinned by \
                         docs/ocr-model-manifest.toml; fetch them with \
                         scripts/fetch_ocr_models.sh (directory override: \
                         DJVU_OCR_MODELS_DIR)"
                            .into(),
                    );
                }
                Ok(Box::new(
                    djvu_rs::ocr_onnx::pipeline::NeuralOcrBackend::load_default()?,
                ))
            }
            #[cfg(not(feature = "ocr-onnx"))]
            {
                let _ = model_path;
                Err("ONNX OCR backend is not enabled; rebuild with --features ocr-onnx".into())
            }
        }
        OcrBackendChoice::Candle => {
            let _ = model_path;
            Err(
                "Candle OCR backend is experimental and has no supported model-specific \
                 implementation yet; use --backend tesseract with --features ocr-tesseract"
                    .into(),
            )
        }
    }
}
