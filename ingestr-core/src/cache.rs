//! Content-addressed conversion cache under `$XDG_CACHE_HOME/ingestr`.

use std::env;
use std::fs;
use std::path::PathBuf;

use anyhow::{Result, bail};

/// The conversion cache root (`$XDG_CACHE_HOME/ingestr` or `~/.cache/ingestr`).
pub fn cache_dir() -> Result<PathBuf> {
    let dir = if let Some(cache) = env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
        PathBuf::from(cache).join("ingestr")
    } else if let Some(home) = dirs::home_dir() {
        home.join(".cache").join("ingestr")
    } else {
        bail!("unable to determine cache directory");
    };
    Ok(dir)
}

/// Compute a cache key from file hash + conversion flags.
/// Bump when conversion output changes for the same input+flags (new engine
/// routing, new OCR default, changed post-processing) so stale cache entries
/// are never served as hits.
pub const CACHE_SCHEMA: &str = "ingestr-cache-v2";

/// Try to read a cached conversion result.
pub fn cache_get(key: &str) -> Result<Option<String>> {
    let path = cache_dir()?.join(&key[..2]).join(key);
    if path.exists() {
        Ok(Some(fs::read_to_string(path)?))
    } else {
        Ok(None)
    }
}

/// Store a conversion result in the cache.
pub fn cache_put(key: &str, content: &str) -> Result<()> {
    let dir = cache_dir()?.join(&key[..2]);
    fs::create_dir_all(&dir)?;
    fs::write(dir.join(key), content)?;
    Ok(())
}
