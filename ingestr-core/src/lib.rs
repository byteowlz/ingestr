//! Core indexing types and full-text search over converted documents.
//!
//! This crate owns the [`tantivy`]-backed index schema and the read/write
//! [`SearchIndex`] handle. Everything document-conversion or MCP-specific lives
//! in the sibling `ingestr-cli` / `ingestr-mcp` crates.

use anyhow::Result;
use serde::Serialize;
use std::fmt;
use std::fs;
use std::path::Path;
use tantivy::{
    DocAddress, Index, IndexReader, IndexWriter, ReloadPolicy, Term,
    collector::TopDocs,
    directory::MmapDirectory,
    query::QueryParser,
    schema::{STORED, STRING, Schema, SchemaBuilder, TEXT, TantivyDocument, Value},
};

/// A single document queued for indexing.
#[derive(Debug, Serialize, Clone)]
pub struct IndexedDocument {
    /// Absolute path of the source (input) file.
    pub source_path: String,
    /// Absolute path of the converted Markdown output file.
    pub output_path: String,
    /// Optional document title extracted at conversion time.
    pub title: Option<String>,
    /// The full converted Markdown/text content.
    pub content: String,
    /// RFC 3339 timestamp of when the conversion happened.
    pub converted_at: Option<String>,
}

/// A single search result returned by [`SearchIndex::search`].
#[derive(Debug, Serialize, Clone)]
pub struct SearchHit {
    /// Tantivy relevance score for this hit.
    pub score: f32,
    /// Absolute path of the source (input) file.
    pub source_path: String,
    /// Absolute path of the converted Markdown output file.
    pub output_path: String,
    /// Optional document title.
    pub title: Option<String>,
    /// RFC 3339 timestamp of the conversion.
    pub converted_at: Option<String>,
}

/// A searchable tantivy index over converted documents.
///
/// An instance is either writable (holds a writer for ingestion) or read-only
/// (opens a reader for searching), never both.
pub struct SearchIndex {
    index: Index,
    reader: Option<IndexReader>,
    writer: Option<IndexWriter>,
    fields: IndexFields,
}

impl fmt::Debug for SearchIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // tantivy's reader/writer do not implement Debug; report presence only.
        f.debug_struct("SearchIndex")
            .field("index", &self.index)
            .field("writable", &self.writer.is_some())
            .field("has_reader", &self.reader.is_some())
            .field("fields", &self.fields)
            .finish()
    }
}

/// Field handles resolved from the index schema.
#[derive(Debug, Clone)]
struct IndexFields {
    source_path: tantivy::schema::Field,
    output_path: tantivy::schema::Field,
    title: tantivy::schema::Field,
    content: tantivy::schema::Field,
    converted_at: tantivy::schema::Field,
}

impl SearchIndex {
    /// Opens (or creates) the index at `path`.
    ///
    /// When `writable` is `true` an index writer is created for ingestion;
    /// otherwise the handle is opened read-only with a searcher reader.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be created, the mmap directory
    /// cannot be opened, or the index cannot be opened/created.
    pub fn open(path: &Path, writable: bool) -> Result<Self> {
        fs::create_dir_all(path)?;
        let schema = build_schema();
        let directory = MmapDirectory::open(path)?;
        let index = Index::open_or_create(directory, schema)?;
        let schema = index.schema();
        let fields = IndexFields {
            source_path: schema.get_field("source_path")?,
            output_path: schema.get_field("output_path")?,
            title: schema.get_field("title")?,
            content: schema.get_field("content")?,
            converted_at: schema.get_field("converted_at")?,
        };

        let reader = if writable {
            None
        } else {
            Some(
                index
                    .reader_builder()
                    .reload_policy(ReloadPolicy::OnCommitWithDelay)
                    .try_into()?,
            )
        };

        let writer = if writable {
            Some(index.writer(50_000_000)?)
        } else {
            None
        };

        Ok(Self {
            index,
            reader,
            writer,
            fields,
        })
    }

    /// Indexes (or replaces) a single document.
    ///
    /// Documents with the same `output_path` are de-duplicated: an existing
    /// document with that key is deleted before the new one is added.
    ///
    /// No-op when this handle was opened read-only.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying tantivy writer cannot delete or add
    /// the document.
    pub fn index_document(&mut self, doc: &IndexedDocument) -> Result<()> {
        let Some(writer) = self.writer.as_mut() else {
            return Ok(());
        };

        // Delete any existing document with the same output_path to avoid duplicates
        let term = Term::from_field_text(self.fields.output_path, &doc.output_path);
        writer.delete_term(term);

        let mut document = TantivyDocument::new();
        document.add_text(self.fields.source_path, &doc.source_path);
        document.add_text(self.fields.output_path, &doc.output_path);
        if let Some(title) = &doc.title {
            document.add_text(self.fields.title, title);
        }
        document.add_text(self.fields.content, &doc.content);
        if let Some(converted_at) = &doc.converted_at {
            document.add_text(self.fields.converted_at, converted_at);
        }

        writer.add_document(document)?;
        Ok(())
    }

    /// Commits pending writes and reloads the searcher.
    ///
    /// # Errors
    ///
    /// Returns an error if the writer commit or the reader reload fails.
    pub fn commit(&mut self) -> Result<()> {
        if let Some(writer) = self.writer.as_mut() {
            writer.commit()?;
        }

        if let Some(reader) = self.reader.as_ref() {
            reader.reload()?;
        }
        Ok(())
    }

    /// Runs a full-text query and returns the top `limit` hits.
    ///
    /// The reader is created lazily on first search if this handle was opened
    /// writable.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails to parse or the search/index access
    /// fails.
    pub fn search(&mut self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        if self.reader.is_none() {
            self.reader = Some(
                self.index
                    .reader_builder()
                    .reload_policy(ReloadPolicy::OnCommitWithDelay)
                    .try_into()?,
            );
        }

        let reader = self
            .reader
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("search index reader not initialized"))?;
        reader.reload()?;
        let searcher = reader.searcher();

        let parser = QueryParser::for_index(
            &self.index,
            vec![
                self.fields.title,
                self.fields.content,
                self.fields.source_path,
                self.fields.output_path,
            ],
        );
        let query = parser.parse_query(query)?;

        let hits: Vec<(f32, DocAddress)> = searcher.search(&query, &TopDocs::with_limit(limit))?;
        let mut results = Vec::with_capacity(hits.len());
        for (score, addr) in hits {
            let retrieved: TantivyDocument = searcher.doc(addr)?;
            let source_path = retrieved
                .get_first(self.fields.source_path)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let output_path = retrieved
                .get_first(self.fields.output_path)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let title = retrieved
                .get_first(self.fields.title)
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let converted_at = retrieved
                .get_first(self.fields.converted_at)
                .and_then(|v| v.as_str())
                .map(str::to_string);

            results.push(SearchHit {
                score,
                source_path,
                output_path,
                title,
                converted_at,
            });
        }

        Ok(results)
    }
}

/// Builds the tantivy index schema used across the workspace.
fn build_schema() -> Schema {
    let mut builder = SchemaBuilder::new();
    builder.add_text_field("source_path", STRING | STORED);
    builder.add_text_field("output_path", STRING | STORED);
    builder.add_text_field("title", TEXT | STORED);
    builder.add_text_field("content", TEXT | STORED);
    builder.add_text_field("converted_at", STRING | STORED);
    builder.build()
}
