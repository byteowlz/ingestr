//! `ingestr` CLI: watch directories, convert documents to Markdown, and
//! index them for full-text search.

use std::any::Any;
use std::collections::HashMap;
use std::env;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcCommand, Stdio};
use std::sync::{
    Arc, LazyLock,
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc::{self, RecvTimeoutError},
};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use config::{Config, Environment, File, FileFormat};
use env_logger::fmt::WriteStyle;
use ingestr_core::{IndexedDocument, SearchIndex};
use liteparse::{LiteParse, LiteParseConfig, OutputFormat};
use log::{LevelFilter, debug, error, info, warn};
use markitdown::{MarkItDown, model::ConversionOptions};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use ocrs::{ImageSource, OcrEngine, OcrEngineParams};
use rayon::prelude::*;
use regex::Regex;
use rten::Model as RtenModel;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::OnceLock;
use sysinfo::{Pid, Signal, System};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::runtime::Runtime;
use walkdir::WalkDir;

const APP_NAME: &str = env!("CARGO_PKG_NAME");
const CONFIG_DIR_NAME: &str = "ingestr";

/// Matches a standalone page number line (optionally prefixed with "Page").
#[expect(
    clippy::expect_used,
    reason = "regex pattern is a compile-time literal that is guaranteed valid"
)]
static PAGE_NUM_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*(page\s+)?\d+(\s+of\s+\d+)?\s*$").expect("valid static regex")
});
/// Matches 4+ consecutive newlines (noise to collapse).
#[expect(
    clippy::expect_used,
    reason = "regex pattern is a compile-time literal that is guaranteed valid"
)]
static MULTI_BLANK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\n{4,}").expect("valid static regex"));
/// Matches a markdown heading block at line start.
#[expect(
    clippy::expect_used,
    reason = "regex pattern is a compile-time literal that is guaranteed valid"
)]
static HEADING_BLOCK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(#{1,6})\s+(.+)$").expect("valid static regex"));
/// Matches a markdown heading marker following a newline.
#[expect(
    clippy::expect_used,
    reason = "regex pattern is a compile-time literal that is guaranteed valid"
)]
static HEADING_LINE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\n#{1,6}\s").expect("valid static regex"));
/// Matches a page break marker (form-feed, dashes, or asterisks).
#[expect(
    clippy::expect_used,
    reason = "regex pattern is a compile-time literal that is guaranteed valid"
)]
static PAGE_BREAK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:\x0c|\n-{3,}\n|\n\* \* \*\n)").expect("valid static regex"));

fn main() {
    if let Err(err) = try_main() {
        // Clean, teachy error to stderr (no stack trace unless --trace).
        let _ = writeln!(io::stderr(), "{err}");
        let chain = err.chain().skip(1);
        for cause in chain {
            let _ = writeln!(io::stderr(), "  caused by: {cause}");
        }
        std::process::exit(1);
    }
}

/// Known subcommand names (used for default-subcommand detection).
const KNOWN_SUBCOMMANDS: &[&str] = &[
    "service",
    "search",
    "convert",
    "init",
    "config",
    "cache",
    "completions",
    "doctor",
    "help",
];

fn try_main() -> Result<()> {
    // Default subcommand: if the first positional arg is not a known subcommand,
    // treat it as `convert <INPUT>`. This lets users write `ingestr file.pdf`
    // instead of `ingestr convert file.pdf`.
    let args: Vec<String> = env::args().collect();
    let cli = if args.len() > 1 {
        let first_arg = &args[1];
        // Skip if it's a flag (starts with -)
        let is_flag = first_arg.starts_with('-');
        let is_subcommand = KNOWN_SUBCOMMANDS.contains(&first_arg.as_str());
        if !is_flag && !is_subcommand {
            // Insert "convert" as the subcommand
            let mut new_args = vec![args[0].clone(), "convert".to_string()];
            new_args.extend_from_slice(&args[1..]);
            Cli::parse_from(new_args)
        } else {
            Cli::parse()
        }
    } else {
        Cli::parse()
    };

    let ctx = RuntimeContext::new(cli.common.clone())?;
    ctx.init_logging()?;
    debug!("resolved paths: {:#?}", ctx.paths);

    match cli.command {
        Command::Service { command } => handle_service(&ctx, command),
        Command::Search(cmd) => handle_search(&ctx, cmd),
        Command::Convert(cmd) => handle_convert(&ctx, cmd),
        Command::Init(cmd) => handle_init(&ctx, cmd),
        Command::Config { command } => handle_config(&ctx, command),
        Command::Cache { command } => handle_cache(&ctx, command),
        Command::Completions { shell } => handle_completions(shell),
        Command::Doctor => handle_doctor(),
    }
}

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "Background service that converts documents to Markdown and indexes them for search.",
    propagate_version = true
)]
struct Cli {
    #[command(flatten)]
    common: CommonOpts,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Clone, Args)]
struct CommonOpts {
    /// Override the config file path
    #[arg(long, value_name = "PATH", global = true)]
    config: Option<PathBuf>,
    /// Reduce output to only errors
    #[arg(short, long, action = clap::ArgAction::SetTrue, global = true)]
    quiet: bool,
    /// Increase logging verbosity (stackable)
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count, global = true)]
    verbose: u8,
    /// Enable debug logging (equivalent to -vv)
    #[arg(long, global = true)]
    debug: bool,
    /// Enable trace logging (overrides other levels)
    #[arg(long, global = true)]
    trace: bool,
    /// Output machine readable JSON
    #[arg(long, global = true)]
    json: bool,
    /// Disable ANSI colors in output
    #[arg(long = "no-color", global = true, conflicts_with = "color")]
    no_color: bool,
    /// Control color output (auto, always, never)
    #[arg(long, value_enum, default_value_t = ColorOption::Auto, global = true)]
    color: ColorOption,
    /// Do not change anything on disk
    #[arg(long = "dry-run", global = true)]
    dry_run: bool,
    /// Assume "yes" for interactive prompts
    #[arg(short = 'y', long = "yes", global = true)]
    assume_yes: bool,
    /// Maximum seconds to allow an operation to run
    #[arg(long = "timeout", value_name = "SECONDS", global = true)]
    timeout: Option<u64>,
    /// Override the degree of parallelism
    #[arg(long = "parallel", value_name = "N", global = true)]
    parallel: Option<usize>,
    /// Disable progress indicators
    #[arg(long = "no-progress", global = true)]
    no_progress: bool,
    /// Emit additional diagnostics for troubleshooting
    #[arg(long, global = true)]
    diagnostics: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum ColorOption {
    Auto,
    Always,
    Never,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Manage the background conversion and indexing service
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
    /// Query the search index
    Search(SearchCommand),
    /// Convert documents to Markdown (single file, directory, or URL)
    #[command(
        after_help = "Examples:\n\n  Convert one file to stdout:\n    ingestr convert report.pdf\n\n  Convert every document in the current directory to .md:\n    ingestr convert .\n\n  Convert every document in a directory tree (incl. subdirs) to .md:\n    ingestr convert . --recursive\n\n  Preview what would be converted (writes nothing):\n    ingestr convert . --dry-run\n\n  Machine-readable JSON summary of a batch conversion:\n    ingestr convert docs/ --recursive --json\n\n  Convert only PDFs and DOCX files, writing to out/:\n    ingestr convert . --recursive --extensions pdf,docx --output out/\n\n  Convert all .pptx files in the current directory:\n    ingestr convert . --batch pptx\n"
    )]
    Convert(ConvertCommand),
    /// Create config directories and default files
    Init(InitCommand),
    /// Inspect and manage configuration
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Manage the conversion cache
    Cache {
        #[command(subcommand)]
        command: CacheCommand,
    },
    /// Generate shell completions
    Completions {
        #[arg(value_enum)]
        shell: Shell,
    },
    /// Report which external tools are installed (doctor)
    Doctor,
}

#[derive(Debug, Subcommand)]
enum CacheCommand {
    /// Clear the conversion cache
    Clear,
    /// Show cache statistics
    Stats,
}

#[derive(Debug, Clone, Args)]
struct ServiceRunOpts {
    /// Directory to watch for new or updated documents
    #[arg(long, value_name = "PATH")]
    watch_dir: Option<PathBuf>,
    /// Directory to write converted markdown files
    #[arg(long, value_name = "PATH")]
    output_dir: Option<PathBuf>,
    /// Directory to store the search index
    #[arg(long, value_name = "PATH")]
    index_dir: Option<PathBuf>,
    /// Disable indexing while still converting files
    #[arg(long)]
    disable_index: bool,
    /// Convert the current contents and exit instead of watching for changes
    #[arg(long)]
    once: bool,
}

#[derive(Debug, Subcommand)]
enum ServiceCommand {
    /// Run the service in the foreground
    Run(ServiceRunOpts),
    /// Start the service in the background
    Start(ServiceRunOpts),
    /// Stop the background service
    Stop,
    /// Restart the background service
    Restart(ServiceRunOpts),
    /// Show service status
    Status,
}

#[derive(Debug, Clone, Args)]
struct SearchCommand {
    /// Search query
    query: String,
    /// Maximum results to return
    #[arg(long, default_value_t = 10)]
    limit: usize,
    /// Override the index directory
    #[arg(long, value_name = "PATH")]
    index_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
struct ConvertCommand {
    /// File, directory, or URL to convert (use '-' or omit for stdin)
    #[arg(value_name = "INPUT")]
    input: Option<String>,
    /// Batch convert all files with given extension in current directory (recursive)
    /// Example: --batch pptx converts all .pptx files to .md alongside sources
    #[arg(long, value_name = "EXT")]
    batch: Option<String>,
    /// Stop at the first conversion failure instead of continuing
    #[arg(long)]
    fail_fast: bool,
    /// Write output to a file or directory instead of stdout. When converting
    /// a directory and no output is specified, writes .md files alongside sources.
    #[arg(short, long, value_name = "PATH")]
    output: Option<PathBuf>,
    /// Input format hint when reading from stdin
    #[arg(long, value_name = "FORMAT", value_enum)]
    from: Option<InputFormat>,
    /// Recursively process directories
    #[arg(short, long)]
    recursive: bool,
    /// File extensions to process (comma-separated, e.g., "pdf,docx,html")
    #[arg(long, value_name = "EXTENSIONS", value_delimiter = ',')]
    extensions: Option<Vec<String>>,
    /// Write output files alongside source files (same directory) when processing
    /// directories. This is the default when no --output is specified.
    #[arg(long)]
    in_place: bool,
    /// Enable VLM processing for images
    #[arg(long)]
    vlm: bool,
    /// Vision model to use (e.g., "glm-4.6v-flash", "qwen/qwen3-vl-8b")
    #[arg(long, value_name = "MODEL")]
    vlm_model: Option<String>,
    /// Custom VLM prompt for image description
    #[arg(long, value_name = "PROMPT")]
    vlm_prompt: Option<String>,
    /// Number of parallel VLM requests (default: 1)
    #[arg(short = 'j', long = "jobs", value_name = "N", default_value = "1")]
    jobs: usize,
    /// Enable OCR processing for scanned documents
    #[arg(long)]
    ocr: bool,
    /// OCR backend to use
    #[arg(long, value_name = "BACKEND", value_enum, default_value = "ocrs")]
    ocr_backend: OcrBackend,
    /// OCR languages (comma-separated, e.g., "eng,deu")
    #[arg(long, value_name = "LANGS", value_delimiter = ',')]
    ocr_languages: Option<Vec<String>>,
    /// Include YAML frontmatter with metadata in output
    #[arg(long)]
    meta: bool,
    /// Skip post-processing cleanup (output raw conversion result)
    #[arg(long)]
    raw: bool,
    /// Maximum tokens to output (approximate, truncates at section boundaries)
    #[arg(long, value_name = "N")]
    max_tokens: Option<usize>,
    /// Maximum characters to output (truncates at section boundaries)
    #[arg(long, value_name = "N")]
    max_chars: Option<usize>,
    /// Character offset to start from (for pagination with --max-tokens/--max-chars)
    #[arg(long, value_name = "N", default_value_t = 0)]
    offset: usize,
    /// Show document structure (table of contents with token estimates)
    #[arg(long)]
    toc: bool,
    /// Extract only a specific section by number or heading (e.g., "2.1" or "Summary")
    #[arg(short = 's', long, value_name = "SECTION")]
    section: Option<String>,
    /// Convert only specific pages (PDF only, e.g., "1-3,7")
    #[arg(long, value_name = "PAGES")]
    pages: Option<String>,
    /// Read input from the system clipboard
    #[arg(long)]
    clipboard: bool,
    /// Bypass the conversion cache
    #[arg(long)]
    no_cache: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum, Default)]
enum InputFormat {
    #[default]
    Auto,
    Html,
    Text,
    Pdf,
    Docx,
    Xlsx,
    Pptx,
    Csv,
    Json,
    Xml,
    Markdown,
}

impl InputFormat {
    const fn extension(self) -> Option<&'static str> {
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

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum OcrBackend {
    Tesseract,
    #[default]
    Ocrs,
    Surya,
    Easyocr,
}

impl std::fmt::Display for OcrBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tesseract => write!(f, "tesseract"),
            Self::Ocrs => write!(f, "ocrs"),
            Self::Surya => write!(f, "surya"),
            Self::Easyocr => write!(f, "easyocr"),
        }
    }
}

#[derive(Debug, Clone, Args)]
struct InitCommand {
    /// Recreate configuration even if it already exists
    #[arg(long = "force")]
    force: bool,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Output the effective configuration
    Show,
    /// Print the resolved config file path
    Path,
    /// Regenerate the default configuration file
    Reset,
}

#[derive(Debug, Clone)]
struct RuntimeContext {
    common: CommonOpts,
    paths: AppPaths,
    config: AppConfig,
    directories: ResolvedDirectories,
}

#[derive(Debug, Clone)]
struct ResolvedDirectories {
    watch_dir: PathBuf,
    output_dir: PathBuf,
    index_dir: PathBuf,
}

impl RuntimeContext {
    fn new(common: CommonOpts) -> Result<Self> {
        let paths = AppPaths::discover(common.config.clone())?;
        let config = load_or_init_config(&paths, &common)?;
        let paths = paths.apply_overrides(&config)?;
        let directories = ResolvedDirectories::from_config(&config, &paths)?;
        let ctx = Self {
            common,
            paths,
            config,
            directories,
        };
        ctx.ensure_directories()?;
        Ok(ctx)
    }

    fn init_logging(&self) -> Result<()> {
        if self.common.quiet {
            log::set_max_level(LevelFilter::Off);
            return Ok(());
        }

        let mut builder =
            env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));

        builder.filter_level(self.effective_log_level());
        // lopdf can emit repetitive "corrupt deflate stream" warnings for encrypted/corrupt PDFs.
        // We handle these cases explicitly and return user-friendly errors, so hide this crate-level noise.
        builder.filter_module("lopdf", LevelFilter::Error);

        let force_color = matches!(self.common.color, ColorOption::Always)
            || env::var_os("FORCE_COLOR").is_some();
        let disable_color = self.common.no_color
            || matches!(self.common.color, ColorOption::Never)
            || env::var_os("NO_COLOR").is_some()
            || (!force_color && !io::stderr().is_terminal());

        if disable_color {
            builder.write_style(WriteStyle::Never);
        } else if force_color {
            builder.write_style(WriteStyle::Always);
        } else {
            builder.write_style(WriteStyle::Auto);
        }

        if self.common.diagnostics {
            builder.format_timestamp_millis();
            builder.format_module_path(true);
            builder.format_target(true);
        }

        builder.try_init().or_else(|err| {
            if self.common.verbose > 0 {
                eprintln!("logger already initialized: {err}");
            }
            Ok(())
        })
    }

    const fn effective_log_level(&self) -> LevelFilter {
        if self.common.trace {
            LevelFilter::Trace
        } else if self.common.debug {
            LevelFilter::Debug
        } else {
            match self.common.verbose {
                0 => LevelFilter::Info,
                1 => LevelFilter::Debug,
                _ => LevelFilter::Trace,
            }
        }
    }

    fn ensure_directories(&self) -> Result<()> {
        if self.common.dry_run {
            info!(
                "dry-run: would ensure data dir {}, state dir {}, output dir {}, and index dir {}",
                self.paths.data_dir.display(),
                self.paths.state_dir.display(),
                self.directories.output_dir.display(),
                self.directories.index_dir.display()
            );
            return Ok(());
        }

        fs::create_dir_all(&self.paths.data_dir).with_context(|| {
            format!("creating data directory {}", self.paths.data_dir.display())
        })?;
        fs::create_dir_all(&self.paths.state_dir).with_context(|| {
            format!(
                "creating state directory {}",
                self.paths.state_dir.display()
            )
        })?;
        fs::create_dir_all(&self.directories.output_dir).with_context(|| {
            format!(
                "creating markdown output directory {}",
                self.directories.output_dir.display()
            )
        })?;

        Ok(())
    }

    fn pid_path(&self) -> PathBuf {
        self.paths.state_dir.join("service.pid")
    }

    fn service_settings(&self, serve: &ServiceRunOpts) -> Result<ServiceSettings> {
        let watch_dir = if let Some(ref provided) = serve.watch_dir {
            expand_path(provided.clone())?
        } else {
            self.directories.watch_dir.clone()
        };

        let output_dir = if let Some(ref provided) = serve.output_dir {
            expand_path(provided.clone())?
        } else {
            self.directories.output_dir.clone()
        };

        let index_dir = if let Some(ref provided) = serve.index_dir {
            expand_path(provided.clone())?
        } else {
            self.directories.index_dir.clone()
        };

        let index_enabled = if serve.disable_index {
            false
        } else {
            self.config.index.enabled
        };

        Ok(ServiceSettings {
            watch_dir,
            output_dir,
            index_dir,
            index_enabled,
            debounce: Duration::from_millis(self.config.watcher.debounce_ms.max(50)),
            skip_hidden: self.config.watcher.skip_hidden,
            data_dir: self.paths.data_dir.clone(),
            state_dir: self.paths.state_dir.clone(),
            llm_enabled: self.config.llm.enabled,
            llm_client: self.config.llm.client.clone(),
            llm_model: self.config.llm.model.clone(),
            llm_base_url: self.config.llm.base_url.clone(),
            llm_api_key: self.config.llm.api_key.clone(),
            processors: self.config.processors.clone(),
        })
    }
}

#[derive(Debug, Clone)]
struct AppPaths {
    active_config: PathBuf,
    global_config: PathBuf,
    local_config: PathBuf,
    config_override: bool,
    data_dir: PathBuf,
    state_dir: PathBuf,
}

impl AppPaths {
    fn discover(override_path: Option<PathBuf>) -> Result<Self> {
        let global_config = default_config_dir()?.join("config.toml");
        let local_config = env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("config.toml");

        let config_override = override_path.is_some();

        let active_config = match override_path {
            Some(ref path) => expand_path(path.clone())?,
            None => global_config.clone(),
        };

        if active_config.parent().is_none() {
            return Err(anyhow!("invalid config file path: {active_config:?}"));
        }

        let data_dir = default_data_dir()?;
        let state_dir = default_state_dir()?;

        Ok(Self {
            active_config,
            global_config,
            local_config,
            config_override,
            data_dir,
            state_dir,
        })
    }

    fn apply_overrides(mut self, cfg: &AppConfig) -> Result<Self> {
        if let Some(ref data_override) = cfg.paths.data_dir {
            self.data_dir = expand_str_path(data_override)?;
        }
        if let Some(ref state_override) = cfg.paths.state_dir {
            self.state_dir = expand_str_path(state_override)?;
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct AppConfig {
    profile: String,
    logging: LoggingConfig,
    runtime: RuntimeConfig,
    paths: PathsConfig,
    watcher: WatcherConfig,
    output: OutputConfig,
    index: IndexConfig,
    llm: LlmConfig,
    processors: ProcessorsConfig,
}

impl AppConfig {
    fn with_profile_override(mut self, profile: Option<String>) -> Self {
        if let Some(profile) = profile {
            self.profile = profile;
        }
        self
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            profile: "default".to_string(),
            logging: LoggingConfig::default(),
            runtime: RuntimeConfig::default(),
            paths: PathsConfig::default(),
            watcher: WatcherConfig::default(),
            output: OutputConfig::default(),
            index: IndexConfig::default(),
            llm: LlmConfig::default(),
            processors: ProcessorsConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct LoggingConfig {
    level: String,
    file: Option<String>,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            file: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct RuntimeConfig {
    parallelism: Option<usize>,
    timeout: Option<u64>,
    fail_fast: bool,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            parallelism: None,
            timeout: Some(60),
            fail_fast: true,
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
struct PathsConfig {
    data_dir: Option<String>,
    state_dir: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct WatcherConfig {
    watch_dir: String,
    debounce_ms: u64,
    skip_hidden: bool,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            watch_dir: default_watch_dir_string(),
            debounce_ms: 750,
            skip_hidden: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct OutputConfig {
    markdown_dir: String,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            markdown_dir: default_output_dir_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct IndexConfig {
    enabled: bool,
    index_dir: Option<String>,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            index_dir: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct LlmConfig {
    /// Enable LLM-based image description generation
    enabled: bool,
    /// LLM provider: "openai", "gemini", or "deepseek"
    client: String,
    /// Model name (e.g., "gpt-4o", "gemini-1.5-flash", "deepseek-chat")
    model: String,
    /// Custom API base URL (for OpenAI-compatible endpoints)
    base_url: Option<String>,
    /// API key (optional, can also use environment variables)
    api_key: Option<String>,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            client: "openai".to_string(),
            model: "gpt-4o".to_string(),
            base_url: None,
            api_key: None,
        }
    }
}

/// Processor pipeline configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct ProcessorsConfig {
    /// Ordered list of processors to try
    pipeline: Vec<String>,
    /// File type routing rules (extension -> processor list)
    routing: HashMap<String, Vec<String>>,
    /// VLM processor configuration
    vlm: VlmConfig,
    /// OCR processor configuration
    ocr: OcrConfig,
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
struct VlmConfig {
    /// Enable VLM processing
    enabled: bool,
    /// LLM server URL (falls back to [llm].`base_url` if empty)
    llm_url: Option<String>,
    /// Model name (falls back to [llm].model if empty)
    model: Option<String>,
    /// Custom prompts per file type
    prompts: HashMap<String, String>,
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
struct OcrConfig {
    /// Enable OCR processing
    enabled: bool,
    /// OCR backend (tesseract, ocrs, surya, easyocr)
    backend: OcrBackend,
    /// Languages for OCR
    languages: Vec<String>,
    /// Render DPI used when OCR-ing scanned pages.
    page_dpi: u32,
    /// Optional local OCR HTTP server URL (liteparse OCR API). When set, the
    /// PDF parser uses it for OCR instead of its built-in Tesseract.
    ocr_server_url: Option<String>,
}

impl Default for OcrConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: OcrBackend::Ocrs,
            languages: vec!["eng".to_string()],
            page_dpi: 300,
            ocr_server_url: None,
        }
    }
}

fn handle_service(ctx: &RuntimeContext, command: ServiceCommand) -> Result<()> {
    match command {
        ServiceCommand::Run(opts) => run_service_foreground(ctx, opts),
        ServiceCommand::Start(opts) => start_service_background(ctx, opts),
        ServiceCommand::Stop => stop_service(ctx),
        ServiceCommand::Restart(opts) => {
            stop_service(ctx).ok();
            start_service_background(ctx, opts)
        }
        ServiceCommand::Status => status_service(ctx),
    }
}

fn run_service_foreground(ctx: &RuntimeContext, cmd: ServiceRunOpts) -> Result<()> {
    let settings = ctx.service_settings(&cmd)?;
    let effective = ctx.config.clone().with_profile_override(None);

    info!(
        "starting {} with profile '{}' watching {} -> {}",
        APP_NAME,
        effective.profile,
        settings.watch_dir.display(),
        settings.output_dir.display()
    );

    if settings.index_enabled {
        info!("indexing enabled at {}", settings.index_dir.display());
    } else {
        warn!("indexing disabled; searches will not include new documents");
    }

    let mut service = ConversionService::new(settings)?;
    if cmd.once {
        service.process_existing()?;
        service.flush_index()?;
        return Ok(());
    }

    service.run()
}

fn start_service_background(ctx: &RuntimeContext, cmd: ServiceRunOpts) -> Result<()> {
    let pid_path = ctx.pid_path();
    if let Some(pid) = read_pid(&pid_path)? {
        if process_running(pid) {
            return Err(anyhow!("service already running with pid {pid}"));
        }
        fs::remove_file(&pid_path).ok();
    }

    let settings = ctx.service_settings(&cmd)?;
    if ctx.common.dry_run {
        info!(
            "dry-run: would start background service watching {} -> {}",
            settings.watch_dir.display(),
            settings.output_dir.display()
        );
        return Ok(());
    }

    let mut command = ProcCommand::new(env::current_exe()?);
    command.arg("service").arg("run");
    append_service_args(&mut command, &cmd);
    if let Some(ref cfg) = ctx.common.config {
        command.arg("--config").arg(cfg);
    }

    let log_path = ctx.paths.state_dir.join("service.log");
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening log file {}", log_path.display()))?;
    let log_err = log.try_clone()?;
    command.stdout(Stdio::from(log));
    command.stderr(Stdio::from(log_err));
    command.stdin(Stdio::null());

    let child = command
        .spawn()
        .context("spawning background service process")?;
    write_pid(&pid_path, child.id() as i32)?;
    info!(
        "started background service pid {} (logs at {})",
        child.id(),
        log_path.display()
    );
    Ok(())
}

fn stop_service(ctx: &RuntimeContext) -> Result<()> {
    let pid_path = ctx.pid_path();
    let Some(pid) = read_pid(&pid_path)? else {
        info!("no running service found (pid file missing)");
        return Ok(());
    };

    if !process_running(pid) {
        info!("stale pid {pid}; removing pid file");
        fs::remove_file(&pid_path).ok();
        return Ok(());
    }

    let mut sys = System::new_all();
    sys.refresh_processes();
    let sys_pid = Pid::from_u32(pid as u32);
    if let Some(proc) = sys.process(sys_pid) {
        let killed = proc.kill_with(Signal::Term).unwrap_or(false) || proc.kill();
        if killed {
            info!("stopped service pid {pid}");
            fs::remove_file(&pid_path).ok();
            return Ok(());
        }
    }

    Err(anyhow!("failed to stop service pid {pid}"))
}

fn status_service(ctx: &RuntimeContext) -> Result<()> {
    let pid_path = ctx.pid_path();
    let Some(pid) = read_pid(&pid_path)? else {
        println!("service status: stopped");
        return Ok(());
    };

    if process_running(pid) {
        println!("service status: running (pid {pid})");
    } else {
        println!("service status: not running (stale pid {pid})");
    }

    Ok(())
}

fn append_service_args(cmd: &mut ProcCommand, opts: &ServiceRunOpts) {
    if let Some(ref dir) = opts.watch_dir {
        cmd.arg("--watch-dir").arg(dir);
    }
    if let Some(ref dir) = opts.output_dir {
        cmd.arg("--output-dir").arg(dir);
    }
    if let Some(ref dir) = opts.index_dir {
        cmd.arg("--index-dir").arg(dir);
    }
    if opts.disable_index {
        cmd.arg("--disable-index");
    }
    if opts.once {
        cmd.arg("--once");
    }
}

fn read_pid(path: &Path) -> Result<Option<i32>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(path)?;
    let pid: i32 = text.trim().parse()?;
    Ok(Some(pid))
}

fn write_pid(path: &Path, pid: i32) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, pid.to_string()).with_context(|| format!("writing pid file {}", path.display()))
}

fn process_running(pid: i32) -> bool {
    let mut sys = System::new();
    sys.refresh_process(Pid::from_u32(pid as u32));
    sys.process(Pid::from_u32(pid as u32)).is_some()
}

fn handle_search(ctx: &RuntimeContext, cmd: SearchCommand) -> Result<()> {
    let index_dir = if let Some(ref provided) = cmd.index_dir {
        expand_path(provided.clone())?
    } else {
        ctx.directories.index_dir.clone()
    };

    let mut index = SearchIndex::open(&index_dir, false)?;

    let results = index.search(&cmd.query, cmd.limit)?;

    if ctx.common.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&results).context("serializing search results to JSON")?
        );
    } else if results.is_empty() {
        println!("No results found");
    } else {
        for hit in &results {
            println!(
                "- {} (score {:.2}) -> {}",
                hit.title.as_deref().unwrap_or(hit.source_path.as_str()),
                hit.score,
                hit.output_path
            );
        }
    }

    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ConvertResult {
    source_path: String,
    output_path: String,
    source_modified: Option<String>,
    title: Option<String>,
    converted_at: String,
    markdown: String,
}

#[derive(Debug, Clone)]
struct ConvertedDocument {
    title: Option<String>,
    text_content: String,
    /// If true, content was already streamed to a file during VLM processing
    already_written: bool,
}

fn panic_payload_to_string(payload: Box<dyn Any + Send>) -> String {
    match payload.downcast::<String>() {
        Ok(message) => *message,
        Err(payload) => match payload.downcast::<&'static str>() {
            Ok(message) => message.to_string(),
            Err(_) => "unknown panic payload".to_string(),
        },
    }
}

fn safe_markitdown_convert<T, E, F>(context: &str, convert: F) -> Option<T>
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

#[cfg(test)]
fn convert_single_file(
    markitdown: &MarkItDown,
    input: &Path,
    conversion_opts: Option<ConversionOptions>,
) -> Result<ConvertedDocument> {
    let path_str = input
        .to_str()
        .ok_or_else(|| anyhow!("invalid path encoding for {}", input.display()))?;

    let context = format!("converting {}", input.display());
    if let Some(converted) =
        safe_markitdown_convert(&context, || markitdown.convert(path_str, conversion_opts))
    {
        return Ok(ConvertedDocument {
            title: converted.title,
            text_content: converted.text_content,
            already_written: false,
        });
    }

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

fn render_frontmatter_markdown(frontmatter: &Frontmatter, text_content: &str) -> Result<String> {
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

// ============================================================
// Post-processing: clean noisy conversion output
// ============================================================

/// Estimate token count (rough: ~4 chars per token for English text)
const fn estimate_tokens(text: &str) -> usize {
    // A simple heuristic: split on whitespace, count words,
    // then apply ~1.3 tokens per word (common for English).
    // For non-English or code-heavy content, chars/4 is more stable.
    text.len().div_ceil(4)
}

/// Clean converted markdown by removing common PDF/document noise.
fn clean_markdown(text: &str) -> String {
    let mut lines: Vec<&str> = text.lines().collect();

    // 1. Detect and remove repeated headers/footers.
    // If the same line appears on 3+ "pages" (roughly every 40-80 lines), it is likely a header/footer.
    if lines.len() > 80 {
        let mut freq: HashMap<String, usize> = HashMap::new();
        for line in &lines {
            let trimmed = line.trim();
            if !trimmed.is_empty() && trimmed.len() < 120 {
                *freq.entry(trimmed.to_lowercase()).or_insert(0) += 1;
            }
        }
        let threshold = (lines.len() / 60).max(3);
        let repeated: std::collections::HashSet<String> = freq
            .into_iter()
            .filter(|(_, count)| *count >= threshold)
            .map(|(line, _)| line)
            .collect();

        if !repeated.is_empty() {
            lines.retain(|line| {
                let trimmed = line.trim().to_lowercase();
                !repeated.contains(&trimmed)
            });
        }
    }

    // 2. Remove standalone page numbers (lines that are just a number, optionally with "Page" prefix)
    lines.retain(|line| !PAGE_NUM_RE.is_match(line));

    // 3. Fix broken line wraps from PDF column layouts:
    // If a line ends without punctuation or a heading marker and the next starts lowercase, join them.
    let mut result = String::with_capacity(text.len());
    let mut i = 0;
    while i < lines.len() {
        let current = lines[i].trim_end();
        if i + 1 < lines.len() {
            let next = lines[i + 1].trim_start();
            let current_trimmed = current.trim();

            // Join if: current doesn't end with sentence-ender or heading,
            // current is not empty, next starts with lowercase
            let should_join = !current_trimmed.is_empty()
                && !next.is_empty()
                && !current_trimmed.ends_with('.')
                && !current_trimmed.ends_with(':')
                && !current_trimmed.ends_with('!')
                && !current_trimmed.ends_with('?')
                && !current_trimmed.ends_with('|')
                && !current_trimmed.starts_with('#')
                && !current_trimmed.starts_with('-')
                && !current_trimmed.starts_with('*')
                && !current_trimmed.starts_with('|')
                && !next.starts_with('#')
                && !next.starts_with('-')
                && !next.starts_with('*')
                && !next.starts_with('|')
                && !next.starts_with('>')
                && next.starts_with(|c: char| c.is_lowercase());

            if should_join {
                result.push_str(current_trimmed);
                result.push(' ');
                i += 1;
                continue;
            }
        }
        result.push_str(current);
        result.push('\n');
        i += 1;
    }

    // 4. Collapse 3+ consecutive blank lines into 2
    let result = MULTI_BLANK_RE.replace_all(&result, "\n\n\n").to_string();

    // 5. Trim leading/trailing whitespace
    result.trim().to_string()
}

// ============================================================
// Document structure: TOC extraction and section retrieval
// ============================================================

#[derive(Debug, Clone, Serialize)]
struct TocEntry {
    /// Heading level (1-6)
    level: usize,
    /// Section number (e.g., "2.1.3")
    number: String,
    /// Heading text
    title: String,
    /// Character offset where this section starts
    char_start: usize,
    /// Character offset where this section ends
    char_end: usize,
    /// Estimated token count for this section
    tokens: usize,
    /// Whether this section contains a markdown table
    has_table: bool,
    /// Whether this section contains a code block
    has_code: bool,
}

/// Extract table of contents from markdown content.
fn extract_toc(content: &str) -> Vec<TocEntry> {
    let heading_re = &HEADING_BLOCK_RE;
    let mut entries: Vec<TocEntry> = Vec::new();
    let mut counters: Vec<usize> = vec![0; 7]; // index 1-6 for heading levels

    // Find all heading positions
    let mut heading_positions: Vec<(usize, usize, String)> = Vec::new();
    for (offset, line) in content.lines().scan(0usize, |pos, line| {
        let start = *pos;
        *pos += line.len() + 1; // +1 for newline
        Some((start, line))
    }) {
        if let Some(caps) = heading_re.captures(line) {
            let level = caps[1].len();
            let title = caps[2].trim().to_string();
            heading_positions.push((offset, level, title));
        }
    }

    for (idx, (char_start, level, title)) in heading_positions.iter().enumerate() {
        let level = *level;
        let char_start = *char_start;

        // Calculate section end: either start of next heading at same or higher level, or end of doc
        let char_end = heading_positions
            .iter()
            .skip(idx + 1)
            .find(|(_, l, _)| *l <= level)
            .map_or(content.len(), |(pos, _, _)| *pos);

        let section_text = &content[char_start..char_end];

        // Update counters for numbering
        counters[level] += 1;
        // Reset all deeper counters
        for c in counters.iter_mut().skip(level + 1) {
            *c = 0;
        }

        // Build section number
        let number: String = counters[1..=level]
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join(".");

        let tokens = estimate_tokens(section_text);
        let has_table = section_text.contains("\n|") && section_text.contains("---|");
        let has_code = section_text.contains("```");

        entries.push(TocEntry {
            level,
            number,
            title: title.clone(),
            char_start,
            char_end,
            tokens,
            has_table,
            has_code,
        });
    }

    entries
}

/// Format TOC for display
fn format_toc(toc: &[TocEntry], total_tokens: usize) -> String {
    let mut out = String::new();
    for entry in toc {
        let indent = "  ".repeat(entry.level.saturating_sub(1));
        let mut markers = Vec::new();
        if entry.has_table {
            markers.push("table");
        }
        if entry.has_code {
            markers.push("code");
        }
        let marker_str = if markers.is_empty() {
            String::new()
        } else {
            format!(", has {}", markers.join("+"))
        };
        out.push_str(&format!(
            "{}{}. {} (tokens: ~{}{})\n",
            indent, entry.number, entry.title, entry.tokens, marker_str
        ));
    }
    out.push_str(&format!(
        "\nTotal: ~{} tokens across {} sections\n",
        total_tokens,
        toc.len()
    ));
    out
}

/// Extract a specific section by number (e.g., "2.1") or by heading text (fuzzy match).
fn extract_section(content: &str, selector: &str) -> Option<String> {
    let toc = extract_toc(content);
    if toc.is_empty() {
        return None;
    }

    // Try exact number match first
    if let Some(entry) = toc.iter().find(|e| e.number == selector) {
        return Some(content[entry.char_start..entry.char_end].to_string());
    }

    // Try heading text match (case-insensitive substring)
    let selector_lower = selector.to_lowercase();
    if let Some(entry) = toc
        .iter()
        .find(|e| e.title.to_lowercase().contains(&selector_lower))
    {
        return Some(content[entry.char_start..entry.char_end].to_string());
    }

    None
}

/// Truncate content at a section boundary, respecting a character budget.
fn truncate_at_boundary(
    content: &str,
    max_chars: usize,
    offset: usize,
) -> (String, Option<String>) {
    if offset >= content.len() {
        return (String::new(), None);
    }

    let sliced = &content[offset..];
    if sliced.len() <= max_chars {
        return (sliced.to_string(), None);
    }

    // Find the last heading boundary before max_chars
    let heading_re = &HEADING_LINE_RE;
    let mut last_break = max_chars;

    for m in heading_re.find_iter(&sliced[..max_chars]) {
        last_break = m.start();
    }

    // If no heading found, try paragraph break
    if last_break == max_chars
        && let Some(pos) = sliced[..max_chars].rfind("\n\n")
    {
        last_break = pos;
    }

    let truncated = sliced[..last_break].trim_end().to_string();
    let remaining_chars = sliced.len() - last_break;
    let remaining_tokens = estimate_tokens(&sliced[last_break..]);

    let toc = extract_toc(content);
    let total_sections = toc.len();
    let sections_shown = extract_toc(&content[..offset + last_break]).len();

    let notice = format!(
        "\n\n[truncated at char {}, ~{} tokens and ~{} chars remaining, sections {}/{}]",
        offset + last_break,
        remaining_tokens,
        remaining_chars,
        sections_shown,
        total_sections
    );

    (truncated, Some(notice))
}

/// Filter content by page numbers. Looks for page break markers or splits by rough page boundaries.
fn filter_pages(content: &str, pages: &[usize]) -> String {
    // Many PDF converters insert form-feed (\x0c) or "---" page breaks.
    // Also look for patterns like "Page N" or just form-feeds.
    let page_break = PAGE_BREAK_RE.split(content);

    let page_texts: Vec<&str> = page_break.collect();

    if page_texts.len() <= 1 {
        // No page breaks found - if the user asked for page 1, return everything
        if pages.contains(&1) {
            return content.to_string();
        }
        // Otherwise, we can't split by pages without markers
        return format!(
            "{content}\n\n[note: no page break markers found in document, showing all content]"
        );
    }

    let mut result = String::new();
    for &page_num in pages {
        if page_num > 0 && page_num <= page_texts.len() {
            if !result.is_empty() {
                result.push_str("\n\n");
            }
            result.push_str(page_texts[page_num - 1].trim());
        }
    }

    if result.is_empty() {
        format!(
            "[no content found for pages {:?}, document has {} pages]",
            pages,
            page_texts.len()
        )
    } else {
        result
    }
}

/// Parse a page range string like "1-3,7,10-12" into a sorted list of page numbers.
fn parse_page_range(spec: &str) -> Result<Vec<usize>> {
    let mut pages = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.contains('-') {
            let bounds: Vec<&str> = part.split('-').collect();
            if bounds.len() != 2 {
                bail!("invalid page range: {part}");
            }
            let start: usize = bounds[0].trim().parse().context("invalid page number")?;
            let end: usize = bounds[1].trim().parse().context("invalid page number")?;
            if start == 0 || end == 0 {
                bail!("page numbers start at 1");
            }
            if start > end {
                bail!("invalid range: {start} > {end}");
            }
            for p in start..=end {
                pages.push(p);
            }
        } else {
            let p: usize = part.parse().context("invalid page number")?;
            if p == 0 {
                bail!("page numbers start at 1");
            }
            pages.push(p);
        }
    }
    pages.sort_unstable();
    pages.dedup();
    Ok(pages)
}

// ============================================================
// Conversion cache
// ============================================================

fn cache_dir() -> Result<PathBuf> {
    let dir = if let Some(cache) = env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
        PathBuf::from(cache).join("ingestr")
    } else if let Some(home) = dirs::home_dir() {
        home.join(".cache").join("ingestr")
    } else {
        bail!("unable to determine cache directory");
    };
    Ok(dir)
}

/// Compute a cache key from file hash + conversion flags.
fn cache_key(
    path: &Path,
    meta: bool,
    raw: bool,
    vlm: bool,
    ocr: bool,
    section: &Option<String>,
    pages: &Option<String>,
) -> Result<String> {
    let data = fs::read(path).context("reading file for cache key")?;
    let mut hasher = Sha256::new();
    hasher.update(&data);
    // Include flags in hash so different flag combos get different cache entries
    hasher.update(if meta { b"meta=1" } else { b"meta=0" });
    hasher.update(if raw { b"raw=1" } else { b"raw=0" });
    hasher.update(if vlm { b"vlm=1" } else { b"vlm=0" });
    hasher.update(if ocr { b"ocr=1" } else { b"ocr=0" });
    if let Some(s) = section {
        hasher.update(format!("section={s}").as_bytes());
    }
    if let Some(p) = pages {
        hasher.update(format!("pages={p}").as_bytes());
    }
    let hash = hasher.finalize();
    Ok(hex::encode(hash))
}

/// Try to read a cached conversion result.
fn cache_get(key: &str) -> Result<Option<String>> {
    let path = cache_dir()?.join(&key[..2]).join(key);
    if path.exists() {
        Ok(Some(fs::read_to_string(path)?))
    } else {
        Ok(None)
    }
}

/// Store a conversion result in the cache.
fn cache_put(key: &str, content: &str) -> Result<()> {
    let dir = cache_dir()?.join(&key[..2]);
    fs::create_dir_all(&dir)?;
    fs::write(dir.join(key), content)?;
    Ok(())
}

fn handle_cache(ctx: &RuntimeContext, command: CacheCommand) -> Result<()> {
    match command {
        CacheCommand::Clear => {
            let dir = cache_dir()?;
            if dir.exists() {
                let count = WalkDir::new(&dir)
                    .into_iter()
                    .filter_map(Result::ok)
                    .filter(|e| e.file_type().is_file())
                    .count();
                if ctx.common.dry_run {
                    info!(
                        "dry-run: would remove {} cached files from {}",
                        count,
                        dir.display()
                    );
                } else {
                    fs::remove_dir_all(&dir)?;
                    fs::create_dir_all(&dir)?;
                    println!("Cleared {count} cached conversions");
                }
            } else {
                println!("Cache is empty");
            }
            Ok(())
        }
        CacheCommand::Stats => {
            let dir = cache_dir()?;
            if !dir.exists() {
                if ctx.common.json {
                    println!(
                        r#"{{"files": 0, "size_bytes": 0, "path": "{}"}}"#,
                        dir.display()
                    );
                } else {
                    println!("Cache is empty ({})", dir.display());
                }
                return Ok(());
            }
            let mut files = 0usize;
            let mut total_size = 0u64;
            for entry in WalkDir::new(&dir).into_iter().filter_map(Result::ok) {
                if entry.file_type().is_file() {
                    files += 1;
                    total_size += entry.metadata().map_or(0, |m| m.len());
                }
            }
            if ctx.common.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "files": files,
                        "size_bytes": total_size,
                        "size_mb": format!("{:.1}", total_size as f64 / 1_048_576.0),
                        "path": dir.display().to_string(),
                    }))?
                );
            } else {
                println!(
                    "Cache: {} files, {:.1} MB ({})",
                    files,
                    total_size as f64 / 1_048_576.0,
                    dir.display()
                );
            }
            Ok(())
        }
    }
}

// ============================================================
// URL fetching
// ============================================================

/// Check if a string looks like a URL
fn is_url(input: &str) -> bool {
    input.starts_with("http://") || input.starts_with("https://")
}

/// Fetch a URL and save to a temp file for conversion.
/// Returns the temp file path and a cleanup guard.
fn fetch_url(url: &str) -> Result<PathBuf> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(10))
        .user_agent("ingestr/0.1")
        .build()
        .context("building HTTP client")?;

    let response = client
        .get(url)
        .send()
        .with_context(|| format!("fetching URL: {url}"))?;

    if !response.status().is_success() {
        bail!("HTTP {} fetching {}", response.status(), url);
    }

    // Determine extension from Content-Type or URL
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();

    let ext = if content_type.contains("pdf") {
        "pdf"
    } else if content_type.contains("html") {
        "html"
    } else if content_type.contains("json") {
        "json"
    } else if content_type.contains("xml") {
        "xml"
    } else if content_type.contains("wordprocessingml") || content_type.contains("docx") {
        "docx"
    } else if content_type.contains("spreadsheetml") || content_type.contains("xlsx") {
        "xlsx"
    } else if content_type.contains("presentationml") || content_type.contains("pptx") {
        "pptx"
    } else {
        // Try to get extension from URL path
        url.rsplit('/')
            .next()
            .and_then(|segment| segment.rsplit('.').next())
            .filter(|ext| ext.len() <= 5 && ext.chars().all(char::is_alphanumeric))
            .unwrap_or("html")
    };

    let temp_path = std::env::temp_dir().join(format!("ingestr-url.{ext}"));
    let bytes = response.bytes().context("reading response body")?;
    fs::write(&temp_path, &bytes).context("writing fetched content to temp file")?;

    Ok(temp_path)
}

// ============================================================
// Clipboard support
// ============================================================

/// Read content from the system clipboard and save to a temp file.
fn read_clipboard() -> Result<PathBuf> {
    let mut clipboard = arboard::Clipboard::new().context("accessing clipboard")?;

    // Try to get text (which might be HTML or plain text)
    match clipboard.get_text() {
        Ok(text) if !text.trim().is_empty() => {
            let ext = detect_format_from_content(text.as_bytes());
            let temp_path = std::env::temp_dir().join(format!("ingestr-clipboard.{ext}"));
            fs::write(&temp_path, &text).context("writing clipboard to temp file")?;
            Ok(temp_path)
        }
        _ => {
            // Try image
            match clipboard.get_image() {
                Ok(img) => {
                    let temp_path = std::env::temp_dir().join("ingestr-clipboard.png");
                    // arboard gives us raw RGBA data; write as PNG
                    let mut png_data = Vec::new();
                    let mut encoder = io::Cursor::new(&mut png_data);
                    // Simple BMP-like approach: just write the raw bytes and let markitdown handle it
                    // Actually, we need a proper image format. Let's write raw RGBA and use a simple approach.
                    // For now, write a simple PPM which is easy to generate
                    write!(encoder, "P6\n{} {}\n255\n", img.width, img.height)?;
                    for pixel in img.bytes.chunks(4) {
                        encoder.write_all(&pixel[..3])?; // RGB, skip A
                    }
                    fs::write(&temp_path, &png_data).context("writing clipboard image")?;
                    // Rename to .ppm since that's what we wrote
                    let ppm_path = std::env::temp_dir().join("ingestr-clipboard.ppm");
                    fs::rename(&temp_path, &ppm_path).ok();
                    Ok(ppm_path)
                }
                Err(_) => bail!("clipboard is empty or contains unsupported content"),
            }
        }
    }
}

/// Statistics for batch conversion
#[derive(Debug, Clone, Default, Serialize)]
struct ConvertStats {
    total: usize,
    converted: usize,
    skipped: usize,
    failed: usize,
    errors: Vec<String>,
}

/// Processor that handles document conversion with fallback chains
struct DocumentProcessor {
    markitdown: MarkItDown,
    settings: ServiceSettings,
    vlm_enabled: bool,
    vlm_model: Option<String>,
    vlm_prompt: Option<String>,
    jobs: usize,
    /// Output file for streaming VLM results (pages written as they complete)
    vlm_output: Option<PathBuf>,
    ocr_enabled: bool,
    ocr_backend: OcrBackend,
    ocr_languages: Vec<String>,
    /// Render DPI used for OCR-ing scanned pages.
    ocr_page_dpi: u32,
    /// Directory where LiteParse writes extracted embedded images (e.g. figures
    /// / charts from a PPTX). `None` keeps image references in the Markdown
    /// without writing image files.
    image_output_dir: Option<PathBuf>,
}

impl DocumentProcessor {
    fn new(settings: ServiceSettings) -> Self {
        Self {
            markitdown: MarkItDown::new(),
            settings,
            vlm_enabled: false,
            vlm_model: None,
            vlm_prompt: None,
            jobs: 1,
            vlm_output: None,
            ocr_enabled: false,
            ocr_backend: OcrBackend::Ocrs,
            ocr_languages: vec!["eng".to_string()],
            ocr_page_dpi: 300,
            image_output_dir: None,
        }
    }

    fn with_vlm(
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

    fn with_ocr(
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

    fn with_image_output_dir(mut self, output_dir: Option<PathBuf>) -> Self {
        self.image_output_dir = output_dir;
        self
    }

    fn process(&self, input: &Path) -> Result<ConvertedDocument> {
        let extension = input
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_lowercase)
            .unwrap_or_default();

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
        if is_liteparse_extension(&extension)
            && let Ok(parsed) = self.process_pdf_with_liteparse(input)
            && !parsed.text_content.trim().is_empty()
        {
            return Ok(parsed);
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
    fn vlm_connection(&self) -> (String, String) {
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

    fn build_conversion_options(&self) -> Option<ConversionOptions> {
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

    fn process_with_vlm(&self, input: &Path, _extension: &str) -> Result<ConvertedDocument> {
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

    fn process_with_ocr(&self, input: &Path) -> Result<ConvertedDocument> {
        let text = run_ocr(input, self.ocr_backend, &self.ocr_languages)?;

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
    fn process_pdf_with_liteparse(&self, input: &Path) -> Result<ConvertedDocument> {
        // Office formats are converted via LibreOffice; make a missing install
        // a clear, actionable error instead of a cryptic failure.
        if is_office_extension(extension_of(input)) && !has_libreoffice() {
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
        let config = LiteParseConfig {
            output_format: OutputFormat::Markdown,
            ocr_enabled: self.ocr_enabled,
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
        let result = rt
            .block_on(LiteParse::new(config).parse(path_str))
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
    fn process_pdf_with_vlm(&self, input: &Path) -> Result<ConvertedDocument> {
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
    fn process_vlm_pages_parallel(
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
    fn process_pptx_with_vlm(&self, input: &Path) -> Result<ConvertedDocument> {
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

fn is_image_extension(ext: &str) -> bool {
    matches!(
        ext,
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "tiff" | "tif"
    )
}

fn is_pdf_extension(ext: &str) -> bool {
    ext == "pdf"
}

fn is_presentation_extension(ext: &str) -> bool {
    matches!(ext, "pptx" | "ppt" | "odp" | "key")
}

/// Whether LiteParse handles this format directly (PDF via PDFium, office
/// formats via LibreOffice conversion). Plain images stay on the markitdown /
/// OCR path.
fn is_liteparse_extension(ext: &str) -> bool {
    matches!(
        ext,
        "pdf" | "pptx" | "ppt" | "odp" | "key" | "docx" | "doc" | "odt" | "xlsx" | "xls" | "ods"
    )
}

/// Whether a format needs LibreOffice for conversion (office documents).
fn is_office_extension(ext: &str) -> bool {
    matches!(
        ext,
        "pptx" | "ppt" | "odp" | "key" | "docx" | "doc" | "odt" | "xlsx" | "xls" | "ods"
    )
}

/// Whether LibreOffice (`soffice`/`libreoffice`) is available on PATH.
fn has_libreoffice() -> bool {
    command_exists("soffice") || command_exists("libreoffice")
}

/// The lowercased file extension of a path, or an empty string.
fn extension_of(path: &Path) -> &str {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
}

/// Check if a PDF file is encrypted/password-protected.
/// Returns true if encrypted, false if not, or an error if the file cannot be read.
fn is_pdf_encrypted(path: &Path) -> Result<bool> {
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

fn base64_encode(data: &[u8]) -> String {
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
fn call_vlm_api(
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

const OCRS_DETECTION_MODEL_URL: &str =
    "https://ocrs-models.s3-accelerate.amazonaws.com/text-detection.rten";
const OCRS_RECOGNITION_MODEL_URL: &str =
    "https://ocrs-models.s3-accelerate.amazonaws.com/text-recognition.rten";

fn ocrs_cache_dir() -> Result<PathBuf> {
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

fn human_bytes(bytes: u64) -> String {
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

fn download_to_cache(url: &str, filename: &str) -> Result<PathBuf> {
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

fn run_ocrs_on_image(input: &Path, engine: &OcrEngine, show_progress: bool) -> Result<String> {
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

fn run_ocrs_on_pdf(input: &Path, engine: &OcrEngine) -> Result<String> {
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

fn run_ocrs(input: &Path, languages: &[String]) -> Result<String> {
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
fn run_ocr(input: &Path, backend: OcrBackend, languages: &[String]) -> Result<String> {
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
fn liteparse_runtime() -> Result<&'static Runtime> {
    static RT: OnceLock<Result<Runtime, String>> = OnceLock::new();
    RT.get_or_init(|| Runtime::new().map_err(|e| e.to_string()))
        .as_ref()
        .map_err(|e| anyhow!("failed to create liteparse tokio runtime: {e}"))
}

/// Whether a path component denotes a hidden file/dir. The special "." and
/// ".." components are not hidden (they are the current/parent directory), so
/// a relative input like "." still matches its own contents.
fn is_hidden_component(component: Option<&str>) -> bool {
    component.is_some_and(|s| s.starts_with('.') && s != "." && s != "..")
}

/// Map an ingestr OCR language to a Tesseract/liteparse language code.
fn map_ocr_lang(lang: &str) -> String {
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

fn handle_convert(ctx: &RuntimeContext, cmd: ConvertCommand) -> Result<()> {
    let mut settings = ctx.service_settings(&ServiceRunOpts {
        watch_dir: None,
        output_dir: None,
        index_dir: None,
        disable_index: true,
        once: true,
    })?;
    settings.index_enabled = false;

    // Clipboard input
    if cmd.clipboard {
        let temp_path = read_clipboard()?;
        let result = convert_single_input(ctx, &cmd, &settings, &temp_path, "clipboard")?;
        let _ = fs::remove_file(&temp_path);
        return result;
    }

    // Check if reading from stdin
    let is_stdin = cmd.input.is_none() || cmd.input.as_ref().is_some_and(|s| s == "-");

    if is_stdin {
        return handle_convert_stdin(ctx, &cmd, &settings);
    }

    // Handle batch conversion flag: --batch EXT
    if let Some(ref batch_ext) = cmd.batch {
        let ext = batch_ext.trim_start_matches('.'); // Allow "pptx" or ".pptx"
        if ext.is_empty() {
            bail!("--batch requires a file extension, e.g., --batch pptx");
        }
        let cmd = ConvertCommand {
            extensions: Some(vec![ext.to_string()]),
            recursive: true,
            in_place: true,
            input: Some(".".to_string()), // Use current directory as base
            ..cmd
        };
        return handle_convert_directory(ctx, &cmd, &settings, &env::current_dir()?);
    }

    let input_str = cmd
        .input
        .as_ref()
        .cloned()
        .ok_or_else(|| anyhow!("no input path provided"))?;

    // URL input
    if is_url(&input_str) {
        let temp_path = fetch_url(&input_str)?;
        let result = convert_single_input(ctx, &cmd, &settings, &temp_path, &input_str);
        let _ = fs::remove_file(&temp_path);
        return result?;
    }

    let input = expand_path(PathBuf::from(&input_str))?;

    // Check if input is a directory
    if input.is_dir() {
        return handle_convert_directory(ctx, &cmd, &settings, &input);
    }

    // Single file conversion
    if !input.is_file() {
        bail!("input path is not a file or directory: {}", input.display());
    }

    let source_label = input.display().to_string();
    convert_single_input(ctx, &cmd, &settings, &input, &source_label)?
}

/// Core single-file conversion with all the new features:
/// caching, cleaning, TOC, section extraction, token budget, frontmatter control.
fn convert_single_input(
    ctx: &RuntimeContext,
    cmd: &ConvertCommand,
    settings: &ServiceSettings,
    input: &Path,
    source_label: &str,
) -> Result<Result<()>> {
    // Check cache first (unless --no-cache or --toc which is always computed fresh)
    if !cmd.no_cache
        && !cmd.toc
        && input.exists()
        && let Ok(key) = cache_key(
            input,
            cmd.meta,
            cmd.raw,
            cmd.vlm,
            cmd.ocr,
            &cmd.section,
            &cmd.pages,
        )
        && let Ok(Some(cached)) = cache_get(&key)
    {
        debug!("cache hit for {source_label}");
        return Ok(output_final(ctx, cmd, cached, source_label));
    }

    let processor = DocumentProcessor::new(settings.clone())
        .with_vlm(
            cmd.vlm,
            cmd.vlm_model.clone(),
            cmd.vlm_prompt.clone(),
            cmd.jobs,
            cmd.output.clone(),
        )
        .with_ocr(cmd.ocr, cmd.ocr_backend, cmd.ocr_languages.clone())
        .with_image_output_dir(image_target_dir(cmd.output.as_deref()));

    let converted = processor.process(input)?;

    // Apply page filtering if --pages was specified
    let text_content = if let Some(ref page_spec) = cmd.pages {
        let pages = parse_page_range(page_spec)?;
        filter_pages(&converted.text_content, &pages)
    } else {
        converted.text_content.clone()
    };

    // Apply cleaning unless --raw
    let content = if cmd.raw {
        text_content
    } else {
        clean_markdown(&text_content)
    };

    // TOC mode: just show structure and exit
    if cmd.toc {
        let toc = extract_toc(&content);
        let total_tokens = estimate_tokens(&content);
        if ctx.common.json {
            let output = serde_json::json!({
                "source": source_label,
                "total_tokens": total_tokens,
                "sections": toc,
            });
            println!("{}", serde_json::to_string_pretty(&output)?);
        } else {
            println!("{}", format_toc(&toc, total_tokens));
        }
        return Ok(Ok(()));
    }

    // Section extraction
    let content = if let Some(ref selector) = cmd.section {
        extract_section(&content, selector)
            .ok_or_else(|| anyhow!("section '{selector}' not found"))?
    } else {
        content
    };

    // Token/char budget truncation
    let content = if let Some(max_tokens) = cmd.max_tokens {
        let max_chars = max_tokens * 4; // rough estimate: 4 chars per token
        let (truncated, notice) = truncate_at_boundary(&content, max_chars, cmd.offset);
        if let Some(notice) = notice {
            format!("{truncated}{notice}")
        } else {
            truncated
        }
    } else if let Some(max_chars) = cmd.max_chars {
        let (truncated, notice) = truncate_at_boundary(&content, max_chars, cmd.offset);
        if let Some(notice) = notice {
            format!("{truncated}{notice}")
        } else {
            truncated
        }
    } else if cmd.offset > 0 {
        if cmd.offset >= content.len() {
            String::new()
        } else {
            content[cmd.offset..].to_string()
        }
    } else {
        content
    };

    // Build final output
    let converted_at = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string());

    let source_modified = if input.exists() {
        fs::metadata(input)
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(system_time_to_rfc3339)
    } else {
        None
    };

    let final_output = if cmd.meta {
        let frontmatter = Frontmatter {
            source_path: source_label.to_string(),
            output_path: cmd
                .output
                .as_ref()
                .map_or_else(|| "-".to_string(), |p| p.display().to_string()),
            source_modified,
            title: converted.title.clone(),
            converted_at,
        };
        render_frontmatter_markdown(&frontmatter, &content)?
    } else {
        content
    };

    // Store in cache (for file inputs only, not URLs/clipboard)
    if !cmd.no_cache
        && input.exists()
        && cmd.section.is_none()
        && cmd.max_tokens.is_none()
        && cmd.max_chars.is_none()
        && cmd.offset == 0
        && let Ok(key) = cache_key(
            input,
            cmd.meta,
            cmd.raw,
            cmd.vlm,
            cmd.ocr,
            &cmd.section,
            &cmd.pages,
        )
    {
        let _ = cache_put(&key, &final_output);
    }

    // If VLM already streamed to file, skip normal output
    if converted.already_written {
        return Ok(Ok(()));
    }

    Ok(output_final(ctx, cmd, final_output, source_label))
}

/// Output the final converted content to stdout, file, or JSON/YAML.
fn output_final(
    ctx: &RuntimeContext,
    cmd: &ConvertCommand,
    content: String,
    source_label: &str,
) -> Result<()> {
    if let Some(ref output_path) = cmd.output {
        let output_path = expand_path(output_path.clone())?;
        if ctx.common.dry_run {
            info!("dry-run: would write to {}", output_path.display());
        } else {
            if let Some(parent) = output_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&output_path, &content)?;
            if !ctx.common.quiet {
                println!("Converted {} -> {}", source_label, output_path.display());
            }
        }
    } else if ctx.common.json {
        let result = serde_json::json!({
            "ok": true,
            "source": source_label,
            "tokens": estimate_tokens(&content),
            "content": content,
        });
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        print!("{content}");
    }
    Ok(())
}

fn handle_convert_stdin(
    ctx: &RuntimeContext,
    cmd: &ConvertCommand,
    settings: &ServiceSettings,
) -> Result<()> {
    let mut content = Vec::new();
    io::stdin()
        .read_to_end(&mut content)
        .context("reading from stdin")?;

    // Determine file extension from format hint
    let extension = cmd.from.unwrap_or(InputFormat::Auto).extension();

    // Create a temporary file with the appropriate extension
    let temp_dir = std::env::temp_dir();
    let temp_file = if let Some(ext) = extension {
        temp_dir.join(format!("ingestr-stdin.{ext}"))
    } else {
        // Try to auto-detect format from content
        let detected_ext = detect_format_from_content(&content);
        temp_dir.join(format!("ingestr-stdin.{detected_ext}"))
    };

    fs::write(&temp_file, &content).context("writing stdin content to temp file")?;

    let result = convert_single_input(ctx, cmd, settings, &temp_file, "stdin");
    let _ = fs::remove_file(&temp_file);
    result?
}

fn detect_format_from_content(content: &[u8]) -> &'static str {
    // Check for common file signatures
    if content.starts_with(b"%PDF") {
        return "pdf";
    }
    if content.starts_with(b"PK\x03\x04") {
        // Could be docx, xlsx, pptx - default to docx
        return "docx";
    }
    if content.starts_with(b"<!DOCTYPE html")
        || content.starts_with(b"<html")
        || content.starts_with(b"<HTML")
    {
        return "html";
    }
    if content.starts_with(b"<?xml") {
        return "xml";
    }
    if content.starts_with(b"{") || content.starts_with(b"[") {
        return "json";
    }

    // Default to text
    "txt"
}

fn handle_convert_directory(
    ctx: &RuntimeContext,
    cmd: &ConvertCommand,
    settings: &ServiceSettings,
    input_dir: &Path,
) -> Result<()> {
    let output_dir = cmd
        .output
        .as_ref()
        .map(|p| expand_path(p.clone()))
        .transpose()
        .context("expanding output path")?;

    // Determine if we should write alongside source files. Computed
    // independently of dry-run so --dry-run shows the real destination.
    let in_place = cmd.in_place || output_dir.is_none();

    // Collect files to process
    let walker = if cmd.recursive {
        WalkDir::new(input_dir)
    } else {
        WalkDir::new(input_dir).max_depth(1)
    };

    let extensions: Option<Vec<String>> = cmd
        .extensions
        .clone()
        .map(|exts| exts.iter().map(|e| e.to_lowercase()).collect());

    let files: Vec<PathBuf> = walker
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            if let Some(ref exts) = extensions {
                e.path()
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| exts.contains(&ext.to_lowercase()))
            } else {
                true
            }
        })
        .filter(|e| {
            // Skip hidden files if skip_hidden is set. Ignore the special "."
            // (current dir) and ".." components so a relative input like "."
            // still matches its contents.
            if settings.skip_hidden {
                !e.path()
                    .components()
                    .any(|c| is_hidden_component(c.as_os_str().to_str()))
            } else {
                true
            }
        })
        .map(|e| e.path().to_path_buf())
        .collect();

    if files.is_empty() {
        info!("no supported documents found to convert");
        if ctx.common.json {
            let output = serde_json::json!({
                "ok": true,
                "found": 0,
                "plans": [],
                "stats": { "total": 0, "converted": 0, "skipped": 0, "failed": 0, "errors": [] },
            });
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        return Ok(());
    }

    let total = files.len();
    if !ctx.common.quiet {
        eprintln!("Found {total} files to convert");
    }

    if ctx.common.dry_run {
        let mut plans: Vec<serde_json::Value> = Vec::with_capacity(files.len());
        for file in &files {
            let output_path = if in_place {
                let mut out = file.clone();
                out.set_extension("md");
                out.display().to_string()
            } else if let Some(ref out_dir) = output_dir {
                let relative = file.strip_prefix(input_dir).unwrap_or(file);
                let mut out = out_dir.join(relative);
                out.set_extension("md");
                out.display().to_string()
            } else {
                "-".to_string()
            };
            if ctx.common.json {
                plans.push(serde_json::json!({
                    "source": file.display().to_string(),
                    "output": output_path,
                }));
            } else {
                println!("Would convert {} -> {}", file.display(), output_path);
            }
        }
        if ctx.common.json {
            let output = serde_json::json!({ "ok": true, "found": total, "plans": plans });
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        return Ok(());
    }

    // Fail-fast demands deterministic, ordered execution, so force the
    // sequential path when requested.
    let parallel = if cmd.fail_fast {
        1
    } else {
        ctx.common.parallel.unwrap_or_else(|| {
            std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
        })
    };

    // Progress animation only on a TTY stderr (not when piped) and honoring
    // NO_COLOR / --no-color, so it never corrupts piped or machine output.
    let stderr_tty = io::stderr().is_terminal();
    let color_ok = !ctx.common.no_color
        && ctx.common.color != ColorOption::Never
        && env::var_os("NO_COLOR").is_none()
        && env::var("TERM").as_deref() != Ok("dumb");
    let show_progress =
        parallel == 1 && !ctx.common.quiet && !ctx.common.no_progress && stderr_tty && color_ok;

    let converted = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    let errors: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

    let results: Vec<Result<ConvertResult>> = if parallel > 1 {
        rayon::ThreadPoolBuilder::new()
            .num_threads(parallel)
            .build()
            .context("building thread pool")?
            .install(|| {
                files
                    .par_iter()
                    .map(|file| {
                        process_file_for_batch(
                            file,
                            input_dir,
                            output_dir.as_ref(),
                            settings,
                            cmd,
                            in_place,
                            &converted,
                            &failed,
                            &errors,
                        )
                    })
                    .collect()
            })
    } else {
        let mut results = Vec::with_capacity(files.len());
        for (idx, file) in files.iter().enumerate() {
            // Show progress before processing
            if show_progress {
                let num = idx + 1;
                let file_name = file
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown");
                eprint!("\r\x1b[K[{num}/{total}] Converting {file_name}...");
                let _ = io::stderr().flush();
            }

            let result = process_file_for_batch(
                file,
                input_dir,
                output_dir.as_ref(),
                settings,
                cmd,
                in_place,
                &converted,
                &failed,
                &errors,
            );

            // Show result indicator
            if show_progress {
                match &result {
                    Ok(_) => eprint!(" ✓"),
                    Err(_) => eprint!(" ✗"),
                }
            }

            // Fail-fast: a genuine conversion failure aborts the batch with a
            // non-zero exit so scripts stop on the first bad input.
            if cmd.fail_fast
                && let Err(err) = result
            {
                if show_progress {
                    eprintln!();
                }
                return Err(err.context(format!("conversion failed at {}", file.display())));
            }

            results.push(result);
        }
        results
    };

    let stats = ConvertStats {
        total,
        converted: converted.load(Ordering::Relaxed),
        skipped: 0,
        failed: failed.load(Ordering::Relaxed),
        errors: errors.into_inner().unwrap_or_default(),
    };

    // Output results
    if ctx.common.json {
        let successful_results: Vec<_> = results.into_iter().filter_map(Result::ok).collect();
        let output = serde_json::json!({
            "ok": stats.failed == 0,
            "stats": stats,
            "results": successful_results,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        // Add newline after progress indicator if used
        if show_progress {
            eprintln!();
        }
        if ctx.common.quiet {
            // Quiet mode: only summarize failures.
            if stats.failed > 0 {
                eprintln!("ingestr: {}/{} files failed", stats.failed, stats.total);
            }
        } else {
            println!("\nConversion complete:");
            println!("  Total files: {}", stats.total);
            println!("  Converted:   {}", stats.converted);
            println!("  Failed:      {}", stats.failed);
            if !stats.errors.is_empty() {
                println!("\nErrors:");
                for err in &stats.errors {
                    println!("  - {err}");
                }
            }
        }
    }

    // Non-zero exit code when any file failed (unless fail-fast already bailed).
    if stats.failed > 0 {
        bail!(
            "{} of {} file(s) failed to convert",
            stats.failed,
            stats.total
        );
    }

    Ok(())
}

fn process_file_for_batch(
    file: &Path,
    input_dir: &Path,
    output_dir: Option<&PathBuf>,
    settings: &ServiceSettings,
    cmd: &ConvertCommand,
    in_place: bool,
    converted: &AtomicUsize,
    failed: &AtomicUsize,
    errors: &std::sync::Mutex<Vec<String>>,
) -> Result<ConvertResult> {
    let processor = DocumentProcessor::new(settings.clone())
        .with_vlm(
            cmd.vlm,
            cmd.vlm_model.clone(),
            cmd.vlm_prompt.clone(),
            cmd.jobs,
            cmd.output.clone(),
        )
        .with_ocr(cmd.ocr, cmd.ocr_backend, cmd.ocr_languages.clone())
        .with_image_output_dir(batch_image_dir(output_dir));

    match processor.process(file) {
        Ok(doc) => {
            // Determine output path: in-place (source dir), output dir, or stdout
            let output_path = if in_place {
                let mut out = file.to_path_buf();
                out.set_extension("md");
                out
            } else if let Some(out_dir) = output_dir {
                let relative = file.strip_prefix(input_dir).unwrap_or(file);
                let mut out = out_dir.join(relative);
                out.set_extension("md");
                out
            } else {
                PathBuf::from("-")
            };

            let converted_at = OffsetDateTime::now_utc()
                .format(&Rfc3339)
                .unwrap_or_else(|_| "unknown".to_string());

            let source_modified = fs::metadata(file)
                .ok()
                .and_then(|meta| meta.modified().ok())
                .and_then(system_time_to_rfc3339);

            // Apply cleaning unless --raw
            let content = if cmd.raw {
                doc.text_content.clone()
            } else {
                clean_markdown(&doc.text_content)
            };

            // Build output: frontmatter only if --meta
            let markdown = if cmd.meta {
                let frontmatter = Frontmatter {
                    source_path: file.display().to_string(),
                    output_path: output_path.display().to_string(),
                    source_modified: source_modified.clone(),
                    title: doc.title.clone(),
                    converted_at: converted_at.clone(),
                };
                render_frontmatter_markdown(&frontmatter, &content)?
            } else {
                content
            };

            // Write output file if in_place or output_dir is specified
            if in_place || output_dir.is_some() {
                if let Some(parent) = output_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&output_path, &markdown)?;
            }

            converted.fetch_add(1, Ordering::Relaxed);
            info!("converted {} -> {}", file.display(), output_path.display());

            Ok(ConvertResult {
                source_path: file.display().to_string(),
                output_path: output_path.display().to_string(),
                source_modified,
                title: doc.title,
                converted_at,
                markdown,
            })
        }
        Err(e) => {
            failed.fetch_add(1, Ordering::Relaxed);
            let err_msg = format!("{}: {}", file.display(), e);
            if let Ok(mut errors) = errors.lock() {
                errors.push(err_msg);
            }
            error!("failed to convert {}: {}", file.display(), e);
            Err(e)
        }
    }
}

fn handle_init(ctx: &RuntimeContext, cmd: InitCommand) -> Result<()> {
    if ctx.paths.active_config.exists() && !(cmd.force || ctx.common.assume_yes) {
        return Err(anyhow!(
            "config already exists at {} (use --force to overwrite)",
            ctx.paths.active_config.display()
        ));
    }

    if ctx.common.dry_run {
        info!(
            "dry-run: would write default config to {}",
            ctx.paths.active_config.display()
        );
        return Ok(());
    }

    write_default_config(&ctx.paths.active_config)
}

fn handle_config(ctx: &RuntimeContext, command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::Show => {
            if ctx.common.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&ctx.config)
                        .context("serializing config to JSON")?
                );
            } else {
                println!("{:#?}", ctx.config);
            }
            Ok(())
        }
        ConfigCommand::Path => {
            println!("{}", ctx.paths.active_config.display());
            Ok(())
        }
        ConfigCommand::Reset => {
            if ctx.common.dry_run {
                info!(
                    "dry-run: would reset config at {}",
                    ctx.paths.active_config.display()
                );
                return Ok(());
            }
            write_default_config(&ctx.paths.active_config)
        }
    }
}

fn handle_completions(shell: Shell) -> Result<()> {
    let mut cmd = Cli::command();
    clap_complete::generate(shell, &mut cmd, APP_NAME, &mut io::stdout());
    Ok(())
}

/// Whether an external command is present on PATH. Checks for the binary file
/// rather than running it (some tools like poppler's pdftoppm exit non-zero on
/// `--version`, which would misreport them as missing).
fn command_exists(name: &str) -> bool {
    let Some(path) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&path)
        .any(|dir| dir.join(name).is_file() || dir.join(format!("{name}.exe")).is_file())
}

/// Report which external tools are installed so users know what conversions
/// and OCR backends are available without a dependency being silently missing.
fn handle_doctor() -> Result<()> {
    let checks: Vec<(&str, bool, &str, &str)> = vec![
        (
            "LibreOffice (soffice)",
            command_exists("soffice") || command_exists("libreoffice"),
            "Required for PPTX/DOCX/XLSX conversion",
            "Ubuntu: apt-get install libreoffice | macOS: brew install --cask libreoffice",
        ),
        (
            "Poppler (pdftoppm)",
            command_exists("pdftoppm"),
            "Renders PDF pages to images (VLM/OCR paths)",
            "Ubuntu: apt-get install poppler-utils",
        ),
        (
            "Poppler (pdftotext)",
            command_exists("pdftotext"),
            "Native PDF text extraction fallback",
            "Ubuntu: apt-get install poppler-utils",
        ),
        (
            "Tesseract",
            command_exists("tesseract"),
            "OCR backend (tesseract); LiteParse bundles its own",
            "Ubuntu: apt-get install tesseract-ocr",
        ),
        (
            "ImageMagick (convert)",
            command_exists("convert") || command_exists("magick"),
            "Image -> PDF conversion (some pipelines)",
            "Ubuntu: apt-get install imagemagick",
        ),
        (
            "Python3",
            command_exists("python3"),
            "Required for surya / easyocr backends",
            "Ubuntu: apt-get install python3",
        ),
    ];

    println!("ingestr doctor — installed tooling\n");
    for (name, ok, purpose, install) in checks {
        let mark = if ok { "✓" } else { "✗" };
        println!("{mark}  {name}");
        println!("    {purpose}");
        if !ok {
            println!("    install: {install}");
        }
    }
    println!();
    println!("PDF conversion (LiteParse/PDFium) and search need no external tools.");
    println!("LiteParse PDFium and Tesseract are bundled; LibreOffice is not.");
    Ok(())
}

fn load_or_init_config(paths: &AppPaths, common: &CommonOpts) -> Result<AppConfig> {
    if !paths.active_config.exists() {
        if common.dry_run {
            info!(
                "dry-run: would create default config at {}",
                paths.active_config.display()
            );
        } else {
            write_default_config(&paths.active_config)?;
        }
    }

    let env_prefix = env_prefix();
    let mut builder = Config::builder()
        .set_default("profile", "default")?
        .set_default("logging.level", "info")?
        .set_default("runtime.parallelism", default_parallelism() as i64)?
        .set_default("runtime.timeout", 60_i64)?
        .set_default("runtime.fail_fast", true)?
        .set_default("watcher.watch_dir", default_watch_dir_string())?
        .set_default("watcher.debounce_ms", 750_i64)?
        .set_default("watcher.skip_hidden", true)?
        .set_default("output.markdown_dir", default_output_dir_string())?
        .set_default("index.enabled", true)?;

    if let Some(ref dir) = paths.global_config.parent() {
        fs::create_dir_all(dir)
            .with_context(|| format!("creating global config directory {}", dir.display()))?;
    }

    builder = builder.add_source(
        File::from(paths.global_config.as_path())
            .format(FileFormat::Toml)
            .required(false),
    );

    builder = builder.add_source(
        File::from(paths.local_config.as_path())
            .format(FileFormat::Toml)
            .required(false),
    );

    builder = builder.add_source(Environment::with_prefix(env_prefix.as_str()).separator("__"));

    if paths.config_override {
        builder = builder.add_source(
            File::from(paths.active_config.as_path())
                .format(FileFormat::Toml)
                .required(false),
        );
    }

    let built = builder.build()?;
    let mut config: AppConfig = built.try_deserialize()?;

    if let Some(ref file) = config.logging.file {
        let expanded = expand_str_path(file)?;
        config.logging.file = Some(expanded.display().to_string());
    }

    Ok(config)
}

fn write_default_config(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating config directory {parent:?}"))?;
    }

    let config = AppConfig::default();
    let toml = toml::to_string_pretty(&config).context("serializing default config to TOML")?;
    let mut body = default_config_header(path)?;
    body.push_str(&toml);
    fs::write(path, body).with_context(|| format!("writing config file to {}", path.display()))
}

fn default_config_header(path: &Path) -> Result<String> {
    let mut buffer = String::new();
    buffer.push_str("# Configuration for ");
    buffer.push_str(APP_NAME);
    buffer.push('\n');
    buffer.push_str("# File: ");
    buffer.push_str(&path.display().to_string());
    buffer.push('\n');
    buffer.push('\n');
    Ok(buffer)
}

fn expand_path(path: PathBuf) -> Result<PathBuf> {
    if let Some(text) = path.to_str() {
        expand_str_path(text)
    } else {
        Ok(path)
    }
}

/// Resolve the directory where extracted embedded images should be written for
/// a single-file `--output`. If `output` is a directory (or trailing slash) use
/// it as-is; if it is a file, write images next to it; if unset, write none.
fn image_target_dir(output: Option<&Path>) -> Option<PathBuf> {
    let output = output?;
    let expanded = expand_path(output.to_path_buf()).ok()?;
    // A trailing separator marks an explicit directory even if it does not yet
    // exist (so `--output out/` writes images into out/).
    let explicit_dir = output.to_string_lossy().ends_with(['/', '\\']);
    if expanded.is_dir() || explicit_dir {
        Some(expanded)
    } else {
        expanded.parent().map(Path::to_path_buf)
    }
}

/// Resolve the directory where extracted embedded images are written in batch
/// (directory) mode: the batch output dir if present, otherwise alongside the
/// source (in-place).
fn batch_image_dir(output_dir: Option<&PathBuf>) -> Option<PathBuf> {
    output_dir.cloned()
}

fn expand_str_path(text: &str) -> Result<PathBuf> {
    let expanded = shellexpand::full(text).context("expanding path")?;
    Ok(PathBuf::from(expanded.to_string()))
}

fn default_config_dir() -> Result<PathBuf> {
    if let Some(dir) = env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        let mut path = PathBuf::from(dir);
        path.push(CONFIG_DIR_NAME);
        return Ok(path);
    }

    if let Some(mut dir) = dirs::config_dir() {
        dir.push(CONFIG_DIR_NAME);
        return Ok(dir);
    }

    dirs::home_dir()
        .map(|home| home.join(".config").join(CONFIG_DIR_NAME))
        .ok_or_else(|| anyhow!("unable to determine configuration directory"))
}

fn default_data_dir() -> Result<PathBuf> {
    if let Some(dir) = env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir).join(APP_NAME));
    }

    if let Some(mut dir) = dirs::data_dir() {
        dir.push(APP_NAME);
        return Ok(dir);
    }

    dirs::home_dir()
        .map(|home| home.join(".local").join("share").join(APP_NAME))
        .ok_or_else(|| anyhow!("unable to determine data directory"))
}

fn default_state_dir() -> Result<PathBuf> {
    if let Some(dir) = env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir).join(APP_NAME));
    }

    if let Some(mut dir) = dirs::state_dir() {
        dir.push(APP_NAME);
        return Ok(dir);
    }

    dirs::home_dir()
        .map(|home| home.join(".local").join("state").join(APP_NAME))
        .ok_or_else(|| anyhow!("unable to determine state directory"))
}

fn env_prefix() -> String {
    APP_NAME
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn default_parallelism() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
}

#[derive(Debug, Clone)]
struct ServiceSettings {
    watch_dir: PathBuf,
    output_dir: PathBuf,
    index_dir: PathBuf,
    index_enabled: bool,
    debounce: Duration,
    skip_hidden: bool,
    data_dir: PathBuf,
    state_dir: PathBuf,
    llm_enabled: bool,
    llm_client: String,
    llm_model: String,
    llm_base_url: Option<String>,
    llm_api_key: Option<String>,
    processors: ProcessorsConfig,
}

impl ResolvedDirectories {
    fn from_config(cfg: &AppConfig, paths: &AppPaths) -> Result<Self> {
        let watch_dir = expand_str_path(&cfg.watcher.watch_dir)?;
        let output_dir = expand_str_path(&cfg.output.markdown_dir)?;
        let index_dir = if let Some(ref explicit) = cfg.index.index_dir {
            expand_str_path(explicit)?
        } else {
            paths.data_dir.join("index")
        };

        Ok(Self {
            watch_dir,
            output_dir,
            index_dir,
        })
    }
}

struct ConversionService {
    settings: ServiceSettings,
    markitdown: MarkItDown,
    indexer: Option<SearchIndex>,
}

impl ConversionService {
    fn new(settings: ServiceSettings) -> Result<Self> {
        let indexer = if settings.index_enabled {
            Some(SearchIndex::open(&settings.index_dir, true)?)
        } else {
            None
        };

        Ok(Self {
            settings,
            markitdown: MarkItDown::new(),
            indexer,
        })
    }

    fn run(&mut self) -> Result<()> {
        self.process_existing()?;
        self.flush_index()?;

        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_flag = shutdown.clone();
        ctrlc::set_handler(move || {
            shutdown_flag.store(true, Ordering::SeqCst);
        })
        .context("installing ctrl+c handler")?;

        let (tx, rx) = mpsc::channel();
        let mut watcher = self.build_watcher(tx)?;

        watcher
            .watch(self.settings.watch_dir.as_path(), RecursiveMode::Recursive)
            .with_context(|| format!("watching directory {}", self.settings.watch_dir.display()))?;

        info!("watching for changes (press Ctrl+C to stop)");
        while !shutdown.load(Ordering::SeqCst) {
            match rx.recv_timeout(self.settings.debounce) {
                Ok(Ok(event)) => {
                    self.handle_event(event)?;
                    self.flush_index()?;
                }
                Ok(Err(err)) => warn!("watch error: {err}"),
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }

        self.flush_index()
    }

    fn build_watcher(
        &self,
        tx: mpsc::Sender<Result<Event, notify::Error>>,
    ) -> Result<RecommendedWatcher> {
        notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })
        .context("creating filesystem watcher")
    }

    fn process_existing(&mut self) -> Result<()> {
        for entry in WalkDir::new(&self.settings.watch_dir)
            .into_iter()
            .filter_map(Result::ok)
        {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if self.should_skip(path) {
                continue;
            }
            self.process_path(path)?;
        }

        // Also index any existing markdown outputs (so prior conversions are searchable
        // even if the source files haven't changed).
        if self.settings.index_enabled {
            self.index_existing_outputs()?;
        }
        Ok(())
    }

    fn handle_event(&mut self, event: Event) -> Result<()> {
        let kind = event.kind;
        for path in event.paths {
            if !path.is_file() {
                continue;
            }
            if self.should_skip(&path) {
                continue;
            }
            match kind {
                EventKind::Create(_) | EventKind::Modify(_) => {
                    self.process_path(&path)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn should_skip(&self, path: &Path) -> bool {
        if self.settings.watch_dir != self.settings.output_dir
            && path.starts_with(&self.settings.output_dir)
        {
            return true;
        }

        if !self.settings.skip_hidden {
            return false;
        }

        path.components().any(|component| {
            component
                .as_os_str()
                .to_str()
                .is_some_and(|s| s.starts_with('.'))
        })
    }

    fn process_path(&mut self, path: &Path) -> Result<()> {
        let output_path = self.output_path_for(path)?;
        let is_markdown = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("md"));

        if is_markdown && output_path == path {
            if let Some(indexer) = self.indexer.as_mut() {
                Self::index_existing_output(indexer, &output_path, path)?;
            }
            return Ok(());
        }

        if !self.needs_processing(path, &output_path)? {
            debug!("skipping {} (already up to date)", path.display());
            if let Some(indexer) = self.indexer.as_mut() {
                Self::index_existing_output(indexer, &output_path, path)?;
            }
            return Ok(());
        }

        let conversion_opts = if self.settings.llm_enabled {
            Some(ConversionOptions {
                file_extension: None,
                url: None,
                llm_client: Some(self.settings.llm_client.clone()),
                llm_model: Some(self.settings.llm_model.clone()),
            })
        } else {
            None
        };

        let path_str = path
            .to_str()
            .ok_or_else(|| anyhow!("invalid path encoding for {}", path.display()))?;
        let context = format!("converting {}", path.display());
        let converted = if let Some(result) = safe_markitdown_convert(&context, || {
            self.markitdown.convert(path_str, conversion_opts)
        }) {
            result
        } else {
            warn!(
                "no converter available or converter failed for {}",
                path.display()
            );
            return Ok(());
        };

        let converted_at = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_else(|_| "unknown".to_string());

        let source_modified = fs::metadata(path)
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(system_time_to_rfc3339);

        let frontmatter = Frontmatter {
            source_path: path.display().to_string(),
            output_path: output_path.display().to_string(),
            source_modified,
            title: converted.title.clone(),
            converted_at: converted_at.clone(),
        };

        // Frontmatter is serialized as JSON (valid YAML subset) for standard `---` frontmatter
        // compatibility, without a YAML dependency.
        let json = serde_json::to_string(&frontmatter).context("serializing frontmatter")?;
        let mut body = String::new();
        body.push_str("---\n");
        body.push_str(&json);
        body.push_str("\n---\n\n");
        body.push_str(&converted.text_content);

        if self.settings.data_dir == self.settings.output_dir {
            warn!(
                "output directory {} overlaps data directory; consider separating them",
                self.settings.output_dir.display()
            );
        }

        if self.settings.state_dir == self.settings.output_dir {
            warn!(
                "output directory {} overlaps state directory; consider separating them",
                self.settings.output_dir.display()
            );
        }

        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating output directory {}", parent.display()))?;
        }

        fs::write(&output_path, body)
            .with_context(|| format!("writing markdown to {}", output_path.display()))?;
        info!("converted {} -> {}", path.display(), output_path.display());

        if let Some(indexer) = self.indexer.as_mut() {
            indexer.index_document(&IndexedDocument {
                source_path: path.display().to_string(),
                output_path: output_path.display().to_string(),
                title: converted.title.clone(),
                content: converted.text_content.clone(),
                converted_at: Some(converted_at),
            })?;
        }

        Ok(())
    }

    fn needs_processing(&self, source: &Path, output: &Path) -> Result<bool> {
        if !output.exists() {
            return Ok(true);
        }

        let source_meta = fs::metadata(source)?;
        let output_meta = fs::metadata(output)?;
        let source_time = source_meta.modified().ok();
        let output_time = output_meta.modified().ok();

        Ok(match (source_time, output_time) {
            (Some(src), Some(out)) => src > out,
            _ => true,
        })
    }

    fn output_path_for(&self, source: &Path) -> Result<PathBuf> {
        let relative = source
            .strip_prefix(&self.settings.watch_dir)
            .unwrap_or(source);
        let mut output = self.settings.output_dir.join(relative);
        output.set_extension("md");
        Ok(output)
    }

    fn flush_index(&mut self) -> Result<()> {
        if let Some(indexer) = self.indexer.as_mut() {
            indexer.commit()?;
        }
        Ok(())
    }

    fn index_existing_output(
        indexer: &mut SearchIndex,
        output_path: &Path,
        source_path: &Path,
    ) -> Result<()> {
        let body = match fs::read_to_string(output_path) {
            Ok(b) => b,
            Err(err) => {
                warn!(
                    "unable to read {} for indexing: {err}",
                    output_path.display()
                );
                return Ok(());
            }
        };

        let (frontmatter, content) = parse_frontmatter(&body);
        let source_path_str = frontmatter.as_ref().map_or_else(
            || source_path.display().to_string(),
            |fm| fm.source_path.clone(),
        );

        let indexed = IndexedDocument {
            source_path: source_path_str,
            output_path: output_path.display().to_string(),
            title: frontmatter.as_ref().and_then(|fm| fm.title.clone()),
            content: content.to_string(),
            converted_at: frontmatter.as_ref().map(|fm| fm.converted_at.clone()),
        };

        indexer.index_document(&indexed)
    }

    fn index_existing_outputs(&mut self) -> Result<()> {
        let Some(indexer) = self.indexer.as_mut() else {
            return Ok(());
        };

        for entry in WalkDir::new(&self.settings.output_dir)
            .into_iter()
            .filter_map(Result::ok)
        {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let is_markdown = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("md"));
            if !is_markdown {
                continue;
            }
            let source_path = path; // will be overwritten by frontmatter if present
            let _ = Self::index_existing_output(indexer, path, source_path);
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Frontmatter {
    source_path: String,
    output_path: String,
    source_modified: Option<String>,
    title: Option<String>,
    converted_at: String,
}

fn system_time_to_rfc3339(time: SystemTime) -> Option<String> {
    let datetime: OffsetDateTime = time.into();
    datetime.format(&Rfc3339).ok()
}

fn parse_frontmatter(body: &str) -> (Option<Frontmatter>, &str) {
    if let Some(rest) = body.strip_prefix("---\n")
        && let Some(idx) = rest.find("\n---\n")
    {
        let (front, content) = rest.split_at(idx);
        let content = &content["\n---\n".len()..];
        if let Ok(parsed) = serde_json::from_str::<Frontmatter>(front) {
            return (Some(parsed), content);
        }
    }
    (None, body)
}

fn default_watch_dir_string() -> String {
    dirs::home_dir().map_or_else(
        || "~/Documents".to_string(),
        |home| home.join("Documents").display().to_string(),
    )
}

fn default_output_dir_string() -> String {
    dirs::home_dir().map_or_else(
        || "~/markdown".to_string(),
        |home| home.join("markdown").display().to_string(),
    )
}

impl fmt::Display for AppPaths {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "config: {}, data: {}, state: {}",
            self.active_config.display(),
            self.data_dir.display(),
            self.state_dir.display()
        )
    }
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
        std::env::temp_dir().join(format!("ingestr-test-{nanos}"))
    }

    fn create_test_settings() -> ServiceSettings {
        ServiceSettings {
            watch_dir: PathBuf::from("/tmp"),
            output_dir: PathBuf::from("/tmp/output"),
            index_dir: PathBuf::from("/tmp/index"),
            index_enabled: false,
            debounce: Duration::from_millis(50),
            skip_hidden: true,
            data_dir: PathBuf::from("/tmp/data"),
            state_dir: PathBuf::from("/tmp/state"),
            llm_enabled: false,
            llm_client: "openai".to_string(),
            llm_model: "gpt-4o".to_string(),
            llm_base_url: None,
            llm_api_key: None,
            processors: ProcessorsConfig::default(),
        }
    }

    #[test]
    fn convert_single_file_renders_frontmatter_and_content() -> Result<()> {
        let dir = unique_temp_dir();
        fs::create_dir_all(&dir)?;
        let input = dir.join("sample.txt");
        fs::write(&input, "Hello world")?;

        let markitdown = MarkItDown::new();
        let converted = convert_single_file(&markitdown, &input, None)?;

        let frontmatter = Frontmatter {
            source_path: input.display().to_string(),
            output_path: "-".to_string(),
            source_modified: None,
            title: converted.title.clone(),
            converted_at: "2025-01-01T00:00:00Z".to_string(),
        };

        let markdown = render_frontmatter_markdown(&frontmatter, &converted.text_content)?;
        let (parsed, body) = parse_frontmatter(&markdown);

        let parsed = parsed.expect("frontmatter parsed");
        assert_eq!(parsed.source_path, input.display().to_string());
        assert_eq!(parsed.output_path, "-");
        assert!(body.contains("Hello world"));

        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn document_processor_converts_text_file() -> Result<()> {
        let dir = unique_temp_dir();
        fs::create_dir_all(&dir)?;
        let input = dir.join("test.txt");
        fs::write(&input, "Test content for processor")?;

        let settings = create_test_settings();
        let processor = DocumentProcessor::new(settings);
        let result = processor.process(&input)?;

        assert!(result.text_content.contains("Test content for processor"));

        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn document_processor_with_vlm_disabled_skips_vlm() -> Result<()> {
        let dir = unique_temp_dir();
        fs::create_dir_all(&dir)?;
        let input = dir.join("test.txt");
        fs::write(&input, "Plain text")?;

        let settings = create_test_settings();
        let processor = DocumentProcessor::new(settings).with_vlm(false, None, None, 1, None);

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

        let settings = create_test_settings();
        let processor =
            DocumentProcessor::new(settings).with_ocr(false, OcrBackend::Tesseract, None);

        assert!(!processor.ocr_enabled);

        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn detect_format_from_content_identifies_pdf() {
        let content = b"%PDF-1.4 rest of content";
        assert_eq!(detect_format_from_content(content), "pdf");
    }

    #[test]
    fn detect_format_from_content_identifies_html() {
        let content = b"<!DOCTYPE html><html>test</html>";
        assert_eq!(detect_format_from_content(content), "html");

        let content2 = b"<html><body>test</body></html>";
        assert_eq!(detect_format_from_content(content2), "html");
    }

    #[test]
    fn detect_format_from_content_identifies_docx() {
        let content = b"PK\x03\x04rest";
        assert_eq!(detect_format_from_content(content), "docx");
    }

    #[test]
    fn detect_format_from_content_identifies_json() {
        let content = b"{\"key\": \"value\"}";
        assert_eq!(detect_format_from_content(content), "json");

        let content2 = b"[1, 2, 3]";
        assert_eq!(detect_format_from_content(content2), "json");
    }

    #[test]
    fn detect_format_from_content_identifies_xml() {
        let content = b"<?xml version=\"1.0\"?><root></root>";
        assert_eq!(detect_format_from_content(content), "xml");
    }

    #[test]
    fn detect_format_from_content_defaults_to_txt() {
        let content = b"Just some plain text";
        assert_eq!(detect_format_from_content(content), "txt");
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
    fn base64_encode_works() {
        let data = b"Hello, World!";
        let encoded = base64_encode(data);
        assert_eq!(encoded, "SGVsbG8sIFdvcmxkIQ==");
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
    fn input_format_extension() {
        assert_eq!(InputFormat::Html.extension(), Some("html"));
        assert_eq!(InputFormat::Pdf.extension(), Some("pdf"));
        assert_eq!(InputFormat::Docx.extension(), Some("docx"));
        assert_eq!(InputFormat::Auto.extension(), None);
    }

    #[test]
    fn ocr_backend_display() {
        assert_eq!(OcrBackend::Tesseract.to_string(), "tesseract");
        assert_eq!(OcrBackend::Ocrs.to_string(), "ocrs");
        assert_eq!(OcrBackend::Surya.to_string(), "surya");
        assert_eq!(OcrBackend::Easyocr.to_string(), "easyocr");
    }

    #[test]
    fn processors_config_default() {
        let config = ProcessorsConfig::default();
        assert_eq!(config.pipeline, vec!["markitdown"]);
        assert!(config.routing.is_empty());
        assert!(!config.vlm.enabled);
        assert!(!config.ocr.enabled);
    }

    #[test]
    fn vlm_config_default_prompts() {
        let config = VlmConfig::default();
        assert!(config.prompts.contains_key("default"));
        assert!(config.prompts.contains_key("diagram"));
        assert!(config.prompts.contains_key("screenshot"));
    }

    #[test]
    fn ocr_config_default() {
        let config = OcrConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.backend, OcrBackend::Ocrs);
        assert_eq!(config.languages, vec!["eng"]);
    }

    #[test]
    fn convert_stats_default() {
        let stats = ConvertStats::default();
        assert_eq!(stats.total, 0);
        assert_eq!(stats.converted, 0);
        assert_eq!(stats.skipped, 0);
        assert_eq!(stats.failed, 0);
        assert!(stats.errors.is_empty());
    }

    #[test]
    fn batch_convert_collects_files_by_extension() -> Result<()> {
        let dir = unique_temp_dir();
        fs::create_dir_all(&dir)?;

        // Create test files
        fs::write(dir.join("doc1.txt"), "text 1")?;
        fs::write(dir.join("doc2.txt"), "text 2")?;
        fs::write(dir.join("doc3.pdf"), "pdf content")?;
        fs::write(dir.join("doc4.html"), "<html>test</html>")?;

        let extensions = Some(vec!["txt".to_string()]);
        let files: Vec<PathBuf> = WalkDir::new(&dir)
            .max_depth(1)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
            .filter(|e| {
                if let Some(ref exts) = extensions {
                    e.path()
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .map(|ext| exts.contains(&ext.to_lowercase()))
                        .unwrap_or(false)
                } else {
                    true
                }
            })
            .map(|e| e.path().to_path_buf())
            .collect();

        assert_eq!(files.len(), 2);
        assert!(
            files
                .iter()
                .all(|f| f.extension().map(|e| e == "txt").unwrap_or(false))
        );

        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn batch_convert_skips_hidden_files() -> Result<()> {
        let dir = unique_temp_dir();
        fs::create_dir_all(&dir)?;

        // Create test files
        fs::write(dir.join("visible.txt"), "visible")?;
        fs::write(dir.join(".hidden.txt"), "hidden")?;

        let skip_hidden = true;
        let files: Vec<PathBuf> = WalkDir::new(&dir)
            .max_depth(1)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
            .filter(|e| {
                if skip_hidden {
                    !e.path().components().any(|c| {
                        c.as_os_str()
                            .to_str()
                            .map(|s| s.starts_with('.'))
                            .unwrap_or(false)
                    })
                } else {
                    true
                }
            })
            .map(|e| e.path().to_path_buf())
            .collect();

        assert_eq!(files.len(), 1);
        assert!(
            files[0]
                .file_name()
                .map(|n| n == "visible.txt")
                .unwrap_or(false)
        );

        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn frontmatter_serialization_roundtrip() -> Result<()> {
        let frontmatter = Frontmatter {
            source_path: "/test/input.txt".to_string(),
            output_path: "/test/output.md".to_string(),
            source_modified: Some("2025-01-01T00:00:00Z".to_string()),
            title: Some("Test Title".to_string()),
            converted_at: "2025-01-01T00:00:01Z".to_string(),
        };

        let content = "Test content here.";
        let markdown = render_frontmatter_markdown(&frontmatter, content)?;
        let (parsed, body) = parse_frontmatter(&markdown);

        let parsed = parsed.expect("frontmatter should parse");
        assert_eq!(parsed.source_path, frontmatter.source_path);
        assert_eq!(parsed.output_path, frontmatter.output_path);
        assert_eq!(parsed.title, frontmatter.title);
        assert!(body.contains(content));

        Ok(())
    }

    #[test]
    fn parse_frontmatter_handles_missing() {
        let content = "No frontmatter here";
        let (fm, body) = parse_frontmatter(content);
        assert!(fm.is_none());
        assert_eq!(body, content);
    }

    #[test]
    fn parse_frontmatter_handles_malformed() {
        let content = "---\ninvalid json here\n---\nBody content";
        let (fm, _body) = parse_frontmatter(content);
        // Should return None for malformed frontmatter
        assert!(fm.is_none());
    }

    #[test]
    fn convert_result_json_serialization() -> Result<()> {
        let result = ConvertResult {
            source_path: "/test.txt".to_string(),
            output_path: "/test.md".to_string(),
            source_modified: Some("2025-01-01T00:00:00Z".to_string()),
            title: Some("Title".to_string()),
            converted_at: "2025-01-01T00:00:01Z".to_string(),
            markdown: "# Title\n\nContent".to_string(),
        };

        let json = serde_json::to_string(&result)?;
        assert!(json.contains("source_path"));
        assert!(json.contains("output_path"));
        assert!(json.contains("markdown"));

        let parsed: ConvertResult = serde_json::from_str(&json)?;
        assert_eq!(parsed.source_path, result.source_path);

        Ok(())
    }

    // ===== New feature tests =====

    #[test]
    fn clean_markdown_removes_page_numbers() {
        let input = "Some content\n\nPage 3 of 12\n\nMore content\n\n5\n\nEnd";
        let cleaned = clean_markdown(input);
        assert!(!cleaned.contains("Page 3 of 12"));
        assert!(!cleaned.contains("\n5\n"));
        assert!(cleaned.contains("Some content"));
        assert!(cleaned.contains("More content"));
    }

    #[test]
    fn clean_markdown_collapses_blank_lines() {
        let input = "A\n\n\n\n\n\nB";
        let cleaned = clean_markdown(input);
        assert!(!cleaned.contains("\n\n\n\n"));
        assert!(cleaned.contains("A"));
        assert!(cleaned.contains("B"));
    }

    #[test]
    fn clean_markdown_joins_broken_lines() {
        let input = "The results\nshow that our\nrevenue grew.";
        let cleaned = clean_markdown(input);
        assert!(cleaned.contains("The results show that our revenue grew."));
    }

    #[test]
    fn clean_markdown_preserves_headings() {
        let input = "# Title\n\nParagraph\n\n## Subtitle\n\nMore text";
        let cleaned = clean_markdown(input);
        assert!(cleaned.contains("# Title"));
        assert!(cleaned.contains("## Subtitle"));
    }

    #[test]
    fn extract_toc_basic() {
        let content = "# Intro\nSome text\n## Sub\nMore text\n# End\nFinal";
        let toc = extract_toc(content);
        assert_eq!(toc.len(), 3);
        assert_eq!(toc[0].number, "1");
        assert_eq!(toc[0].title, "Intro");
        assert_eq!(toc[0].level, 1);
        assert_eq!(toc[1].number, "1.1");
        assert_eq!(toc[1].title, "Sub");
        assert_eq!(toc[2].number, "2");
        assert_eq!(toc[2].title, "End");
    }

    #[test]
    fn extract_section_by_number() {
        let content = "# First\nContent A\n# Second\nContent B\n# Third\nContent C";
        let section = extract_section(content, "2");
        assert!(section.is_some());
        let text = section.unwrap();
        assert!(text.contains("# Second"));
        assert!(text.contains("Content B"));
        assert!(!text.contains("Content A"));
        assert!(!text.contains("Content C"));
    }

    #[test]
    fn extract_section_by_name() {
        let content = "# Introduction\nHello\n# Methods\nWorld\n# Results\nDone";
        let section = extract_section(content, "methods");
        assert!(section.is_some());
        assert!(section.unwrap().contains("World"));
    }

    #[test]
    fn extract_section_not_found() {
        let content = "# Title\nContent";
        assert!(extract_section(content, "nonexistent").is_none());
    }

    #[test]
    fn truncate_at_boundary_within_budget() {
        let content = "Short text";
        let (result, notice) = truncate_at_boundary(content, 100, 0);
        assert_eq!(result, "Short text");
        assert!(notice.is_none());
    }

    #[test]
    fn truncate_at_boundary_cuts() {
        let content = "# First\nAAA\n\n# Second\nBBB\n\n# Third\nCCC";
        let (result, notice) = truncate_at_boundary(content, 20, 0);
        assert!(result.len() <= 20);
        assert!(notice.is_some());
        assert!(notice.unwrap().contains("truncated"));
    }

    #[test]
    fn parse_page_range_single() {
        let pages = parse_page_range("5").unwrap();
        assert_eq!(pages, vec![5]);
    }

    #[test]
    fn parse_page_range_range() {
        let pages = parse_page_range("2-5").unwrap();
        assert_eq!(pages, vec![2, 3, 4, 5]);
    }

    #[test]
    fn parse_page_range_complex() {
        let pages = parse_page_range("1-3,7,10-12").unwrap();
        assert_eq!(pages, vec![1, 2, 3, 7, 10, 11, 12]);
    }

    #[test]
    fn parse_page_range_zero_rejected() {
        assert!(parse_page_range("0").is_err());
    }

    #[test]
    fn parse_page_range_invalid_rejected() {
        assert!(parse_page_range("abc").is_err());
    }

    #[test]
    fn estimate_tokens_basic() {
        // ~4 chars per token
        let text = "Hello world, this is a test sentence.";
        let tokens = estimate_tokens(text);
        assert!(tokens > 0);
        assert!(tokens < text.len()); // Should be less than char count
    }

    #[test]
    fn is_url_detection() {
        assert!(is_url("https://example.com"));
        assert!(is_url("http://example.com/path"));
        assert!(!is_url("/local/path"));
        assert!(!is_url("file.pdf"));
        assert!(!is_url("service"));
    }

    #[test]
    fn filter_pages_no_breaks() {
        let content = "All in one page without breaks";
        let result = filter_pages(content, &[1]);
        assert!(result.contains("All in one page"));
    }

    #[test]
    fn filter_pages_with_formfeed() {
        let content = "Page one content\x0cPage two content\x0cPage three content";
        let result = filter_pages(content, &[2]);
        assert!(result.contains("Page two content"));
        assert!(!result.contains("Page one"));
        assert!(!result.contains("Page three"));
    }

    #[test]
    fn filter_pages_range() {
        let content = "Page 1\x0cPage 2\x0cPage 3\x0cPage 4";
        let result = filter_pages(content, &[1, 3]);
        assert!(result.contains("Page 1"));
        assert!(result.contains("Page 3"));
        assert!(!result.contains("Page 2"));
        assert!(!result.contains("Page 4"));
    }

    #[test]
    fn toc_detects_tables_and_code() {
        let content = "# Section A\nNo special content\n# Section B\n| col1 | col2 |\n|---|---|\n| a | b |\n# Section C\n```rust\nfn main() {}\n```\n";
        let toc = extract_toc(content);
        assert_eq!(toc.len(), 3);
        assert!(!toc[0].has_table);
        assert!(!toc[0].has_code);
        assert!(toc[1].has_table);
        assert!(!toc[1].has_code);
        assert!(!toc[2].has_table);
        assert!(toc[2].has_code);
    }

    #[test]
    fn map_ocr_lang_normalizes_long_names() {
        assert_eq!(map_ocr_lang("english"), "eng");
        assert_eq!(map_ocr_lang("german"), "deu");
        assert_eq!(map_ocr_lang("ENG"), "eng");
        assert_eq!(map_ocr_lang("deu"), "deu");
        assert_eq!(map_ocr_lang("fra"), "fra");
    }

    #[test]
    fn hidden_component_ignores_dot_and_dotdot() {
        assert!(is_hidden_component(Some(".git")));
        assert!(is_hidden_component(Some(".env")));
        assert!(!is_hidden_component(Some(".")));
        assert!(!is_hidden_component(Some("..")));
        assert!(!is_hidden_component(Some("src")));
        assert!(!is_hidden_component(None));
    }

    #[test]
    fn liteparse_extension_covers_pdf_and_office() {
        assert!(is_liteparse_extension("pdf"));
        assert!(is_liteparse_extension("pptx"));
        assert!(is_liteparse_extension("key"));
        assert!(is_liteparse_extension("docx"));
        assert!(is_liteparse_extension("xlsx"));
        assert!(!is_liteparse_extension("png"));
        assert!(!is_liteparse_extension("html"));
        assert!(!is_liteparse_extension("csv"));
    }

    #[test]
    fn office_extension_requires_libreoffice() {
        assert!(is_office_extension("pptx"));
        assert!(is_office_extension("docx"));
        assert!(is_office_extension("xlsx"));
        assert!(!is_office_extension("pdf"));
        assert!(!is_office_extension("png"));
    }

    #[test]
    fn extension_of_falls_back_empty() {
        assert_eq!(extension_of(Path::new("a/b/report.PDF")), "PDF");
        assert_eq!(extension_of(Path::new("noext")), "");
    }

    #[test]
    fn image_target_dir_resolves_file_parent_or_dir() {
        // A file output: images go next to it.
        let d = image_target_dir(Some(Path::new("/tmp/out/deck.md")));
        assert_eq!(d.as_deref(), Some(Path::new("/tmp/out")));
        // A directory output: images go in it.
        let d = image_target_dir(Some(Path::new("/tmp/out/")));
        assert_eq!(d.as_deref(), Some(Path::new("/tmp/out")));
        // No output: no image writing.
        assert!(image_target_dir(None).is_none());
    }
}
