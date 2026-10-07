# Registry format

`registry.toml` lists the language entries and pins the build tools. Each `grammars/<name>/grammar.toml` contains:

```toml
name = "rust"
repository = "https://github.com/tree-sitter/tree-sitter-rust"
revision = "77a3747266f4d621d0757825e6b11edcbf991ca5"
path = "."
language = "rust"
license = "MIT"
scope = "source.rust"
injection_regex = "rust"

[files]
extensions = ["rs"]

[queries]
highlights = "highlights.scm"
injections = "injections.scm"
tags = "tags.scm"

[query-sources]
highlights = ["rust:queries/highlights.scm"]
injections = ["rust:queries/injections.scm"]
tags = ["rust:queries/tags.scm"]
```

`path` is relative to the upstream repository root. `language` is the parser's exported name, which can differ from the registry name. `query-sources` lists `<registry-name>:<upstream-path>` entries in concatenation order. Each source resolves to that entry's pinned repository and commit. C++ inherits C queries, and TypeScript inherits JavaScript queries. The published files include the inherited queries' license notices.

`files.extensions` contains suffixes without a leading dot. `files.names` contains exact filenames such as `.bashrc`. Optional `files.first_line_regex` and `files.content_regex` contain upstream detection patterns. Extensions can overlap, such as `.h` for C and C++. The IDE decides precedence. JSDoc, Regex, OCaml types, and PHP-only have no standalone file extensions.

Missing query kinds mean that no upstream query is available for that entry. In particular, upstream Verilog supplies no queries. The registry does not invent queries or require that every injection target exists in this registry.

## Published index

`index.json` has `schema_version`, `tree_sitter_version`, `registry`, and a `languages` object keyed by registry name. Each language contains source provenance, detection metadata, parser name, `abi_version`, `wasm`, `metadata`, `license`, `dependency_licenses`, and `queries`.

Every file record contains `path`, `sha256`, and `size` in bytes. Paths are relative to the index URL, without a leading slash. For example, `languages.rust.wasm.path` is `grammars/rust/parser.wasm`, and `languages.rust.queries.highlights.path` is `grammars/rust/highlights.scm`.

The `license` record also contains the SPDX identifier in `spdx`. `dependency_licenses` maps inherited-query grammar names to license file records. `metadata` points to the entry's `grammar.toml`. `registry` points to the top-level `registry.toml`.

The index preserves optional `scope` and `injection_regex` values from upstream. `files` contains the file-detection fields from the grammar entry. Injection names come from upstream queries and may need alias resolution in the IDE.

The parsers require a Tree-sitter runtime compatible with their `abi_version`. The build tests against the runtime in `tree_sitter_version`. Queries use upstream capture names and predicates, which the IDE must implement.
