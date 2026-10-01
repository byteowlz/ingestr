//! Spreadsheets convert with every sheet, including legacy `.xls`
//! (ingestr-6vsx).

use std::path::Path;

use anyhow::Result;
use ingestr_core::pipeline::DocumentProcessor;
use ingestr_core::settings::ProcessorSettings;

fn convert(name: &str) -> Result<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    Ok(DocumentProcessor::new(ProcessorSettings::default())
        .process(&path)?
        .text_content)
}

#[test]
fn xlsx_converts_every_sheet() -> Result<()> {
    let md = convert("multi.xlsx")?;
    for needle in [
        "## Stoffe",
        "| Ethanol | 64-17-5 |",
        "## Lager",
        "| Halle A\\|1 | 120 |",
    ] {
        assert!(md.contains(needle), "missing {needle:?} in:\n{md}");
    }
    Ok(())
}

#[test]
fn legacy_xls_converts_every_sheet() -> Result<()> {
    // Written by xlwt without a class ID: content sniffing calls it "msi".
    let md = convert("old.xls")?;
    for needle in [
        "## Stoffe",
        "| Aceton | 67-64-1 |",
        "## Lager",
        "| Halle B | 7 |",
    ] {
        assert!(md.contains(needle), "missing {needle:?} in:\n{md}");
    }
    Ok(())
}
