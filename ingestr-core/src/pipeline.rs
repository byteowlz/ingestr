//! The conversion pipeline: engine routing (LiteParse / markitdown), OCR
//! gating and backends, VLM, and the optional semantic router.

use std::any::Any;
use std::collections::HashMap;
use std::env;
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::Command as ProcCommand;
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use liteparse::ocr::oar::OarOcrEngine;
// Aliased: ingestr also imports `ocrs::OcrEngine` for the legacy image path.
use liteparse::ocr::{OcrEngine as LiteOcrEngine, OcrOptions, OcrResult};
use liteparse::{LiteParse, LiteParseConfig, OutputFormat};
use log::{debug, info, warn};
use markitdown::{MarkItDown, model::ConversionOptions};
use ocrs::{ImageSource, OcrEngine, OcrEngineParams};
use rten::Model as RtenModel;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::runtime::Runtime;

use crate::formats::{extension_of, is_liteparse_extension, is_office_extension};
use crate::settings::*;

/// The result of converting one document.
#[derive(Debug, Clone)]
pub struct ConvertedDocument {
    /// Document title, when the engine could determine one.
    pub title: Option<String>,
    /// The Markdown body.
    pub text_content: String,
    /// If true, content was already streamed to a file during VLM processing
    pub already_written: bool,
}

pub(crate) fn panic_payload_to_string(payload: Box<dyn Any + Send>) -> String {
    match payload.downcast::<String>() {
        Ok(message) => *message,
        Err(payload) => match payload.downcast::<&'static str>() {
            Ok(message) => message.to_string(),
            Err(_) => "unknown panic payload".to_string(),
        },
    }
}

/// Run a markitdown conversion, turning errors and panics into `None` (with a
/// warning) so one bad file never takes down a batch or the watch service.
pub fn safe_markitdown_convert<T, E, F>(context: &str, convert: F) -> Option<T>
where
    F: FnOnce() -> std::result::Result<Option<T>, E>,
    E: fmt::Display,
{
    match catch_unwind(AssertUnwindSafe(convert)) {
        Ok(Ok(result)) => result,
        Ok(Err(err)) => {
            warn!("markitdown failed while {context}: {err}");
            None
        }
        Err(payload) => {
            warn!(
                "markitdown panicked while {}: {}",
                context,
                panic_payload_to_string(payload)
            );
            None
        }
    }
}

/// Prefix Markdown with a `---` frontmatter block (JSON, a valid YAML subset).
pub fn render_frontmatter_markdown(
    frontmatter: &Frontmatter,
    text_content: &str,
) -> Result<String> {
    // Frontmatter is serialized as JSON, which is a valid YAML subset, so
    // standard markdown frontmatter consumers (`---` blocks) still parse it.
    let json = serde_json::to_string(frontmatter).context("serializing frontmatter")?;
    let mut body = String::new();
    body.push_str("---\n");
    body.push_str(&json);
    body.push_str("\n---\n\n");
    body.push_str(text_content);
    Ok(body)
}

/// Processor that handles document conversion with fallback chains
pub struct DocumentProcessor {
    pub(crate) markitdown: MarkItDown,
    pub(crate) settings: ProcessorSettings,
    pub(crate) vlm_enabled: bool,
    pub(crate) vlm_model: Option<String>,
    pub(crate) vlm_prompt: Option<String>,
    pub(crate) jobs: usize,
    /// Output file for streaming VLM results (pages written as they complete)
    pub(crate) vlm_output: Option<PathBuf>,
    pub(crate) ocr_enabled: bool,
    pub(crate) ocr_backend: OcrBackend,
    pub(crate) ocr_languages: Vec<String>,
    /// Render DPI used for OCR-ing scanned pages.
    pub(crate) ocr_page_dpi: u32,
    /// Directory where LiteParse writes extracted embedded images (e.g. figures
    /// / charts from a PPTX). `None` keeps image references in the Markdown
    /// without writing image files.
    pub(crate) image_output_dir: Option<PathBuf>,
    /// Engine selection (auto / liteparse / markitdown).
    pub(crate) engine: ConvertEngine,
}

impl fmt::Debug for DocumentProcessor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DocumentProcessor")
            .field("engine", &self.engine)
            .field("ocr_enabled", &self.ocr_enabled)
            .field("ocr_backend", &self.ocr_backend)
            .field("vlm_enabled", &self.vlm_enabled)
            .finish_non_exhaustive()
    }
}

impl DocumentProcessor {
    /// Create a processor with defaults (no VLM, no OCR, `auto` engine).
    pub fn new(settings: ProcessorSettings) -> Self {
        Self {
            markitdown: MarkItDown::new(),
            settings,
            vlm_enabled: false,
            vlm_model: None,
            vlm_prompt: None,
            jobs: 1,
            vlm_output: None,
            ocr_enabled: false,
            ocr_backend: OcrBackend::Paddle,
            ocr_languages: vec!["eng".to_string()],
            ocr_page_dpi: 300,
            image_output_dir: None,
            engine: ConvertEngine::Auto,
        }
    }

    /// Enable VLM conversion (`enabled` or config), with optional model/prompt
    /// overrides, page parallelism, and a file to stream pages into.
    pub fn with_vlm(
        mut self,
        enabled: bool,
        model: Option<String>,
        prompt: Option<String>,
        jobs: usize,
        output: Option<PathBuf>,
    ) -> Self {
        self.vlm_enabled = enabled || self.settings.processors.vlm.enabled;
        self.vlm_model = model;
        self.vlm_prompt = prompt;
        self.jobs = jobs.max(1);
        self.vlm_output = output;
        self
    }

    /// Enable OCR (`enabled` or config) with the given backend and languages.
    pub fn with_ocr(
        mut self,
        enabled: bool,
        backend: OcrBackend,
        languages: Option<Vec<String>>,
    ) -> Self {
        self.ocr_enabled = enabled || self.settings.processors.ocr.enabled;
        if enabled {
            self.ocr_backend = backend;
        } else {
            self.ocr_backend = self.settings.processors.ocr.backend;
        }
        self.ocr_languages =
            languages.unwrap_or_else(|| self.settings.processors.ocr.languages.clone());
        self.ocr_page_dpi = self.settings.processors.ocr.page_dpi.max(72);
        self
    }

    /// Write extracted embedded images into this directory.
    pub fn with_image_output_dir(mut self, output_dir: Option<PathBuf>) -> Self {
        self.image_output_dir = output_dir;
        self
    }

    /// Select the conversion engine.
    pub fn with_engine(mut self, engine: ConvertEngine) -> Self {
        self.engine = engine;
        self
    }

    /// Build the compact `RoutingInput` metadata block the router sees from a
    /// document on disk.
    pub(crate) fn build_routing_input(
        &self,
        input: &Path,
        extension: &str,
    ) -> Result<RoutingInput> {
        let data =
            fs::read(input).with_context(|| format!("reading {} for routing", input.display()))?;
        let text = String::from_utf8_lossy(&data);
        let text_chars = text.chars().count();
        let snippet: String = text.chars().take(120).collect();
        Ok(RoutingInput {
            extension: extension.to_string(),
            filename: input
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string(),
            file_bytes: data.len(),
            is_complex: false,
            needs_ocr: false,
            text_chars,
            snippet,
        })
    }

    /// The semantic tier-router seam. Returns `Some(Ok(converted))` when the
    /// router selection should redirect processing (e.g. `vlm` in `route` mode
    /// when VLM is available); otherwise returns `None` to let the existing
    /// deterministic pipeline run unchanged. Any router failure logs a warning
    /// and falls back to heuristic behavior.
    pub(crate) fn route_input(
        &self,
        input: &Path,
        extension: &str,
    ) -> Option<Result<ConvertedDocument>> {
        let cfg = &self.settings.routing;
        if cfg.mode == RoutingMode::Heuristic {
            return None;
        }
        let router_url = cfg.router_url.clone().filter(|s| !s.is_empty())?;
        let signals = match self.build_routing_input(input, extension) {
            Ok(s) => s,
            Err(e) => {
                warn!("[routing] could not build routing state, keeping heuristic: {e}");
                return None;
            }
        };
        let decision = match route_document(
            &router_url,
            &cfg.model,
            cfg.api_key.as_deref(),
            input,
            &signals,
        ) {
            Ok(d) => d,
            Err(e) => {
                warn!("[routing] router unavailable, falling back to heuristic: {e}");
                return None;
            }
        };
        info!(
            "[routing] mode={:?} tier={} confidence={:.2} probabilities={:?} model={} latency_ms={:?}",
            cfg.mode,
            decision.tier.as_str(),
            decision.confidence,
            decision.probabilities,
            decision.model,
            decision.latency_ms,
        );
        if cfg.mode == RoutingMode::Route && decision.tier == RoutingTier::Vlm && self.vlm_enabled {
            if is_image_extension(extension) {
                return Some(self.process_with_vlm(input, extension));
            }
            if is_pdf_extension(extension) {
                return Some(self.process_pdf_with_vlm(input));
            }
            if is_presentation_extension(extension) {
                return Some(self.process_pptx_with_vlm(input));
            }
            debug!(
                "[routing] router selected vlm but no VLM-capable path for extension={extension}"
            );
        }
        None
    }

    /// Convert one file to Markdown.
    pub fn process(&self, input: &Path) -> Result<ConvertedDocument> {
        let extension = input
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_lowercase)
            .unwrap_or_default();

        // Semantic tier-router seam (`[routing]`). In `heuristic` (default)
        // mode this is a strict no-op so existing behavior is unchanged. In
        // `shadow` mode it logs the router decision; in `route` mode it may
        // redirect to the VLM path. See `route_input`.
        if let Some(result) = self.route_input(input, &extension) {
            return result;
        }

        // Check if VLM is enabled for images, PDFs, or presentations
        if self.vlm_enabled && is_image_extension(&extension) {
            return self.process_with_vlm(input, &extension);
        }
        if self.vlm_enabled && is_pdf_extension(&extension) {
            return self.process_pdf_with_vlm(input);
        }
        if self.vlm_enabled && is_presentation_extension(&extension) {
            return self.process_pptx_with_vlm(input);
        }

        // Check for encrypted PDFs before passing to markitdown to avoid panics
        if is_pdf_extension(&extension) {
            match is_pdf_encrypted(input) {
                Ok(true) => {
                    // Encrypted PDF - try OCR if available, otherwise return error
                    if self.ocr_enabled
                        && let Ok(ocr_result) = self.process_with_ocr(input)
                        && !ocr_result.text_content.trim().is_empty()
                    {
                        return Ok(ocr_result);
                    }
                    bail!(
                        "PDF is encrypted/password-protected and cannot be converted. \
                        Consider using --vlm flag to process it via a vision model, \
                        or --ocr flag to extract text via OCR."
                    );
                }
                Ok(false) => {} // Not encrypted, proceed with markitdown
                Err(e) => {
                    warn!(
                        "Could not check PDF encryption status: {e}, attempting conversion anyway"
                    );
                }
            }
        }

        // PDF + office path: use LiteParse as the Tier-0 parser. For PDFs it
        // extracts native text for text/vector pages and OCRs only scanned/
        // text-sparse pages, so mixed PDFs no longer drop scanned pages. For
        // PPTX/DOCX/XLSX it converts via LibreOffice and extracts per-slide /
        // per-section text plus embedded images. This replaces markitdown for
        // these formats (markitdown's PPTX path is broken).
        if self.engine != ConvertEngine::Markitdown && is_liteparse_extension(&extension) {
            match self.process_pdf_with_liteparse(input) {
                Ok(parsed) if !parsed.text_content.trim().is_empty() => return Ok(parsed),
                Ok(_) if self.engine == ConvertEngine::Liteparse => {
                    bail!("liteparse produced no text for {}", input.display());
                }
                Err(e) if self.engine == ConvertEngine::Liteparse => return Err(e),
                Ok(_) => {}
                Err(e) => warn!(
                    "liteparse failed on {}, falling back to markitdown: {e}",
                    input.display()
                ),
            }
        }

        // Try markitdown first
        let conversion_opts = self.build_conversion_options();
        let path_str = input
            .to_str()
            .ok_or_else(|| anyhow!("invalid path encoding"))?;
        let context = format!("converting {}", input.display());

        match safe_markitdown_convert(&context, || {
            self.markitdown.convert(path_str, conversion_opts)
        }) {
            Some(result) if !result.text_content.trim().is_empty() => {
                return Ok(ConvertedDocument {
                    title: result.title,
                    text_content: result.text_content,
                    already_written: false,
                });
            }
            _ => {}
        }

        // If OCR is enabled and we got empty content, try OCR
        if self.ocr_enabled
            && (is_pdf_extension(&extension) || is_image_extension(&extension))
            && let Ok(ocr_result) = self.process_with_ocr(input)
            && !ocr_result.text_content.trim().is_empty()
        {
            return Ok(ocr_result);
        }

        if self.ocr_enabled && is_image_extension(&extension) {
            bail!(
                "no text found in {} (OCR produced no output)",
                input.display()
            );
        }

        // Fallback: try reading as text
        let text = fs::read_to_string(input).with_context(|| {
            format!(
                "no converter available for {}, and file could not be read as UTF-8 text",
                input.display()
            )
        })?;

        Ok(ConvertedDocument {
            title: None,
            text_content: text,
            already_written: false,
        })
    }

    /// Resolve effective VLM connection settings.
    /// Priority: CLI --vlm-model > [processors.vlm].model > [llm].model
    /// Priority: [processors.vlm].`llm_url` > [llm].`base_url` > localhost:11434
    pub(crate) fn vlm_connection(&self) -> (String, String) {
        let vlm = &self.settings.processors.vlm;
        let url = vlm
            .llm_url
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| self.settings.llm_base_url.clone())
            .unwrap_or_else(|| "http://localhost:11434".to_string())
            .trim_end_matches('/')
            .to_string();
        let model = self
            .vlm_model
            .clone()
            .or_else(|| vlm.model.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.settings.llm_model.clone());
        info!("VLM connection: url={url}, model={model}");
        (url, model)
    }

    pub(crate) fn build_conversion_options(&self) -> Option<ConversionOptions> {
        if self.settings.llm_enabled {
            Some(ConversionOptions {
                file_extension: None,
                url: None,
                llm_client: Some(self.settings.llm_client.clone()),
                llm_model: Some(self.settings.llm_model.clone()),
            })
        } else {
            None
        }
    }

    pub(crate) fn process_with_vlm(
        &self,
        input: &Path,
        _extension: &str,
    ) -> Result<ConvertedDocument> {
        let vlm_config = &self.settings.processors.vlm;
        let prompt = self
            .vlm_prompt
            .as_ref()
            .or(vlm_config.prompts.get("default"))
            .map_or(
                "Describe this image in detail.",
                std::string::String::as_str,
            );

        // Read and encode image as base64
        let image_data =
            fs::read(input).with_context(|| format!("reading image file {}", input.display()))?;
        let base64_image = base64_encode(&image_data);

        // Determine MIME type
        let extension = input.extension().and_then(|e| e.to_str()).unwrap_or("png");
        let mime_type = match extension.to_lowercase().as_str() {
            "jpg" | "jpeg" => "image/jpeg",
            "png" => "image/png",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "bmp" => "image/bmp",
            _ => "image/png",
        };

        // Call VLM API
        let (url, model) = self.vlm_connection();
        let description = call_vlm_api(
            &url,
            &model,
            &base64_image,
            mime_type,
            prompt,
            self.settings.llm_api_key.as_deref(),
        )?;

        let title = input
            .file_stem()
            .and_then(|s| s.to_str())
            .map(std::string::ToString::to_string);

        Ok(ConvertedDocument {
            title,
            text_content: format!(
                "# Image: {}\n\n{}",
                input
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("image"),
                description
            ),
            already_written: false,
        })
    }

    pub(crate) fn process_with_ocr(&self, input: &Path) -> Result<ConvertedDocument> {
        let text = if self.ocr_backend == OcrBackend::Paddle {
            run_paddle_on_image(input, &self.settings.processors.ocr.paddle_model)?
        } else {
            run_ocr(input, self.ocr_backend, &self.ocr_languages)?
        };

        let title = input
            .file_stem()
            .and_then(|s| s.to_str())
            .map(std::string::ToString::to_string);

        Ok(ConvertedDocument {
            title,
            text_content: text,
            already_written: false,
        })
    }

    /// Convert a PDF or office document to Markdown with LiteParse.
    ///
    /// LiteParse is the Tier-0 parser: for PDFs it extracts text/vector pages
    /// natively (PDFium, ~2-5ms/page, no ML model) and only OCRs the pages it
    /// classifies as scanned or text-sparse. For office formats (pptx/docx/
    /// xlsx/…) it converts via LibreOffice and extracts per-slide/per-section
    /// text plus embedded images. It replaces both the markitdown native path
    /// and the hand-rolled page routing.
    pub(crate) fn process_pdf_with_liteparse(&self, input: &Path) -> Result<ConvertedDocument> {
        // Office formats are converted via LibreOffice; make a missing install
        // a clear, actionable error instead of a cryptic failure.
        if is_office_extension(&extension_of(input)) && !has_libreoffice() {
            bail!(
                "LibreOffice is required to convert {} (see `ingestr doctor`). \
                Install it with: apt-get install libreoffice (Debian/Ubuntu) \
                or brew install --cask libreoffice (macOS)",
                input.display()
            );
        }

        let path_str = input
            .to_str()
            .ok_or_else(|| anyhow!("invalid path encoding"))?;

        let ocr_lang = self
            .ocr_languages
            .first()
            .map(String::as_str)
            .unwrap_or("eng");
        let image_dir = self
            .image_output_dir
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());
        // OCR gating. Office documents always carry a native text layer after
        // the LibreOffice conversion, and most PDFs do too, so only enable OCR
        // when the cheap page classification says a page truly needs it (see
        // `pdf_needs_ocr`). The gate is per document: once enabled, LiteParse
        // OCRs every page it flags.
        let ext = extension_of(input);
        let ocr_enabled = self.ocr_enabled
            && !is_office_extension(&ext)
            && (ext != "pdf"
                || pdf_needs_ocr(input, self.settings.processors.ocr.ocr_sparse_pages));
        let config = LiteParseConfig {
            output_format: OutputFormat::Markdown,
            ocr_enabled,
            ocr_language: map_ocr_lang(ocr_lang),
            ocr_server_url: self.settings.processors.ocr.ocr_server_url.clone(),
            dpi: self.ocr_page_dpi as f32,
            num_workers: std::thread::available_parallelism()
                .map(|n| n.get().saturating_sub(1).max(1))
                .unwrap_or(1),
            // Extract and write embedded images (figures / charts) next to the
            // output so a PPTX/DOCX comes back as text + image components.
            extract_images: image_dir.is_some(),
            image_output_dir: image_dir,
            ..Default::default()
        };

        info!(
            "liteparse: parsing {} (ocr={} lang={} server={:?}) in {:.0} DPI",
            input.display(),
            config.ocr_enabled,
            config.ocr_language,
            config.ocr_server_url,
            config.dpi
        );
        let rt = liteparse_runtime()?;
        let mut parser = LiteParse::new(config);
        // Default CPU OCR engine: PP-OCR via the bundled ONNX runtime. An
        // explicit OCR server URL still wins; other backends fall back to
        // LiteParse's built-in Tesseract for PDF pages.
        if self.ocr_enabled
            && self.ocr_backend == OcrBackend::Paddle
            && self.settings.processors.ocr.ocr_server_url.is_none()
        {
            match paddle_engine(&self.settings.processors.ocr.paddle_model) {
                Ok(engine) => parser = parser.with_ocr_engine(engine),
                Err(e) => warn!("paddle OCR unavailable ({e}); falling back to built-in tesseract"),
            }
        }
        let result = rt
            .block_on(parser.parse(path_str))
            .with_context(|| format!("liteparse failed on {}", input.display()))?;

        info!(
            "liteparse: {} pages, {} chars",
            result.pages.len(),
            result.text.len()
        );

        let title = input
            .file_stem()
            .and_then(|s| s.to_str())
            .map(std::string::ToString::to_string);

        Ok(ConvertedDocument {
            title,
            text_content: result.text,
            already_written: false,
        })
    }

    /// Convert a PDF page-by-page using VLM: render each page to an image with
    /// pdftoppm, send each image through the vision model, and concatenate the
    /// descriptions into a single markdown document.
    pub(crate) fn process_pdf_with_vlm(&self, input: &Path) -> Result<ConvertedDocument> {
        let vlm_config = &self.settings.processors.vlm;
        let prompt = self.vlm_prompt.as_ref()
            .or(vlm_config.prompts.get("default"))
            .map_or("Describe this page in detail, including all visible text, tables, figures, and layout.", std::string::String::as_str);

        let temp_dir = std::env::temp_dir().join("ingestr-pdf-vlm");
        fs::create_dir_all(&temp_dir)?;

        // Render PDF pages to PNG images using pdftoppm
        eprint!("Rendering PDF pages...");
        let status = ProcCommand::new("pdftoppm")
            .args(["-png", "-r", "200"])
            .arg(input)
            .arg(temp_dir.join("page"))
            .status()
            .context("running pdftoppm (is poppler installed?)")?;

        if !status.success() {
            let _ = fs::remove_dir_all(&temp_dir);
            bail!("pdftoppm failed with status {status}");
        }

        // Collect page images sorted by name
        let mut page_images: Vec<PathBuf> = fs::read_dir(&temp_dir)?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("png"))
            .collect();
        page_images.sort();

        if page_images.is_empty() {
            let _ = fs::remove_dir_all(&temp_dir);
            bail!("pdftoppm produced no page images");
        }

        let total_pages = page_images.len();
        let (url, model) = self.vlm_connection();
        eprintln!(" {total_pages} pages (model: {model})");

        let filename = input
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("document");
        let header = format!("# {filename}\n");

        // Resolve output file: explicit -o, or default to <stem>.md in cwd
        let output_path = self.vlm_output.clone().unwrap_or_else(|| {
            let stem = input
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("document");
            PathBuf::from(format!("{stem}.md"))
        });

        // Write header immediately
        fs::write(&output_path, &header)
            .with_context(|| format!("writing to {}", output_path.display()))?;
        eprintln!("Streaming to {}", output_path.display());

        // Process pages with thread pool, streaming to file
        let jobs = self.jobs.min(total_pages);
        let sections = self.process_vlm_pages_parallel(
            &page_images,
            &url,
            &model,
            prompt,
            total_pages,
            jobs,
            "Page",
            &output_path,
        );

        // Clean up temp images
        let _ = fs::remove_dir_all(&temp_dir);

        let title = input
            .file_stem()
            .and_then(|s| s.to_str())
            .map(std::string::ToString::to_string);

        let text_content = format!("{}\n{}\n", header, sections.join("\n\n"));

        // Write final clean version (replaces the streamed file)
        fs::write(&output_path, &text_content)
            .with_context(|| format!("writing final output to {}", output_path.display()))?;

        Ok(ConvertedDocument {
            title,
            text_content,
            already_written: true,
        })
    }

    /// Process page images through VLM in parallel, streaming results to a file
    /// as they complete (in order). Shows progress on stderr.
    pub(crate) fn process_vlm_pages_parallel(
        &self,
        page_images: &[PathBuf],
        url: &str,
        model: &str,
        prompt: &str,
        total: usize,
        jobs: usize,
        label: &str,
        output_path: &Path,
    ) -> Vec<String> {
        use std::sync::mpsc;
        use std::thread;

        let (tx, rx) = mpsc::channel::<(usize, String)>();

        // Read all images upfront so threads don't need filesystem access
        let images: Vec<(usize, Vec<u8>)> = page_images
            .iter()
            .enumerate()
            .filter_map(|(i, path)| fs::read(path).ok().map(|data| (i, data)))
            .collect();

        let url = url.to_string();
        let model = model.to_string();
        let prompt = prompt.to_string();
        let api_key = self.settings.llm_api_key.as_deref().map(str::to_string);

        // Spawn worker threads
        let chunk_size = images.len().div_ceil(jobs);
        let mut handles = Vec::new();

        for chunk in images.chunks(chunk_size) {
            let chunk: Vec<(usize, Vec<u8>)> = chunk.to_vec();
            let tx = tx.clone();
            let url = url.clone();
            let model = model.clone();
            let prompt = prompt.clone();
            let api_key = api_key.clone();

            let handle = thread::spawn(move || {
                for (page_idx, image_data) in chunk {
                    let base64_image = base64_encode(&image_data);
                    let result = match call_vlm_api(
                        &url,
                        &model,
                        &base64_image,
                        "image/png",
                        &prompt,
                        api_key.as_deref(),
                    ) {
                        Ok(desc) => desc,
                        Err(e) => format!("[VLM processing failed: {e}]"),
                    };
                    let _ = tx.send((page_idx, result));
                }
            });
            handles.push(handle);
        }
        drop(tx); // Close sender so rx iterator terminates

        // Collect results, append to file in order as pages become ready
        let mut results: Vec<Option<String>> = vec![None; total];
        let mut next_to_write = 0;
        let mut completed = 0;
        let start = std::time::Instant::now();
        let label = label.to_string();

        // Open file in append mode (fall back to create if missing). If the
        // output cannot be opened we fall back to a sink so streaming never
        // panics; the caller owns the real error path.
        let mut file: Box<dyn std::io::Write> =
            match fs::OpenOptions::new().append(true).open(output_path) {
                Ok(f) => Box::new(f),
                Err(_) => match fs::File::create(output_path) {
                    Ok(f) => Box::new(f),
                    Err(err) => {
                        warn!(
                            "could not open output file {}: {}",
                            output_path.display(),
                            err
                        );
                        Box::new(std::io::sink())
                    }
                },
            };

        for (page_idx, description) in rx {
            completed += 1;
            let page_num = page_idx + 1;
            let elapsed = start.elapsed().as_secs_f32();
            let avg = elapsed / completed as f32;
            let remaining = avg * (total - completed) as f32;
            eprint!(
                "\r\x1b[K[{completed}/{total}] {label} {page_num} done ({avg:.1}s avg, ~{remaining:.0}s remaining)"
            );

            let section = format!("## {label} {page_num}\n\n{description}");
            results[page_idx] = Some(section);

            // Append all consecutive ready pages to file
            while next_to_write < total {
                if let Some(ref s) = results[next_to_write] {
                    let _ = writeln!(file, "\n{s}");
                    let _ = file.flush();
                    next_to_write += 1;
                } else {
                    break;
                }
            }
        }

        // Wait for all threads
        for h in handles {
            let _ = h.join();
        }

        let elapsed = start.elapsed().as_secs_f32();
        eprintln!(
            "\r\x1b[K[{}/{}] All {} pages done in {:.1}s -> {}",
            total,
            total,
            label.to_lowercase(),
            elapsed,
            output_path.display()
        );

        results.into_iter().flatten().collect()
    }

    /// Convert a PPTX slide-by-slide using VLM: first convert to PDF with
    /// `LibreOffice`, then render each page to an image and send through VLM.
    pub(crate) fn process_pptx_with_vlm(&self, input: &Path) -> Result<ConvertedDocument> {
        let vlm_config = &self.settings.processors.vlm;
        let prompt = self.vlm_prompt.as_ref()
            .or(vlm_config.prompts.get("screenshot"))
            .or(vlm_config.prompts.get("default"))
            .map_or("Describe this presentation slide in detail, including all text, diagrams, charts, images, bullet points, and visual layout.", std::string::String::as_str);

        let temp_dir = std::env::temp_dir().join("ingestr-pptx-vlm");
        fs::create_dir_all(&temp_dir)?;

        // Step 1: Convert to PDF using LibreOffice
        eprint!("Converting to PDF...");
        let status = ProcCommand::new("soffice")
            .args(["--headless", "--convert-to", "pdf", "--outdir"])
            .arg(&temp_dir)
            .arg(input)
            .status()
            .context("running LibreOffice (is soffice installed?)")?;

        if !status.success() {
            let _ = fs::remove_dir_all(&temp_dir);
            bail!("LibreOffice conversion failed with status {status}");
        }

        // Find the generated PDF
        let pdf_path = {
            let stem = input
                .file_stem()
                .and_then(|s| s.to_str())
                .ok_or_else(|| anyhow!("invalid input filename"))?;
            let pdf = temp_dir.join(format!("{stem}.pdf"));
            if pdf.exists() {
                pdf
            } else {
                // Try to find any PDF in the temp dir
                fs::read_dir(&temp_dir)?
                    .filter_map(Result::ok)
                    .map(|e| e.path())
                    .find(|p| p.extension().and_then(|e| e.to_str()) == Some("pdf"))
                    .ok_or_else(|| anyhow!("LibreOffice produced no PDF output"))?
            }
        };

        // Step 2: Render slides to images
        eprint!(" Rendering slides...");
        let status = ProcCommand::new("pdftoppm")
            .args(["-png", "-r", "200"])
            .arg(&pdf_path)
            .arg(temp_dir.join("slide"))
            .status()
            .context("running pdftoppm (is poppler installed?)")?;

        if !status.success() {
            let _ = fs::remove_dir_all(&temp_dir);
            bail!("pdftoppm failed with status {status}");
        }

        // Collect slide images sorted by name
        let mut slide_images: Vec<PathBuf> = fs::read_dir(&temp_dir)?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("slide") && n.ends_with(".png"))
            })
            .collect();
        slide_images.sort();

        if slide_images.is_empty() {
            let _ = fs::remove_dir_all(&temp_dir);
            bail!("pdftoppm produced no slide images");
        }

        let (url, model) = self.vlm_connection();
        let total_slides = slide_images.len();
        eprintln!(" {total_slides} slides (model: {model})");

        let filename = input
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("presentation");
        let header = format!("# {filename}\n");

        // Resolve output file
        let output_path = self.vlm_output.clone().unwrap_or_else(|| {
            let stem = input
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("presentation");
            PathBuf::from(format!("{stem}.md"))
        });

        // Write header immediately
        fs::write(&output_path, &header)
            .with_context(|| format!("writing to {}", output_path.display()))?;
        eprintln!("Streaming to {}", output_path.display());

        let jobs = self.jobs.min(total_slides);
        let sections = self.process_vlm_pages_parallel(
            &slide_images,
            &url,
            &model,
            prompt,
            total_slides,
            jobs,
            "Slide",
            &output_path,
        );

        // Clean up temp files
        let _ = fs::remove_dir_all(&temp_dir);

        let title = input
            .file_stem()
            .and_then(|s| s.to_str())
            .map(std::string::ToString::to_string);

        let text_content = format!("{}\n{}\n", header, sections.join("\n\n"));

        // Write final clean version
        fs::write(&output_path, &text_content)
            .with_context(|| format!("writing final output to {}", output_path.display()))?;

        Ok(ConvertedDocument {
            title,
            text_content,
            already_written: true,
        })
    }
}

pub(crate) fn is_image_extension(ext: &str) -> bool {
    matches!(
        ext,
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "tiff" | "tif"
    )
}

pub(crate) fn is_pdf_extension(ext: &str) -> bool {
    ext == "pdf"
}

pub(crate) fn is_presentation_extension(ext: &str) -> bool {
    matches!(ext, "pptx" | "ppt" | "odp" | "key")
}

/// Whether LibreOffice (`soffice`/`libreoffice`) is available on PATH.
pub fn has_libreoffice() -> bool {
    command_exists("soffice") || command_exists("libreoffice")
}

/// Check if a PDF file is encrypted/password-protected.
/// Returns true if encrypted, false if not, or an error if the file cannot be read.
pub(crate) fn is_pdf_encrypted(path: &Path) -> Result<bool> {
    use lopdf::{Document, Object};

    let doc =
        Document::load(path).with_context(|| format!("failed to load PDF: {}", path.display()))?;

    // Check the Encrypt dictionary in trailer
    if let Ok(encrypt) = doc.trailer.get(b"Encrypt")
        && *encrypt != Object::Null
    {
        return Ok(true);
    }

    Ok(false)
}

pub(crate) fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity(data.len().div_ceil(3) * 4);

    for chunk in data.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = chunk.get(1).copied().unwrap_or(0) as usize;
        let b2 = chunk.get(2).copied().unwrap_or(0) as usize;

        result.push(ALPHABET[b0 >> 2] as char);
        result.push(ALPHABET[((b0 & 0x03) << 4) | (b1 >> 4)] as char);

        if chunk.len() > 1 {
            result.push(ALPHABET[((b1 & 0x0f) << 2) | (b2 >> 6)] as char);
        } else {
            result.push('=');
        }

        if chunk.len() > 2 {
            result.push(ALPHABET[b2 & 0x3f] as char);
        } else {
            result.push('=');
        }
    }

    result
}

/// Call an OpenAI-compatible LLM API for vision processing.
/// Works with Ollama, LM Studio, vLLM, or any server implementing
/// the `OpenAI` /v1/chat/completions endpoint with vision support.
///
/// When `api_key` is `Some`, it is sent as a `Bearer` Authorization header;
/// local endpoints (Ollama/LM Studio) typically pass `None`.
pub(crate) fn call_vlm_api(
    llm_url: &str,
    model: &str,
    base64_image: &str,
    mime_type: &str,
    prompt: &str,
    api_key: Option<&str>,
) -> Result<String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .context("building VLM HTTP client")?;

    let request_body = serde_json::json!({
        "model": model,
        "messages": [{
            "role": "user",
            "content": [
                {
                    "type": "text",
                    "text": prompt
                },
                {
                    "type": "image_url",
                    "image_url": {
                        "url": format!("data:{};base64,{}", mime_type, base64_image)
                    }
                }
            ]
        }],
        "max_tokens": 4096,
        // Disable thinking/reasoning for models that support it (Qwen3.5, etc.)
        // This puts the full response in "content" instead of "reasoning"
        "chat_template_kwargs": {"enable_thinking": false}
    });

    info!("VLM request to {llm_url}/v1/chat/completions model={model}");

    let request = client
        .post(format!("{llm_url}/v1/chat/completions"))
        .header("Content-Type", "application/json");
    let request = if let Some(key) = api_key {
        request.bearer_auth(key)
    } else {
        request
    };
    let response = request
        .json(&request_body)
        .send()
        .context("sending VLM request")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        bail!("VLM request failed with status {status}: {body}");
    }

    let response_json: serde_json::Value = response.json().context("parsing VLM response")?;

    let message = &response_json["choices"][0]["message"];

    // Try content first, then reasoning (for thinking models like Qwen3.5)
    let text = message["content"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| message["reasoning"].as_str())
        .or_else(|| message["reasoning_content"].as_str());

    text.map(std::string::ToString::to_string)
        .ok_or_else(|| anyhow!("no content in VLM response: {response_json}"))
}

// ============================================================================
// Semantic tier-router seam (System One / kev)
// ============================================================================

/// The `answers.tier` question returned by a System-One endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SystemOneQuestion {
    #[serde(rename = "type")]
    pub(crate) qtype: String,
    /// The chosen option (e.g. `gpu`).
    #[serde(default)]
    pub(crate) choice: Option<String>,
    /// Calibrated probabilities per option.
    #[serde(default)]
    pub(crate) probabilities: HashMap<String, f64>,
}

/// The `answers` map returned by a System-One endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SystemOneAnswers {
    pub(crate) tier: SystemOneQuestion,
}

/// Token usage reported by the router.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SystemOneUsage {
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
}

/// The full `/v1/systemone` response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SystemOneResponse {
    pub(crate) model: String,
    pub(crate) answers: SystemOneAnswers,
    #[serde(default)]
    pub(crate) usage: Option<SystemOneUsage>,
    #[serde(default)]
    pub(crate) latency_ms: Option<u64>,
}

/// A parsed routing decision: the chosen tier plus its calibrated probabilities.
#[derive(Debug, Clone)]
pub(crate) struct RoutingDecision {
    pub(crate) tier: RoutingTier,
    pub(crate) probabilities: HashMap<String, f64>,
    pub(crate) confidence: f64,
    pub(crate) model: String,
    pub(crate) latency_ms: Option<u64>,
}

impl RoutingDecision {
    /// Build a decision from a System-One response, validating the chosen tier.
    pub(crate) fn from_response(resp: &SystemOneResponse) -> Result<Self> {
        let q = &resp.answers.tier;
        let choice = q
            .choice
            .as_deref()
            .ok_or_else(|| anyhow!("routing response missing `choice`"))?;
        let tier =
            RoutingTier::parse(choice).ok_or_else(|| anyhow!("unknown routing tier `{choice}`"))?;
        let confidence = q.probabilities.get(choice).copied().unwrap_or(0.0);
        Ok(Self {
            tier,
            probabilities: q.probabilities.clone(),
            confidence,
            model: resp.model.clone(),
            latency_ms: resp.latency_ms,
        })
    }
}

/// Compact, cheap metadata the router sees. Built from what ingestr already
/// knows about a document before conversion (extension, size, a text sample).
#[derive(Debug, Clone)]
pub(crate) struct RoutingInput {
    pub(crate) extension: String,
    pub(crate) filename: String,
    pub(crate) file_bytes: usize,
    pub(crate) is_complex: bool,
    pub(crate) needs_ocr: bool,
    pub(crate) text_chars: usize,
    pub(crate) snippet: String,
}

impl RoutingInput {
    /// Build the `state` string POSTed to the router.
    pub(crate) fn build_state(&self) -> String {
        let density = if self.file_bytes > 0 {
            (self.text_chars as f64 / self.file_bytes as f64) * 1000.0
        } else {
            0.0
        };
        format!(
            "document to ingest; extension={}; filename={}; size_bytes={}; text_chars={}; \
             text_density_per_kb={density:.1}; complex={}; needs_ocr={}; snippet=\"{}\"",
            self.extension,
            self.filename,
            self.file_bytes,
            self.text_chars,
            self.is_complex,
            self.needs_ocr,
            self.snippet,
        )
    }
}

/// In-process cache of router decisions keyed by a content hash (sha256 of the
/// file bytes), so repeated/converted or shared documents reuse decisions
/// without another router call. Not persisted for this spike.
pub(crate) static ROUTING_DECISION_CACHE: LazyLock<Mutex<HashMap<String, RoutingDecision>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Compute the routing cache key from document bytes.
pub(crate) fn routing_cache_key(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

pub(crate) fn routing_cache_get(key: &str) -> Option<RoutingDecision> {
    ROUTING_DECISION_CACHE.lock().ok()?.get(key).cloned()
}

pub(crate) fn routing_cache_put(key: &str, decision: &RoutingDecision) {
    if let Ok(mut cache) = ROUTING_DECISION_CACHE.lock() {
        cache.insert(key.to_string(), decision.clone());
    }
}

/// POST a document `state` to a System-One endpoint and return the parsed
/// tier decision. Mirrors the `call_vlm_api` auth pattern. Errors are returned
/// to the caller, which falls back to heuristic routing.
pub(crate) fn call_system_one(
    router_url: &str,
    model: &str,
    api_key: Option<&str>,
    state: &str,
) -> Result<SystemOneResponse> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .context("building System-One HTTP client")?;

    let request_body = serde_json::json!({
        "state": state,
        "model": model,
        "questions": {
            "tier": {
                "type": "choice",
                "instructions": "Which ingestion tier should this page use?",
                "criteria": {
                    "native": "clean text/vector extraction",
                    "cpu_ocr": "scanned or text-sparse page",
                    "gpu": "dense/noisy tables, charts, handwriting",
                    "vlm": "image, diagram, screenshot",
                    "skip": "non-document or low value"
                }
            }
        }
    });

    let url = format!("{router_url}/v1/systemone");
    info!("system-one routing request to {url} model={model}");

    let request = client.post(&url).header("Content-Type", "application/json");
    let request = if let Some(key) = api_key {
        request.bearer_auth(key)
    } else {
        request
    };
    let response = request
        .json(&request_body)
        .send()
        .context("sending System-One routing request")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        bail!("System-One routing request failed with status {status}: {body}");
    }

    let parsed: SystemOneResponse = response
        .json()
        .context("parsing System-One routing response")?;
    Ok(parsed)
}

/// Call the router for a document and build a decision, using the in-process
/// content-hash cache. Returns `Err` (and thus falls back to heuristic) on any
/// network/parse problem or when the router is unconfigured.
pub(crate) fn route_document(
    router_url: &str,
    model: &str,
    api_key: Option<&str>,
    input: &Path,
    signals: &RoutingInput,
) -> Result<RoutingDecision> {
    let data =
        fs::read(input).with_context(|| format!("reading {} for routing", input.display()))?;
    let cache_key = routing_cache_key(&data);
    debug!("system-one routing cache check key={cache_key}");
    if let Some(cached) = routing_cache_get(&cache_key) {
        debug!("system-one routing cache hit key={cache_key}");
        return Ok(cached);
    }

    let state = signals.build_state();
    let resp = call_system_one(router_url, model, api_key, &state)?;
    let decision = RoutingDecision::from_response(&resp)?;
    routing_cache_put(&cache_key, &decision);
    Ok(decision)
}

pub(crate) const OCRS_DETECTION_MODEL_URL: &str =
    "https://ocrs-models.s3-accelerate.amazonaws.com/text-detection.rten";
pub(crate) const OCRS_RECOGNITION_MODEL_URL: &str =
    "https://ocrs-models.s3-accelerate.amazonaws.com/text-recognition.rten";

pub(crate) fn ocrs_cache_dir() -> Result<PathBuf> {
    let base_cache = std::env::var("XDG_CACHE_HOME")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
        .or_else(dirs::cache_dir)
        .unwrap_or_else(|| {
            std::env::var("HOME")
                .map_or_else(|_| PathBuf::from("."), PathBuf::from)
                .join(".cache")
        });

    let cache_dir = base_cache.join("ocrs");
    fs::create_dir_all(&cache_dir)
        .with_context(|| format!("creating OCRS cache dir {}", cache_dir.display()))?;
    Ok(cache_dir)
}

pub(crate) fn human_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;

    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.2} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

pub(crate) fn download_to_cache(url: &str, filename: &str) -> Result<PathBuf> {
    let path = ocrs_cache_dir()?.join(filename);
    if path.exists() {
        eprintln!("OCRS model cached: {}", path.display());
        return Ok(path);
    }

    info!("downloading OCRS model from {url}");
    let mut response = reqwest::blocking::get(url)
        .with_context(|| format!("downloading model from {url}"))?
        .error_for_status()
        .with_context(|| format!("failed to download model from {url}"))?;

    let total = response.content_length();
    eprintln!(
        "Downloading OCRS model {}{}",
        filename,
        total
            .map(|t| format!(" ({})", human_bytes(t)))
            .unwrap_or_default()
    );

    let mut file = fs::File::create(&path)
        .with_context(|| format!("creating model file {}", path.display()))?;
    let mut downloaded: u64 = 0;
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut last_print = Instant::now();
    let started = Instant::now();

    loop {
        let n = response
            .read(&mut buffer)
            .with_context(|| format!("reading model bytes from {url}"))?;
        if n == 0 {
            break;
        }

        file.write_all(&buffer[..n])
            .with_context(|| format!("writing model to {}", path.display()))?;
        downloaded += n as u64;

        if last_print.elapsed() >= Duration::from_millis(200) {
            match total {
                Some(t) if t > 0 => {
                    let pct = (downloaded as f64 / t as f64) * 100.0;
                    eprint!(
                        "\r  -> {} / {} ({:.1}%)",
                        human_bytes(downloaded),
                        human_bytes(t),
                        pct
                    );
                }
                _ => {
                    eprint!("\r  -> downloaded {}", human_bytes(downloaded));
                }
            }
            let _ = io::stderr().flush();
            last_print = Instant::now();
        }
    }

    let elapsed = started.elapsed().as_secs_f32();
    eprintln!(
        "\r  -> done: {} in {:.1}s         ",
        human_bytes(downloaded),
        elapsed
    );

    Ok(path)
}

pub(crate) fn run_ocrs_on_image(
    input: &Path,
    engine: &OcrEngine,
    show_progress: bool,
) -> Result<String> {
    let started = Instant::now();

    if show_progress {
        eprintln!("OCRS: loading image {}", input.display());
    }
    let image = image::open(input)
        .with_context(|| format!("reading image for OCRS: {}", input.display()))?
        .into_rgb8();

    let image_source = ImageSource::from_bytes(image.as_raw(), image.dimensions())
        .context("creating OCRS image source")?;

    if show_progress {
        eprintln!("OCRS: preparing input tensor");
    }
    let ocr_input = engine
        .prepare_input(image_source)
        .context("preparing OCRS input")?;

    if show_progress {
        eprintln!("OCRS: detecting words");
    }
    let word_rects = engine
        .detect_words(&ocr_input)
        .context("OCRS word detection failed")?;

    if show_progress {
        eprintln!("OCRS: grouping into lines");
    }
    let line_rects = engine.find_text_lines(&ocr_input, &word_rects);

    if show_progress {
        eprintln!("OCRS: recognizing text");
    }
    let line_texts = engine
        .recognize_text(&ocr_input, &line_rects)
        .context("OCRS text recognition failed")?;

    let text = line_texts
        .iter()
        .flatten()
        .map(std::string::ToString::to_string)
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");

    if show_progress {
        eprintln!(
            "OCRS: done in {:.2}s ({} words, {} lines)",
            started.elapsed().as_secs_f32(),
            word_rects.len(),
            line_rects.len()
        );
    }

    Ok(text)
}

pub(crate) fn run_ocrs_on_pdf(input: &Path, engine: &OcrEngine) -> Result<String> {
    let temp_dir = std::env::temp_dir().join(format!("ingestr-ocrs-pdf-{}", std::process::id()));
    if temp_dir.exists() {
        let _ = fs::remove_dir_all(&temp_dir);
    }
    fs::create_dir_all(&temp_dir)
        .with_context(|| format!("creating temp dir {}", temp_dir.display()))?;

    let result = (|| -> Result<String> {
        eprint!("OCRS: rendering PDF pages...");
        let render_started = Instant::now();
        let status = ProcCommand::new("pdftoppm")
            .args(["-png", "-r", "200"])
            .arg(input)
            .arg(temp_dir.join("page"))
            .status()
            .context("running pdftoppm (is poppler installed?)")?;

        if !status.success() {
            bail!("pdftoppm failed with status {status}");
        }

        let mut page_images: Vec<PathBuf> = fs::read_dir(&temp_dir)?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("png"))
            .collect();
        page_images.sort();

        if page_images.is_empty() {
            bail!("pdftoppm produced no page images");
        }

        let total = page_images.len();
        eprintln!(
            " {} pages in {:.1}s",
            total,
            render_started.elapsed().as_secs_f32()
        );

        let mut sections = Vec::with_capacity(total);
        let pipeline_started = Instant::now();

        for (idx, page) in page_images.iter().enumerate() {
            let page_idx = idx + 1;
            let page_started = Instant::now();
            let page_text = run_ocrs_on_image(page, engine, false)
                .with_context(|| format!("OCRS failed on PDF page {page_idx}"))?;
            sections.push(format!("## Page {page_idx}\n\n{page_text}"));

            let done = page_idx;
            let avg = pipeline_started.elapsed().as_secs_f32() / done as f32;
            let remaining_pages = total.saturating_sub(done);
            let eta = avg * remaining_pages as f32;
            eprintln!(
                "OCRS PDF: [{}/{}] page {} done ({:.2}s page, {:.2}s avg, ~{:.0}s remaining)",
                done,
                total,
                page_idx,
                page_started.elapsed().as_secs_f32(),
                avg,
                eta
            );
        }

        eprintln!(
            "OCRS PDF: completed {} pages in {:.1}s",
            total,
            pipeline_started.elapsed().as_secs_f32()
        );

        Ok(sections.join("\n\n"))
    })();

    let _ = fs::remove_dir_all(&temp_dir);
    result
}

pub(crate) fn run_ocrs(input: &Path, languages: &[String]) -> Result<String> {
    if !languages.is_empty() && languages.iter().all(|l| l != "eng") {
        warn!(
            "ocrs backend currently works best for Latin/English; requested languages: {languages:?}"
        );
    }

    let init_started = Instant::now();
    eprintln!("OCRS: preparing models");
    let detection_model_path = download_to_cache(OCRS_DETECTION_MODEL_URL, "text-detection.rten")?;
    let recognition_model_path =
        download_to_cache(OCRS_RECOGNITION_MODEL_URL, "text-recognition.rten")?;

    eprintln!(
        "OCRS: loading detection model {}",
        detection_model_path.display()
    );
    let detection_model =
        RtenModel::load_file(&detection_model_path).context("loading OCRS detection model")?;

    eprintln!(
        "OCRS: loading recognition model {}",
        recognition_model_path.display()
    );
    let recognition_model =
        RtenModel::load_file(&recognition_model_path).context("loading OCRS recognition model")?;

    let engine = OcrEngine::new(OcrEngineParams {
        detection_model: Some(detection_model),
        recognition_model: Some(recognition_model),
        ..Default::default()
    })
    .context("initializing OCRS engine")?;
    eprintln!(
        "OCRS: models ready in {:.2}s",
        init_started.elapsed().as_secs_f32()
    );

    if input
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
    {
        run_ocrs_on_pdf(input, &engine)
    } else {
        run_ocrs_on_image(input, &engine, true)
    }
}

/// Run OCR on a file
pub(crate) fn run_ocr(input: &Path, backend: OcrBackend, languages: &[String]) -> Result<String> {
    let lang_arg = languages.join("+");

    match backend {
        OcrBackend::Tesseract => {
            let output = ProcCommand::new("tesseract")
                .arg(input)
                .arg("stdout")
                .arg("-l")
                .arg(&lang_arg)
                .output()
                .context("running tesseract OCR")?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                bail!("tesseract failed: {stderr}");
            }

            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        }
        OcrBackend::Paddle => run_paddle_on_image(input, "small"),
        OcrBackend::Ocrs => run_ocrs(input, languages),
        OcrBackend::Surya => {
            // Surya uses Python, call via python
            let output = ProcCommand::new("surya_ocr")
                .arg(input)
                .arg("--langs")
                .arg(&lang_arg)
                .output()
                .context("running surya OCR")?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                bail!("surya failed: {stderr}");
            }

            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        }
        OcrBackend::Easyocr => {
            // EasyOCR via Python command
            let script = format!(
                r"import easyocr; import sys; reader = easyocr.Reader(['{}']); result = reader.readtext('{}'); print('\n'.join([text for _, text, _ in result]))",
                lang_arg.replace('+', "','"),
                input.display()
            );

            let output = ProcCommand::new("python3")
                .arg("-c")
                .arg(&script)
                .output()
                .context("running easyocr")?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                bail!("easyocr failed: {stderr}");
            }

            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        }
    }
}

/// A shared multi-threaded tokio runtime for blocking on LiteParse's async API.
/// Created once and reused, so every PDF conversion does not pay runtime setup.
pub(crate) fn liteparse_runtime() -> Result<&'static Runtime> {
    static RT: OnceLock<Result<Runtime, String>> = OnceLock::new();
    RT.get_or_init(|| Runtime::new().map_err(|e| e.to_string()))
        .as_ref()
        .map_err(|e| anyhow!("failed to create liteparse tokio runtime: {e}"))
}

/// Map an ingestr OCR language to a Tesseract/liteparse language code.
pub(crate) fn map_ocr_lang(lang: &str) -> String {
    match lang.to_ascii_lowercase().as_str() {
        // ingestr accepts long forms; liteparse/Tesseract use ISO 639-1 codes.
        "english" => "eng",
        "german" => "deu",
        "french" => "fra",
        "spanish" => "spa",
        "italian" => "ita",
        "portuguese" => "por",
        "chinese" => "chi_sim",
        "japanese" => "jpn",
        "korean" => "kor",
        "russian" => "rus",
        "dutch" => "nld",
        other => other,
    }
    .to_string()
}

/// Process-wide PP-OCR engine (PaddleOCR family via `oar-ocr`/ONNX). Built
/// once and shared: model loading is the expensive part, and a batch run OCRs
/// many pages/images with the same engine. The first call downloads the
/// detection/recognition models and dictionary (SHA-256 verified) into
/// `$OAR_HOME` (default `~/.oar`).
pub(crate) fn paddle_engine(model: &str) -> Result<std::sync::Arc<OarOcrEngine>> {
    static ENGINE: OnceLock<Result<std::sync::Arc<OarOcrEngine>, String>> = OnceLock::new();
    ENGINE
        .get_or_init(|| {
            let size = model.trim().to_ascii_lowercase();
            info!("paddle: loading PP-OCRv6 {size} (models auto-download on first use)");
            let built = match size.as_str() {
                "tiny" => OarOcrEngine::ppocr_v6_tiny(),
                "medium" => OarOcrEngine::ppocr_v6_medium(),
                _ => OarOcrEngine::ppocr_v6_small(),
            };
            built.map(std::sync::Arc::new).map_err(|e| e.to_string())
        })
        .clone()
        .map_err(|e| anyhow!("initializing paddle OCR engine: {e}"))
}

/// OCR a standalone image with the PP-OCR engine and reassemble the word
/// boxes into reading-order lines.
pub(crate) fn run_paddle_on_image(input: &Path, model: &str) -> Result<String> {
    let engine = paddle_engine(model)?;
    let image = image::open(input)
        .with_context(|| format!("reading image for paddle OCR: {}", input.display()))?
        .into_rgb8();
    let (width, height) = image.dimensions();
    let options = OcrOptions {
        language: "en".to_string(),
        dpi: 300.0,
    };
    let rt = liteparse_runtime()?;
    let results = rt
        .block_on(engine.recognize(image.as_raw(), width, height, &options))
        .map_err(|e| anyhow!("paddle OCR failed on {}: {e}", input.display()))?;
    Ok(layout_ocr_results(results))
}

/// Turn unordered word-level OCR results into text: group boxes whose
/// vertical centres are within about half a line height into one line, order
/// lines top-to-bottom and words left-to-right.
pub(crate) fn layout_ocr_results(mut results: Vec<OcrResult>) -> String {
    results.retain(|r| !r.text.trim().is_empty());
    if results.is_empty() {
        return String::new();
    }
    let centre_y = |r: &OcrResult| (r.bbox[1] + r.bbox[3]) / 2.0;
    let box_h = |r: &OcrResult| (r.bbox[3] - r.bbox[1]).abs().max(1.0);
    results.sort_by(|a, b| centre_y(a).total_cmp(&centre_y(b)));

    let mut lines: Vec<Vec<OcrResult>> = Vec::new();
    let mut line_y = centre_y(&results[0]);
    let mut line_h = box_h(&results[0]);
    for r in results {
        let same_line = (centre_y(&r) - line_y).abs() <= 0.6 * line_h.max(box_h(&r));
        if same_line && !lines.is_empty() {
            if let Some(last) = lines.last_mut() {
                last.push(r);
            }
        } else {
            line_y = centre_y(&r);
            line_h = box_h(&r);
            lines.push(vec![r]);
        }
    }

    lines
        .into_iter()
        .map(|mut line| {
            line.sort_by(|a, b| a.bbox[0].total_cmp(&b.bbox[0]));
            line.iter()
                .map(|r| r.text.trim())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Per-page OCR decision from LiteParse's classification: OCR when the page is
/// scanned, has no text layer, or its text is garbled; a text-sparse page only
/// when it also carries embedded images (a scan with a small text stamp) or the
/// caller asked for recall-first behaviour. Pure sparse-text pages are short
/// native-text pages. Unknown reasons are ignored (LiteParse may add variants).
pub(crate) fn page_needs_ocr(
    flagged: bool,
    reasons: &[liteparse::ocr_merge::ComplexityReason],
    ocr_sparse_pages: bool,
) -> bool {
    use liteparse::ocr_merge::ComplexityReason as R;
    if !flagged {
        return false;
    }
    let has = |pred: fn(&R) -> bool| reasons.iter().any(pred);
    let hard = has(|r| matches!(r, R::Scanned | R::NoText | R::Garbled));
    let sparse = has(|r| matches!(r, R::SparseText));
    let images = has(|r| matches!(r, R::EmbeddedImages));
    hard || (sparse && (images || ocr_sparse_pages))
}

/// Whether a PDF needs OCR at all, decided from a cheap classification pass
/// (no rendering, no model). Classification failures return `true` so OCR is
/// never skipped by mistake.
pub(crate) fn pdf_needs_ocr(path: &Path, ocr_sparse_pages: bool) -> bool {
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
    let needed = stats
        .iter()
        .any(|page| page_needs_ocr(page.needs_ocr, &page.reasons, ocr_sparse_pages));
    if !needed {
        debug!(
            "OCR skipped for {}: {} page(s), none scanned/text-less/garbled",
            path.display(),
            stats.len()
        );
    }
    needed
}

/// Whether an external command is present on PATH. Checks for the binary file
/// rather than running it (some tools like poppler's pdftoppm exit non-zero on
/// `--version`, which would misreport them as missing).
pub fn command_exists(name: &str) -> bool {
    let Some(path) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&path)
        .any(|dir| dir.join(name).is_file() || dir.join(format!("{name}.exe")).is_file())
}

/// Frontmatter written at the top of converted Markdown files.
#[derive(Debug, Serialize, Deserialize)]
pub struct Frontmatter {
    /// Input file.
    pub source_path: String,
    /// Written Markdown file.
    pub output_path: String,
    /// Input modification time (RFC 3339).
    pub source_modified: Option<String>,
    /// Document title, if known.
    pub title: Option<String>,
    /// Conversion time (RFC 3339).
    pub converted_at: String,
}

/// Format a `SystemTime` as RFC 3339 (UTC).
pub fn system_time_to_rfc3339(time: SystemTime) -> Option<String> {
    let datetime: OffsetDateTime = time.into();
    datetime.format(&Rfc3339).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    fn unique_temp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time since epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("ingestr-core-test-{nanos}"))
    }

    #[test]
    fn base64_encode_empty() {
        let data = b"";
        let encoded = base64_encode(data);
        assert_eq!(encoded, "");
    }

    #[test]
    fn base64_encode_single_byte() {
        let data = b"A";
        let encoded = base64_encode(data);
        assert_eq!(encoded, "QQ==");
    }

    #[test]
    fn base64_encode_works() {
        let data = b"Hello, World!";
        let encoded = base64_encode(data);
        assert_eq!(encoded, "SGVsbG8sIFdvcmxkIQ==");
    }

    /// A tiny hand-rolled System-One endpoint so the client can be exercised
    /// end-to-end without downloading/running a real `kev` model.
    #[test]
    fn call_system_one_hits_mock_server() -> Result<()> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").context("binding mock")?;
        let addr = listener.local_addr().context("local addr")?;
        let expected_state = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
        let expected = expected_state.clone();

        std::thread::spawn(move || -> std::io::Result<()> {
            let (mut stream, _) = listener.accept()?;
            let mut buf = [0_u8; 8192];
            let n = stream.read(&mut buf)?;
            let request = std::str::from_utf8(&buf[..n]).unwrap_or("");
            if let Some(start) = request.find("{") {
                let body = &request[start..];
                let _ = body.find("\"state\"");
                // capture state value for assertion below
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
                    let state = v["state"].as_str().map(str::to_string);
                    *expected.lock().unwrap() = state;
                }
            }
            let _ = n;
            let response = r#"{"model":"kev-latest","answers":{"tier":{"type":"choice","choice":"vlm","probabilities":{"native":0.0,"cpu_ocr":0.1,"gpu":0.1,"vlm":0.8,"skip":0.0}}},"usage":{"input_tokens":10,"output_tokens":5},"latency_ms":12}"#;
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response.len()
            );
            stream.write_all(header.as_bytes())?;
            stream.write_all(response.as_bytes())?;
            stream.flush()?;
            Ok(())
        });

        let resp = call_system_one(&format!("http://{addr}"), "kev-latest", None, "test state")?;
        let decision = RoutingDecision::from_response(&resp)?;
        assert_eq!(decision.tier, RoutingTier::Vlm);
        assert!(decision.probabilities.get("vlm") > Some(&0.7));
        assert_eq!(decision.latency_ms, Some(12));

        let captured = expected_state.lock().unwrap().clone();
        assert_eq!(captured.as_deref(), Some("test state"));
        Ok(())
    }

    #[test]
    fn is_image_extension_works() {
        assert!(is_image_extension("jpg"));
        assert!(is_image_extension("jpeg"));
        assert!(is_image_extension("png"));
        assert!(is_image_extension("gif"));
        assert!(is_image_extension("webp"));
        assert!(!is_image_extension("pdf"));
        assert!(!is_image_extension("txt"));
        assert!(!is_image_extension("docx"));
    }

    #[test]
    fn is_pdf_extension_works() {
        assert!(is_pdf_extension("pdf"));
        assert!(!is_pdf_extension("txt"));
        assert!(!is_pdf_extension("docx"));
    }

    #[test]
    fn layout_ocr_results_groups_lines_and_orders_words() {
        // Two lines; words supplied out of order, second line slightly skewed.
        let results = vec![
            ocr_box("world", 60.0, 10.0, 100.0, 30.0),
            ocr_box("total", 60.0, 52.0, 100.0, 72.0),
            ocr_box("Hello", 10.0, 10.0, 50.0, 30.0),
            ocr_box("Invoice", 10.0, 50.0, 50.0, 70.0),
            ocr_box("  ", 200.0, 10.0, 210.0, 30.0),
        ];
        assert_eq!(layout_ocr_results(results), "Hello world\nInvoice total");
        assert_eq!(layout_ocr_results(Vec::new()), "");
    }

    #[test]
    fn map_ocr_lang_normalizes_long_names() {
        assert_eq!(map_ocr_lang("english"), "eng");
        assert_eq!(map_ocr_lang("german"), "deu");
        assert_eq!(map_ocr_lang("ENG"), "eng");
        assert_eq!(map_ocr_lang("deu"), "deu");
        assert_eq!(map_ocr_lang("fra"), "fra");
    }

    fn ocr_box(text: &str, x1: f32, y1: f32, x2: f32, y2: f32) -> OcrResult {
        OcrResult {
            text: text.to_string(),
            bbox: [x1, y1, x2, y2],
            confidence: 0.9,
            polygon: None,
        }
    }

    #[test]
    fn page_needs_ocr_rule() {
        use liteparse::ocr_merge::ComplexityReason as R;
        // Not flagged by LiteParse at all: never OCR.
        assert!(!page_needs_ocr(false, &[R::Scanned], false));
        // Hard cases always OCR.
        assert!(page_needs_ocr(true, &[R::Scanned], false));
        assert!(page_needs_ocr(true, &[R::NoText], false));
        assert!(page_needs_ocr(true, &[R::Garbled], false));
        // Pure sparse text is a short native page: skip unless recall-first.
        assert!(!page_needs_ocr(true, &[R::SparseText], false));
        assert!(page_needs_ocr(true, &[R::SparseText], true));
        // Sparse text over embedded images (a scan with a stamp): OCR.
        assert!(page_needs_ocr(
            true,
            &[R::SparseText, R::EmbeddedImages],
            false
        ));
        // Images alongside real text: no OCR.
        assert!(!page_needs_ocr(true, &[R::EmbeddedImages], false));
    }

    #[test]
    fn route_document_unreachable_returns_err() -> Result<()> {
        let dir = unique_temp_dir();
        fs::create_dir_all(&dir)?;
        let input = dir.join("sample.txt");
        fs::write(&input, "hello world")?;
        let signals = RoutingInput {
            extension: "txt".to_string(),
            filename: "sample.txt".to_string(),
            file_bytes: 11,
            is_complex: false,
            needs_ocr: false,
            text_chars: 11,
            snippet: "hello world".to_string(),
        };
        // Point at a port almost certainly closed.
        let result = route_document("http://127.0.0.1:1", "kev-latest", None, &input, &signals);
        assert!(result.is_err());
        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn route_document_uses_content_hash_cache() -> Result<()> {
        // Hash-only function check: identical bytes map to the same key.
        let a = routing_cache_key(b"hello");
        let b = routing_cache_key(b"hello");
        let c = routing_cache_key(b"world");
        assert_eq!(a, b);
        assert_ne!(a, c);
        Ok(())
    }

    #[test]
    fn routing_state_builder_contains_metadata() {
        let input = RoutingInput {
            extension: "pdf".to_string(),
            filename: "report.pdf".to_string(),
            file_bytes: 2048,
            is_complex: true,
            needs_ocr: false,
            text_chars: 512,
            snippet: "first page snippet".to_string(),
        };
        let state = input.build_state();
        assert!(state.contains("extension=pdf"));
        assert!(state.contains("filename=report.pdf"));
        assert!(state.contains("text_density_per_kb=250.0"));
        assert!(state.contains("complex=true"));
        assert!(state.contains("needs_ocr=false"));
        assert!(state.contains("snippet=\"first page snippet\""));
    }

    #[test]
    fn routing_tier_round_trip() {
        for tier in [
            RoutingTier::Native,
            RoutingTier::CpuOcr,
            RoutingTier::Gpu,
            RoutingTier::Vlm,
            RoutingTier::Skip,
        ] {
            assert_eq!(RoutingTier::parse(tier.as_str()), Some(tier));
        }
        assert_eq!(RoutingTier::parse("bogus"), None);
    }

    #[test]
    fn system_one_response_missing_choice_errors() -> Result<()> {
        let body = r#"{
            "model": "kev-latest",
            "answers": { "tier": { "type": "choice", "probabilities": { "gpu": 0.9 } } }
        }"#;
        let resp: SystemOneResponse = serde_json::from_str(body).context("parsing")?;
        assert!(RoutingDecision::from_response(&resp).is_err());
        Ok(())
    }

    #[test]
    fn system_one_response_parses_choice_and_probabilities() -> Result<()> {
        let body = r#"{
            "model": "kev-latest",
            "answers": {
                "tier": {
                    "type": "choice",
                    "choice": "gpu",
                    "confidence": 0.6,
                    "probabilities": {
                        "native": 0.02, "cpu_ocr": 0.31, "gpu": 0.6, "vlm": 0.05, "skip": 0.02
                    }
                }
            },
            "usage": { "input_tokens": 100, "output_tokens": 50 },
            "latency_ms": 495
        }"#;
        let resp: SystemOneResponse = serde_json::from_str(body).context("parsing response")?;
        assert_eq!(resp.model, "kev-latest");
        let decision = RoutingDecision::from_response(&resp)?;
        assert_eq!(decision.tier, RoutingTier::Gpu);
        assert!((decision.confidence - 0.6).abs() < 1e-9);
        assert_eq!(decision.probabilities.get("native"), Some(&0.02));
        assert_eq!(decision.latency_ms, Some(495));
        Ok(())
    }

    #[test]
    fn document_processor_with_vlm_disabled_skips_vlm() -> Result<()> {
        let dir = unique_temp_dir();
        fs::create_dir_all(&dir)?;
        let input = dir.join("test.txt");
        fs::write(&input, "Plain text")?;

        let processor = DocumentProcessor::new(ProcessorSettings::default())
            .with_vlm(false, None, None, 1, None);

        assert!(!processor.vlm_enabled);

        let result = processor.process(&input)?;
        assert!(result.text_content.contains("Plain text"));

        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn document_processor_with_ocr_disabled_skips_ocr() -> Result<()> {
        let dir = unique_temp_dir();
        fs::create_dir_all(&dir)?;
        let input = dir.join("test.txt");
        fs::write(&input, "Plain text")?;

        let processor = DocumentProcessor::new(ProcessorSettings::default()).with_ocr(
            false,
            OcrBackend::Tesseract,
            None,
        );

        assert!(!processor.ocr_enabled);

        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }
}
