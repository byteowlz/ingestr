//! `ingestr-mcp`: Model Context Protocol server exposing ingestr document
//! conversion to AI assistants.
//!
//! Built on the official Rust SDK (`rmcp`) and served over stdio. Tools:
//! `convert_document` (file or URL to Markdown, returned inline),
//! `convert_to_file` (write Markdown plus extracted images next to an output
//! path), `supported_formats`, and `doctor`. Conversion runs the `ingestr` CLI
//! as a subprocess until the pipeline lives in `ingestr-core`
//! (`ingestr-0frv`); the two binaries never depend on each other.

use std::env;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use ingestr_core::formats::{SUPPORTED_EXTENSIONS, extension_of, is_supported_extension};
use log::{debug, info, warn};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use serde::Deserialize;
use tokio::process::Command;

const APP_NAME: &str = env!("CARGO_PKG_NAME");

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "MCP server exposing ingestr document conversion (stdio transport)",
    propagate_version = true
)]
struct Cli {
    /// Path to the `ingestr` CLI binary (default: $INGESTR_BIN, then a sibling
    /// of this executable, then PATH)
    #[arg(long, value_name = "PATH", env = "INGESTR_BIN")]
    ingestr_bin: Option<PathBuf>,
    /// Maximum seconds a single conversion may take (OCR-heavy inputs are slow)
    #[arg(long, value_name = "SECONDS", default_value_t = 600)]
    timeout: u64,
    /// Set log level (error, warn, info, debug, trace); logs go to stderr
    #[arg(long, default_value = "info")]
    log_level: String,
    /// Emit the MCP client configuration JSON and exit
    #[arg(long)]
    show_config: bool,
}

/// Arguments for `convert_document`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ConvertDocumentArgs {
    /// Absolute path of the document, or an http(s) URL.
    path: String,
    /// Run OCR on scanned pages and images (slower; default false).
    #[serde(default)]
    ocr: bool,
    /// Return the raw conversion without Markdown cleanup (default false).
    #[serde(default)]
    raw: bool,
    /// Truncate the returned Markdown to about this many characters, cutting
    /// at a section boundary.
    #[serde(default)]
    max_chars: Option<usize>,
    /// Only convert these pages of a PDF, e.g. "1-3,7".
    #[serde(default)]
    pages: Option<String>,
    /// Only return this section, by number ("2.1") or heading text.
    #[serde(default)]
    section: Option<String>,
}

/// Arguments for `convert_to_file`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ConvertToFileArgs {
    /// Absolute path of the document, or an http(s) URL.
    path: String,
    /// Output Markdown file path. Extracted images are written next to it.
    output: String,
    /// Run OCR on scanned pages and images (slower; default false).
    #[serde(default)]
    ocr: bool,
    /// Write the raw conversion without Markdown cleanup (default false).
    #[serde(default)]
    raw: bool,
}

/// JSON envelope printed by `ingestr convert --json` for a single input.
#[derive(Debug, Deserialize)]
struct ConvertEnvelope {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    content: String,
    #[serde(default)]
    tokens: Option<u64>,
}

#[derive(Clone)]
struct IngestrMcp {
    ingestr_bin: PathBuf,
    timeout: Duration,
}

#[tool_router]
impl IngestrMcp {
    fn new(ingestr_bin: PathBuf, timeout: Duration) -> Self {
        Self {
            ingestr_bin,
            timeout,
        }
    }

    #[tool(
        description = "Convert a document (PDF, Office, HTML, images, ...) or an http(s) URL to Markdown and return it. Use `ocr: true` for scanned pages; use `max_chars` or `section` to keep the result small."
    )]
    async fn convert_document(
        &self,
        Parameters(args): Parameters<ConvertDocumentArgs>,
    ) -> Result<CallToolResult, McpError> {
        if let Err(msg) = validate_input(&args.path) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(msg)]));
        }
        let mut cli_args: Vec<String> = vec![
            "convert".into(),
            args.path.clone(),
            "--json".into(),
            "--quiet".into(),
        ];
        if args.ocr {
            cli_args.push("--ocr".into());
        }
        if args.raw {
            cli_args.push("--raw".into());
        }
        if let Some(n) = args.max_chars {
            cli_args.push("--max-chars".into());
            cli_args.push(n.to_string());
        }
        if let Some(p) = &args.pages {
            cli_args.push("--pages".into());
            cli_args.push(p.clone());
        }
        if let Some(sec) = &args.section {
            cli_args.push("--section".into());
            cli_args.push(sec.clone());
        }

        match self.run_ingestr(&cli_args).await {
            Ok(stdout) => match serde_json::from_str::<ConvertEnvelope>(&stdout) {
                Ok(env) if env.ok => {
                    let mut blocks = vec![ContentBlock::text(env.content)];
                    if let Some(tokens) = env.tokens {
                        blocks.push(ContentBlock::text(format!("(approx. {tokens} tokens)")));
                    }
                    Ok(CallToolResult::success(blocks))
                }
                Ok(_) => Ok(CallToolResult::error(vec![ContentBlock::text(
                    "conversion reported failure",
                )])),
                Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "could not parse ingestr output: {e}"
                ))])),
            },
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(
                e.to_string(),
            )])),
        }
    }

    #[tool(
        description = "Convert a document or URL to Markdown and write it to `output`; embedded images (slides, figures) are written next to it. Returns the output path."
    )]
    async fn convert_to_file(
        &self,
        Parameters(args): Parameters<ConvertToFileArgs>,
    ) -> Result<CallToolResult, McpError> {
        if let Err(msg) = validate_input(&args.path) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(msg)]));
        }
        let mut cli_args: Vec<String> = vec![
            "convert".into(),
            args.path.clone(),
            "--output".into(),
            args.output.clone(),
            "--quiet".into(),
        ];
        if args.ocr {
            cli_args.push("--ocr".into());
        }
        if args.raw {
            cli_args.push("--raw".into());
        }
        match self.run_ingestr(&cli_args).await {
            Ok(_) => Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "written: {}",
                args.output
            ))])),
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(
                e.to_string(),
            )])),
        }
    }

    #[tool(description = "List the file extensions ingestr can convert.")]
    fn supported_formats(&self) -> Result<CallToolResult, McpError> {
        Ok(CallToolResult::success(vec![ContentBlock::text(
            SUPPORTED_EXTENSIONS.join(", "),
        )]))
    }

    #[tool(
        description = "Report which optional external tools (LibreOffice, Poppler, Tesseract, ...) are installed for ingestr."
    )]
    async fn doctor(&self) -> Result<CallToolResult, McpError> {
        match self.run_ingestr(&["doctor".to_string()]).await {
            Ok(out) => Ok(CallToolResult::success(vec![ContentBlock::text(out)])),
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(
                e.to_string(),
            )])),
        }
    }

    /// Run the `ingestr` CLI with `args`, returning its stdout. Non-zero exit
    /// status is an error carrying the CLI's stderr (its errors are one line
    /// each, written for humans and agents alike).
    async fn run_ingestr(&self, args: &[String]) -> Result<String> {
        debug!("running {} {}", self.ingestr_bin.display(), args.join(" "));
        let child = Command::new(&self.ingestr_bin)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output();
        let output = tokio::time::timeout(self.timeout, child)
            .await
            .map_err(|_| anyhow!("conversion timed out after {}s", self.timeout.as_secs()))?
            .with_context(|| format!("running {}", self.ingestr_bin.display()))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let msg = stderr
                .lines()
                .next()
                .unwrap_or("conversion failed")
                .to_string();
            return Err(anyhow!("{msg}"));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

#[tool_handler(router = Self::tool_router())]
impl ServerHandler for IngestrMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(APP_NAME, env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Converts documents (PDF, Office, HTML, images, URLs) to Markdown with \
                 ingestr. Prefer `convert_document` with `max_chars` or `section` for \
                 large files; use `convert_to_file` when the Markdown and extracted \
                 images should land on disk."
                    .to_string(),
            )
    }
}

/// Reject inputs the CLI cannot handle before spawning it: local paths must
/// exist and have a supported extension; http(s) URLs pass through.
fn validate_input(input: &str) -> std::result::Result<(), String> {
    if input.starts_with("http://") || input.starts_with("https://") {
        return Ok(());
    }
    let path = Path::new(input);
    if !path.is_file() {
        return Err(format!("not a file: {input}"));
    }
    let ext = extension_of(path);
    if !is_supported_extension(&ext) {
        return Err(format!(
            "unsupported format '.{ext}'; supported: {}",
            SUPPORTED_EXTENSIONS.join(", ")
        ));
    }
    Ok(())
}

/// Locate the `ingestr` CLI: explicit override, then a sibling of this
/// executable, then PATH.
fn locate_ingestr(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        if p.is_file() {
            return Ok(p);
        }
        return Err(anyhow!("ingestr binary not found at {}", p.display()));
    }
    if let Ok(exe) = env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join("ingestr");
        if sibling.is_file() {
            return Ok(sibling);
        }
    }
    if let Some(path) = env::var_os("PATH") {
        for dir in env::split_paths(&path) {
            let candidate = dir.join("ingestr");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(anyhow!(
        "ingestr CLI not found: install ingestr-cli or pass --ingestr-bin / INGESTR_BIN"
    ))
}

fn output_mcp_config(ingestr_bin: &Path) -> Result<()> {
    let mcp_bin = env::current_exe().context("resolving current executable")?;
    let config = serde_json::json!({
        "mcpServers": {
            "ingestr": {
                "command": mcp_bin.display().to_string(),
                "args": [],
                "env": { "INGESTR_BIN": ingestr_bin.display().to_string() }
            }
        }
    });
    println!("{}", serde_json::to_string_pretty(&config)?);
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(&cli.log_level))
        .target(env_logger::Target::Stderr)
        .init();

    let ingestr_bin = locate_ingestr(cli.ingestr_bin)?;
    if cli.show_config {
        return output_mcp_config(&ingestr_bin);
    }

    info!(
        "{APP_NAME} {}: serving over stdio using {}",
        env!("CARGO_PKG_VERSION"),
        ingestr_bin.display()
    );
    let service = IngestrMcp::new(ingestr_bin, Duration::from_secs(cli.timeout))
        .serve(rmcp::transport::stdio())
        .await
        .inspect_err(|e| warn!("serving error: {e:?}"))?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_input_accepts_urls_and_rejects_unknown() {
        assert!(validate_input("https://example.com/report.pdf").is_ok());
        assert!(validate_input("/definitely/missing.pdf").is_err());
        let tmp = env::temp_dir().join("ingestr-mcp-test.exe");
        std::fs::write(&tmp, b"x").ok();
        let err = validate_input(&tmp.display().to_string()).unwrap_err();
        assert!(err.contains("unsupported format"));
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn convert_envelope_parses_cli_json() {
        let env: ConvertEnvelope =
            serde_json::from_str(r##"{"ok":true,"source":"a.pdf","tokens":12,"content":"# A"}"##)
                .unwrap();
        assert!(env.ok);
        assert_eq!(env.tokens, Some(12));
        assert_eq!(env.content, "# A");
    }
}
