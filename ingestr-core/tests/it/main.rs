//! Integration tests, linked into one binary: each test binary statically
//! links ONNX Runtime and PDFium, so separate files multiply link time and
//! memory.

mod encrypted_pdfs;
mod spreadsheets;
