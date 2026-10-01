//! Encrypted PDFs (ingestr-98ja): restriction-only PDFs open without a
//! password; locked ones open with the first working candidate password.
//!
//! Fixtures (reportlab + pypdf): `open.pdf` AES-256 with an owner password
//! only, `locked.pdf` AES-256 and `locked-rc4.pdf` RC4-128 with the user
//! password "geheim".

use std::path::{Path, PathBuf};

use anyhow::Result;
use ingestr_core::pipeline::DocumentProcessor;
use ingestr_core::settings::ProcessorSettings;

const TEXT: &str = "Einatmen: An die frische Luft bringen.";

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn processor(command: Option<&str>) -> DocumentProcessor {
    let mut settings = ProcessorSettings::default();
    settings.processors.pdf.password_command = command.map(str::to_string);
    DocumentProcessor::new(settings)
}

#[test]
fn restriction_only_pdf_opens_without_password() -> Result<()> {
    let doc = processor(None).process(&fixture("open.pdf"))?;
    assert!(doc.text_content.contains(TEXT), "{}", doc.text_content);
    Ok(())
}

#[test]
fn locked_pdf_without_working_password_fails_clearly() {
    for passwords in [vec![], vec!["Wrong1Pw".to_string()]] {
        let err = processor(None)
            .with_pdf_passwords(passwords)
            .process(&fixture("locked.pdf"))
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(err.contains("password-protected"), "{err}");
        assert!(!err.contains("Wrong1Pw"), "password leaked: {err}");
    }
}

#[test]
fn locked_pdf_opens_with_a_later_candidate() -> Result<()> {
    for name in ["locked.pdf", "locked-rc4.pdf"] {
        let doc = processor(None)
            .with_pdf_passwords(vec!["nope".into(), "geheim".into()])
            .process(&fixture(name))?;
        assert!(
            doc.text_content.contains(TEXT),
            "{name}: {}",
            doc.text_content
        );
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn locked_pdf_opens_with_password_command() -> Result<()> {
    let doc = processor(Some("printf 'nope\\ngeheim\\n'")).process(&fixture("locked.pdf"))?;
    assert!(doc.text_content.contains(TEXT), "{}", doc.text_content);
    Ok(())
}
