//! A pool of PP-OCR engines so pages can OCR in parallel.
//!
//! One `OarOcrEngine` serializes every page behind a mutex. The pool holds N
//! engines, gives each `cores / N` ONNX threads, and hands each page to a free
//! engine; LiteParse already schedules pages concurrently.
//!
//! Default is one engine. Measured on an 8-vCPU i7-8700 VM (8 scanned A4
//! pages): 1 engine 8.5 s, 2 engines 8.1 s, 4 engines 8.8 s, while memory grew
//! ~300 MB per engine. A single engine already saturates the physical cores
//! there, so more engines only help on hosts with many real cores; measure
//! before raising `[processors.ocr] engines`.
//!
//! Pools are cached per process, keyed by model tier and size, so a host that
//! switches between `small` and `medium` gets the model it asked for.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Condvar, LazyLock, Mutex};

use anyhow::{Result, anyhow};
use liteparse::ocr::oar::{OAROCRBuilder, OarOcrEngine};
use liteparse::ocr::{OcrEngine, OcrOptions, OcrResult};
use log::{debug, info};
use oar_ocr::core::config::OrtSessionConfig;

use crate::models::{PpOcrTier, ensure_ppocr};

type OcrFuture<'a> = Pin<
    Box<
        dyn Future<Output = Result<Vec<OcrResult>, Box<dyn std::error::Error + Send + Sync>>>
            + Send
            + 'a,
    >,
>;

/// Pool size for `requested`; 0 means the default of one engine.
pub const fn effective_engines(requested: usize) -> usize {
    if requested == 0 { 1 } else { requested }
}

/// N PP-OCR engines behind one `OcrEngine`.
pub struct OcrPool {
    engines: Vec<OarOcrEngine>,
    free: Mutex<Vec<usize>>,
    available: Condvar,
}

impl std::fmt::Debug for OcrPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OcrPool")
            .field("engines", &self.engines.len())
            .finish_non_exhaustive()
    }
}

/// Returns an engine slot to the pool when dropped (also on error or panic).
struct Lease<'a> {
    pool: &'a OcrPool,
    index: usize,
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        if let Ok(mut free) = self.pool.free.lock() {
            free.push(self.index);
            self.pool.available.notify_one();
        }
    }
}

impl OcrPool {
    fn build(tier: PpOcrTier, engines: usize) -> Result<Self> {
        let models = ensure_ppocr(tier)?;
        let cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
        let threads = (cores / engines).max(1);
        info!(
            "paddle: loading PP-OCRv6 {} x{engines} ({threads} threads each)",
            tier.as_str()
        );
        let built = (0..engines)
            .map(|_| {
                let builder = OAROCRBuilder::new(
                    models.det.as_path(),
                    models.rec.as_path(),
                    std::path::PathBuf::new(),
                )
                .character_dict_content(models.dict.to_string())
                .ort_session(OrtSessionConfig::new().with_intra_threads(threads));
                OarOcrEngine::from_builder(builder).map_err(|e| anyhow!("{e}"))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            free: Mutex::new((0..built.len()).collect()),
            engines: built,
            available: Condvar::new(),
        })
    }

    /// Number of engines in the pool.
    pub fn len(&self) -> usize {
        self.engines.len()
    }

    /// Whether the pool has no engines (never true for a built pool).
    pub fn is_empty(&self) -> bool {
        self.engines.is_empty()
    }

    /// Block until an engine is free. Called from LiteParse's blocking OCR
    /// workers (and our own sync image path), never on an async worker.
    fn lease(&self) -> Result<Lease<'_>, Box<dyn std::error::Error + Send + Sync>> {
        let mut free = self.free.lock().map_err(|_| "OCR pool mutex poisoned")?;
        loop {
            if let Some(index) = free.pop() {
                return Ok(Lease { pool: self, index });
            }
            free = self
                .available
                .wait(free)
                .map_err(|_| "OCR pool mutex poisoned")?;
        }
    }
}

impl OcrEngine for OcrPool {
    fn name(&self) -> &str {
        "oar-ocr-pool"
    }

    fn recognize<'a, 'b: 'a, 'c: 'a>(
        &'a self,
        image_data: &'c [u8],
        width: u32,
        height: u32,
        options: &'b OcrOptions,
    ) -> OcrFuture<'a> {
        Box::pin(async move {
            let waited = std::time::Instant::now();
            let lease = self.lease()?;
            let engine = self
                .engines
                .get(lease.index)
                .ok_or("OCR pool index out of range")?;
            let started = std::time::Instant::now();
            let result = engine.recognize(image_data, width, height, options).await;
            debug!(
                "paddle: engine {} {width}x{height} waited {:?} recognized in {:?}",
                lease.index,
                started.duration_since(waited),
                started.elapsed()
            );
            result
        })
    }
}

type PoolKey = (&'static str, usize);
static POOLS: LazyLock<Mutex<HashMap<PoolKey, Arc<OcrPool>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The shared pool for `tier` with `engines` engines (0 = auto), built once
/// per process and reused for every page and file.
pub fn ocr_pool(tier: PpOcrTier, engines: usize) -> Result<Arc<OcrPool>> {
    let engines = effective_engines(engines);
    let key = (tier.as_str(), engines);
    let mut pools = POOLS
        .lock()
        .map_err(|_| anyhow!("OCR pool cache poisoned"))?;
    if let Some(pool) = pools.get(&key) {
        return Ok(Arc::clone(pool));
    }
    let pool = Arc::new(OcrPool::build(tier, engines)?);
    pools.insert(key, Arc::clone(&pool));
    Ok(pool)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_engine_count_wins() {
        assert_eq!(effective_engines(3), 3);
    }

    #[test]
    fn default_is_one_engine() {
        assert_eq!(effective_engines(0), 1);
    }
}
