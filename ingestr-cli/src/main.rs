//! `ingestr` CLI: convert documents to Markdown (one file, a directory
//! tree, a URL, or a watched folder). It does not index or search (ADR-0003).

use std::collections::HashMap;
use std::env;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcCommand, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc::{self, RecvTimeoutError},
};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use config::{Config, Environment, File, FileFormat};
use env_logger::fmt::WriteStyle;
use ingestr_core::cache::{CACHE_SCHEMA, cache_dir, cache_get, cache_put};
use ingestr_core::fetch::{fetch_url, is_url};
use ingestr_core::formats::{SUPPORTED_EXTENSIONS, is_hidden_component};
use ingestr_core::markdown::*;
use ingestr_core::pipeline::*;
use ingestr_core::settings::*;
use log::{LevelFilter, debug, error, info, warn};
use markitdown::{MarkItDown, model::ConversionOptions};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sysinfo::{Pid, Signal, System};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use walkdir::WalkDir;

const APP_NAME: &str = env!("CARGO_PKG_NAME");
const CONFIG_DIR_NAME: &str = "ingestr";

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
        Command::Convert(cmd) => handle_convert(&ctx, cmd),
        Command::Init(cmd) => handle_init(&ctx, cmd),
        Command::Config { command } => handle_config(&ctx, command),
        Command::Cache { command } => handle_cache(&ctx, command),
        Command::Completions { shell } => handle_completions(shell),
        Command::Doctor => handle_doctor(&ctx),
    }
}

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "Convert documents to Markdown: one file, a directory tree, a URL, or a watched folder.",
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
#[expect(
    clippy::large_enum_variant,
    reason = "clap subcommand payloads: Convert carries every conversion flag and is built once per process; clap's derive cannot box a variant"
)]
enum Command {
    /// Manage the background conversion service (watch a directory)
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
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
    /// File extensions to process (comma-separated, e.g., "pdf,docx,html").
    /// Defaults to the set of known document formats; see --all-files.
    #[arg(long, value_name = "EXTENSIONS", value_delimiter = ',')]
    extensions: Option<Vec<String>>,
    /// Attempt every file regardless of extension (disables the default
    /// document-format allowlist; binaries will be reported as failures)
    #[arg(long, conflicts_with = "extensions")]
    all_files: bool,
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
    /// OCR backend to use (paddle = bundled PP-OCR ONNX engine, the default)
    #[arg(long, value_name = "BACKEND", value_enum, default_value = "paddle")]
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
    /// Conversion engine: auto (LiteParse for PDF/office, markitdown otherwise),
    /// liteparse (no markitdown fallback for LiteParse formats), or markitdown
    /// (skip LiteParse entirely). Useful for A/B comparisons and debugging.
    #[arg(long, value_enum, default_value_t = ConvertEngine::Auto)]
    engine: ConvertEngine,
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
}

impl RuntimeContext {
    fn new(common: CommonOpts) -> Result<Self> {
        let paths = AppPaths::discover(common.config.clone())?;
        let config = load_or_init_config(&paths, &common)?;
        let paths = paths.apply_overrides(&config)?;
        let directories = ResolvedDirectories::from_config(&config)?;
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
                "dry-run: would ensure data dir {}, state dir {}, and output dir {}",
                self.paths.data_dir.display(),
                self.paths.state_dir.display(),
                self.directories.output_dir.display()
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

        Ok(ServiceSettings {
            watch_dir,
            output_dir,
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
            routing: self.config.routing.clone(),
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
    llm: LlmConfig,
    processors: ProcessorsConfig,
    routing: RoutingConfig,
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
            llm: LlmConfig::default(),
            processors: ProcessorsConfig::default(),
            routing: RoutingConfig::default(),
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

    let mut service = ConversionService::new(settings)?;
    if cmd.once {
        service.process_existing()?;
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ConvertResult {
    source_path: String,
    output_path: String,
    source_modified: Option<String>,
    title: Option<String>,
    converted_at: String,
    markdown: String,
}

// ============================================================
// Post-processing: clean noisy conversion output
// ============================================================

// ============================================================
// Conversion cache
// ============================================================

/// Compute a cache key from the file's content hash plus every option that can
/// change the conversion result.
fn cache_key(path: &Path, cmd: &ConvertCommand) -> Result<String> {
    let data = fs::read(path).context("reading file for cache key")?;
    let mut hasher = Sha256::new();
    hasher.update(CACHE_SCHEMA.as_bytes());
    hasher.update(&data);
    hasher.update(if cmd.meta { b"meta=1" } else { b"meta=0" });
    hasher.update(if cmd.raw { b"raw=1" } else { b"raw=0" });
    hasher.update(if cmd.vlm { b"vlm=1" } else { b"vlm=0" });
    hasher.update(if cmd.ocr { b"ocr=1" } else { b"ocr=0" });
    if cmd.ocr {
        hasher.update(format!("ocr_backend={}", cmd.ocr_backend).as_bytes());
        if let Some(langs) = &cmd.ocr_languages {
            hasher.update(format!("ocr_langs={}", langs.join("+")).as_bytes());
        }
    }
    if cmd.vlm
        && let Some(model) = &cmd.vlm_model
    {
        hasher.update(format!("vlm_model={model}").as_bytes());
    }
    hasher.update(format!("engine={:?}", cmd.engine).as_bytes());
    if let Some(s) = &cmd.section {
        hasher.update(format!("section={s}").as_bytes());
    }
    if let Some(p) = &cmd.pages {
        hasher.update(format!("pages={p}").as_bytes());
    }
    Ok(hex::encode(hasher.finalize()))
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

fn handle_convert(ctx: &RuntimeContext, cmd: ConvertCommand) -> Result<()> {
    let settings = ctx.service_settings(&ServiceRunOpts {
        watch_dir: None,
        output_dir: None,
        once: true,
    })?;

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
        && let Ok(key) = cache_key(input, cmd)
        && let Ok(Some(cached)) = cache_get(&key)
    {
        debug!("cache hit for {source_label}");
        return Ok(output_final(ctx, cmd, cached, source_label));
    }

    let processor = DocumentProcessor::new(settings.processor_settings())
        .with_vlm(
            cmd.vlm,
            cmd.vlm_model.clone(),
            cmd.vlm_prompt.clone(),
            cmd.jobs,
            cmd.output.clone(),
        )
        .with_ocr(cmd.ocr, cmd.ocr_backend, cmd.ocr_languages.clone())
        .with_image_output_dir(image_target_dir(cmd.output.as_deref()))
        .with_engine(cmd.engine);

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
        && let Ok(key) = cache_key(input, cmd)
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

    // Extension filter: explicit --extensions wins; otherwise default to the
    // known document formats so a mixed pile does not spend time (and report
    // failures) on binaries, archives and other non-documents. --all-files
    // disables the filter entirely.
    let extensions: Option<Vec<String>> = if cmd.all_files {
        None
    } else if let Some(exts) = &cmd.extensions {
        Some(
            exts.iter()
                .map(|e| e.trim_start_matches('.').to_lowercase())
                .collect(),
        )
    } else {
        Some(
            SUPPORTED_EXTENSIONS
                .iter()
                .map(|e| (*e).to_string())
                .collect(),
        )
    };

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
        let planned = plan_output_paths(&files, input_dir, output_dir.as_ref(), in_place);
        for (file, output_path) in files.iter().zip(&planned) {
            let output_path = output_path.display().to_string();
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
    let skipped = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    let errors: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

    let planned = plan_output_paths(&files, input_dir, output_dir.as_ref(), in_place);
    let results: Vec<Result<ConvertResult>> = if parallel > 1 {
        rayon::ThreadPoolBuilder::new()
            .num_threads(parallel)
            .build()
            .context("building thread pool")?
            .install(|| {
                files
                    .par_iter()
                    .zip(planned.par_iter())
                    .map(|(file, output_path)| {
                        process_file_for_batch(
                            file,
                            output_path.clone(),
                            output_dir.as_ref(),
                            settings,
                            cmd,
                            in_place,
                            &converted,
                            &skipped,
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
                planned[idx].clone(),
                output_dir.as_ref(),
                settings,
                cmd,
                in_place,
                &converted,
                &skipped,
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
        skipped: skipped.load(Ordering::Relaxed),
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
            println!("  Skipped:     {} (cached)", stats.skipped);
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

/// Resolve the batch output path for a source file: in-place (source dir),
/// under the output dir (mirroring the relative path), or stdout (`-`).
fn batch_output_path(
    file: &Path,
    input_dir: &Path,
    output_dir: Option<&PathBuf>,
    in_place: bool,
) -> PathBuf {
    if in_place {
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
    }
}

/// Plan every batch output path up front so no two inputs write the same file.
///
/// Normally `dir/report.pdf` -> `out/report.md`. When several inputs map to the
/// same Markdown path (`parts.csv` + `parts.xlsx`), or the target would be the
/// source itself (`notes.md` converted in place), each of those keeps its full
/// file name: `parts.csv.md`, `parts.xlsx.md`, `notes.md.md`.
fn plan_output_paths(
    files: &[PathBuf],
    input_dir: &Path,
    output_dir: Option<&PathBuf>,
    in_place: bool,
) -> Vec<PathBuf> {
    let defaults: Vec<PathBuf> = files
        .iter()
        .map(|f| batch_output_path(f, input_dir, output_dir, in_place))
        .collect();
    let mut counts: HashMap<&Path, usize> = HashMap::new();
    for d in &defaults {
        *counts.entry(d.as_path()).or_default() += 1;
    }
    files
        .iter()
        .zip(&defaults)
        .map(|(file, default)| {
            let stdout = default.as_os_str() == "-";
            let clash = counts.get(default.as_path()).copied().unwrap_or(0) > 1
                || default.as_path() == file.as_path();
            if stdout || !clash {
                return default.clone();
            }
            let mut name = file.file_name().unwrap_or_default().to_os_string();
            name.push(".md");
            default.with_file_name(name)
        })
        .collect()
}

fn process_file_for_batch(
    file: &Path,
    output_path: PathBuf,
    output_dir: Option<&PathBuf>,
    settings: &ServiceSettings,
    cmd: &ConvertCommand,
    in_place: bool,
    converted: &AtomicUsize,
    skipped: &AtomicUsize,
    failed: &AtomicUsize,
    errors: &std::sync::Mutex<Vec<String>>,
) -> Result<ConvertResult> {
    // Resume: reuse a cached conversion keyed by content hash + flags. This
    // makes re-running a large batch after an interruption cheap, and shared
    // or re-converted documents reuse their prior result. Write the cached
    // Markdown to the output path if it is missing so the on-disk tree is
    // complete even after a crash mid-run.
    if !cmd.no_cache
        && let Ok(key) = cache_key(file, cmd)
        && let Ok(Some(cached)) = cache_get(&key)
    {
        let writes_file = in_place || output_dir.is_some();
        if writes_file && !output_path.exists() {
            if let Some(parent) = output_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&output_path, &cached)?;
        }
        skipped.fetch_add(1, Ordering::Relaxed);
        debug!("cache hit, skipped {}", file.display());
        let source_modified = fs::metadata(file)
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(system_time_to_rfc3339);
        return Ok(ConvertResult {
            source_path: file.display().to_string(),
            output_path: output_path.display().to_string(),
            source_modified,
            title: None,
            converted_at: "cached".to_string(),
            markdown: cached,
        });
    }

    let processor = DocumentProcessor::new(settings.processor_settings())
        .with_vlm(
            cmd.vlm,
            cmd.vlm_model.clone(),
            cmd.vlm_prompt.clone(),
            cmd.jobs,
            cmd.output.clone(),
        )
        .with_ocr(cmd.ocr, cmd.ocr_backend, cmd.ocr_languages.clone())
        .with_image_output_dir(batch_image_dir(output_dir))
        .with_engine(cmd.engine);

    match processor.process(file) {
        Ok(doc) => {
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

            // Populate the resume cache so a re-run skips this file.
            if !cmd.no_cache
                && let Ok(key) = cache_key(file, cmd)
            {
                let _ = cache_put(&key, &markdown);
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

/// Report which external tools are installed so users know what conversions
/// and OCR backends are available without a dependency being silently missing.
fn handle_doctor(ctx: &RuntimeContext) -> Result<()> {
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
    println!("PDF conversion (LiteParse/PDFium) needs no external tools.");
    println!("Bundled: PDFium, PP-OCR (paddle, default OCR), Tesseract. Not bundled: LibreOffice.");

    // PP-OCR models: Hugging Face cache, pinned by SHA-256 (ADR-0004).
    let tier = ingestr_core::models::PpOcrTier::parse(&ctx.config.processors.ocr.paddle_model);
    println!(
        "\nPP-OCRv6 {} models (Hugging Face, sha256-pinned)",
        tier.as_str()
    );
    for m in ingestr_core::models::ppocr_status(tier) {
        match m.path {
            Some(p) => println!("✓  {}\n    {}", m.repo, p.display()),
            None => println!("·  {}\n    not cached; downloaded on first OCR use", m.repo),
        }
    }
    if let Some(shared) = env::var_os(ingestr_core::models::SHARED_HF_HOME_ENV) {
        println!("    shared cache: {}", PathBuf::from(shared).display());
    }
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
        .set_default("output.markdown_dir", default_output_dir_string())?;

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
    routing: RoutingConfig,
}

impl ServiceSettings {
    fn processor_settings(&self) -> ProcessorSettings {
        ProcessorSettings {
            llm_enabled: self.llm_enabled,
            llm_client: self.llm_client.clone(),
            llm_model: self.llm_model.clone(),
            llm_base_url: self.llm_base_url.clone(),
            llm_api_key: self.llm_api_key.clone(),
            processors: self.processors.clone(),
            routing: self.routing.clone(),
        }
    }
}

impl ResolvedDirectories {
    fn from_config(cfg: &AppConfig) -> Result<Self> {
        let watch_dir = expand_str_path(&cfg.watcher.watch_dir)?;
        let output_dir = expand_str_path(&cfg.output.markdown_dir)?;
        Ok(Self {
            watch_dir,
            output_dir,
        })
    }
}

struct ConversionService {
    settings: ServiceSettings,
    markitdown: MarkItDown,
}

impl ConversionService {
    fn new(settings: ServiceSettings) -> Result<Self> {
        Ok(Self {
            settings,
            markitdown: MarkItDown::new(),
        })
    }

    fn run(&mut self) -> Result<()> {
        self.process_existing()?;

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
                }
                Ok(Err(err)) => warn!("watch error: {err}"),
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }

        Ok(())
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
            return Ok(());
        }

        if !self.needs_processing(path, &output_path)? {
            debug!("skipping {} (already up to date)", path.display());
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
    use std::time::{SystemTime, UNIX_EPOCH};

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
            routing: RoutingConfig::default(),
        }
    }

    #[test]
    fn document_processor_converts_text_file() -> Result<()> {
        let dir = unique_temp_dir();
        fs::create_dir_all(&dir)?;
        let input = dir.join("test.txt");
        fs::write(&input, "Test content for processor")?;

        let settings = create_test_settings();
        let processor = DocumentProcessor::new(settings.processor_settings());
        let result = processor.process(&input)?;

        assert!(result.text_content.contains("Test content for processor"));

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
        assert_eq!(config.backend, OcrBackend::Paddle);
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
    fn plan_output_paths_disambiguates_collisions() {
        let input = Path::new("/in");
        let out = PathBuf::from("/out");
        let files: Vec<PathBuf> = [
            "/in/parts.csv",
            "/in/parts.xlsx",
            "/in/report.pdf",
            "/in/sub/parts.csv",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        let planned = plan_output_paths(&files, input, Some(&out), false);
        assert_eq!(planned[0], PathBuf::from("/out/parts.csv.md"));
        assert_eq!(planned[1], PathBuf::from("/out/parts.xlsx.md"));
        assert_eq!(planned[2], PathBuf::from("/out/report.md"));
        // Same stem in another directory does not collide.
        assert_eq!(planned[3], PathBuf::from("/out/sub/parts.md"));

        // In place, a Markdown source must never be overwritten by its output.
        let files = vec![PathBuf::from("/in/notes.md"), PathBuf::from("/in/a.pdf")];
        let planned = plan_output_paths(&files, input, None, true);
        assert_eq!(planned[0], PathBuf::from("/in/notes.md.md"));
        assert_eq!(planned[1], PathBuf::from("/in/a.md"));
    }

    #[test]
    fn render_frontmatter_is_json_in_yaml_fence() {
        let fm = Frontmatter {
            source_path: "/in/a.pdf".to_string(),
            output_path: "/out/a.md".to_string(),
            source_modified: None,
            title: Some("A".to_string()),
            converted_at: "2026-09-27T00:00:00Z".to_string(),
        };
        let out = render_frontmatter_markdown(&fm, "# A\n").unwrap();
        assert!(out.starts_with("---\n{"), "{out}");
        assert!(out.contains("\"source_path\":\"/in/a.pdf\""));
        assert!(out.trim_end().ends_with("# A"));
        let json_line = out.lines().nth(1).unwrap();
        assert!(serde_json::from_str::<serde_json::Value>(json_line).is_ok());
    }

    #[test]
    fn ocr_backend_default_is_paddle() {
        assert_eq!(OcrBackend::default(), OcrBackend::Paddle);
        assert_eq!(OcrConfig::default().backend, OcrBackend::Paddle);
        assert_eq!(OcrConfig::default().paddle_model, "small");
        let parsed: OcrBackend = serde_json::from_str("\"paddle\"").unwrap();
        assert_eq!(parsed, OcrBackend::Paddle);
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

    #[test]
    fn routing_config_default_is_heuristic() {
        let config = RoutingConfig::default();
        assert_eq!(config.mode, RoutingMode::Heuristic);
        assert!(config.router_url.is_none());
        assert_eq!(config.model, "kev-latest");
        assert!(config.api_key.is_none());
    }

    #[test]
    fn routing_mode_serde_lowercase() {
        assert_eq!(
            serde_json::to_string(&RoutingMode::Heuristic).unwrap(),
            "\"heuristic\""
        );
        assert_eq!(
            serde_json::to_string(&RoutingMode::Shadow).unwrap(),
            "\"shadow\""
        );
        assert_eq!(
            serde_json::to_string(&RoutingMode::Route).unwrap(),
            "\"route\""
        );
        assert_eq!(
            serde_json::from_str::<RoutingMode>("\"route\"").unwrap(),
            RoutingMode::Route
        );
    }

    // Keep a `route_document`-style smoke test that does not need a network:
    // with an unreachable URL it must fall back (return Err) rather than panic.
}
