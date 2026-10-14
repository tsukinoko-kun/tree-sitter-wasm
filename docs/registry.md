# Registry format

`registry.toml` lists the language entries and pins the build tools. Each `grammars/<name>/grammar.toml` contains:

```toml
name = "rust"
aliases = ["rust", "rs"]
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

`license` is the SPDX identifier. Optional `license_file` selects the upstream license path relative to the repository root, defaulting to `LICENSE`. The registry vendors its bytes unchanged as `LICENSE`, regardless of the upstream filename. Sync and build use the declared path, including for inherited-query licenses. A missing declared file is an error, not a request to search for another license.

`aliases` is a required, nonempty list of Markdown code-fence labels, including the registry name. Labels contain lowercase ASCII letters, digits, and the punctuation `_`, `+`, `#`, `.`, or `-`, and start with a letter or digit. Aliases are unique within an entry and across the registry. They are maintained here, independently of `injection_regex` patterns and file extensions.

Consumers match the lowercased language token from a fence's info string exactly against these aliases. Additional fence attributes are not part of the label. Each alias resolves to one grammar. For example, `pwsh` resolves to `powershell`, `jsx` to `javascript`, and `tsx` to `tsx`. Unknown labels have no declared grammar.

`files.extensions` contains suffixes without a leading dot. `files.names` contains exact filenames such as `.bashrc`. Optional `files.first_line_regex` and `files.content_regex` contain upstream detection patterns. Extensions can overlap, such as `.h` for C and C++. The IDE decides precedence. JSDoc, Regex, OCaml types, and PHP-only have no standalone file extensions.

TypeScript covers `.ts`, `.mts`, `.cts`, `.js`, `.mjs`, and `.cjs`. TSX covers `.tsx` and `.jsx`. The JavaScript grammar also covers JavaScript files, including `.jsx`. Code-fence aliases remain unique, so `js` resolves to `javascript` even though TypeScript can parse it. Consumers detect extensions from the URL pathname, without query strings or fragments.

`injection_regex` matches language labels, not filenames or URLs. Patterns can overlap when multiple grammars support a language. The TypeScript, TSX, and JavaScript patterns are maintained here to cover module variants and distinguish JSX. Other entries preserve their upstream patterns.

Missing query kinds mean that no upstream query is available for that entry. In particular, upstream Verilog supplies no queries. The registry does not invent queries or require that every injection target exists in this registry.

## Published index

`index.json` has `schema_version`, `tree_sitter_version`, `registry`, and a `languages` object keyed by registry name. Each language contains source provenance, detection metadata, parser name, `aliases`, `abi_version`, `wasm`, `metadata`, `license`, `dependency_licenses`, and `queries`.

Every file record contains `path`, `sha256`, and `size` in bytes. Paths are relative to the index URL, without a leading slash. For example, `languages.rust.wasm.path` is `grammars/rust/parser.wasm`, and `languages.rust.queries.highlights.path` is `grammars/rust/highlights.scm`.

The `license` record also contains the SPDX identifier in `spdx`. `dependency_licenses` maps inherited-query grammar names to license file records. `metadata` points to the entry's `grammar.toml`. `registry` points to the top-level `registry.toml`.

The index publishes each entry's `aliases` list and optional `scope` and `injection_regex` values unchanged. `files` contains the file-detection fields from the grammar entry. Injection names come from upstream queries. They can match declared aliases, but not every injection target has an entry in this registry.

The parsers require a Tree-sitter runtime compatible with their `abi_version`. The build tests against the runtime in `tree_sitter_version`. Queries use upstream capture names and predicates, which the IDE must implement.

SQL dialect aliases select the same general SQL grammar; they do not enable dialect-specific modes or guarantee complete dialect coverage. Make's shell injections assume Bash rather than tracking the `SHELL` variable. Diff injections use filename captures and offset directives. Consumers must implement those conventions to highlight embedded content; loading the parser and compiling queries alone does not apply injections.
