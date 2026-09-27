//! Fetch URLs to a temp file for conversion.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};

/// Check if a string looks like a URL
pub fn is_url(input: &str) -> bool {
    input.starts_with("http://") || input.starts_with("https://")
}

/// Fetch a URL and save to a temp file for conversion.
/// Returns the temp file path and a cleanup guard.
pub fn fetch_url(url: &str) -> Result<PathBuf> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(10))
        .user_agent("ingestr/0.1")
        .build()
        .context("building HTTP client")?;

    let response = client
        .get(url)
        .send()
        .with_context(|| format!("fetching URL: {url}"))?;

    if !response.status().is_success() {
        bail!("HTTP {} fetching {}", response.status(), url);
    }

    // Determine extension from Content-Type or URL
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();

    let ext = if content_type.contains("pdf") {
        "pdf"
    } else if content_type.contains("html") {
        "html"
    } else if content_type.contains("json") {
        "json"
    } else if content_type.contains("xml") {
        "xml"
    } else if content_type.contains("wordprocessingml") || content_type.contains("docx") {
        "docx"
    } else if content_type.contains("spreadsheetml") || content_type.contains("xlsx") {
        "xlsx"
    } else if content_type.contains("presentationml") || content_type.contains("pptx") {
        "pptx"
    } else {
        // Try to get extension from URL path
        url.rsplit('/')
            .next()
            .and_then(|segment| segment.rsplit('.').next())
            .filter(|ext| ext.len() <= 5 && ext.chars().all(char::is_alphanumeric))
            .unwrap_or("html")
    };

    let temp_path = std::env::temp_dir().join(format!("ingestr-url.{ext}"));
    let bytes = response.bytes().context("reading response body")?;
    fs::write(&temp_path, &bytes).context("writing fetched content to temp file")?;

    Ok(temp_path)
}
