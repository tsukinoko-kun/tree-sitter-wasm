# Tree-sitter WASM registry

Tree-sitter grammars compiled to WASM, with pinned upstream sources, vendored queries, and licenses. The published index includes file-detection metadata and checksums for each artifact.

See [`registry.toml`](registry.toml) for available grammars and pinned build tools. Each entry under [`grammars/`](grammars/) records its upstream repository, commit, and query sources.

## Use from Rust

The `tree-sitter-wasm` crate downloads and validates grammars, detects languages, and runs Tree-sitter through Wasmtime. It has no UI or database dependency.

Add this checkout as a dependency:

```toml
[dependencies]
tree-sitter-wasm = { path = "../tree-sitter-wasm" }
```

Resolve a language and parse on the crate's large-stack workers:

```rust
use anyhow::Context;
use tree_sitter_wasm::Registry;

fn main() -> anyhow::Result<()> {
	let registry = Registry::new()?;
	let resolution = registry.resolve_language("rust")?;
	if let Some(warning) = resolution.warning {
		eprintln!("{warning}");
	}
	let grammar = resolution.grammar.context("Unknown language")?;
	let workers = grammar.workers.clone();
	workers.run_blocking(move || {
		let mut parser = grammar.new_parser()?;
		let tree = parser.parse("fn main() {}", None).context("Parse cancelled")?;
		println!("{}", tree.root_node().to_sexp());
		Ok(())
	})
}
```

`Registry::resolve(path, sample)` detects languages from filenames, extensions, shebangs, and content patterns. `resolve_language(label)` matches declared aliases exactly, ignoring case and surrounding whitespace. Unknown languages return `Resolution { grammar: None, .. }`. Download, validation, and compilation failures return errors.

`Grammar` exposes the Tree-sitter language, highlight query, optional injection query, and `new_parser()`. A grammar without highlight queries still supports parsing and has an empty highlight query. The crate compiles injection queries but does not discover or parse embedded languages for you. Other query kinds, such as tags and locals, are not loaded. `tree_sitter_wasm::tree_sitter` re-exports the compatible Tree-sitter API. `Grammar::native` adapts an application's bundled native grammar to the same API.

Registry calls block and belong on a background thread. Do not call them from the registry's parser workers, because grammar loading waits for that pool. Run parser creation, parsing, and queries through `grammar.workers.run_blocking(...)` or the executor-independent async `run(...)`. `Runtime` and `Workers` are also available separately.

See [`examples/parse.rs`](examples/parse.rs) for file detection:

```sh
cargo run --example parse -- examples/parse.rs
```

### Shared cache

All applications use the same per-user cache by default:

| Platform | Directory |
| --- | --- |
| macOS | `~/Library/Caches/tree-sitter-wasm/v1/` |
| Linux | `$XDG_CACHE_HOME/tree-sitter-wasm/v1/`, normally `~/.cache/tree-sitter-wasm/v1/` |
| Windows | `%LOCALAPPDATA%\tree-sitter-wasm\v1\` |

`objects/` stores checksum-verified parsers, queries, and licenses by SHA-256. `compiled/` stores Wasmtime's version-specific compiled code. `registries/<URL hash>/` stores each registry's index, HTTP validators, refresh state, and process lock. Apps reuse downloads and compatible compiled code without sharing their own settings or databases. Atomic file replacement and OS file locks coordinate concurrent processes.

The index refreshes at most once per day. Failed refreshes retry after an hour and retain a cached index with a warning. Cached artifacts remain usable without a connection. Corrupt downloads are fetched again and verified before use. There is no automatic cache migration or cleanup.

To choose a cache or prohibit network requests:

```rust
use tree_sitter_wasm::{Network, Registry};

let registry = Registry::builder()
	.cache_dir("/path/to/shared-cache")
	.network(Network::Offline)
	.build()?;
```

Offline mode returns an error if the index or a required artifact is missing. `Registry::with_cache_dir(...)` changes only the cache location. `RegistryBuilder::registry_url(...)` accepts an HTTP or HTTPS base URL ending in `/`, including a locally served registry. Custom registries have separate indexes but share content-addressed artifacts. Treat the cache as trusted local storage and do not put it in a directory writable by untrusted users.

### Check the Rust crate

```sh
cargo fmt -- --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

The default tests use temporary directories and local HTTP servers. To verify real downloads, parsing, queries, and offline reloads:

```sh
cargo test --locked downloaded_grammars_parse_query_and_reload_offline -- --ignored --nocapture
```

## Build locally

Install Python 3.12+ and Node.js 22+ on macOS or Linux. Run:

```sh
scripts/validate
scripts/build-all
```

To build one grammar:

```sh
scripts/build-all rust
```

The build downloads and verifies the pinned Tree-sitter CLI and web runtime, then fetches grammar sources at their pinned commits. The CLI downloads its WASI SDK automatically. No Docker, npm install, or parser generation is required.

The build verifies that each WASM parser loads and runs, each query compiles, and each indexed file matches its recorded size and hash. It replaces `dist/` only after all checks pass. A single-grammar build replaces `dist/` with only that grammar. Build and verification failures preserve the previous output.

Only one build or query sync can run in a checkout at a time.

To serve the result:

```sh
python3 -m http.server 8000 --directory dist
```

The index is at `http://localhost:8000/index.json`.

## Publish to GitHub Pages

Set **Settings > Pages > Build and deployment > Source** to **GitHub Actions**. Push to `main` or run the **Build and publish registry** workflow. Pull requests build and verify the full registry without deploying it.

For this repository, the published index URL is:

```text
https://tsukinoko-kun.github.io/tree-sitter-wasm/index.json
```

The workflow publishes all of `dist/`, including metadata, queries, licenses, and WASM parsers.

## Load a grammar in your IDE

Fetch `index.json`, then resolve the file paths against its URL:

```js
const indexURL = "https://tsukinoko-kun.github.io/tree-sitter-wasm/index.json";
const response = await fetch(indexURL);
if (!response.ok) throw new Error(`Registry request failed: ${response.status}`);
const index = await response.json();
if (index.schema_version !== 1) throw new Error("Unsupported registry schema");
const wasmURL = new URL(index.languages.rust.wasm.path, indexURL);
```

Use a Tree-sitter runtime compatible with the parser's `abi_version`. Read [the registry format reference](docs/registry.md) for the index fields, query provenance, and file-detection conventions.

## Update a grammar

1. Change `revision` in `grammars/<name>/grammar.toml` to a full commit SHA. Update `path`, detection metadata, and `query-sources` if upstream changed them.
2. Fetch the pinned queries and license:

   ```sh
   scripts/sync-queries rust
   ```

3. Review the vendored changes and build:

   ```sh
   scripts/build-all rust
   python3 -m unittest discover -s tests
   ```

4. Run `scripts/build-all` before pushing. This also catches inherited-query incompatibilities in other languages.

`scripts/sync-queries` without an argument refreshes every entry. The build rejects query or license files that differ from their declared upstream sources.

## License

The Rust crate and registry tooling use the [zlib license](LICENSE). Third-party grammars and queries retain their upstream licenses, included alongside their resources.
