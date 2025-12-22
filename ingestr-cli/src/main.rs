use std::env;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcCommand, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, RecvTimeoutError},
};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use config::{Config, Environment, File, FileFormat};
use env_logger::fmt::WriteStyle;
use ingestr_core::{IndexedDocument, SearchIndex};
use log::{LevelFilter, debug, info, warn};
use markitdown::{model::ConversionOptions, MarkItDown};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use sysinfo::{Pid, Signal, System};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use walkdir::WalkDir;

const APP_NAME: &str = env!("CARGO_PKG_NAME");
const CONFIG_DIR_NAME: &str = "ingestr";

fn main() {
    if let Err(err) = try_main() {
        let _ = writeln!(io::stderr(), "{err:?}");
        std::process::exit(1);
    }
}

fn try_main() -> Result<()> {
    let cli = Cli::parse();

    let mut ctx = RuntimeContext::new(cli.common.clone())?;
    ctx.init_logging()?;
    debug!("resolved paths: {:#?}", ctx.paths);

    match cli.command {
        Command::Service { command } => handle_service(&mut ctx, command),
        Command::Search(cmd) => handle_search(&ctx, cmd),
        Command::Init(cmd) => handle_init(&ctx, cmd),
        Command::Config { command } => handle_config(&ctx, command),
        Command::Completions { shell } => handle_completions(shell),
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
    #[arg(long, global = true, conflicts_with = "yaml")]
    json: bool,
    /// Output machine readable YAML
    #[arg(long, global = true)]
    yaml: bool,
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
    #[arg(short = 'y', long = "yes", alias = "force", global = true)]
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

#[derive(Debug, Clone, Copy, ValueEnum)]
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
    /// Create config directories and default files
    Init(InitCommand),
    /// Inspect and manage configuration
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Generate shell completions
    Completions {
        #[arg(value_enum)]
        shell: Shell,
    },
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
        let mut paths = AppPaths::discover(common.config.clone())?;
        let config = load_or_init_config(&mut paths, &common)?;
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

    fn effective_log_level(&self) -> LevelFilter {
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

fn handle_service(ctx: &mut RuntimeContext, command: ServiceCommand) -> Result<()> {
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

fn run_service_foreground(ctx: &mut RuntimeContext, cmd: ServiceRunOpts) -> Result<()> {
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

fn start_service_background(ctx: &mut RuntimeContext, cmd: ServiceRunOpts) -> Result<()> {
    let pid_path = ctx.pid_path();
    if let Some(pid) = read_pid(&pid_path)? {
        if process_running(pid) {
            return Err(anyhow!("service already running with pid {}", pid));
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
        info!("stale pid {}; removing pid file", pid);
        fs::remove_file(&pid_path).ok();
        return Ok(());
    }

    let mut sys = System::new_all();
    sys.refresh_processes();
    let sys_pid = Pid::from_u32(pid as u32);
    if let Some(proc) = sys.process(sys_pid) {
        let killed = proc.kill_with(Signal::Term).unwrap_or(false) || proc.kill();
        if killed {
            info!("stopped service pid {}", pid);
            fs::remove_file(&pid_path).ok();
            return Ok(());
        }
    }

    Err(anyhow!("failed to stop service pid {}", pid))
}

fn status_service(ctx: &RuntimeContext) -> Result<()> {
    let pid_path = ctx.pid_path();
    let Some(pid) = read_pid(&pid_path)? else {
        println!("service status: stopped");
        return Ok(());
    };

    if process_running(pid) {
        println!("service status: running (pid {})", pid);
    } else {
        println!("service status: not running (stale pid {})", pid);
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
    } else if ctx.common.yaml {
        println!(
            "{}",
            serde_yaml::to_string(&results).context("serializing search results to YAML")?
        );
    } else if results.is_empty() {
        println!("No results found");
    } else {
        for hit in &results {
            println!(
                "- {} (score {:.2}) -> {}",
                hit.title
                    .as_deref()
                    .unwrap_or_else(|| hit.source_path.as_str()),
                hit.score,
                hit.output_path
            );
        }
    }

    Ok(())
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
            } else if ctx.common.yaml {
                println!(
                    "{}",
                    serde_yaml::to_string(&ctx.config).context("serializing config to YAML")?
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

fn load_or_init_config(paths: &mut AppPaths, common: &CommonOpts) -> Result<AppConfig> {
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
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
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
        // Set LLM environment variables from config if provided
        Self::configure_llm_env(&settings);

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

    fn configure_llm_env(settings: &ServiceSettings) {
        if !settings.llm_enabled {
            return;
        }

        // Set API key environment variable based on provider
        if let Some(ref api_key) = settings.llm_api_key {
            let env_var = match settings.llm_client.as_str() {
                "openai" => "OPENAI_API_KEY",
                "gemini" => "GEMINI_API_KEY",
                "deepseek" => "DEEPSEEK_API_KEY",
                _ => "OPENAI_API_KEY",
            };
            // SAFETY: This is called at service startup before any threads are spawned,
            // and we control the environment variable names being set.
            unsafe {
                env::set_var(env_var, api_key);
            }
        }

        // Set base URL environment variable (OpenAI-compatible format)
        if let Some(ref base_url) = settings.llm_base_url {
            // SAFETY: This is called at service startup before any threads are spawned,
            // and we control the environment variable names being set.
            unsafe {
                env::set_var("OPENAI_API_BASE", base_url);
                env::set_var("OPENAI_BASE_URL", base_url);
            }
        }
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
        let kind = event.kind.clone();
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
                .map(|s| s.starts_with('.'))
                .unwrap_or(false)
        })
    }

    fn process_path(&mut self, path: &Path) -> Result<()> {
        let output_path = self.output_path_for(path)?;
        let is_markdown = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|ext| ext.eq_ignore_ascii_case("md"))
            .unwrap_or(false);

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

        let converted = match self.markitdown.convert(
            path.to_str()
                .ok_or_else(|| anyhow!("invalid path encoding for {}", path.display()))?,
            conversion_opts,
        ) {
            Some(result) => result,
            None => {
                warn!("no converter available for {}", path.display());
                return Ok(());
            }
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

        let yaml = serde_yaml::to_string(&frontmatter).context("serializing frontmatter")?;
        let mut body = String::new();
        body.push_str("---\n");
        body.push_str(&yaml);
        body.push_str("---\n\n");
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
                converted_at: Some(converted_at.clone()),
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
        let source_path_str = frontmatter
            .as_ref()
            .map(|fm| fm.source_path.clone())
            .unwrap_or_else(|| source_path.display().to_string());

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
                .map(|ext| ext.eq_ignore_ascii_case("md"))
                .unwrap_or(false);
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
    if let Some(rest) = body.strip_prefix("---\n") {
        if let Some(idx) = rest.find("\n---\n") {
            let (front, content) = rest.split_at(idx);
            let content = &content["\n---\n".len()..];
            if let Ok(parsed) = serde_yaml::from_str::<Frontmatter>(front) {
                return (Some(parsed), content);
            }
        }
    }
    (None, body)
}

fn default_watch_dir_string() -> String {
    dirs::home_dir()
        .map(|home| home.join("Documents").display().to_string())
        .unwrap_or_else(|| "~/Documents".to_string())
}

fn default_output_dir_string() -> String {
    dirs::home_dir()
        .map(|home| home.join("markdown").display().to_string())
        .unwrap_or_else(|| "~/markdown".to_string())
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
