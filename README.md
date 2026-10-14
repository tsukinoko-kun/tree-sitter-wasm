# Tree-sitter WASM registry

Pinned grammars from the [Tree-sitter upstream parser list](https://tree-sitter.github.io/tree-sitter/#parsers) and [Airbus CERT's PowerShell grammar](https://github.com/airbus-cert/tree-sitter-powershell), with vendored queries and licenses. The registry contains 32 language entries from 25 repositories, including TSX, Flow, PHP-only, PowerShell, OCaml interfaces and types, and the embedded-template variants.

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

The build downloads the pinned Tree-sitter CLI and matching web runtime, verifies their checksums, and fetches grammar sources at their pinned commits. The CLI downloads its WASI SDK automatically. No Docker, npm install, or parser generation is required. Downloads are cached under `.cache/` and Tree-sitter's system cache.

The build loads every WASM parser in `web-tree-sitter`, parses input, compiles each query, and checks every indexed file's size and hash. It publishes `dist/` only after all checks pass. A single-grammar build replaces `dist/` with only that grammar, so its index never advertises absent or stale parsers. Build and verification failures leave the previous `dist/` intact. Publication keeps a backup in `.cache/previous-dist` until the new output is in place. If publication is interrupted, the next build recovers that backup.

Only one build or query sync can run in a checkout at a time. A concurrent invocation exits with an error before changing the cache or output.

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

The workflow publishes all of `dist/`, including metadata, queries, licenses, and WASM parsers. Every action uses the latest release available when this registry was created.

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
