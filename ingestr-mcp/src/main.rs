use std::env;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use config::{Config, Environment, File, FileFormat};
use env_logger::Env;
use ingestr_core::SearchIndex;
use log::{debug, error, info};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::process::{Command, Stdio};
use std::str;
use sysinfo::{Pid, System};

const APP_NAME: &str = env!("CARGO_PKG_NAME");
const CONFIG_APP_NAME: &str = "ingestr";
const CONFIG_ENV_PREFIX: &str = "INGESTR";

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "MCP server exposing ingestr search",
    propagate_version = true
)]
struct Cli {
    /// Override the config file path
    #[arg(long)]
    config: Option<PathBuf>,
    /// Override the index directory (highest priority)
    #[arg(long)]
    index_dir: Option<PathBuf>,
    /// Set log level (error, warn, info, debug, trace)
    #[arg(long, default_value = "info")]
    log_level: String,
    /// Emit the MCP client configuration JSON and exit
    #[arg(long)]
    show_config: bool,
}

#[derive(Debug, Clone)]
struct AppPaths {
    global_config: PathBuf,
    local_config: PathBuf,
    active_config: PathBuf,
    state_dir: PathBuf,
}

impl AppPaths {
    fn discover(override_path: Option<PathBuf>) -> Result<Self> {
        let global_config = default_config_dir()?.join("config.toml");
        let local_config = env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("config.toml");

        let active_config = match override_path {
            Some(path) => expand_path(path)?,
            None => global_config.clone(),
        };

        if active_config.parent().is_none() {
            return Err(anyhow!("invalid config file path: {active_config:?}"));
        }

        let state_dir = default_state_dir()?;

        Ok(Self {
            global_config,
            local_config,
            active_config,
            state_dir,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(Default)]
#[serde(default)]
struct AppConfig {
    index: IndexConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct IndexConfig {
    index_dir: String,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            index_dir: default_index_dir_string(),
        }
    }
}

fn main() {
    if let Err(err) = try_main() {
        let _ = writeln!(io::stderr(), "{err:?}");
        std::process::exit(1);
    }
}

fn try_main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(&cli.log_level)?;

    let mut paths = AppPaths::discover(cli.config.clone())?;
    let config = load_or_init_config(&mut paths, cli.config.as_ref())?;

    if cli.show_config {
        output_mcp_config()?;
        return Ok(());
    }

    let index_dir = if let Some(custom) = cli.index_dir {
        expand_path(custom)?
    } else {
        expand_str_path(&config.index.index_dir)?
    };

    info!("starting MCP server with index at {}", index_dir.display());

    ensure_service_running(&paths, &index_dir)?;

    let search_index = SearchIndex::open(&index_dir, false)?;
    run_server(search_index)?;
    Ok(())
}

fn run_server(mut index: SearchIndex) -> Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    let lines = stdin.lock().lines();

    for line in lines {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let response = match handle_message(&mut index, &line) {
            Ok(value) => json!({ "result": value }),
            Err(err) => {
                error!("request failed: {err:?}");
                json!({ "error": err.to_string() })
            }
        };

        serde_json::to_writer(&mut stdout, &response)?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
    }

    std::thread::sleep(Duration::from_millis(10));
    let _ = stdout.flush();
    let _ = stderr.flush();
    Ok(())
}

fn handle_message(index: &mut SearchIndex, line: &str) -> Result<serde_json::Value> {
    let request: serde_json::Value = serde_json::from_str(line)?;
    let method = request
        .get("method")
        .and_then(|m| m.as_str())
        .ok_or_else(|| anyhow!("missing method"))?;

    match method {
        "initialize" => Ok(json!({
            "serverInfo": { "name": APP_NAME, "version": env!("CARGO_PKG_VERSION") },
            "capabilities": { "tools": true }
        })),
        "list_tools" => Ok(json!({
            "tools": [
                {
                    "name": "search",
                    "description": "Search ingestr markdown index for relevant documents.",
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "query": { "type": "string" },
                            "limit": { "type": "integer", "minimum": 1, "maximum": 50 }
                        },
                        "required": ["query"]
                    }
                },
                {
                    "name": "open_source",
                    "description": "Open a source document on the local machine (requires confirmation).",
                    "requires_confirmation": true,
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string" },
                            "confirm": { "type": "boolean" }
                        },
                        "required": ["path"]
                    }
                }
            ]
        })),
        "call_tool" => {
            let params = request
                .get("params")
                .and_then(|p| p.as_object())
                .ok_or_else(|| anyhow!("missing params"))?;

            let name = params
                .get("name")
                .and_then(|n| n.as_str())
                .ok_or_else(|| anyhow!("missing tool name"))?;

            let args = params
                .get("arguments")
                .and_then(|a| a.as_object())
                .ok_or_else(|| anyhow!("missing arguments"))?;

            match name {
                "search" => {
                    let query = args
                        .get("query")
                        .and_then(|q| q.as_str())
                        .ok_or_else(|| anyhow!("missing query"))?;
                    let limit = args
                        .get("limit")
                        .and_then(|l| l.as_u64())
                        .unwrap_or(10)
                        .clamp(1, 50) as usize;

                    let hits = index.search(query, limit)?;
                    Ok(json!({ "content": hits }))
                }
                "open_source" => {
                    let path_str = args
                        .get("path")
                        .and_then(|p| p.as_str())
                        .ok_or_else(|| anyhow!("missing path"))?;
                    let confirm = args
                        .get("confirm")
                        .and_then(|c| c.as_bool())
                        .unwrap_or(false);

                    if !confirm {
                        return Err(anyhow!(
                            "confirmation required: set confirm=true to open {}",
                            path_str
                        ));
                    }

                    let expanded = expand_str_path(path_str)?;
                    let target = expanded.canonicalize().unwrap_or(expanded);

                    if !target.exists() {
                        return Err(anyhow!("path does not exist: {}", target.display()));
                    }

                    open_path(&target)?;
                    Ok(json!({ "content": format!("opened {}", target.display()) }))
                }
                _ => Err(anyhow!("unknown tool: {name}")),
            }
        }
        _ => Err(anyhow!("unknown method: {method}")),
    }
}

fn init_logging(level: &str) -> Result<()> {
    let env = Env::default().default_filter_or(level);
    env_logger::Builder::from_env(env)
        .format_timestamp_millis()
        .init();
    Ok(())
}

fn load_or_init_config(paths: &mut AppPaths, cli_override: Option<&PathBuf>) -> Result<AppConfig> {
    if !paths.active_config.exists() && cli_override.is_none() {
        if let Some(parent) = paths.active_config.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating config directory {}", parent.display()))?;
        }
        write_default_config(&paths.active_config)?;
    }

    let env_prefix = env_prefix();
    let builder = Config::builder()
        .set_default("index.index_dir", default_index_dir_string())?
        .add_source(
            File::from(paths.global_config.as_path())
                .format(FileFormat::Toml)
                .required(false),
        )
        .add_source(
            File::from(paths.local_config.as_path())
                .format(FileFormat::Toml)
                .required(false),
        )
        .add_source(Environment::with_prefix(env_prefix.as_str()).separator("__"))
        .add_source(
            File::from(paths.active_config.as_path())
                .format(FileFormat::Toml)
                .required(false),
        );

    let built = builder.build()?;
    let mut config: AppConfig = built.try_deserialize()?;

    config.index.index_dir = expand_str_path(&config.index.index_dir)?
        .display()
        .to_string();

    debug!("effective config: {:?}", config);
    Ok(config)
}

fn write_default_config(path: &Path) -> Result<()> {
    let config = AppConfig::default();
    let toml = toml::to_string_pretty(&config)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating config directory {}", parent.display()))?;
    }
    fs::write(path, toml).with_context(|| format!("writing config to {}", path.display()))
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
        path.push(CONFIG_APP_NAME);
        return Ok(path);
    }

    if let Some(mut dir) = dirs::config_dir() {
        dir.push(CONFIG_APP_NAME);
        return Ok(dir);
    }

    dirs::home_dir()
        .map(|home| home.join(".config").join(CONFIG_APP_NAME))
        .ok_or_else(|| anyhow!("unable to determine configuration directory"))
}

fn default_data_dir() -> Result<PathBuf> {
    if let Some(dir) = env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir).join(CONFIG_APP_NAME));
    }

    if let Some(mut dir) = dirs::data_dir() {
        dir.push(CONFIG_APP_NAME);
        return Ok(dir);
    }

    dirs::home_dir()
        .map(|home| home.join(".local").join("share").join(CONFIG_APP_NAME))
        .ok_or_else(|| anyhow!("unable to determine data directory"))
}

fn default_state_dir() -> Result<PathBuf> {
    if let Some(dir) = env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir).join(CONFIG_APP_NAME));
    }

    if let Some(mut dir) = dirs::state_dir() {
        dir.push(CONFIG_APP_NAME);
        return Ok(dir);
    }

    dirs::home_dir()
        .map(|home| home.join(".local").join("state").join(CONFIG_APP_NAME))
        .ok_or_else(|| anyhow!("unable to determine state directory"))
}

fn default_index_dir_string() -> String {
    default_data_dir()
        .unwrap_or_else(|_| PathBuf::from("~/.local/share").join(CONFIG_APP_NAME))
        .join("index")
        .display()
        .to_string()
}

fn env_prefix() -> String {
    CONFIG_ENV_PREFIX.to_string()
}

fn open_path(path: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = Command::new("open");
        c.arg(path);
        c
    };

    #[cfg(target_os = "linux")]
    let mut cmd = {
        let mut c = Command::new("xdg-open");
        c.arg(path);
        c
    };

    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = Command::new("cmd");
        c.args(["/C", "start", "", path.to_string_lossy().as_ref()]);
        c
    };

    let status = cmd.status()?;
    if !status.success() {
        return Err(anyhow!("failed to open {}", path.display()));
    }
    Ok(())
}

fn ensure_service_running(paths: &AppPaths, index_dir: &Path) -> Result<()> {
    let pid_path = paths.state_dir.join("service.pid");

    if let Some(pid) = read_pid(&pid_path)? {
        if process_running(pid) {
            return Ok(());
        } else {
            fs::remove_file(&pid_path).ok();
        }
    }

    let ingestr_bin = which_ingestr_cli()?;

    let mut cmd = Command::new(ingestr_bin);
    cmd.arg("service")
        .arg("start")
        .arg("--index-dir")
        .arg(index_dir);

    cmd.arg("--config").arg(&paths.active_config);

    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::piped());
    cmd.stdin(Stdio::null());

    let output = cmd.output().context("starting ingestr service")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("service already running") {
            return Ok(());
        }
        return Err(anyhow!(
            "failed to start ingestr service (status {}){}",
            output.status,
            if stderr.trim().is_empty() {
                "".to_string()
            } else {
                format!(": {}", stderr.trim())
            }
        ));
    }

    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        if let Some(pid) = read_pid(&pid_path)?
            && process_running(pid)
        {
            info!("ingestr service started pid {}", pid);
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    Err(anyhow!(
        "service start did not produce a running process within timeout"
    ))
}

fn read_pid(path: &Path) -> Result<Option<i32>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(path)?;
    let pid: i32 = text.trim().parse()?;
    Ok(Some(pid))
}

fn process_running(pid: i32) -> bool {
    let mut sys = System::new_all();
    sys.refresh_processes();
    sys.process(Pid::from_u32(pid as u32)).is_some()
}

fn output_mcp_config() -> Result<()> {
    let ingestr_path = which_ingestr_mcp()?;
    let config = json!({
        "name": "ingestr",
        "command": ingestr_path,
        "args": [],
    });
    println!("{}", serde_json::to_string_pretty(&config)?);
    Ok(())
}

fn which_ingestr_mcp() -> Result<String> {
    if let Ok(current) = env::current_exe()
        && let Some(name) = current.file_name().and_then(|n| n.to_str())
        && name.contains("ingestr-mcp")
    {
        return Ok(current.display().to_string());
    }
    if let Ok(current) = env::current_exe()
        && let Some(parent) = current.parent()
    {
        let sibling = parent.join("ingestr-mcp");
        if sibling.is_file() {
            return Ok(sibling.display().to_string());
        }
    }

    let path = env::var_os("PATH").ok_or_else(|| anyhow!("PATH not set"))?;
    for entry in env::split_paths(&path) {
        let candidate = entry.join("ingestr-mcp");
        if candidate.is_file() {
            return Ok(candidate.display().to_string());
        }
    }

    Err(anyhow!("unable to locate ingestr-mcp binary on PATH"))
}

fn which_ingestr_cli() -> Result<String> {
    if let Ok(current) = env::current_exe()
        && let Some(parent) = current.parent()
    {
        let sibling = parent.join("ingestr");
        if sibling.is_file() {
            return Ok(sibling.display().to_string());
        }
    }

    let path = env::var_os("PATH").ok_or_else(|| anyhow!("PATH not set"))?;
    for entry in env::split_paths(&path) {
        let candidate = entry.join("ingestr");
        if candidate.is_file() {
            return Ok(candidate.display().to_string());
        }
    }

    Err(anyhow!("unable to locate ingestr binary on PATH"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn default_config_writes() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        write_default_config(&config_path).unwrap();
        let body = fs::read_to_string(&config_path).unwrap();
        assert!(body.contains("index_dir"));
    }
}
