//! Forced `--engine markitdown` must convert DOCX (ingestr-ayq3).
//!
//! Regression for the vendored markitdown crate: OOXML packages are ZIP
//! containers, so content sniffing reported plain `.zip` and routed DOCX into
//! the generic ZIP listing branch. That branch crashed on nested members
//! (`_rels/.rels`, `word/document.xml`) because it never created parent
//! directories, and the pipeline then surfaced the swallowed failure as a
//! misleading "could not be read as UTF-8" error.

use std::io::Write as _;
use std::path::Path;

use anyhow::Result;
use ingestr_core::pipeline::DocumentProcessor;
use ingestr_core::settings::{ConvertEngine, ProcessorSettings};
use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

const CONTENT_TYPES_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#;

const ROOT_RELS_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;

const DOCUMENT_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>Synthetic DOCX dependency diagnostic</w:t></w:r></w:p><w:p><w:r><w:t>This document contains no private information.</w:t></w:r></w:p><w:sectPr><w:pgSz w:w="11906" w:h="16838"/></w:sectPr></w:body></w:document>"#;

/// Write a minimal but ordinary DOCX package layout, including nested
/// members (`_rels/.rels`, `word/document.xml`).
fn write_synthetic_docx(path: &Path) -> Result<()> {
    let file = std::fs::File::create(path)?;
    let mut zip = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    zip.add_directory("_rels", options)?;
    zip.add_directory("word", options)?;
    zip.start_file("[Content_Types].xml", options)?;
    zip.write_all(CONTENT_TYPES_XML.as_bytes())?;
    zip.start_file("_rels/.rels", options)?;
    zip.write_all(ROOT_RELS_XML.as_bytes())?;
    zip.start_file("word/document.xml", options)?;
    zip.write_all(DOCUMENT_XML.as_bytes())?;
    zip.finish()?;
    Ok(())
}

/// Write a plain ZIP archive whose members live in nested directories,
/// including a CSV that one of the converters can handle.
fn write_nested_zip(path: &Path) -> Result<()> {
    let file = std::fs::File::create(path)?;
    let mut zip = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    zip.add_directory("_rels", options)?;
    zip.add_directory("data", options)?;
    zip.add_directory("data/nested", options)?;
    zip.start_file("_rels/.rels", options)?;
    zip.write_all(ROOT_RELS_XML.as_bytes())?;
    zip.start_file("data/nested/table.csv", options)?;
    zip.write_all(b"col-a,col-b\n1,2\n")?;
    zip.finish()?;
    Ok(())
}

fn detect(path: &Path) -> Option<String> {
    markitdown::MarkItDown::new().detect_file_type(&path.to_str()?.to_string())
}

#[test]
fn detection_trusts_ooxml_name_over_zip_sniff() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let docx = dir.path().join("input.docx");
    write_synthetic_docx(&docx)?;
    assert_eq!(
        detect(&docx).as_deref(),
        Some(".docx"),
        "an OOXML package must not be detected as plain .zip"
    );
    Ok(())
}

#[test]
fn detection_keeps_plain_zip_and_spreadsheet_routing() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let zip_path = dir.path().join("plain.zip");
    write_nested_zip(&zip_path)?;
    assert_eq!(detect(&zip_path).as_deref(), Some(".zip"));

    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    assert_eq!(
        detect(&fixtures.join("multi.xlsx")).as_deref(),
        Some(".xlsx"),
        "xlsx must keep reaching the spreadsheet converter"
    );
    assert_eq!(detect(&fixtures.join("old.xls")).as_deref(), Some(".xls"));
    Ok(())
}

#[test]
fn forced_markitdown_converts_docx() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let docx = dir.path().join("input.docx");
    write_synthetic_docx(&docx)?;

    let md = DocumentProcessor::new(ProcessorSettings::default())
        .with_engine(ConvertEngine::Markitdown)
        .process(&docx)?
        .text_content;

    assert!(
        md.contains("Synthetic DOCX dependency diagnostic"),
        "missing first paragraph in:\n{md}"
    );
    assert!(
        md.contains("This document contains no private information."),
        "missing second paragraph in:\n{md}"
    );
    Ok(())
}

#[test]
fn zip_with_nested_members_converts_without_enoent() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let zip_path = dir.path().join("nested.zip");
    write_nested_zip(&zip_path)?;

    let md = DocumentProcessor::new(ProcessorSettings::default())
        .with_engine(ConvertEngine::Markitdown)
        .process(&zip_path)?
        .text_content;

    assert!(
        md.contains("Content from the zip file nested.zip"),
        "missing zip header in:\n{md}"
    );
    assert!(
        md.contains("## File: data/nested/table.csv"),
        "nested CSV member not converted in:\n{md}"
    );
    Ok(())
}

#[test]
fn default_route_converts_docx() -> Result<()> {
    // The default route goes through LiteParse/LibreOffice; it must keep
    // working and must not depend on a markitdown CLI executable.
    let dir = tempfile::tempdir()?;
    let docx = dir.path().join("input.docx");
    write_synthetic_docx(&docx)?;

    let md = DocumentProcessor::new(ProcessorSettings::default())
        .process(&docx)?
        .text_content;

    assert!(md.contains("Synthetic DOCX dependency diagnostic"), "{md}");
    assert!(
        md.contains("This document contains no private information."),
        "{md}"
    );
    Ok(())
}

#[test]
fn broken_docx_surfaces_conversion_failure_not_utf8() -> Result<()> {
    // A truncated ZIP container named .docx: the parser genuinely fails
    // (unlike a valid zip merely missing word/document.xml, which docx_rust
    // tolerates as an empty document). The terminal error must name the
    // conversion, not an unrelated UTF-8 decoding problem or a false
    // permissions diagnosis.
    let dir = tempfile::tempdir()?;
    let broken = dir.path().join("broken.docx");
    write_nested_zip(&broken)?;
    let bytes = std::fs::read(&broken)?;
    // Cut off the central directory so the archive cannot be opened.
    std::fs::write(&broken, &bytes[..bytes.len() / 2])?;

    let err = DocumentProcessor::new(ProcessorSettings::default())
        .with_engine(ConvertEngine::Markitdown)
        .process(&broken)
        .expect_err("conversion must fail for a DOCX without document.xml");
    let chain = format!("{err:#}");

    assert!(
        chain.contains("conversion failed for"),
        "error must name the conversion failure, got:\n{chain}"
    );
    assert!(
        !chain.contains("UTF-8"),
        "error must not be a UTF-8 decoding diagnosis, got:\n{chain}"
    );
    assert!(
        !chain.contains("permissions"),
        "error must not falsely diagnose file permissions, got:\n{chain}"
    );
    Ok(())
}
