//! Auto OCR (ingestr-13pa): with default settings only pages without usable
//! text are OCR'd; text pages keep their native text, and `--no-ocr` (OCR
//! off) leaves a scan empty.
//!
//! Fixture `mixed-scan.pdf`: two text pages followed by a scan rotated 90°.
//! Needs the PP-OCR and orientation models (fetched from Hugging Face on the
//! first run, or `ingestr doctor --fetch`).

use std::path::{Path, PathBuf};

use anyhow::Result;
use ingestr_core::pipeline::DocumentProcessor;
use ingestr_core::settings::{OcrBackend, ProcessorSettings};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[test]
fn ocr_is_on_by_default_and_only_touches_scanned_pages() -> Result<()> {
    let path = fixture("mixed-scan.pdf");
    let native = DocumentProcessor::new(ProcessorSettings::default())
        .with_ocr(false, OcrBackend::Paddle, None)
        .process(&path)?;
    let auto = DocumentProcessor::new(ProcessorSettings::default()).process(&path)?;

    // The text pages come through unchanged ...
    for line in ["Einatmen: An die frische Luft bringen.", "Seite zwei"] {
        assert!(
            native.text_content.contains(line),
            "{}",
            native.text_content
        );
        assert!(auto.text_content.contains(line), "{}", auto.text_content);
    }
    // ... and the sideways scan is read upright, only with OCR.
    assert!(!native.text_content.contains("Falcon"));
    assert!(
        auto.text_content.contains("Falcon"),
        "{}",
        auto.text_content
    );
    assert!(auto.text_content.contains("Emilio Sanchez"));
    Ok(())
}
