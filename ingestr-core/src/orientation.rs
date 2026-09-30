//! Page orientation for scans: a page scanned sideways or upside down OCRs to
//! nothing or to garbage. Before the OCR parse, the full-page scans the OCR
//! gate picked are rendered small and classified (PP-LCNet doc orientation,
//! 0/90/180/270). Confidently rotated pages are handed to LiteParse as
//! `page_orientation_corrections`, which turns them upright for text
//! extraction, OCR and rendering alike.

use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::{Context, Result, anyhow};
use liteparse::config::PageOrientationCorrection;
use liteparse::{LiteParse, LiteParseConfig};
use log::{debug, info, warn};
use oar_ocr::predictors::DocumentOrientationPredictor;

use crate::models::ensure_doc_orientation;
use crate::pipeline::liteparse_runtime;

/// Render DPI for classification; the model looks at a 224 px thumbnail.
const CLASSIFY_DPI: f32 = 72.0;

/// Below this score a page is left as it is: turning an upright page is far
/// worse than leaving a rotated one. The model's scores top out around 0.93
/// (measured on clean scans at every rotation and render DPI), so a bar close
/// to that would reject correct answers.
const MIN_SCORE: f32 = 0.75;

static CLASSIFIER: LazyLock<Mutex<Option<Arc<DocumentOrientationPredictor>>>> =
    LazyLock::new(|| Mutex::new(None));

fn classifier() -> Result<Arc<DocumentOrientationPredictor>> {
    let mut slot = CLASSIFIER
        .lock()
        .map_err(|_| anyhow!("orientation model lock poisoned"))?;
    if let Some(c) = slot.as_ref() {
        return Ok(Arc::clone(c));
    }
    let path = ensure_doc_orientation()?;
    let c = Arc::new(
        DocumentOrientationPredictor::builder()
            .build(&path)
            .map_err(|e| anyhow!("loading orientation model {}: {e}", path.display()))?,
    );
    *slot = Some(Arc::clone(&c));
    Ok(c)
}

/// The correction for one classified page, if it should be turned.
pub(crate) fn correction(page: u32, label: &str, score: f32) -> Option<PageOrientationCorrection> {
    let angle: u16 = label.trim().parse().ok()?;
    (matches!(angle, 90 | 180 | 270) && score >= MIN_SCORE)
        .then_some(PageOrientationCorrection { page, angle })
}

/// Orientation corrections for `pages` (1-based) of the PDF at `path`.
/// A failure costs the correction, never the conversion.
pub(crate) fn corrections(path: &Path, pages: &[u32]) -> Vec<PageOrientationCorrection> {
    if pages.is_empty() {
        return Vec::new();
    }
    match classify(path, pages) {
        Ok(found) => {
            if !found.is_empty() {
                info!("orientation: turning pages upright {found:?}");
            }
            found
        }
        Err(e) => {
            warn!("orientation check failed ({e:#}); pages left as they are");
            Vec::new()
        }
    }
}

fn classify(path: &Path, pages: &[u32]) -> Result<Vec<PageOrientationCorrection>> {
    let path_str = path
        .to_str()
        .ok_or_else(|| anyhow!("invalid path encoding"))?;
    let renderer = LiteParse::new(LiteParseConfig {
        dpi: CLASSIFY_DPI,
        quiet: true,
        ..Default::default()
    });
    let shots = liteparse_runtime()?
        .block_on(renderer.screenshot(path_str, Some(pages.to_vec())))
        .with_context(|| format!("rendering pages of {}", path.display()))?;
    let model = classifier()?;
    let mut out = Vec::new();
    for shot in shots {
        let img = image::load_from_memory(&shot.image_bytes)
            .with_context(|| format!("decoding render of page {}", shot.page_num))?
            .into_rgb8();
        let result = model
            .predict(vec![img])
            .map_err(|e| anyhow!("orientation of page {}: {e}", shot.page_num))?;
        let Some(top) = result.orientations.first().and_then(|c| c.first()) else {
            continue;
        };
        debug!(
            "orientation: page {} -> {} ({:.3})",
            shot.page_num, top.label, top.score
        );
        out.extend(correction(shot.page_num, &top.label, top.score));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_confident_rotations_become_corrections() {
        assert_eq!(
            correction(3, "180", 0.99),
            Some(PageOrientationCorrection {
                page: 3,
                angle: 180
            })
        );
        assert_eq!(correction(1, "270", 0.92).map(|c| c.angle), Some(270));
        // Upright, unsure, or unknown labels leave the page alone.
        assert_eq!(correction(1, "0", 0.99), None);
        assert_eq!(correction(1, "90", 0.6), None);
        assert_eq!(correction(1, "90", 0.8).map(|c| c.angle), Some(90));
        assert_eq!(correction(1, "45", 0.99), None);
        assert_eq!(correction(1, "upright", 0.99), None);
    }
}
