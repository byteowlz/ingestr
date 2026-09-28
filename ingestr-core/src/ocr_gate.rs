//! Whether a PDF needs OCR at all (ingestr-38tn).
//!
//! A cheap LiteParse classification pass gives per-page reasons. Most pages
//! are decided from those alone. Pages whose only question is "do these
//! embedded images contain text?" (a pasted screenshot vs. a photo) get a
//! second look: their images are extracted and run through PP-OCR's text
//! *detection* model only, which takes milliseconds per image.
//!
//! The gate is per document: once any page needs OCR, LiteParse OCRs every
//! page it flags.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::{Result, anyhow};
use liteparse::ocr_merge::{ComplexityReason as R, PageComplexityStats};
use liteparse::{LiteParse, LiteParseConfig};
use log::{debug, warn};
use oar_ocr::predictors::TextDetectionPredictor;

use crate::models::{PpOcrTier, ensure_ppocr};
use crate::pipeline::liteparse_runtime;
use crate::settings::OcrConfig;

/// OCR decision for one page from its classification alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PageOcr {
    /// OCR recovers text the native layer lacks.
    Yes,
    /// The native layer already has the page's text, or there is nothing to read.
    No,
    /// Only if one of the page's embedded images contains text.
    IfImagesHaveText,
}

/// Per-page decision. Unknown reasons are ignored.
pub(crate) fn page_ocr(stats: &PageComplexityStats, ocr_sparse_pages: bool) -> PageOcr {
    if !stats.needs_ocr {
        return PageOcr::No;
    }
    let has = |reason: R| stats.reasons.contains(&reason);
    let inline_images = has(R::EmbeddedImages);
    if has(R::Scanned) || has(R::Garbled) || has(R::VectorText) || has(R::AnnotationText) {
        return PageOcr::Yes;
    }
    if has(R::NoText) {
        // Blank, or text drawn in a way the cheap pass cannot see: OCR. A page
        // holding only inline pictures is worth OCR only if they carry text.
        return if inline_images && !stats.full_page_image {
            PageOcr::IfImagesHaveText
        } else {
            PageOcr::Yes
        };
    }
    if has(R::SparseText) {
        // A scan whose only native text is a small stamp, or recall-first
        // mode. A searchable scan (full hidden text layer) has far more text.
        let stamped_scan = stats.full_page_image && stats.text_length < STAMP_MAX_CHARS;
        if stamped_scan || ocr_sparse_pages {
            return PageOcr::Yes;
        }
    }
    if inline_images {
        return PageOcr::IfImagesHaveText;
    }
    PageOcr::No
}

/// Native text on a full-page scan up to which it is a stamp or header, not a
/// searchable scan's hidden text layer (measured: those carry 500+ chars).
const STAMP_MAX_CHARS: usize = 200;
/// An image must cover at least this fraction of the page to matter; logos
/// and icons with a word or two stay below it.
const MIN_IMAGE_PAGE_FRACTION: f32 = 0.03;
/// Line-shaped text boxes needed to call an image textual. Measured: pasted
/// text 11-20, diagrams with labels 4-8, a chart 3, photos 0.
const MIN_TEXT_LINES: usize = 4;
/// Longest image side fed to the detector.
const DETECT_MAX_SIDE: u32 = 960;

/// Whether detected boxes look like real text: enough boxes wider than tall.
pub(crate) fn is_textual(box_sizes: &[(f32, f32)]) -> bool {
    box_sizes.iter().filter(|(w, h)| *w > 2.0 * *h).count() >= MIN_TEXT_LINES
}

static DETECTORS: LazyLock<Mutex<HashMap<&'static str, Arc<TextDetectionPredictor>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn detector(tier: PpOcrTier) -> Result<Arc<TextDetectionPredictor>> {
    let mut cache = DETECTORS
        .lock()
        .map_err(|_| anyhow!("text detector cache poisoned"))?;
    if let Some(det) = cache.get(tier.as_str()) {
        return Ok(Arc::clone(det));
    }
    let models = ensure_ppocr(tier)?;
    let det = Arc::new(
        TextDetectionPredictor::builder()
            .build(models.det.as_path())
            .map_err(|e| anyhow!("loading text detector: {e}"))?,
    );
    cache.insert(tier.as_str(), Arc::clone(&det));
    Ok(det)
}

fn image_has_text(det: &TextDetectionPredictor, path: &Path) -> Result<bool> {
    let img = image::open(path)?.into_rgb8();
    let (w, h) = img.dimensions();
    let longest = w.max(h);
    let img = if longest > DETECT_MAX_SIDE {
        let scale = f64::from(DETECT_MAX_SIDE) / f64::from(longest);
        let size = |v: u32| ((f64::from(v) * scale).round().max(1.0)) as u32;
        image::imageops::resize(
            &img,
            size(w),
            size(h),
            image::imageops::FilterType::Triangle,
        )
    } else {
        img
    };
    let result = det
        .predict(vec![img])
        .map_err(|e| anyhow!("text detection: {e}"))?;
    let sizes: Vec<(f32, f32)> = result
        .detections
        .first()
        .map(|boxes| {
            boxes
                .iter()
                .map(|d| {
                    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
                    for p in &d.bbox.points {
                        x0 = x0.min(p.x);
                        y0 = y0.min(p.y);
                        x1 = x1.max(p.x);
                        y1 = y1.max(p.y);
                    }
                    (x1 - x0, y1 - y0)
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(is_textual(&sizes))
}

/// Whether any sizeable embedded image on `pages` contains text.
fn images_have_text(
    path: &str,
    pages: &HashSet<usize>,
    page_area: f32,
    ocr: &OcrConfig,
) -> Result<bool> {
    let det = detector(PpOcrTier::parse(&ocr.paddle_model))?;
    let dir = tempfile::tempdir()?;
    let parser = LiteParse::new(LiteParseConfig {
        ocr_enabled: false,
        quiet: true,
        extract_images: true,
        image_output_dir: Some(dir.path().to_string_lossy().into_owned()),
        ..Default::default()
    });
    let parsed = liteparse_runtime()?.block_on(parser.parse(path))?;
    for image in &parsed.images {
        let (Some(file), true) = (&image.path, pages.contains(&(image.page as usize))) else {
            continue;
        };
        if page_area > 0.0
            && image.bbox.width * image.bbox.height / page_area < MIN_IMAGE_PAGE_FRACTION
        {
            continue;
        }
        if image_has_text(&det, Path::new(file))? {
            debug!(
                "OCR needed: text found in embedded image on page {}",
                image.page
            );
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether a PDF needs OCR at all. Any failure returns `true`, so OCR is
/// never skipped by mistake.
pub(crate) fn pdf_needs_ocr(path: &Path, ocr: &OcrConfig) -> bool {
    let Some(path_str) = path.to_str() else {
        return true;
    };
    let Ok(rt) = liteparse_runtime() else {
        return true;
    };
    let probe = LiteParse::new(LiteParseConfig {
        ocr_enabled: false,
        quiet: true,
        ..Default::default()
    });
    let input = liteparse::types::PdfInput::Path(path_str.to_string());
    let stats = match rt.block_on(probe.is_complex(input)) {
        Ok(stats) => stats,
        Err(e) => {
            warn!(
                "page classification failed for {}; OCR-ing anyway: {e}",
                path.display()
            );
            return true;
        }
    };
    let mut image_pages = HashSet::new();
    let mut page_area = 0.0_f32;
    for page in &stats {
        match page_ocr(page, ocr.ocr_sparse_pages) {
            PageOcr::Yes => return true,
            PageOcr::No => {}
            PageOcr::IfImagesHaveText => {
                image_pages.insert(page.page_number);
                page_area = page_area.max(page.page_area);
            }
        }
    }
    if image_pages.is_empty() {
        debug!(
            "OCR skipped for {}: {} page(s), none need it",
            path.display(),
            stats.len()
        );
        return false;
    }
    match images_have_text(path_str, &image_pages, page_area, ocr) {
        Ok(true) => true,
        Ok(false) => {
            debug!(
                "OCR skipped for {}: embedded images on {} page(s) hold no text",
                path.display(),
                image_pages.len()
            );
            false
        }
        Err(e) => {
            warn!(
                "image text check failed for {}; OCR-ing anyway: {e:#}",
                path.display()
            );
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(reasons: &[R], full_page_image: bool) -> PageComplexityStats {
        stats_with_text(reasons, full_page_image, 40)
    }

    fn stats_with_text(reasons: &[R], full_page_image: bool, chars: usize) -> PageComplexityStats {
        serde_json::from_value(serde_json::json!({
            "page_number": 1, "text_length": chars, "text_coverage": 0.0,
            "has_substantial_images": false, "image_block_count": 0,
            "image_coverage": 0.0, "largest_image_coverage": 0.0,
            "full_page_image": full_page_image, "uncovered_vector_area": null,
            "is_garbled": false, "page_area": 500_000.0,
            "needs_ocr": !reasons.is_empty(), "reasons": reasons,
        }))
        .unwrap_or_else(|e| unreachable!("test stats: {e}"))
    }

    #[test]
    fn decision_table() {
        use PageOcr::{IfImagesHaveText as Img, No, Yes};
        let cases: &[(&[R], bool, bool, PageOcr)] = &[
            (&[], false, false, No),
            (&[R::Scanned], true, false, Yes),
            (&[R::Garbled], false, false, Yes),
            (&[R::VectorText], false, false, Yes),
            (&[R::NoText, R::AnnotationText], false, false, Yes),
            // Blank or outlined text: OCR.
            (&[R::NoText], false, false, Yes),
            // Only inline pictures: depends on whether they hold text.
            (&[R::NoText, R::EmbeddedImages], false, false, Img),
            // Short native page: skip unless recall-first.
            (&[R::SparseText], false, false, No),
            (&[R::SparseText], false, true, Yes),
            // A scan with a small native stamp.
            (&[R::SparseText], true, false, Yes),
            // Images next to text: only if they contain text.
            (&[R::EmbeddedImages], false, false, Img),
            (&[R::SparseText, R::EmbeddedImages], false, false, Img),
        ];
        for (reasons, full_page, sparse_mode, want) in cases {
            assert_eq!(
                page_ocr(&stats(reasons, *full_page), *sparse_mode),
                *want,
                "{reasons:?} full_page={full_page} sparse_mode={sparse_mode}"
            );
        }
    }

    #[test]
    fn searchable_scan_is_not_a_stamped_scan() {
        let page = stats_with_text(&[R::SparseText], true, 1_100);
        assert_eq!(page_ocr(&page, false), PageOcr::No);
    }

    #[test]
    fn textual_needs_several_line_shaped_boxes() {
        let line = (200.0, 20.0);
        let blob = (50.0, 40.0);
        assert!(!is_textual(&[]));
        assert!(!is_textual(&[line, line, line, blob, blob]));
        assert!(is_textual(&[line, line, line, line]));
    }
}
