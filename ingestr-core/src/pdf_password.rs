//! Passwords for encrypted PDFs.
//!
//! Most "encrypted" PDFs only restrict printing or copying and open without a
//! password; PDFium reads them as they are. Only a PDF that PDFium refuses
//! with "password required" goes through the candidate list, in order:
//! passwords supplied by the caller (CLI `--password`, `--password-file`,
//! [`PASSWORDS_ENV`]), then the lines printed by `[processors.pdf]
//! password_command`, which runs at most once per process and only when a
//! document needs it. Passwords are never logged and never shown in errors.

use std::fmt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use anyhow::{Result, anyhow, bail};
use liteparse::{LiteParse, LiteParseConfig, LiteParseError};
use log::{info, warn};
use pdfium::PdfiumError;

use crate::pipeline::liteparse_runtime;

/// Environment variable holding PDF passwords, one per line (e.g. injected
/// by `kyz exec`).
pub const PASSWORDS_ENV: &str = "INGESTR_PDF_PASSWORDS";

/// Candidate passwords for encrypted PDFs. Cheap to share; the `Debug` output
/// shows counts only.
#[derive(Default)]
pub struct PdfPasswords {
    supplied: Vec<String>,
    command: Option<String>,
    from_command: OnceLock<Vec<String>>,
}

impl fmt::Debug for PdfPasswords {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PdfPasswords")
            .field("supplied", &self.supplied.len())
            .field("command", &self.command.is_some())
            .finish()
    }
}

impl PdfPasswords {
    /// Passwords to try in order, plus an optional shell command whose
    /// output lines are tried after them.
    #[must_use]
    pub fn new(supplied: Vec<String>, command: Option<String>) -> Self {
        Self {
            supplied: supplied.into_iter().filter(|p| !p.is_empty()).collect(),
            command: command.filter(|c| !c.trim().is_empty()),
            from_command: OnceLock::new(),
        }
    }

    /// Whether any password source is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.supplied.is_empty() && self.command.is_none()
    }

    fn command_passwords(&self) -> &[String] {
        self.from_command.get_or_init(|| {
            let Some(command) = &self.command else {
                return Vec::new();
            };
            match run_command(command) {
                Ok(list) => list,
                Err(e) => {
                    warn!("PDF password command failed: {e:#}");
                    Vec::new()
                }
            }
        })
    }
}

/// Split a password list: one password per line, line endings stripped,
/// empty lines skipped. Other whitespace is kept, since it may be part of a
/// password.
#[must_use]
pub fn parse_password_list(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

fn run_command(command: &str) -> Result<Vec<String>> {
    let (shell, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };
    let output = Command::new(shell)
        .args([flag, command])
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| anyhow!("running password command: {e}"))?;
    if !output.status.success() {
        bail!("password command exited with {}", output.status);
    }
    let text = String::from_utf8(output.stdout)
        .map_err(|_| anyhow!("password command printed non-UTF-8 output"))?;
    Ok(parse_password_list(&text))
}

/// Outcome of opening a PDF.
enum Probe {
    Opened,
    PasswordRequired,
    /// Some other failure; left to the normal conversion path to report.
    Other,
}

fn probe(path: &str, password: Option<&str>) -> Probe {
    let Ok(rt) = liteparse_runtime() else {
        return Probe::Other;
    };
    let parser = LiteParse::new(LiteParseConfig {
        password: password.map(str::to_string),
        target_pages: Some("1".to_string()),
        ocr_enabled: false,
        quiet: true,
        ..Default::default()
    });
    match rt.block_on(parser.is_complex(liteparse::types::PdfInput::Path(path.to_string()))) {
        Ok(_) => Probe::Opened,
        Err(LiteParseError::Pdf(PdfiumError::PasswordRequired)) => Probe::PasswordRequired,
        Err(_) => Probe::Other,
    }
}

/// The password that opens the PDF at `path`: `Ok(None)` if it opens without
/// one (including restriction-only encryption) or can't be checked, an error
/// if it is locked and no candidate works.
pub(crate) fn unlock(path: &Path, passwords: &PdfPasswords) -> Result<Option<String>> {
    let path_str = path
        .to_str()
        .ok_or_else(|| anyhow!("invalid path encoding"))?;
    match probe(path_str, None) {
        Probe::Opened | Probe::Other => return Ok(None),
        Probe::PasswordRequired => {}
    }
    let mut tried = 0;
    for (source, list) in [
        ("supplied", passwords.supplied.as_slice()),
        ("password_command", passwords.command_passwords()),
    ] {
        for (i, candidate) in list.iter().enumerate() {
            tried += 1;
            if let Probe::Opened = probe(path_str, Some(candidate)) {
                info!(
                    "{}: opened with {source} password #{}",
                    path.display(),
                    i + 1
                );
                return Ok(Some(candidate.clone()));
            }
        }
    }
    let how = "Supply one with --password, --password-file, \
        INGESTR_PDF_PASSWORDS or [processors.pdf] password_command";
    if tried == 0 {
        bail!("{} is password-protected. {how}.", path.display());
    }
    bail!(
        "{} is password-protected and none of the {tried} password(s) opened it. {how}.",
        path.display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_list_is_one_per_line() {
        assert_eq!(
            parse_password_list("geheim\r\n\n  spaced \nlast"),
            vec!["geheim", "  spaced ", "last"]
        );
        assert!(parse_password_list("").is_empty());
    }

    #[test]
    fn debug_output_hides_passwords() {
        let p = PdfPasswords::new(vec!["geheim".into()], Some("kyz get x".into()));
        let shown = format!("{p:?}");
        assert!(
            !shown.contains("geheim") && !shown.contains("kyz"),
            "{shown}"
        );
        assert!(!p.is_empty());
        assert!(PdfPasswords::new(vec![String::new()], Some(" ".into())).is_empty());
    }

    #[test]
    fn command_output_becomes_candidates_once() -> Result<()> {
        let p = PdfPasswords::new(Vec::new(), Some("printf 'a\\nb\\n'".into()));
        assert_eq!(p.command_passwords(), ["a", "b"]);
        assert_eq!(p.command_passwords(), ["a", "b"]);
        Ok(())
    }
}
