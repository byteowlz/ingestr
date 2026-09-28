//! Conversion options and the `[processors]` / `[routing]` config types.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Everything the conversion pipeline reads from configuration: the `[llm]`
/// fallback endpoint for VLM, `[processors]`, and `[routing]`.
#[derive(Debug, Clone, Default)]
pub struct ProcessorSettings {
    /// Whether the `[llm]` endpoint is enabled.
    pub llm_enabled: bool,
    /// LLM client kind (e.g. `openai`).
    pub llm_client: String,
    /// Default LLM model (VLM fallback).
    pub llm_model: String,
    /// OpenAI-compatible base URL.
    pub llm_base_url: Option<String>,
    /// API key for the LLM endpoint.
    pub llm_api_key: Option<String>,
    /// `[processors]` config.
    pub processors: ProcessorsConfig,
    /// `[routing]` config (semantic router spike).
    pub routing: RoutingConfig,
}

/// Which conversion engine to use for a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
pub enum ConvertEngine {
    /// LiteParse for PDF/office formats, markitdown for everything else.
    #[default]
    Auto,
    /// Force LiteParse for its formats; fail instead of falling back.
    Liteparse,
    /// Skip LiteParse and use markitdown for everything.
    Markitdown,
}

/// Explicit input format for stdin conversion.
#[derive(Debug, Clone, Copy, Default)]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
pub enum InputFormat {
    /// Detect from content.
    #[default]
    Auto,
    /// HTML.
    Html,
    /// Plain text.
    Text,
    /// PDF.
    Pdf,
    /// Word (DOCX).
    Docx,
    /// Excel (XLSX).
    Xlsx,
    /// PowerPoint (PPTX).
    Pptx,
    /// CSV.
    Csv,
    /// JSON.
    Json,
    /// XML.
    Xml,
    /// Markdown.
    Markdown,
}

impl InputFormat {
    /// File extension this format maps to (`None` for auto).
    pub const fn extension(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::Html => Some("html"),
            Self::Text => Some("txt"),
            Self::Pdf => Some("pdf"),
            Self::Docx => Some("docx"),
            Self::Xlsx => Some("xlsx"),
            Self::Pptx => Some("pptx"),
            Self::Csv => Some("csv"),
            Self::Json => Some("json"),
            Self::Xml => Some("xml"),
            Self::Markdown => Some("md"),
        }
    }
}

/// OCR backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
#[serde(rename_all = "lowercase")]
pub enum OcrBackend {
    /// PP-OCR (PaddleOCR family) via the bundled ONNX runtime. Models are
    /// downloaded on first use. CPU-only, fast, permissive license.
    #[default]
    Paddle,
    /// Tesseract (bundled via tesseract-rs).
    Tesseract,
    /// ocrs (pure Rust, legacy).
    Ocrs,
    /// Surya (external CLI).
    Surya,
    /// EasyOCR (external CLI).
    Easyocr,
}

impl std::fmt::Display for OcrBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Paddle => write!(f, "paddle"),
            Self::Tesseract => write!(f, "tesseract"),
            Self::Ocrs => write!(f, "ocrs"),
            Self::Surya => write!(f, "surya"),
            Self::Easyocr => write!(f, "easyocr"),
        }
    }
}

/// Processor pipeline configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProcessorsConfig {
    /// Ordered list of processors to try
    pub pipeline: Vec<String>,
    /// File type routing rules (extension -> processor list)
    pub routing: HashMap<String, Vec<String>>,
    /// VLM processor configuration
    pub vlm: VlmConfig,
    /// OCR processor configuration
    pub ocr: OcrConfig,
}

impl Default for ProcessorsConfig {
    fn default() -> Self {
        Self {
            pipeline: vec!["markitdown".to_string()],
            routing: HashMap::new(),
            vlm: VlmConfig::default(),
            ocr: OcrConfig::default(),
        }
    }
}

/// VLM (Vision Language Model) processor configuration.
/// When `llm_url` or `model` are left empty/default, the main `[llm]` config
/// is used as fallback so users only need to configure their LLM once.
/// Works with any OpenAI-compatible vision API (Ollama, LM Studio, vLLM, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VlmConfig {
    /// Enable VLM processing
    pub enabled: bool,
    /// LLM server URL (falls back to [llm].`base_url` if empty)
    pub llm_url: Option<String>,
    /// Model name (falls back to [llm].model if empty)
    pub model: Option<String>,
    /// Custom prompts per file type
    pub prompts: HashMap<String, String>,
}

impl Default for VlmConfig {
    fn default() -> Self {
        let mut prompts = HashMap::new();
        prompts.insert(
            "default".to_string(),
            "Describe this image in detail, including any text visible.".to_string(),
        );
        prompts.insert(
            "diagram".to_string(),
            "Describe this diagram, including its structure, labels, and relationships."
                .to_string(),
        );
        prompts.insert(
            "screenshot".to_string(),
            "Describe this screenshot, including the UI elements and any visible text.".to_string(),
        );

        Self {
            enabled: false,
            llm_url: None,
            model: None,
            prompts,
        }
    }
}

/// OCR processor configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OcrConfig {
    /// Enable OCR processing
    pub enabled: bool,
    /// OCR backend (paddle, tesseract, ocrs, surya, easyocr)
    pub backend: OcrBackend,
    /// Languages for OCR
    pub languages: Vec<String>,
    /// PP-OCR model size for the paddle backend: tiny | small | medium.
    /// Larger is more accurate and slower; models come from the Hugging Face cache (ADR-0004).
    pub paddle_model: String,
    /// Also OCR PDF pages whose native text layer is merely sparse and that
    /// carry no embedded images (recall-first). Off by default: such pages are
    /// almost always short native-text pages, and OCR-ing them costs seconds
    /// each for nothing. Scanned, text-less and garbled pages are always OCR'd.
    pub ocr_sparse_pages: bool,
    /// Render DPI used when OCR-ing scanned pages.
    pub page_dpi: u32,
    /// Optional local OCR HTTP server URL (liteparse OCR API). When set, the
    /// PDF parser uses it for OCR instead of its built-in Tesseract.
    pub ocr_server_url: Option<String>,
}

impl Default for OcrConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: OcrBackend::Paddle,
            languages: vec!["eng".to_string()],
            paddle_model: "small".to_string(),
            ocr_sparse_pages: false,
            page_dpi: 300,
            ocr_server_url: None,
        }
    }
}

/// Semantic tier-router mode. `heuristic` (default) keeps the existing
/// deterministic routing and never calls a model; `shadow` calls the router
/// but only logs its decision (no behavior change); `route` acts on the
/// router's tier selection (for the spike this redirects to the VLM path when
/// the router says `vlm`, and otherwise logs and falls back to the
/// deterministic pipeline).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RoutingMode {
    /// Deterministic routing only (never calls a model).
    #[default]
    Heuristic,
    /// Call the router, log its decision, change nothing.
    Shadow,
    /// Act on the router decision.
    Route,
}

/// The small, closed set of ingestion tiers a document/page may route to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RoutingTier {
    /// Clean text/vector extraction (native).
    Native,
    /// Scanned or text-sparse page (CPU OCR).
    CpuOcr,
    /// Dense/noisy tables, charts, handwriting (GPU model).
    Gpu,
    /// Image, diagram, screenshot (VLM).
    Vlm,
    /// Non-document or low value (skip).
    Skip,
}

impl RoutingTier {
    /// Parse a tier from the `choice` string returned by the router.
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s {
            "native" => Some(Self::Native),
            "cpu_ocr" => Some(Self::CpuOcr),
            "gpu" => Some(Self::Gpu),
            "vlm" => Some(Self::Vlm),
            "skip" => Some(Self::Skip),
            _ => None,
        }
    }

    /// The lowercase canonical name for a tier.
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::CpuOcr => "cpu_ocr",
            Self::Gpu => "gpu",
            Self::Vlm => "vlm",
            Self::Skip => "skip",
        }
    }
}

/// Configuration for the semantic tier-router seam (`[routing]`). The router is
/// a local System-One endpoint (e.g. `jaredpalmer/kev` serving
/// `POST /v1/systemone`). See `docs/research/2026-ocr-vlm-semantic-router.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RoutingConfig {
    /// Routing mode: `heuristic` | `shadow` | `route`.
    pub mode: RoutingMode,
    /// System-One base URL (e.g. `http://localhost:8009`). Required for
    /// `shadow`/`route` modes; empty means the router is skipped.
    pub router_url: Option<String>,
    /// Router model name (default `kev-latest`).
    pub model: String,
    /// Optional bearer API key for the router endpoint.
    pub api_key: Option<String>,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            mode: RoutingMode::Heuristic,
            router_url: None,
            model: "kev-latest".to_string(),
            api_key: None,
        }
    }
}
