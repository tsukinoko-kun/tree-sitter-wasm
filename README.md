# Tree-sitter WASM registry

Tree-sitter grammars compiled to WASM, with pinned upstream sources, vendored queries, and licenses. The published index includes file-detection metadata and checksums for each artifact.

See [`registry.toml`](registry.toml) for available grammars and pinned build tools. Each entry under [`grammars/`](grammars/) records its upstream repository, commit, and query sources.

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
