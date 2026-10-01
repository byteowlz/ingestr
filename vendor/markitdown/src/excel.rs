use calamine::{open_workbook_auto, open_workbook_auto_from_rs, Reader, Sheets};
use std::io::{Cursor, Read, Seek};
use std::path::Path;

use crate::error::MarkitdownError;
use crate::model::{ConversionOptions, DocumentConverter, DocumentConverterResult};

pub struct ExcelConverter;

const EXTENSIONS: [&str; 5] = [".xlsx", ".xls", ".xlsm", ".xlsb", ".ods"];

fn check_extension(args: &Option<ConversionOptions>) -> Result<(), MarkitdownError> {
    if let Some(ext) = args.as_ref().and_then(|o| o.file_extension.as_deref()) {
        if !EXTENSIONS.contains(&ext) {
            return Err(MarkitdownError::InvalidFile(format!(
                "Expected a spreadsheet ({}), got {}",
                EXTENSIONS.join(", "),
                ext
            )));
        }
    }
    Ok(())
}

/// Render every non-empty sheet as a Markdown table under a `## <sheet>`
/// heading (the heading is omitted for single-sheet workbooks).
fn workbook_to_markdown<RS: Read + Seek>(
    workbook: &mut Sheets<RS>,
) -> Result<String, MarkitdownError> {
    let names = workbook.sheet_names();
    let multi = names.len() > 1;
    let mut markdown = String::new();
    for name in names {
        let range = workbook
            .worksheet_range(&name)
            .map_err(|e| MarkitdownError::ParseError(format!("Failed to read sheet {name}: {e}")))?;
        let rows: Vec<Vec<String>> = range
            .rows()
            .map(|row| row.iter().map(|cell| cell.to_string().replace('|', "\\|")).collect())
            .collect();
        if rows.is_empty() {
            continue;
        }
        if !markdown.is_empty() {
            markdown.push('\n');
        }
        if multi {
            markdown.push_str(&format!("## {name}\n\n"));
        }
        markdown.push('|');
        for cell in &rows[0] {
            markdown.push_str(&format!(" {} |", cell));
        }
        markdown.push_str("\n|");
        for _ in &rows[0] {
            markdown.push_str(" --- |");
        }
        markdown.push('\n');
        for row in rows.iter().skip(1) {
            markdown.push('|');
            for cell in row {
                markdown.push_str(&format!(" {} |", cell));
            }
            markdown.push('\n');
        }
    }
    Ok(markdown)
}

impl DocumentConverter for ExcelConverter {
    fn convert(
        &self,
        local_path: &str,
        args: Option<ConversionOptions>,
    ) -> Result<DocumentConverterResult, MarkitdownError> {
        check_extension(&args)?;
        let path = Path::new(local_path);
        log::debug!("Opening file: {:#?}", path);
        let mut workbook = open_workbook_auto(path)
            .map_err(|e| MarkitdownError::ParseError(format!("Failed to open spreadsheet: {}", e)))?;
        Ok(DocumentConverterResult {
            title: None,
            text_content: workbook_to_markdown(&mut workbook)?,
        })
    }

    fn convert_bytes(
        &self,
        bytes: &[u8],
        args: Option<ConversionOptions>,
    ) -> Result<DocumentConverterResult, MarkitdownError> {
        check_extension(&args)?;
        let mut workbook = open_workbook_auto_from_rs(Cursor::new(bytes.to_vec()))
            .map_err(|e| MarkitdownError::ParseError(format!("Failed to open spreadsheet: {}", e)))?;
        Ok(DocumentConverterResult {
            title: None,
            text_content: workbook_to_markdown(&mut workbook)?,
        })
    }
}
