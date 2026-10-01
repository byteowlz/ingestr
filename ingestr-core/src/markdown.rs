//! Markdown post-processing: noise cleanup, TOC, section/page filters.

use std::collections::HashMap;
use std::sync::LazyLock;

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::Serialize;

/// Matches a standalone page number line (optionally prefixed with "Page").
#[expect(
    clippy::expect_used,
    reason = "regex pattern is a compile-time literal that is guaranteed valid"
)]
pub static PAGE_NUM_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*(page\s+)?\d+(\s+of\s+\d+)?\s*$").expect("valid static regex")
});
/// Matches 4+ consecutive newlines (noise to collapse).
#[expect(
    clippy::expect_used,
    reason = "regex pattern is a compile-time literal that is guaranteed valid"
)]
pub static MULTI_BLANK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\n{4,}").expect("valid static regex"));
/// Matches a markdown heading block at line start.
#[expect(
    clippy::expect_used,
    reason = "regex pattern is a compile-time literal that is guaranteed valid"
)]
pub static HEADING_BLOCK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(#{1,6})\s+(.+)$").expect("valid static regex"));
/// Matches a markdown heading marker following a newline.
#[expect(
    clippy::expect_used,
    reason = "regex pattern is a compile-time literal that is guaranteed valid"
)]
pub static HEADING_LINE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\n#{1,6}\s").expect("valid static regex"));
/// Matches a page break marker (form-feed, dashes, or asterisks).
#[expect(
    clippy::expect_used,
    reason = "regex pattern is a compile-time literal that is guaranteed valid"
)]
pub static PAGE_BREAK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:\x0c|\n-{3,}\n|\n\* \* \*\n)").expect("valid static regex"));

/// Estimate token count (rough: ~4 chars per token for English text)
pub const fn estimate_tokens(text: &str) -> usize {
    // A simple heuristic: split on whitespace, count words,
    // then apply ~1.3 tokens per word (common for English).
    // For non-English or code-heavy content, chars/4 is more stable.
    text.len().div_ceil(4)
}

/// A Markdown code fence line (```` ``` ```` or ```` ```lang ````).
fn is_fence(trimmed: &str) -> bool {
    trimmed.starts_with("```")
}

/// Clean converted markdown by removing common PDF/document noise.
pub fn clean_markdown(text: &str) -> String {
    let mut lines: Vec<&str> = text.lines().collect();

    // 1. Detect and remove repeated headers/footers.
    // If the same line appears on 3+ "pages" (roughly every 40-80 lines), it is likely a header/footer.
    if lines.len() > 80 {
        let mut freq: HashMap<String, usize> = HashMap::new();
        for line in &lines {
            let trimmed = line.trim();
            if !trimmed.is_empty() && trimmed.len() < 120 && !is_fence(trimmed) {
                *freq.entry(trimmed.to_lowercase()).or_insert(0) += 1;
            }
        }
        let threshold = (lines.len() / 60).max(3);
        let repeated: std::collections::HashSet<String> = freq
            .into_iter()
            .filter(|(_, count)| *count >= threshold)
            .map(|(line, _)| line)
            .collect();

        if !repeated.is_empty() {
            lines.retain(|line| {
                let trimmed = line.trim().to_lowercase();
                !repeated.contains(&trimmed)
            });
        }
    }

    // 2. Remove standalone page numbers (lines that are just a number, optionally with "Page" prefix)
    lines.retain(|line| !PAGE_NUM_RE.is_match(line));

    // Fence lines and everything between them are code: never joined.
    let mut in_code = Vec::with_capacity(lines.len());
    let mut open = false;
    for line in &lines {
        let fence = is_fence(line.trim());
        in_code.push(open || fence);
        if fence {
            open = !open;
        }
    }

    // 3. Fix broken line wraps from PDF column layouts:
    // If a line ends without punctuation or a heading marker and the next starts lowercase, join them.
    let mut result = String::with_capacity(text.len());
    let mut i = 0;
    while i < lines.len() {
        let current = lines[i].trim_end();
        if i + 1 < lines.len() {
            let next = lines[i + 1].trim_start();
            let current_trimmed = current.trim();

            // Join if: current doesn't end with sentence-ender or heading,
            // current is not empty, next starts with lowercase
            let should_join = !in_code[i]
                && !in_code[i + 1]
                && !current_trimmed.is_empty()
                && !next.is_empty()
                && !current_trimmed.ends_with('.')
                && !current_trimmed.ends_with(':')
                && !current_trimmed.ends_with('!')
                && !current_trimmed.ends_with('?')
                && !current_trimmed.ends_with('|')
                && !current_trimmed.starts_with('#')
                && !current_trimmed.starts_with('-')
                && !current_trimmed.starts_with('*')
                && !current_trimmed.starts_with('|')
                && !next.starts_with('#')
                && !next.starts_with('-')
                && !next.starts_with('*')
                && !next.starts_with('|')
                && !next.starts_with('>')
                && next.starts_with(|c: char| c.is_lowercase());

            if should_join {
                result.push_str(current_trimmed);
                result.push(' ');
                i += 1;
                continue;
            }
        }
        result.push_str(current);
        result.push('\n');
        i += 1;
    }

    // 4. Collapse 3+ consecutive blank lines into 2
    let result = MULTI_BLANK_RE.replace_all(&result, "\n\n\n").to_string();

    // 5. Trim leading/trailing whitespace
    result.trim().to_string()
}

// ============================================================
// Document structure: TOC extraction and section retrieval
// ============================================================

/// One heading in a document table of contents.
#[derive(Debug, Clone, Serialize)]
pub struct TocEntry {
    /// Heading level (1-6)
    pub level: usize,
    /// Section number (e.g., "2.1.3")
    pub number: String,
    /// Heading text
    pub title: String,
    /// Character offset where this section starts
    pub char_start: usize,
    /// Character offset where this section ends
    pub char_end: usize,
    /// Estimated token count for this section
    pub tokens: usize,
    /// Whether this section contains a markdown table
    pub has_table: bool,
    /// Whether this section contains a code block
    pub has_code: bool,
}

/// Extract table of contents from markdown content.
pub fn extract_toc(content: &str) -> Vec<TocEntry> {
    let heading_re = &HEADING_BLOCK_RE;
    let mut entries: Vec<TocEntry> = Vec::new();
    let mut counters: Vec<usize> = vec![0; 7]; // index 1-6 for heading levels

    // Find all heading positions
    let mut heading_positions: Vec<(usize, usize, String)> = Vec::new();
    for (offset, line) in content.lines().scan(0usize, |pos, line| {
        let start = *pos;
        *pos += line.len() + 1; // +1 for newline
        Some((start, line))
    }) {
        if let Some(caps) = heading_re.captures(line) {
            let level = caps[1].len();
            let title = caps[2].trim().to_string();
            heading_positions.push((offset, level, title));
        }
    }

    for (idx, (char_start, level, title)) in heading_positions.iter().enumerate() {
        let level = *level;
        let char_start = *char_start;

        // Calculate section end: either start of next heading at same or higher level, or end of doc
        let char_end = heading_positions
            .iter()
            .skip(idx + 1)
            .find(|(_, l, _)| *l <= level)
            .map_or(content.len(), |(pos, _, _)| *pos);

        let section_text = &content[char_start..char_end];

        // Update counters for numbering
        counters[level] += 1;
        // Reset all deeper counters
        for c in counters.iter_mut().skip(level + 1) {
            *c = 0;
        }

        // Build section number
        let number: String = counters[1..=level]
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join(".");

        let tokens = estimate_tokens(section_text);
        let has_table = section_text.contains("\n|") && section_text.contains("---|");
        let has_code = section_text.contains("```");

        entries.push(TocEntry {
            level,
            number,
            title: title.clone(),
            char_start,
            char_end,
            tokens,
            has_table,
            has_code,
        });
    }

    entries
}

/// Format TOC for display
pub fn format_toc(toc: &[TocEntry], total_tokens: usize) -> String {
    let mut out = String::new();
    for entry in toc {
        let indent = "  ".repeat(entry.level.saturating_sub(1));
        let mut markers = Vec::new();
        if entry.has_table {
            markers.push("table");
        }
        if entry.has_code {
            markers.push("code");
        }
        let marker_str = if markers.is_empty() {
            String::new()
        } else {
            format!(", has {}", markers.join("+"))
        };
        out.push_str(&format!(
            "{}{}. {} (tokens: ~{}{})\n",
            indent, entry.number, entry.title, entry.tokens, marker_str
        ));
    }
    out.push_str(&format!(
        "\nTotal: ~{} tokens across {} sections\n",
        total_tokens,
        toc.len()
    ));
    out
}

/// Extract a specific section by number (e.g., "2.1") or by heading text (fuzzy match).
pub fn extract_section(content: &str, selector: &str) -> Option<String> {
    let toc = extract_toc(content);
    if toc.is_empty() {
        return None;
    }

    // Try exact number match first
    if let Some(entry) = toc.iter().find(|e| e.number == selector) {
        return Some(content[entry.char_start..entry.char_end].to_string());
    }

    // Try heading text match (case-insensitive substring)
    let selector_lower = selector.to_lowercase();
    if let Some(entry) = toc
        .iter()
        .find(|e| e.title.to_lowercase().contains(&selector_lower))
    {
        return Some(content[entry.char_start..entry.char_end].to_string());
    }

    None
}

/// Truncate content at a section boundary, respecting a character budget.
pub fn truncate_at_boundary(
    content: &str,
    max_chars: usize,
    offset: usize,
) -> (String, Option<String>) {
    if offset >= content.len() {
        return (String::new(), None);
    }

    let sliced = &content[offset..];
    if sliced.len() <= max_chars {
        return (sliced.to_string(), None);
    }

    // Find the last heading boundary before max_chars
    let heading_re = &HEADING_LINE_RE;
    let mut last_break = max_chars;

    for m in heading_re.find_iter(&sliced[..max_chars]) {
        last_break = m.start();
    }

    // If no heading found, try paragraph break
    if last_break == max_chars
        && let Some(pos) = sliced[..max_chars].rfind("\n\n")
    {
        last_break = pos;
    }

    let truncated = sliced[..last_break].trim_end().to_string();
    let remaining_chars = sliced.len() - last_break;
    let remaining_tokens = estimate_tokens(&sliced[last_break..]);

    let toc = extract_toc(content);
    let total_sections = toc.len();
    let sections_shown = extract_toc(&content[..offset + last_break]).len();

    let notice = format!(
        "\n\n[truncated at char {}, ~{} tokens and ~{} chars remaining, sections {}/{}]",
        offset + last_break,
        remaining_tokens,
        remaining_chars,
        sections_shown,
        total_sections
    );

    (truncated, Some(notice))
}

/// Filter content by page numbers. Looks for page break markers or splits by rough page boundaries.
pub fn filter_pages(content: &str, pages: &[usize]) -> String {
    // Many PDF converters insert form-feed (\x0c) or "---" page breaks.
    // Also look for patterns like "Page N" or just form-feeds.
    let page_break = PAGE_BREAK_RE.split(content);

    let page_texts: Vec<&str> = page_break.collect();

    if page_texts.len() <= 1 {
        // No page breaks found - if the user asked for page 1, return everything
        if pages.contains(&1) {
            return content.to_string();
        }
        // Otherwise, we can't split by pages without markers
        return format!(
            "{content}\n\n[note: no page break markers found in document, showing all content]"
        );
    }

    let mut result = String::new();
    for &page_num in pages {
        if page_num > 0 && page_num <= page_texts.len() {
            if !result.is_empty() {
                result.push_str("\n\n");
            }
            result.push_str(page_texts[page_num - 1].trim());
        }
    }

    if result.is_empty() {
        format!(
            "[no content found for pages {:?}, document has {} pages]",
            pages,
            page_texts.len()
        )
    } else {
        result
    }
}

/// Parse a page range string like "1-3,7,10-12" into a sorted list of page numbers.
pub fn parse_page_range(spec: &str) -> Result<Vec<usize>> {
    let mut pages = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.contains('-') {
            let bounds: Vec<&str> = part.split('-').collect();
            if bounds.len() != 2 {
                bail!("invalid page range: {part}");
            }
            let start: usize = bounds[0].trim().parse().context("invalid page number")?;
            let end: usize = bounds[1].trim().parse().context("invalid page number")?;
            if start == 0 || end == 0 {
                bail!("page numbers start at 1");
            }
            if start > end {
                bail!("invalid range: {start} > {end}");
            }
            for p in start..=end {
                pages.push(p);
            }
        } else {
            let p: usize = part.parse().context("invalid page number")?;
            if p == 0 {
                bail!("page numbers start at 1");
            }
            pages.push(p);
        }
    }
    pages.sort_unstable();
    pages.dedup();
    Ok(pages)
}
