use std::{path::Path, sync::Mutex};

use anyhow::{Context, Result};
use tree_sitter::{Language, Parser, Query, WasmStore};
use wasmtime::{Cache as CompilationCache, CacheConfig, Config, Engine};

use crate::Workers;

/// A Wasmtime engine and large-stack workers shared by a registry's grammars.
#[derive(Clone)]
pub struct Runtime {
    pub engine: Engine,
    pub workers: Workers,
}

impl Runtime {
    pub fn new(cache_directory: &Path) -> Result<Self> {
        Ok(Self {
            engine: runtime_engine(cache_directory)?.0,
            workers: Workers::new()?,
        })
    }
}

pub(crate) fn runtime_engine(directory: &Path) -> Result<(Engine, Option<CompilationCache>)> {
    let mut cache_config = CacheConfig::new();
    cache_config.with_directory(directory.join("compiled"));
    let cache = match CompilationCache::new(cache_config) {
        Ok(cache) => Some(cache),
        Err(error) => {
            log::warn!("Could not enable compiled grammar cache; using uncached WASM: {error:#}");
            None
        }
    };
    let mut config = Config::new();
    config.cache(cache.clone());
    Ok((Engine::new(&config)?, cache))
}

pub struct Grammar {
    /// Identifies a grammar within a registry, suitable for in-memory caches.
    pub key: String,
    pub name: String,
    pub language: Language,
    /// An empty query when the registry has no highlights for this language.
    pub query: Query,
    pub injections: Option<Query>,
    pub engine: Engine,
    pub workers: Workers,
    pub(crate) prepared_parser: Mutex<Option<Parser>>,
}

impl Grammar {
    /// Adapt a native grammar to the same parser and worker API as WASM grammars.
    pub fn native(
        name: &str,
        language: Language,
        highlights: &str,
        injections: &str,
        runtime: &Runtime,
    ) -> Result<Self> {
        anyhow::ensure!(!language.is_wasm(), "Expected a native grammar for {name}");
        let query = Query::new(&language, highlights)
            .with_context(|| format!("Invalid highlight query for {name}"))?;
        let injections = if injections.is_empty() {
            None
        } else {
            Some(
                Query::new(&language, injections)
                    .with_context(|| format!("Invalid injection query for {name}"))?,
            )
        };
        Ok(Self {
            key: format!("native:{name}"),
            name: name.to_owned(),
            language,
            query,
            injections,
            engine: runtime.engine.clone(),
            workers: runtime.workers.clone(),
            prepared_parser: Mutex::new(None),
        })
    }

    /// Call on this grammar's workers so WASM initialization has enough stack.
    pub fn new_parser(&self) -> Result<Parser> {
        if let Some(parser) = self
            .prepared_parser
            .lock()
            .map_err(|_| anyhow::anyhow!("Prepared parser lock poisoned"))?
            .take()
        {
            return Ok(parser);
        }
        let mut parser = Parser::new();
        if self.language.is_wasm() {
            parser.set_wasm_store(WasmStore::new(&self.engine)?)?;
        }
        parser.set_language(&self.language)?;
        Ok(parser)
    }
}
