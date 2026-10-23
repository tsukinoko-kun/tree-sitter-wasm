use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{Grammar, Runtime};
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use regex::Regex;
use reqwest::{StatusCode, blocking::Client, header};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tree_sitter::{Parser, Query, WasmStore};
use url::Url;

const REGISTRY_URL: &str = "https://tsukinoko-kun.github.io/tree-sitter-wasm/";
const REFRESH_INTERVAL: u64 = 24 * 60 * 60;
const RETRY_INTERVAL: u64 = 60 * 60;
const INDEX_LIMIT: u64 = 4 * 1024 * 1024;
const ARTIFACT_LIMIT: u64 = 64 * 1024 * 1024;

/// Default cache shared by all applications running as the current user.
pub fn default_cache_dir() -> Result<PathBuf> {
    Ok(dirs::cache_dir()
        .context("Could not locate the user cache directory")?
        .join("tree-sitter-wasm")
        .join("v1"))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Network {
    #[default]
    Online,
    /// Never make network requests. Missing or corrupt artifacts return an error.
    Offline,
}

#[derive(Default)]
pub struct RegistryBuilder {
    directory: Option<PathBuf>,
    url: Option<Url>,
    network: Network,
}

impl RegistryBuilder {
    pub fn cache_dir(mut self, directory: impl Into<PathBuf>) -> Self {
        self.directory = Some(directory.into());
        self
    }

    /// Base URL containing index.json. Must end with a slash.
    pub fn registry_url(mut self, url: Url) -> Self {
        self.url = Some(url);
        self
    }

    pub fn network(mut self, network: Network) -> Self {
        self.network = network;
        self
    }

    pub fn build(self) -> Result<Registry> {
        let directory = match self.directory {
            Some(directory) => directory,
            None => default_cache_dir()?,
        };
        let base_url = self.url.unwrap_or(Url::parse(REGISTRY_URL)?);
        ensure!(
            matches!(base_url.scheme(), "http" | "https")
                && base_url.host_str().is_some()
                && base_url.path().ends_with('/')
                && base_url.query().is_none()
                && base_url.fragment().is_none()
                && base_url.username().is_empty()
                && base_url.password().is_none(),
            "Registry URL must be an HTTP(S) directory URL without credentials, query, or fragment: {base_url}"
        );
        let namespace = directory.join("registries").join(format!(
            "{:x}",
            Sha256::digest(base_url.as_str().as_bytes())
        ));
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&namespace)
            .with_context(|| format!("Could not create {}", namespace.display()))?;
        let runtime = Runtime::new(&directory)?;
        Ok(Registry {
            cache: Arc::new(Mutex::new(Cache {
                directory,
                namespace,
                base_url,
                client: Client::builder()
                    .user_agent(concat!("tree-sitter-wasm/", env!("CARGO_PKG_VERSION")))
                    .connect_timeout(Duration::from_secs(5))
                    .timeout(Duration::from_secs(30))
                    .build()?,
                network: self.network,
                manifest: None,
                status: Status::default(),
                warning: None,
                loaded: HashMap::new(),
                failures: HashMap::new(),
                runtime: runtime.clone(),
            })),
            runtime,
        })
    }
}

/// A cloneable, blocking registry loader. Construction does not use the network.
#[derive(Clone)]
pub struct Registry {
    cache: Arc<Mutex<Cache>>,
    runtime: Runtime,
}

pub struct Resolution {
    /// None means that the index has no matching language.
    pub grammar: Option<Arc<Grammar>>,
    /// A refresh failed, but a cached index remains usable.
    pub warning: Option<String>,
}

struct Cache {
    directory: PathBuf,
    namespace: PathBuf,
    base_url: Url,
    client: Client,
    network: Network,
    manifest: Option<Manifest>,
    status: Status,
    warning: Option<String>,
    loaded: HashMap<String, Arc<Grammar>>,
    failures: HashMap<String, Failure>,
    runtime: Runtime,
}

struct Failure {
    attempted_at: u64,
    message: String,
    missing: Vec<Artifact>,
}

#[derive(Default, Deserialize, Serialize)]
struct Status {
    checked_at: u64,
    attempted_at: u64,
    etag: Option<String>,
    last_modified: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct StoredRegistry {
    manifest: Option<Manifest>,
    status: Status,
    warning: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct Manifest {
    schema_version: u32,
    languages: BTreeMap<String, Entry>,
}

#[derive(Deserialize, Serialize)]
struct Entry {
    language: String,
    aliases: Vec<String>,
    abi_version: usize,
    files: FileTypes,
    wasm: Artifact,
    #[serde(default)]
    queries: BTreeMap<String, Artifact>,
    license: Artifact,
    #[serde(default)]
    dependency_licenses: BTreeMap<String, Artifact>,
}

#[derive(Default, Deserialize, Serialize)]
struct FileTypes {
    #[serde(default)]
    extensions: Vec<String>,
    #[serde(default)]
    names: Vec<String>,
    first_line_regex: Option<String>,
    content_regex: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
struct Artifact {
    path: String,
    sha256: String,
    size: u64,
}

impl Registry {
    pub fn new() -> Result<Self> {
        Self::builder().build()
    }

    pub fn builder() -> RegistryBuilder {
        RegistryBuilder::default()
    }

    pub fn with_cache_dir(directory: impl Into<PathBuf>) -> Result<Self> {
        Self::builder().cache_dir(directory).build()
    }

    pub fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    /// Detect a language by filename, extension, shebang, and sample contents.
    /// Do not call from this registry's parser workers.
    pub fn resolve(&self, path: &Path, sample: &str) -> Result<Resolution> {
        self.resolve_with(|manifest| manifest.detect(path, sample))
    }

    /// Resolve a declared language alias, ignoring case and surrounding whitespace.
    /// Do not call from this registry's parser workers: loading waits on that pool.
    pub fn resolve_language(&self, label: &str) -> Result<Resolution> {
        self.resolve_with(|manifest| Ok(manifest.detect_language(label)))
    }

    fn resolve_with(
        &self,
        select: impl FnOnce(&Manifest) -> Result<Option<(&str, &Entry)>>,
    ) -> Result<Resolution> {
        // Serializing cache work also coalesces requests from different documents.
        // This lock is only taken on background threads, never WASM workers.
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| anyhow::anyhow!("Grammar cache lock poisoned"))?;
        let _lock = cache.lock_disk()?;
        cache.restore()?;
        cache.refresh()?;
        let manifest = cache
            .manifest
            .as_ref()
            .context("No cached grammar registry is available")?;
        let Some((name, entry)) = select(manifest)? else {
            return Ok(Resolution {
                grammar: None,
                warning: cache.warning.clone(),
            });
        };
        let key = format!("{name}:{:x}", Sha256::digest(serde_json::to_vec(entry)?));
        if let Some(grammar) = cache.loaded.get(&key) {
            return Ok(Resolution {
                grammar: Some(grammar.clone()),
                warning: cache.warning.clone(),
            });
        }
        if let Some(failure) = cache.failures.get(&key)
            && now().saturating_sub(failure.attempted_at) < RETRY_INTERVAL
            && failure
                .missing
                .iter()
                .all(|artifact| cache.cached_artifact(artifact).is_none())
        {
            bail!("{}", failure.message);
        }
        let result = cache.load(name, entry, key.clone());
        match result {
            Ok(grammar) => {
                let grammar = Arc::new(grammar);
                cache.loaded.insert(key.clone(), grammar.clone());
                cache.failures.remove(&key);
                Ok(Resolution {
                    grammar: Some(grammar),
                    warning: cache.warning.clone(),
                })
            }
            Err(error) => {
                let missing = entry
                    .artifacts()
                    .filter(|artifact| cache.cached_artifact(artifact).is_none())
                    .cloned()
                    .collect();
                cache.failures.insert(
                    key,
                    Failure {
                        attempted_at: now(),
                        message: format!("{error:#}"),
                        missing,
                    },
                );
                Err(error)
            }
        }
    }
}

impl Cache {
    fn lock_disk(&self) -> Result<fs::File> {
        let path = self.namespace.join("registry.lock");
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("Could not open {}", path.display()))?;
        file.lock_exclusive()
            .with_context(|| format!("Could not lock {}", path.display()))?;
        Ok(file)
    }

    fn restore(&mut self) -> Result<()> {
        let path = self.namespace.join("index.json");
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| format!("Could not read {}", path.display()));
            }
        };
        let stored = serde_json::from_slice::<StoredRegistry>(&bytes)
            .context("Invalid cached grammar registry")
            .and_then(|stored| {
                if let Some(manifest) = &stored.manifest {
                    validate_manifest(manifest)?;
                }
                Ok(stored)
            });
        match stored {
            Ok(stored) => {
                self.manifest = stored.manifest;
                self.status = stored.status;
                self.warning = stored.warning;
            }
            Err(error) => log::warn!("Ignoring invalid cache {}: {error:#}", path.display()),
        }
        Ok(())
    }

    fn refresh(&mut self) -> Result<()> {
        if self.network == Network::Offline {
            ensure!(
                self.manifest.is_some(),
                "No cached grammar registry is available in offline mode"
            );
            return Ok(());
        }
        let now = now();
        if self.manifest.is_some() && now.saturating_sub(self.status.checked_at) < REFRESH_INTERVAL
        {
            return Ok(());
        }
        if now.saturating_sub(self.status.attempted_at) < RETRY_INTERVAL {
            ensure!(
                self.manifest.is_some(),
                "Grammar registry is unavailable; retrying within an hour"
            );
            return Ok(());
        }
        self.status.attempted_at = now;
        let result = self.fetch_manifest();
        match result {
            Ok(()) => {
                self.status.checked_at = now;
                self.warning = None;
            }
            Err(error) => {
                self.warning = Some(format!(
                    "Could not refresh syntax grammars, using cached files: {error:#}"
                ));
                if self.manifest.is_none() {
                    // Persist the backoff even when the first request fails.
                    self.persist_status()?;
                    return Err(error);
                }
            }
        }
        self.persist_status()
    }

    fn persist_status(&self) -> Result<()> {
        #[derive(Serialize)]
        struct Stored<'a> {
            manifest: &'a Option<Manifest>,
            status: &'a Status,
            warning: &'a Option<String>,
        }
        atomic_write(
            &self.namespace.join("index.json"),
            &serde_json::to_vec(&Stored {
                manifest: &self.manifest,
                status: &self.status,
                warning: &self.warning,
            })?,
        )
    }

    fn fetch_manifest(&mut self) -> Result<()> {
        let mut request = self.client.get(self.base_url.join("index.json")?);
        if self.manifest.is_some() {
            if let Some(etag) = &self.status.etag {
                request = request.header(header::IF_NONE_MATCH, etag);
            }
            if let Some(modified) = &self.status.last_modified {
                request = request.header(header::IF_MODIFIED_SINCE, modified);
            }
        }
        let response = request.send().context("Could not download grammar index")?;
        if response.status() == StatusCode::NOT_MODIFIED {
            ensure!(
                self.manifest.is_some(),
                "Registry returned 304 without a cached index"
            );
            return Ok(());
        }
        let response = response.error_for_status()?;
        let etag = response
            .headers()
            .get(header::ETAG)
            .and_then(|h| h.to_str().ok())
            .map(str::to_owned);
        let modified = response
            .headers()
            .get(header::LAST_MODIFIED)
            .and_then(|h| h.to_str().ok())
            .map(str::to_owned);
        let mut bytes = Vec::new();
        response.take(INDEX_LIMIT + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= INDEX_LIMIT,
            "Grammar index exceeds 4 MiB"
        );
        let manifest = parse_manifest(&bytes)?;
        self.manifest = Some(manifest);
        self.status.etag = etag;
        self.status.last_modified = modified;
        Ok(())
    }

    fn artifact_path(&self, artifact: &Artifact) -> Result<PathBuf> {
        ensure!(
            artifact.size <= ARTIFACT_LIMIT,
            "Grammar artifact is too large: {}",
            artifact.path
        );
        ensure!(
            artifact.sha256.len() == 64 && artifact.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "Invalid SHA-256 for {}",
            artifact.path
        );
        Ok(self.directory.join("objects").join(&artifact.sha256))
    }

    fn cached_artifact(&self, artifact: &Artifact) -> Option<Vec<u8>> {
        let path = self.artifact_path(artifact).ok()?;
        let mut bytes = Vec::new();
        fs::File::open(path)
            .ok()?
            .take(artifact.size + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        valid_artifact(&bytes, artifact).then_some(bytes)
    }

    fn artifact(&self, artifact: &Artifact) -> Result<Vec<u8>> {
        let path = self.artifact_path(artifact)?;
        if let Some(bytes) = self.cached_artifact(artifact) {
            return Ok(bytes);
        }
        ensure!(
            self.network == Network::Online,
            "Grammar artifact is missing or corrupt in offline mode: {}",
            artifact.path
        );
        // Only relative paths under this Pages registry are accepted.
        ensure!(
            !artifact.path.is_empty()
                && artifact
                    .path
                    .split('/')
                    .all(|part| !part.is_empty() && part != "." && part != ".."),
            "Invalid registry path: {}",
            artifact.path
        );
        let url = self.base_url.join(&artifact.path)?;
        ensure!(
            url.origin() == self.base_url.origin()
                && url.path().starts_with(self.base_url.path())
                && url.query().is_none()
                && url.fragment().is_none(),
            "Artifact URL leaves the registry: {}",
            artifact.path
        );
        let response = self.client.get(url).send()?.error_for_status()?;
        let mut bytes = Vec::new();
        response.take(artifact.size + 1).read_to_end(&mut bytes)?;
        ensure!(
            valid_artifact(&bytes, artifact),
            "Size or SHA-256 mismatch for {}",
            artifact.path
        );
        atomic_write(&path, &bytes)?;
        Ok(bytes)
    }

    fn load(&self, name: &str, entry: &Entry, key: String) -> Result<Grammar> {
        ensure!(
            (tree_sitter::MIN_COMPATIBLE_LANGUAGE_VERSION..=tree_sitter::LANGUAGE_VERSION)
                .contains(&entry.abi_version),
            "Unsupported Tree-sitter ABI {} for {name}",
            entry.abi_version
        );
        let wasm = self
            .artifact(&entry.wasm)
            .with_context(|| format!("Could not load {name} grammar"))?;
        let highlights = entry
            .queries
            .get("highlights")
            .map(|artifact| {
                self.artifact(artifact)
                    .and_then(|bytes| Ok(String::from_utf8(bytes)?))
            })
            .transpose()
            .with_context(|| format!("Could not load highlight query for {name}"))?
            .unwrap_or_default();
        let injections = entry
            .queries
            .get("injections")
            .map(|artifact| {
                self.artifact(artifact)
                    .and_then(|bytes| Ok(String::from_utf8(bytes)?))
            })
            .transpose()
            .with_context(|| format!("Could not load injection query for {name}"))?;
        // Keep the grammar's licenses alongside its downloaded resources.
        self.artifact(&entry.license)?;
        for license in entry.dependency_licenses.values() {
            self.artifact(license)?;
        }
        let engine = self.runtime.engine.clone();
        let language_name = entry.language.clone();
        let grammar_name = name.to_owned();
        let (language, query, injections, parser) =
            self.runtime.workers.run_blocking(move || {
                let mut store = WasmStore::new(&engine)?;
                let language = store
                    .load_language(&language_name, &wasm)
                    .with_context(|| format!("Could not compile {grammar_name} WASM grammar"))?;
                let query = Query::new(&language, &highlights)
                    .with_context(|| format!("Invalid highlight query for {grammar_name}"))?;
                let injections = injections
                    .map(|source| Query::new(&language, &source))
                    .transpose()
                    .with_context(|| format!("Invalid injection query for {grammar_name}"))?;
                let mut parser = Parser::new();
                parser.set_wasm_store(store)?;
                parser.set_language(&language)?;
                Ok((language, query, injections, parser))
            })?;
        Ok(Grammar {
            key,
            name: name.to_owned(),
            language,
            query,
            injections,
            engine: self.runtime.engine.clone(),
            workers: self.runtime.workers.clone(),
            prepared_parser: Mutex::new(Some(parser)),
        })
    }
}

impl Entry {
    fn artifacts(&self) -> impl Iterator<Item = &Artifact> {
        std::iter::once(&self.wasm)
            .chain(
                ["highlights", "injections"]
                    .into_iter()
                    .filter_map(|kind| self.queries.get(kind)),
            )
            .chain(std::iter::once(&self.license))
            .chain(self.dependency_licenses.values())
    }
}

impl Manifest {
    fn detect_language(&self, label: &str) -> Option<(&str, &Entry)> {
        let label = label.trim().to_ascii_lowercase();
        self.languages.iter().find_map(|(name, entry)| {
            entry
                .aliases
                .iter()
                .any(|alias| alias == &label)
                .then_some((name.as_str(), entry))
        })
    }

    fn detect(&self, path: &Path, sample: &str) -> Result<Option<(&str, &Entry)>> {
        let extension = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let filename = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let first_line = sample.lines().next().unwrap_or_default();
        let mut best = None;
        let mut score = 0;
        for (name, entry) in &self.languages {
            let files = &entry.files;
            let extension_matches = files.extensions.iter().any(|ext| ext == extension);
            let content_matches = matches_regex(files.content_regex.as_deref(), sample)?;
            if files.content_regex.is_some() && !content_matches {
                continue;
            }
            let candidate = if files.names.iter().any(|s| s == filename) {
                4
            } else if matches_regex(files.first_line_regex.as_deref(), first_line)? {
                3
            } else if extension_matches && content_matches {
                2
            } else if extension_matches {
                1
            } else {
                0
            };
            if candidate > score {
                best = Some((name.as_str(), entry));
                score = candidate;
            }
        }
        Ok(best)
    }
}

fn matches_regex(pattern: Option<&str>, text: &str) -> Result<bool> {
    pattern
        .map(|pattern| Regex::new(pattern).map(|regex| regex.is_match(text)))
        .transpose()
        .context("Invalid file-detection regex in grammar registry")
        .map(|value| value.unwrap_or(false))
}

fn parse_manifest(bytes: &[u8]) -> Result<Manifest> {
    let manifest: Manifest = serde_json::from_slice(bytes).context("Invalid grammar index")?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

fn validate_manifest(manifest: &Manifest) -> Result<()> {
    ensure!(
        manifest.schema_version == 1,
        "Unsupported grammar registry schema {}",
        manifest.schema_version
    );
    for entry in manifest.languages.values() {
        ensure!(
            !entry.language.is_empty()
                && entry
                    .language
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
            "Invalid WASM language name {:?}",
            entry.language
        );
    }
    Ok(())
}

fn valid_artifact(bytes: &[u8], artifact: &Artifact) -> bool {
    bytes.len() as u64 == artifact.size && format!("{:x}", Sha256::digest(bytes)) == artifact.sha256
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("Cache path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("Could not create {}", parent.display()))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.persist(path)
        .with_context(|| format!("Could not write {}", path.display()))?;
    Ok(())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Workers, runtime::runtime_engine};
    use tiny_http::{Header, Response, Server};
    use wasmtime::Engine;

    fn with_url(directory: &Path, url: Url) -> Registry {
        Registry::builder()
            .cache_dir(directory)
            .registry_url(url)
            .build()
            .unwrap()
    }

    #[test]
    fn unavailable_compilation_cache_does_not_prevent_registry_startup() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("compiled"), b"not a directory").unwrap();
        assert!(Registry::with_cache_dir(directory.path()).is_ok());
    }

    #[test]
    fn compiled_wasm_is_reused_by_new_stores_and_after_restart() {
        let directory = tempfile::tempdir().unwrap();
        let workers = Workers::new().unwrap();
        let (engine, cache) = runtime_engine(directory.path()).unwrap();
        let cache = cache.unwrap();
        workers
            .run_blocking(move || {
                WasmStore::new(&engine)?;
                WasmStore::new(&engine)?;
                Ok(())
            })
            .unwrap();
        assert!(
            cache.cache_hits() > 0,
            "New stores should reuse compiled WASM"
        );
        let (engine, restarted_cache) = runtime_engine(directory.path()).unwrap();
        let restarted_cache = restarted_cache.unwrap();
        workers
            .run_blocking(move || Ok(WasmStore::new(&engine)?))
            .unwrap();
        assert!(restarted_cache.cache_hits() > 0);
        assert_eq!(
            restarted_cache.cache_misses(),
            0,
            "Restarting should load compiled code from disk"
        );
    }

    #[test]
    fn wasm_store_creation_from_small_stack() {
        const CHILD: &str = "TREE_SITTER_WASM_SMALL_STACK_TEST";
        if std::env::var_os(CHILD).is_some() {
            let engine = Engine::default();
            let workers = Workers::new().unwrap();
            std::thread::Builder::new()
                .name("small-stack-caller".into())
                .stack_size(512 * 1024)
                .spawn(move || workers.run_blocking(move || Ok(WasmStore::new(&engine)?)))
                .unwrap()
                .join()
                .unwrap()
                .unwrap();
            return;
        }
        // A native stack overflow aborts the process, so isolate the check.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "registry::tests::wasm_store_creation_from_small_stack",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "Small-stack WASM caller crashed: {}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn index_refresh_is_conditional_persistent_and_works_offline() {
        let directory = tempfile::tempdir().unwrap();
        let server = Server::http("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!("http://{}/", server.server_addr())).unwrap();
        let thread = std::thread::spawn(move || {
            let request = server
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
                .unwrap();
            assert_eq!(request.url(), "/index.json");
            request
                .respond(
                    Response::from_string(r#"{"schema_version":1,"languages":{}}"#)
                        .with_header(Header::from_bytes("ETag", "\"v1\"").unwrap()),
                )
                .unwrap();
            let request = server
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
                .unwrap();
            assert!(
                request
                    .headers()
                    .iter()
                    .any(|header| header.field.equiv("If-None-Match")
                        && header.value.as_str() == "\"v1\"")
            );
            request.respond(Response::empty(304)).unwrap();
            let request = server
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
                .unwrap();
            request.respond(Response::empty(503)).unwrap();
        });
        let registry = with_url(directory.path(), url.clone());
        let mut old_manifest = serde_json::to_value(fence_manifest()).unwrap();
        for entry in old_manifest["languages"]
            .as_object_mut()
            .unwrap()
            .values_mut()
        {
            entry.as_object_mut().unwrap().remove("aliases");
        }
        atomic_write(&registry.cache.lock().unwrap().namespace.join("index.json"), &serde_json::to_vec(&serde_json::json!({
            "manifest": old_manifest,
            "status": { "checked_at": now(), "attempted_at": now(), "etag": "old", "last_modified": null }
        })).unwrap()).unwrap();
        assert!(registry.cache.lock().unwrap().manifest.is_none());
        registry.resolve(Path::new("plain.txt"), "").unwrap();
        // Repeated documents and a restarted app use the on-disk freshness time.
        registry.resolve(Path::new("other.txt"), "").unwrap();
        let restarted = with_url(directory.path(), url);
        assert!(
            restarted
                .resolve(Path::new("plain.txt"), "")
                .unwrap()
                .warning
                .is_none()
        );
        {
            let mut cache = restarted.cache.lock().unwrap();
            cache.status.checked_at = 0;
            cache.status.attempted_at = 0;
            cache.persist_status().unwrap();
        }
        assert!(
            restarted
                .resolve(Path::new("plain.txt"), "")
                .unwrap()
                .warning
                .is_none()
        );
        {
            let mut cache = restarted.cache.lock().unwrap();
            cache.status.checked_at = 0;
            cache.status.attempted_at = 0;
            cache.persist_status().unwrap();
        }
        assert!(
            restarted
                .resolve(Path::new("plain.txt"), "")
                .unwrap()
                .warning
                .is_some()
        );
        thread.join().unwrap();
        // The server is now gone. Failure backoff still serves the cached index.
        assert!(
            restarted
                .resolve(Path::new("plain.txt"), "")
                .unwrap()
                .warning
                .is_some()
        );
    }

    #[test]
    fn artifacts_are_cached_by_hash_and_corruption_is_repaired() {
        let directory = tempfile::tempdir().unwrap();
        let server = Server::http("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!("http://{}/", server.server_addr())).unwrap();
        let bytes = b"grammar bytes";
        let artifact = Artifact {
            path: "grammars/test/parser.wasm".into(),
            sha256: format!("{:x}", Sha256::digest(bytes)),
            size: bytes.len() as u64,
        };
        let thread = std::thread::spawn(move || {
            for _ in 0..2 {
                let request = server
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap()
                    .unwrap();
                assert_eq!(request.url(), "/grammars/test/parser.wasm");
                request
                    .respond(Response::from_data(bytes.to_vec()))
                    .unwrap();
            }
        });
        let registry = with_url(directory.path(), url);
        let cache = registry.cache.lock().unwrap();
        assert_eq!(cache.artifact(&artifact).unwrap(), bytes);
        assert_eq!(cache.artifact(&artifact).unwrap(), bytes);
        fs::write(
            directory.path().join("objects").join(&artifact.sha256),
            b"damaged",
        )
        .unwrap();
        assert_eq!(cache.artifact(&artifact).unwrap(), bytes);
        thread.join().unwrap();
        assert_eq!(cache.artifact(&artifact).unwrap(), bytes);
        let bad = Artifact {
            sha256: "not a hash".into(),
            ..artifact
        };
        assert!(cache.artifact(&bad).is_err());
    }

    #[test]
    fn download_rejects_hash_mismatches() {
        let directory = tempfile::tempdir().unwrap();
        let server = Server::http("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!("http://{}/", server.server_addr())).unwrap();
        let thread = std::thread::spawn(move || {
            server
                .recv()
                .unwrap()
                .respond(Response::from_data(b"wrong".to_vec()))
                .unwrap();
        });
        let registry = with_url(directory.path(), url);
        let artifact = Artifact {
            path: "parser.wasm".into(),
            sha256: format!("{:x}", Sha256::digest(b"right")),
            size: 5,
        };
        assert!(
            registry
                .cache
                .lock()
                .unwrap()
                .artifact(&artifact)
                .unwrap_err()
                .to_string()
                .contains("SHA-256 mismatch")
        );
        thread.join().unwrap();
    }

    #[test]
    fn separate_processes_share_index_and_artifacts() {
        const DIRECTORY: &str = "TREE_SITTER_WASM_TEST_CACHE";
        const URL: &str = "TREE_SITTER_WASM_TEST_URL";
        let bytes = b"shared artifact";
        let artifact = Artifact {
            path: "parser.wasm".into(),
            sha256: format!("{:x}", Sha256::digest(bytes)),
            size: bytes.len() as u64,
        };
        if let Some(directory) = std::env::var_os(DIRECTORY) {
            let registry = with_url(
                Path::new(&directory),
                Url::parse(&std::env::var(URL).unwrap()).unwrap(),
            );
            assert!(
                registry
                    .resolve_language("unknown")
                    .unwrap()
                    .grammar
                    .is_none()
            );
            let cache = registry.cache.lock().unwrap();
            let _lock = cache.lock_disk().unwrap();
            assert_eq!(cache.artifact(&artifact).unwrap(), bytes);
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let server = Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", server.server_addr());
        let server_thread = std::thread::spawn(move || {
            for expected in ["/index.json", "/parser.wasm"] {
                let request = server
                    .recv_timeout(Duration::from_secs(20))
                    .unwrap()
                    .unwrap();
                assert_eq!(request.url(), expected);
                let body = if expected == "/index.json" {
                    br#"{"schema_version":1,"languages":{}}"#.to_vec()
                } else {
                    bytes.to_vec()
                };
                request.respond(Response::from_data(body)).unwrap();
            }
        });
        let children: Vec<_> = (0..2)
            .map(|_| {
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "registry::tests::separate_processes_share_index_and_artifacts",
                        "--nocapture",
                    ])
                    .env(DIRECTORY, directory.path())
                    .env(URL, &url)
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect();
        for child in children {
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        server_thread.join().unwrap();
        let offline = Registry::builder()
            .cache_dir(directory.path())
            .registry_url(Url::parse(&url).unwrap())
            .network(Network::Offline)
            .build()
            .unwrap();
        assert!(
            offline
                .resolve_language("unknown")
                .unwrap()
                .grammar
                .is_none()
        );
        let cache = offline.cache.lock().unwrap();
        assert_eq!(cache.artifact(&artifact).unwrap(), bytes);
        fs::write(
            cache.directory.join("objects").join(&artifact.sha256),
            b"corrupt",
        )
        .unwrap();
        assert!(
            cache
                .artifact(&artifact)
                .unwrap_err()
                .to_string()
                .contains("offline mode")
        );
    }

    #[test]
    fn registry_urls_have_independent_indexes_and_offline_never_fetches() {
        let directory = tempfile::tempdir().unwrap();
        let server = Server::http("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!("http://{}/", server.server_addr())).unwrap();
        let first = with_url(directory.path(), url.clone());
        {
            let mut cache = first.cache.lock().unwrap();
            cache.manifest = Some(Manifest {
                schema_version: 1,
                languages: BTreeMap::new(),
            });
            cache.status.checked_at = now();
            cache.persist_status().unwrap();
        }
        let offline = Registry::builder()
            .cache_dir(directory.path())
            .registry_url(url.join("other/").unwrap())
            .network(Network::Offline)
            .build()
            .unwrap();
        assert!(
            offline
                .resolve_language("rust")
                .err()
                .unwrap()
                .to_string()
                .contains("offline mode")
        );
        assert!(
            server
                .recv_timeout(Duration::from_millis(50))
                .unwrap()
                .is_none()
        );
        for invalid in [
            "file:///registry/",
            "https://example.com/index.json",
            "https://example.com/?q=1",
        ] {
            assert!(
                Registry::builder()
                    .cache_dir(directory.path())
                    .registry_url(Url::parse(invalid).unwrap())
                    .build()
                    .is_err()
            );
        }
    }

    #[test]
    fn first_download_failure_is_shared_and_backed_off() {
        let directory = tempfile::tempdir().unwrap();
        let server = Server::http("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!("http://{}/", server.server_addr())).unwrap();
        let responder = std::thread::spawn(move || {
            server
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
                .unwrap()
                .respond(Response::empty(503))
                .unwrap();
        });
        let first = with_url(directory.path(), url.clone());
        assert!(first.resolve_language("rust").is_err());
        responder.join().unwrap();
        let second = with_url(directory.path(), url);
        assert!(
            second
                .resolve_language("rust")
                .err()
                .unwrap()
                .to_string()
                .contains("retrying within an hour")
        );
    }

    #[test]
    fn another_apps_download_unblocks_a_failed_grammar_load() {
        let directory = tempfile::tempdir().unwrap();
        let server = Server::http("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!("http://{}/", server.server_addr())).unwrap();
        let bytes = b"not a WASM parser";
        let artifact = Artifact {
            path: "parser.wasm".into(),
            sha256: format!("{:x}", Sha256::digest(bytes)),
            size: bytes.len() as u64,
        };
        let manifest = serde_json::json!({
            "schema_version": 1,
            "languages": { "test": {
                "language": "test", "aliases": ["test"], "abi_version": tree_sitter::LANGUAGE_VERSION,
                "files": {}, "wasm": artifact,
                "license": { "path": "LICENSE", "sha256": format!("{:x}", Sha256::digest(b"license")), "size": 7 }
            }}
        });
        let responder = std::thread::spawn(move || {
            server
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
                .unwrap()
                .respond(Response::from_string(manifest.to_string()))
                .unwrap();
            server
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
                .unwrap()
                .respond(Response::empty(503))
                .unwrap();
            server
        });
        let registry = with_url(directory.path(), url.clone());
        let error = registry.resolve_language("test").err().unwrap();
        assert!(format!("{error:#}").contains("503"));
        let server = responder.join().unwrap();
        let other_app = with_url(directory.path(), url);
        let cache = other_app.cache.lock().unwrap();
        let _lock = cache.lock_disk().unwrap();
        atomic_write(
            &cache.directory.join("objects").join(&artifact.sha256),
            bytes,
        )
        .unwrap();
        drop(_lock);
        drop(cache);
        let responder = std::thread::spawn(move || {
            if let Some(request) = server.recv_timeout(Duration::from_secs(1)).unwrap() {
                assert_eq!(request.url(), "/LICENSE");
                request.respond(Response::empty(500)).unwrap();
            }
        });
        let error = registry.resolve_language("test").err().unwrap();
        responder.join().unwrap();
        assert!(
            format!("{error:#}").contains("500") && format!("{error:#}").contains("/LICENSE"),
            "The repaired parser must allow remaining downloads, not return the previous parser failure: {error:#}"
        );
    }

    #[test]
    #[ignore = "downloads grammars from the live registry"]
    fn downloaded_grammars_parse_query_and_reload_offline() {
        use tree_sitter::{QueryCursor, StreamingIterator};
        let directory = tempfile::tempdir().unwrap();
        let registry = Registry::with_cache_dir(directory.path()).unwrap();
        for (label, source) in [
            ("json", "{\"value\": 42}"),
            ("rust", "fn main() {}"),
            ("verilog", "module test; endmodule"),
        ] {
            let grammar = registry.resolve_language(label).unwrap().grammar.unwrap();
            let parse = move |grammar: Arc<Grammar>| {
                let work = grammar.clone();
                grammar
                    .workers
                    .run_blocking(move || {
                        for _ in 0..2 {
                            let mut parser = work.new_parser()?;
                            let tree = parser.parse(source, None).context("Parse cancelled")?;
                            ensure!(!tree.root_node().has_error(), "{label} parse failed");
                            if label != "verilog" {
                                let mut cursor = QueryCursor::new();
                                ensure!(
                                    cursor
                                        .captures(&work.query, tree.root_node(), source.as_bytes())
                                        .next()
                                        .is_some(),
                                    "No {label} highlights"
                                );
                            }
                        }
                        Ok(())
                    })
                    .unwrap();
            };
            parse(grammar);
            let offline = Registry::builder()
                .cache_dir(directory.path())
                .network(Network::Offline)
                .build()
                .unwrap();
            parse(offline.resolve_language(label).unwrap().grammar.unwrap());
        }
    }

    fn fence_manifest() -> Manifest {
        let mut manifest = Manifest {
            schema_version: 1,
            languages: BTreeMap::new(),
        };
        for (name, language, extensions, aliases) in [
            ("flow", "tsx", vec!["js"], vec!["flow"]),
            (
                "javascript",
                "javascript",
                vec!["js", "jsx"],
                vec!["javascript", "js", "jsx"],
            ),
            ("tsx", "tsx", vec!["tsx", "jsx"], vec!["tsx"]),
            (
                "typescript",
                "typescript",
                vec!["ts", "js"],
                vec!["typescript", "ts"],
            ),
            ("rust", "rust", vec!["rs"], vec!["rust", "rs"]),
            ("bash", "bash", vec!["sh"], vec!["bash", "shell", "sh"]),
            (
                "ocaml-interface",
                "ocaml_interface",
                vec!["mli"],
                vec!["ocaml-interface"],
            ),
            ("cpp", "cpp", vec!["hpp"], vec!["cpp", "c++"]),
            (
                "powershell",
                "powershell",
                vec!["ps1"],
                vec!["powershell", "pwsh"],
            ),
        ] {
            let mut value = serde_json::json!({
                "language": language, "aliases": aliases, "abi_version": 15,
                "files": { "extensions": extensions },
                "wasm": { "path": "parser.wasm", "sha256": "", "size": 0 },
                "license": { "path": "LICENSE", "sha256": "", "size": 0 }
            });
            // Injection patterns are independent of fence aliases.
            value["injection_regex"] = serde_json::json!("^(ts|typescript|undeclared)$");
            if name == "flow" {
                value["files"]["content_regex"] = serde_json::json!("@flow");
            }
            manifest
                .languages
                .insert(name.to_owned(), serde_json::from_value(value).unwrap());
        }
        manifest
    }

    #[test]
    fn fence_detection_uses_declared_aliases_despite_overlapping_metadata() {
        let manifest = fence_manifest();
        for (label, expected) in [
            ("tsx", "tsx"),               // Not flow's exported tsx language.
            ("typescript", "typescript"), // Not tsx's injection regex.
            ("ts", "typescript"),         // Extension beats tsx's injection regex.
            (" TSX ", "tsx"),
            ("Ts", "typescript"),
            ("js", "javascript"), // Not the content-gated Flow variant.
            ("flow", "flow"),
        ] {
            assert_eq!(
                manifest.detect_language(label).unwrap().0,
                expected,
                "label: {label:?}"
            );
        }
    }

    #[test]
    fn fence_aliases_are_normalized_and_matched_exactly() {
        let manifest = fence_manifest();
        for (label, expected) in [
            ("rust", "rust"),
            (" RS ", "rust"),
            ("ocaml-interface", "ocaml-interface"),
            ("C++", "cpp"),
            ("pwsh", "powershell"),
            ("jsx", "javascript"),
            ("shell", "bash"),
            (" ShElL ", "bash"),
        ] {
            assert_eq!(
                manifest.detect_language(label).unwrap().0,
                expected,
                "label: {label:?}"
            );
        }
        for label in [
            "",
            " \t ",
            "unknown-language",
            "console",
            "shellfish",
            "ocaml_interface",
            "hpp",
            "undeclared",
            "rust extra",
        ] {
            assert!(
                manifest.detect_language(label).is_none(),
                "label: {label:?}"
            );
        }
    }

    #[test]
    fn detection_distinguishes_tsx_flow_and_named_shell_files() {
        let mut manifest = Manifest {
            schema_version: 1,
            languages: BTreeMap::new(),
        };
        for (name, extensions, names, content, first_line) in [
            ("javascript", vec!["js", "jsx"], vec![], None, None),
            ("flow", vec!["js"], vec![], Some("@flow"), None),
            ("typescript", vec!["ts"], vec![], None, None),
            ("tsx", vec!["tsx"], vec![], None, None),
            ("bash", vec!["sh"], vec![".bashrc"], None, Some("^#!.*bash")),
        ] {
            let entry = serde_json::from_value(serde_json::json!({
                "language": name, "aliases": [name], "abi_version": 15,
                "files": { "extensions": extensions, "names": names, "content_regex": content, "first_line_regex": first_line },
                "wasm": { "path": "parser.wasm", "sha256": "", "size": 0 },
                "license": { "path": "LICENSE", "sha256": "", "size": 0 }
            })).unwrap();
            manifest.languages.insert(name.to_owned(), entry);
        }
        for (file, sample, expected) in [
            ("a.js", "const x = 1", "javascript"),
            ("a.js", "// @flow\nconst x = 1", "flow"),
            ("a.ts", "", "typescript"),
            ("a.tsx", "", "tsx"),
            (".bashrc", "", "bash"),
            ("script", "#!/bin/bash\n", "bash"),
        ] {
            assert_eq!(
                manifest.detect(Path::new(file), sample).unwrap().unwrap().0,
                expected
            );
        }
        assert!(
            manifest
                .detect(Path::new("a.unknown"), "")
                .unwrap()
                .is_none()
        );
    }
}
