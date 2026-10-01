use calamine::{open_workbook_auto, open_workbook_auto_from_rs, Reader};
use std::io::Cursor;
use std::path::Path;

use crate::error::MarkitdownError;
use crate::model::{ConversionOptions, DocumentConverter, DocumentConverterResult};

/// Render the first worksheet of any calamine `Reader` (Xlsx or legacy Xls)
/// into a Markdown table, preserving the header row.
fn render_worksheet<R, RS>(workbook: &mut R) -> String
where
    R: Reader<RS>,
    RS: std::io::Read + std::io::Seek,
{
    let mut markdown = String::new();

    if let Some(Ok(range)) = workbook.worksheet_range_at(0) {
        let rows: Vec<Vec<String>> = range
            .rows()
            .map(|row| row.iter().map(|cell| cell.to_string()).collect())
            .collect();

        if rows.is_empty() {
            return markdown;
        }

        markdown.push_str("|");
        for cell in &rows[0] {
            markdown.push_str(&format!(" {} |", cell));
        }
        markdown.push_str("\n|");

        for _ in &rows[0] {
            markdown.push_str(" --- |");
        }
        markdown.push_str("\n");

        for row in rows.iter().skip(1) {
            markdown.push_str("|");
            for cell in row {
                markdown.push_str(&format!(" {} |", cell));
            }
            markdown.push_str("\n");
        }
    }

    markdown
}

fn is_excel_ext(ext: Option<&str>) -> bool {
    matches!(
        ext,
        Some(".xlsx") | Some(".xls") | Some(".xlsm") | Some(".xlsb")
    )
}

pub struct ExcelConverter;

impl DocumentConverter for ExcelConverter {
    fn convert(
        &self,
        local_path: &str,
        args: Option<ConversionOptions>,
    ) -> Result<DocumentConverterResult, MarkitdownError> {
        if let Some(opts) = &args {
            if let Some(ext) = &opts.file_extension {
                if !is_excel_ext(Some(ext)) {
                    return Err(MarkitdownError::InvalidFile(
                        format!("Expected an Excel file, got {}", ext)
                    ));
                }
            }
        }

        let path = Path::new(local_path);
        log::debug!("Opening file: {:#?}", path);
        // Auto-detects the workbook format (XlsxOLE2 legacy Xls, xlsx, xlsb) so
        // old binary .xls files are no longer rejected by the Xlsx-only reader.
        let mut workbook = open_workbook_auto(path)
            .map_err(|e| MarkitdownError::ParseError(format!("Failed to open Excel file: {}", e)))?;

        Ok(DocumentConverterResult {
            title: None,
            text_content: render_worksheet(&mut workbook),
        })
    }

    fn convert_bytes(
        &self,
        bytes: &[u8],
        args: Option<ConversionOptions>,
    ) -> Result<DocumentConverterResult, MarkitdownError> {
        if let Some(opts) = &args {
            if let Some(ext) = &opts.file_extension {
                if !is_excel_ext(Some(ext)) {
                    return Err(MarkitdownError::InvalidFile(
                        format!("Expected an Excel file, got {}", ext)
                    ));
                }
            }
        }
        let reader = Cursor::new(bytes);
        // Auto-detect the workbook format (xlsx / legacy xls / xlsb) from bytes.
        let mut workbook = open_workbook_auto_from_rs(reader)
            .map_err(|e| MarkitdownError::ParseError(format!("Failed to open Excel file: {}", e)))?;

        Ok(DocumentConverterResult {
            title: None,
            text_content: render_worksheet(&mut workbook),
        })
    }
}
