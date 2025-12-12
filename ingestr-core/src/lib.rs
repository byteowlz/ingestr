use anyhow::Result;
use serde::Serialize;
use std::fs;
use std::path::Path;
use tantivy::{
    DocAddress, Index, IndexReader, IndexWriter, ReloadPolicy, Term,
    collector::TopDocs,
    directory::MmapDirectory,
    query::QueryParser,
    schema::{STORED, STRING, Schema, SchemaBuilder, TEXT, TantivyDocument, Value},
};

#[derive(Debug, Serialize, Clone)]
pub struct IndexedDocument {
    pub source_path: String,
    pub output_path: String,
    pub title: Option<String>,
    pub content: String,
    pub converted_at: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct SearchHit {
    pub score: f32,
    pub source_path: String,
    pub output_path: String,
    pub title: Option<String>,
    pub converted_at: Option<String>,
}

pub struct SearchIndex {
    index: Index,
    reader: Option<IndexReader>,
    writer: Option<IndexWriter>,
    fields: IndexFields,
}

#[derive(Debug, Clone)]
struct IndexFields {
    source_path: tantivy::schema::Field,
    output_path: tantivy::schema::Field,
    title: tantivy::schema::Field,
    content: tantivy::schema::Field,
    converted_at: tantivy::schema::Field,
}

impl SearchIndex {
    pub fn open(path: &Path, writable: bool) -> Result<Self> {
        fs::create_dir_all(path)?;
        let schema = build_schema();
        let directory = MmapDirectory::open(path)?;
        let index = Index::open_or_create(directory, schema.clone())?;
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

    pub fn commit(&mut self) -> Result<()> {
        if let Some(writer) = self.writer.as_mut() {
            writer.commit()?;
        }

        if let Some(reader) = self.reader.as_ref() {
            reader.reload()?;
        }
        Ok(())
    }

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
            .expect("reader present after initialization");
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

fn build_schema() -> Schema {
    let mut builder = SchemaBuilder::new();
    builder.add_text_field("source_path", STRING | STORED);
    builder.add_text_field("output_path", STRING | STORED);
    builder.add_text_field("title", TEXT | STORED);
    builder.add_text_field("content", TEXT | STORED);
    builder.add_text_field("converted_at", STRING | STORED);
    builder.build()
}
