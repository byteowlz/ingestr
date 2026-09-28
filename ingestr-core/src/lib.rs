//! `ingestr-core`: the shared conversion library behind the `ingestr` CLI and
//! the `ingestr-mcp` server.
//!
//! ingestr converts documents to Markdown; it does not index or search them
//! (see `docs/adr/0003-ingestr-converts-it-does-not-index.md`). This crate is
//! the dependency root for both binaries and the library hosts such as oqto
//! embed: build a [`pipeline::DocumentProcessor`] from
//! [`settings::ProcessorSettings`] and call `process(path)`.

pub mod cache;
pub mod fetch;
pub mod markdown;
pub mod models;
mod ocr_gate;
pub mod ocr_pool;
pub mod pipeline;
pub mod settings;

pub mod formats {
    //! Which file formats ingestr converts, and which engine handles each.

    use std::path::Path;

    /// File extensions ingestr knows how to convert. Used as the default
    /// filter in directory (batch) mode so a mixed pile does not waste time on
    /// binaries, archives or other non-documents.
    pub const SUPPORTED_EXTENSIONS: &[&str] = &[
        // Documents (LiteParse: PDFium / LibreOffice)
        "pdf", "pptx", "ppt", "odp", "key", "docx", "doc", "odt", "xlsx", "xls", "ods",
        // Web / feeds / data (markitdown)
        "html", "htm", "xhtml", "rss", "atom", "xml", "csv", "tsv", "json", "ipynb",
        // Plain text
        "txt", "md", "markdown", "rst", "log", // Images (OCR / VLM paths)
        "png", "jpg", "jpeg", "gif", "bmp", "webp", "tif", "tiff",
    ];

    /// Whether ingestr has a converter for this (lowercase) extension.
    #[must_use]
    pub fn is_supported_extension(ext: &str) -> bool {
        SUPPORTED_EXTENSIONS.contains(&ext)
    }

    /// Whether LiteParse handles this format directly (PDF via PDFium, office
    /// formats via LibreOffice conversion). Spreadsheets (`xlsx`/`xls`) are
    /// deliberately excluded: markitdown renders them as proper Markdown tables
    /// (header row, every sheet), where LiteParse's LibreOffice-to-PDF path
    /// loses the header row and flattens small sheets. Plain images stay on
    /// the markitdown / OCR path.
    #[must_use]
    pub fn is_liteparse_extension(ext: &str) -> bool {
        matches!(
            ext,
            "pdf" | "pptx" | "ppt" | "odp" | "key" | "docx" | "doc" | "odt" | "ods"
        )
    }

    /// Whether a format needs LibreOffice for conversion (office documents).
    #[must_use]
    pub fn is_office_extension(ext: &str) -> bool {
        matches!(
            ext,
            "pptx" | "ppt" | "odp" | "key" | "docx" | "doc" | "odt" | "ods"
        )
    }

    /// Whether a path component denotes a hidden file or directory. The
    /// special `.` and `..` components are not hidden (they are the current
    /// and parent directory), so a relative input like `.` still matches its
    /// own contents.
    #[must_use]
    pub fn is_hidden_component(component: Option<&str>) -> bool {
        component.is_some_and(|s| s.starts_with('.') && s != "." && s != "..")
    }

    /// The lowercased file extension of a path, or an empty string.
    #[must_use]
    pub fn extension_of(path: &Path) -> String {
        path.extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn supported_extensions_cover_documents_not_binaries() {
            assert!(is_supported_extension("pdf"));
            assert!(is_supported_extension("xlsx"));
            assert!(is_supported_extension("md"));
            assert!(is_supported_extension("png"));
            assert!(!is_supported_extension("exe"));
            assert!(!is_supported_extension("zip"));
            assert!(!is_supported_extension("bin"));
        }

        #[test]
        fn liteparse_extension_covers_pdf_and_office() {
            assert!(is_liteparse_extension("pdf"));
            assert!(is_liteparse_extension("pptx"));
            assert!(is_liteparse_extension("key"));
            assert!(is_liteparse_extension("docx"));
            // Spreadsheets go to markitdown (better tables, no LibreOffice needed).
            assert!(!is_liteparse_extension("xlsx"));
            assert!(!is_liteparse_extension("xls"));
            assert!(!is_liteparse_extension("png"));
            assert!(!is_liteparse_extension("html"));
            assert!(!is_liteparse_extension("csv"));
        }

        #[test]
        fn office_extension_requires_libreoffice() {
            assert!(is_office_extension("pptx"));
            assert!(is_office_extension("docx"));
            assert!(!is_office_extension("xlsx"));
            assert!(!is_office_extension("pdf"));
            assert!(!is_office_extension("png"));
        }

        #[test]
        fn hidden_component_ignores_dot_and_dotdot() {
            assert!(is_hidden_component(Some(".git")));
            assert!(is_hidden_component(Some(".env")));
            assert!(!is_hidden_component(Some(".")));
            assert!(!is_hidden_component(Some("..")));
            assert!(!is_hidden_component(Some("src")));
            assert!(!is_hidden_component(None));
        }

        #[test]
        fn extension_of_is_lowercase_or_empty() {
            assert_eq!(extension_of(Path::new("a/b/report.PDF")), "pdf");
            assert_eq!(extension_of(Path::new("deck.PpTx")), "pptx");
            assert_eq!(extension_of(Path::new("noext")), "");
        }
    }
}
