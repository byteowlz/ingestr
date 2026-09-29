//! Layout analysis (ADR-0005): crop charts, diagrams and pictures that the
//! native parse cannot deliver as images, and link them in reading order.
//!
//! LiteParse already extracts embedded raster images. What it cannot extract
//! are figures drawn with vector graphics (most charts in papers and
//! reports) and figures inside scanned pages. For those pages we render the
//! page, run PP-DocLayout_plus-L, crop every chart / figure / picture region
//! that no extracted image already covers, and insert a link into the page's
//! Markdown just above the first text below the region (usually its
//! caption).
//!
//! Only pages that can carry such figures are analysed: pages with vector
//! figure clusters and scanned pages. Plain text pages cost nothing.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::{Context, Result, anyhow};
use liteparse::ocr_merge::PageComplexityStats;
use liteparse::types::{ExtractedImage, Rect};
use liteparse::{LiteParse, LiteParseConfig, ParsedPage, TextItem};
use log::{debug, info};
use oar_ocr::predictors::LayoutDetectionPredictor;

use crate::models::{LAYOUT_MODEL_NAME, ensure_layout};
use crate::ocr_gate::{PageOcr, page_ocr};
use crate::pipeline::liteparse_runtime;
use crate::settings::LayoutConfig;

/// Region labels worth cropping. Header/footer images (logos) are left out.
const FIGURE_LABELS: &[&str] = &["chart", "figure", "image"];

/// Regions smaller than this share of the page are icons, not figures.
const MIN_PAGE_FRACTION: f32 = 0.02;

/// An extracted image counts as already covering a region when it overlaps
/// this share of the region's area.
const COVERED_FRACTION: f32 = 0.5;

/// Extracted images larger than this share of the page are page scans; the
/// figures inside them still need cropping.
const PAGE_SCAN_FRACTION: f32 = 0.8;

/// Vector clusters with more of their area under text are ruled tables.
/// Measured: charts 0.06, boxed diagrams 0.0-0.15, ruled tables mostly 0.2-0.43.
const MAX_FIGURE_TEXT_COVERAGE: f32 = 0.2;

/// How many text items below a region to try as the link anchor.
const MAX_ANCHOR_TRIES: usize = 8;

/// Shortest text usable as an anchor (shorter strings match anywhere).
const MIN_ANCHOR_CHARS: usize = 4;

/// Longest prefix of the anchor text searched for in the Markdown.
const ANCHOR_PREFIX_CHARS: usize = 24;

/// A detected region in page points (top-left origin), `x1`/`y1` exclusive.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Region {
    pub(crate) x0: f32,
    pub(crate) y0: f32,
    pub(crate) x1: f32,
    pub(crate) y1: f32,
}

impl Region {
    fn area(self) -> f32 {
        (self.x1 - self.x0).max(0.0) * (self.y1 - self.y0).max(0.0)
    }

    fn overlap(self, r: &Rect) -> f32 {
        let w = (self.x1.min(r.x + r.width) - self.x0.max(r.x)).max(0.0);
        let h = (self.y1.min(r.y + r.height) - self.y0.max(r.y)).max(0.0);
        w * h
    }
}

/// Share of `rect` covered by text items. Tables are dense with text; charts
/// and diagrams carry a few labels.
pub(crate) fn text_coverage(rect: &Rect, items: &[TextItem]) -> f32 {
    let area = rect.width * rect.height;
    if area <= 0.0 {
        return 0.0;
    }
    let covered: f32 = items
        .iter()
        .map(|t| {
            let w = ((t.x + t.width).min(rect.x + rect.width) - t.x.max(rect.x)).max(0.0);
            let h = ((t.y + t.height).min(rect.y + rect.height) - t.y.max(rect.y)).max(0.0);
            w * h
        })
        .sum();
    covered / area
}

/// Whether a page can hold figures the native parse misses: a scanned page
/// (the OCR gate wants the whole page read), or a vector figure cluster with
/// little text in it. `figure_text_coverage` holds one entry per cluster; a
/// densely texted cluster is a ruled table, not a figure.
pub(crate) fn wants_layout(stats: &PageComplexityStats, figure_text_coverage: &[f32]) -> bool {
    page_ocr(stats, false) == PageOcr::Yes
        || figure_text_coverage
            .iter()
            .any(|c| *c < MAX_FIGURE_TEXT_COVERAGE)
}

/// Keep the regions worth cropping: figure labels, confident, not tiny, and
/// not already delivered as an extracted image. Sorted top to bottom.
pub(crate) fn figure_regions(
    detections: &[(String, f32, Region)],
    page_w: f32,
    page_h: f32,
    embedded: &[Rect],
    min_score: f32,
) -> Vec<Region> {
    let page_area = page_w * page_h;
    let images: Vec<&Rect> = embedded
        .iter()
        .filter(|r| r.width * r.height < page_area * PAGE_SCAN_FRACTION)
        .collect();
    let mut out: Vec<Region> = detections
        .iter()
        .filter(|(label, score, _)| FIGURE_LABELS.contains(&label.as_str()) && *score >= min_score)
        .map(|(_, _, region)| *region)
        .filter(|r| r.area() >= page_area * MIN_PAGE_FRACTION)
        .filter(|r| {
            !images
                .iter()
                .any(|img| r.overlap(img) >= r.area() * COVERED_FRACTION)
        })
        .collect();
    out.sort_by(|a, b| a.y0.total_cmp(&b.y0).then(a.x0.total_cmp(&b.x0)));
    out
}

/// Insert `![](name)` into `markdown` just above the first text below
/// `region` (in reading order that is usually the caption). Falls back to
/// the end of the page when no anchor text can be found in the Markdown.
pub(crate) fn insert_link(
    markdown: &str,
    items: &[TextItem],
    region: Region,
    name: &str,
) -> String {
    let link = format!("![]({name})");
    let mut below: Vec<&TextItem> = items
        .iter()
        .filter(|t| t.y >= region.y1 - 1.0)
        .filter(|t| t.x < region.x1 && t.x + t.width > region.x0)
        .collect();
    below.sort_by(|a, b| a.y.total_cmp(&b.y).then(a.x.total_cmp(&b.x)));

    for item in below.into_iter().take(MAX_ANCHOR_TRIES) {
        let text = item.text.trim();
        if text.chars().count() < MIN_ANCHOR_CHARS {
            continue;
        }
        let anchor: String = text.chars().take(ANCHOR_PREFIX_CHARS).collect();
        if let Some(pos) = markdown.find(anchor.as_str()) {
            let line_start = markdown[..pos].rfind('\n').map_or(0, |i| i + 1);
            let (head, tail) = markdown.split_at(line_start);
            return format!("{head}{link}\n\n{tail}");
        }
    }
    let trimmed = markdown.trim_end();
    if trimmed.is_empty() {
        link
    } else {
        format!("{trimmed}\n\n{link}\n")
    }
}

static DETECTOR: LazyLock<Mutex<Option<Arc<LayoutDetectionPredictor>>>> =
    LazyLock::new(|| Mutex::new(None));

fn detector() -> Result<Arc<LayoutDetectionPredictor>> {
    let mut slot = DETECTOR
        .lock()
        .map_err(|_| anyhow!("layout model lock poisoned"))?;
    if let Some(det) = slot.as_ref() {
        return Ok(Arc::clone(det));
    }
    let path = ensure_layout()?;
    let det = LayoutDetectionPredictor::builder()
        .model_name(LAYOUT_MODEL_NAME)
        .build(&path)
        .map_err(|e| anyhow!("loading layout model {}: {e}", path.display()))?;
    let det = Arc::new(det);
    *slot = Some(Arc::clone(&det));
    Ok(det)
}

fn detect(
    det: &LayoutDetectionPredictor,
    img: &image::RgbImage,
    px_to_pt: f32,
) -> Result<Vec<(String, f32, Region)>> {
    let result = det
        .predict(vec![img.clone()])
        .map_err(|e| anyhow!("layout detection: {e}"))?;
    let elements = result.elements.into_iter().next().unwrap_or_default();
    Ok(elements
        .into_iter()
        .map(|e| {
            let xs = e.bbox.points.iter().map(|p| p.x);
            let ys = e.bbox.points.iter().map(|p| p.y);
            let region = Region {
                x0: xs.clone().fold(f32::MAX, f32::min).max(0.0) * px_to_pt,
                y0: ys.clone().fold(f32::MAX, f32::min).max(0.0) * px_to_pt,
                x1: xs.fold(0.0, f32::max) * px_to_pt,
                y1: ys.fold(0.0, f32::max) * px_to_pt,
            };
            (e.element_type, e.score, region)
        })
        .collect())
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "region coordinates are clamped to the non-negative image bounds"
)]
fn crop(img: &image::RgbImage, r: Region, pt_to_px: f32) -> Option<image::RgbImage> {
    let x0 = ((r.x0 * pt_to_px).max(0.0) as u32).min(img.width());
    let y0 = ((r.y0 * pt_to_px).max(0.0) as u32).min(img.height());
    let x1 = ((r.x1 * pt_to_px).max(0.0).ceil() as u32).min(img.width());
    let y1 = ((r.y1 * pt_to_px).max(0.0).ceil() as u32).min(img.height());
    (x1 > x0 && y1 > y0)
        .then(|| image::imageops::crop_imm(img, x0, y0, x1 - x0, y1 - y0).to_image())
}

/// Crop figures on the qualifying pages of `input`, write them into
/// `image_dir` and link them in each page's Markdown. Returns the number of
/// figures added. `pages` must come from a parse with complexity enabled.
pub(crate) fn add_figures(
    input: &Path,
    pages: &mut [ParsedPage],
    images: &[ExtractedImage],
    image_dir: &Path,
    cfg: &LayoutConfig,
) -> Result<usize> {
    let selected: Vec<u32> = pages
        .iter()
        .filter(|p| {
            let coverage: Vec<f32> = p
                .figures
                .iter()
                .map(|r| text_coverage(r, &p.text_items))
                .collect();
            p.complexity
                .as_ref()
                .is_some_and(|c| wants_layout(c, &coverage))
        })
        .filter_map(|p| u32::try_from(p.page_number).ok())
        .collect();
    if selected.is_empty() {
        debug!("layout: no page qualifies");
        return Ok(0);
    }
    info!("layout: analysing pages {selected:?}");
    let det = detector()?;
    let dpi = cfg.page_dpi.max(72);
    let renderer = LiteParse::new(LiteParseConfig {
        dpi: dpi as f32,
        ..Default::default()
    });
    let path = input
        .to_str()
        .ok_or_else(|| anyhow!("invalid path encoding"))?;
    let shots = liteparse_runtime()?
        .block_on(renderer.screenshot(path, Some(selected)))
        .with_context(|| format!("rendering pages of {}", input.display()))?;

    let mut embedded: BTreeMap<u32, Vec<Rect>> = BTreeMap::new();
    for img in images {
        embedded.entry(img.page).or_default().push(img.bbox.clone());
    }
    std::fs::create_dir_all(image_dir)
        .with_context(|| format!("creating {}", image_dir.display()))?;

    let pt_to_px = dpi as f32 / 72.0;
    let mut added = 0;
    for shot in shots {
        let Some(page) = pages
            .iter_mut()
            .find(|p| u32::try_from(p.page_number).ok() == Some(shot.page_num))
        else {
            continue;
        };
        let img = image::load_from_memory(&shot.image_bytes)
            .with_context(|| format!("decoding render of page {}", shot.page_num))?
            .into_rgb8();
        let detections = detect(&det, &img, 1.0 / pt_to_px)?;
        let none = Vec::new();
        let regions = figure_regions(
            &detections,
            page.page_width,
            page.page_height,
            embedded.get(&shot.page_num).unwrap_or(&none),
            cfg.min_score,
        );
        debug!(
            "layout: page {} -> {} figure(s)",
            shot.page_num,
            regions.len()
        );
        for (k, region) in regions.into_iter().enumerate() {
            let Some(cropped) = crop(&img, region, pt_to_px) else {
                continue;
            };
            let name = format!("fig_p{}_{}.png", shot.page_num, k + 1);
            cropped
                .save(image_dir.join(&name))
                .with_context(|| format!("writing {name}"))?;
            page.markdown = insert_link(&page.markdown, &page.text_items, region, &name);
            added += 1;
        }
    }
    Ok(added)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(reasons: &[&str], full_page_image: bool, chars: usize) -> PageComplexityStats {
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

    fn region(x0: f32, y0: f32, x1: f32, y1: f32) -> Region {
        Region { x0, y0, x1, y1 }
    }

    fn rect(x: f32, y: f32, width: f32, height: f32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn item(text: &str, x: f32, y: f32) -> TextItem {
        TextItem {
            text: text.into(),
            x,
            y,
            width: 200.0,
            height: 10.0,
            ..Default::default()
        }
    }

    #[test]
    fn layout_runs_only_on_figure_or_scanned_pages() {
        let plain = stats(&[], false, 500);
        assert!(!wants_layout(&plain, &[]));
        // Scanned pages: the OCR gate reads the whole page.
        assert!(wants_layout(&stats(&["scanned"], true, 0), &[]));
        // A logo on every page, or a short page, is no reason.
        assert!(!wants_layout(&stats(&["embedded-images"], false, 500), &[]));
        assert!(!wants_layout(&stats(&["sparse-text"], false, 80), &[]));
        // Vector clusters: a chart has a few labels, a ruled table is dense.
        assert!(wants_layout(&plain, &[0.06]));
        assert!(!wants_layout(&plain, &[0.3, 0.25]));
        assert!(wants_layout(&plain, &[0.3, 0.1]));
    }

    #[test]
    fn text_coverage_measures_share_under_text() {
        let area = rect(0.0, 0.0, 100.0, 100.0);
        let items = [
            TextItem {
                x: 0.0,
                y: 0.0,
                width: 50.0,
                height: 10.0,
                ..Default::default()
            },
            // Half outside the rect.
            TextItem {
                x: 90.0,
                y: 50.0,
                width: 20.0,
                height: 10.0,
                ..Default::default()
            },
        ];
        assert!((text_coverage(&area, &items) - 0.06).abs() < 1e-6);
        assert!(text_coverage(&rect(0.0, 0.0, 0.0, 10.0), &items).abs() < f32::EPSILON);
    }

    #[test]
    fn keeps_confident_figures_not_logos_or_extracted_images() {
        let dets = vec![
            ("chart".to_string(), 0.9, region(50.0, 300.0, 550.0, 600.0)),
            ("image".to_string(), 0.9, region(50.0, 100.0, 300.0, 250.0)),
            (
                "header_image".to_string(),
                0.9,
                region(0.0, 0.0, 200.0, 60.0),
            ),
            ("table".to_string(), 0.9, region(50.0, 650.0, 550.0, 750.0)),
            ("figure".to_string(), 0.3, region(50.0, 650.0, 550.0, 750.0)),
            ("image".to_string(), 0.9, region(10.0, 10.0, 20.0, 20.0)),
        ];
        // The picture at the top was already extracted as an embedded image.
        let embedded = [rect(50.0, 100.0, 250.0, 150.0)];
        let kept = figure_regions(&dets, 612.0, 792.0, &embedded, 0.5);
        assert_eq!(kept, vec![region(50.0, 300.0, 550.0, 600.0)]);
    }

    #[test]
    fn figures_inside_a_page_scan_are_kept() {
        let dets = vec![("chart".to_string(), 0.9, region(50.0, 300.0, 550.0, 600.0))];
        let scan = [rect(0.0, 0.0, 612.0, 792.0)];
        assert_eq!(figure_regions(&dets, 612.0, 792.0, &scan, 0.5).len(), 1);
    }

    #[test]
    fn link_goes_above_the_caption() {
        let md =
            "Intro text\n\nAIME 2024 Codeforces\n\nFigure 1 | Benchmark performance\n\nMore text\n";
        let items = [
            item("AIME 2024", 60.0, 250.0),
            item("Figure 1 | Benchmark performance", 60.0, 420.0),
            item("More text", 60.0, 500.0),
        ];
        let out = insert_link(
            md,
            &items,
            region(50.0, 100.0, 550.0, 400.0),
            "fig_p1_1.png",
        );
        assert_eq!(
            out,
            "Intro text\n\nAIME 2024 Codeforces\n\n![](fig_p1_1.png)\n\nFigure 1 | Benchmark performance\n\nMore text\n"
        );
    }

    #[test]
    fn link_falls_back_to_page_end() {
        let out = insert_link("Only text\n", &[], region(0.0, 0.0, 10.0, 10.0), "f.png");
        assert_eq!(out, "Only text\n\n![](f.png)\n");
    }
}
