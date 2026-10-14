import hashlib
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "scripts"))
from registry import (file_record, load_registry, publish_output, query_content,
                      recover_output, registry_lock, safe_path, select)


class SelectionTests(unittest.TestCase):
    def test_default_selects_every_grammar(self):
        entries = {"rust": {}, "python": {}}
        self.assertEqual(select(entries, None), entries)

    def test_name_selects_only_that_grammar(self):
        self.assertEqual(select({"rust": {"language": "rust"}, "python": {}}, "rust"),
                         {"rust": {"language": "rust"}})

    def test_unknown_name_reports_available_grammars(self):
        with self.assertRaisesRegex(ValueError, "Unknown grammar.*Choose from: rust, python"):
            select({"rust": {}, "python": {}}, "typo")


class MetadataTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.config = '''schema_version = 1
grammars = ["rust"]
[tree-sitter]
version = "0.27.0"
[tree-sitter.sha256]
linux-x64 = "CHECKSUM"
[web-tree-sitter]
integrity = "INTEGRITY"
'''.replace("CHECKSUM", "a" * 64).replace("INTEGRITY", "sha512-" + "A" * 86 + "==")
        self.metadata = '''name = "rust"
aliases = ["rust", "rs"]
repository = "https://github.com/tree-sitter/tree-sitter-rust"
revision = "REVISION"
path = "."
language = "rust"
license = "MIT"
[files]
extensions = ["rs"]
[queries]
highlights = "highlights.scm"
[query-sources]
highlights = ["rust:queries/highlights.scm"]
'''.replace("REVISION", "b" * 40)
        self.directory = self.root / "grammars/rust"
        self.directory.mkdir(parents=True)
        (self.directory / "LICENSE").write_text("license notice\n")
        (self.directory / "highlights.scm").write_text("(identifier) @variable\n")
        self.write()

    def write(self):
        (self.root / "registry.toml").write_text(self.config)
        (self.directory / "grammar.toml").write_text(self.metadata)

    def test_valid_entry(self):
        _, entries = load_registry(self.root)
        self.assertEqual(entries["rust"]["files"]["extensions"], ["rs"])
        self.assertEqual(entries["rust"]["aliases"], ["rust", "rs"])

    def test_aliases_are_required(self):
        self.metadata = self.metadata.replace('aliases = ["rust", "rs"]\n', '')
        self.write()
        with self.assertRaisesRegex(ValueError, "rust: aliases"):
            load_registry(self.root)

    def test_aliases_must_be_lowercase_fence_labels(self):
        for value in ('[]', '"rust"', '[1]', '[""]', '["Rust"]',
                      '["rust script"]', '["rust/rs"]'):
            with self.subTest(value=value):
                metadata = self.metadata.replace('["rust", "rs"]', value)
                (self.directory / "grammar.toml").write_text(metadata)
                with self.assertRaisesRegex(ValueError, "rust: aliases"):
                    load_registry(self.root)

    def test_aliases_include_the_registry_name(self):
        self.metadata = self.metadata.replace('["rust", "rs"]', '["rs"]')
        self.write()
        with self.assertRaisesRegex(ValueError, "rust: aliases must include.*rust"):
            load_registry(self.root)

    def test_duplicate_aliases_fail(self):
        self.metadata = self.metadata.replace('["rust", "rs"]', '["rust", "rust"]')
        self.write()
        with self.assertRaisesRegex(ValueError, "rust: duplicate alias"):
            load_registry(self.root)

    def test_aliases_cannot_be_shared_by_grammars(self):
        self.config = self.config.replace('grammars = ["rust"]',
                                          'grammars = ["rust", "python"]')
        self.write()
        extra = self.root / "grammars/python"
        extra.mkdir()
        (extra / "grammar.toml").write_text(
            self.metadata.replace('name = "rust"', 'name = "python"')
            .replace('aliases = ["rust", "rs"]', 'aliases = ["python", "rs"]'))
        (extra / "LICENSE").write_text("license notice\n")
        (extra / "highlights.scm").write_text("(identifier) @variable\n")
        with self.assertRaisesRegex(ValueError, "python: alias 'rs'.*rust"):
            load_registry(self.root)

    def test_aliases_allow_punctuation_used_in_language_names(self):
        self.metadata = self.metadata.replace('["rust", "rs"]',
                                              '["rust", "c++", "c#", "c_sharp", "foo-bar", "foo.bar"]')
        self.write()
        _, entries = load_registry(self.root)
        self.assertIn("c++", entries["rust"]["aliases"])

    def test_requires_full_commit_pin(self):
        self.metadata = self.metadata.replace("b" * 40, "main")
        self.write()
        with self.assertRaisesRegex(ValueError, "full commit SHA"):
            load_registry(self.root)

    def test_duplicate_registry_entries_fail(self):
        self.config = self.config.replace('["rust"]', '["rust", "rust"]')
        self.write()
        with self.assertRaisesRegex(ValueError, "Duplicate grammar"):
            load_registry(self.root)

    def test_unlisted_grammar_fails(self):
        extra = self.root / "grammars/python"
        extra.mkdir()
        (extra / "grammar.toml").write_text('name = "python"')
        with self.assertRaisesRegex(ValueError, "unlisted=.*python"):
            load_registry(self.root)

    def test_missing_query_fails(self):
        (self.directory / "highlights.scm").unlink()
        with self.assertRaisesRegex(ValueError, "missing query"):
            load_registry(self.root)

    def test_query_cannot_escape_grammar_directory(self):
        self.metadata = self.metadata.replace('"highlights.scm"', '"../highlights.scm"')
        self.write()
        with self.assertRaisesRegex(ValueError, "Unsafe relative path"):
            load_registry(self.root)

    def test_unknown_query_dependency_fails(self):
        self.metadata = self.metadata.replace("rust:queries", "python:queries")
        self.write()
        with self.assertRaisesRegex(ValueError, "unknown query dependency python"):
            load_registry(self.root)

    def test_query_requires_provenance(self):
        self.metadata = self.metadata.split("[query-sources]")[0]
        self.write()
        with self.assertRaisesRegex(ValueError, "query-sources must match"):
            load_registry(self.root)

    def test_query_source_cannot_escape_upstream_directory(self):
        self.metadata = self.metadata.replace("rust:queries/highlights.scm", "rust:../query.scm")
        self.write()
        with self.assertRaisesRegex(ValueError, "Unsafe relative path"):
            load_registry(self.root)


class ArtifactTests(unittest.TestCase):
    def test_overlapping_build_or_sync_is_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            lock = Path(temporary) / "registry.lock"
            with registry_lock(lock):
                with self.assertRaisesRegex(ValueError, "Another build or query sync is running"):
                    with registry_lock(lock):
                        self.fail("Overlapping process acquired the registry lock")

    def test_lock_is_released_after_failure(self):
        with tempfile.TemporaryDirectory() as temporary:
            lock = Path(temporary) / "registry.lock"
            with self.assertRaisesRegex(ValueError, "build failed"):
                with registry_lock(lock):
                    raise ValueError("build failed")
            with registry_lock(lock):
                pass

    def test_publication_failure_restores_previous_output(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output, destination, backup = root / "new", root / "dist", root / "backup"
            output.mkdir()
            destination.mkdir()
            (output / "index.json").write_text("new registry")
            (destination / "index.json").write_text("previous registry")
            rename = Path.rename

            def fail_publication(path, target):
                if path == output:
                    raise OSError("simulated publication failure")
                return rename(path, target)

            with patch.object(Path, "rename", fail_publication):
                with self.assertRaisesRegex(OSError, "simulated publication failure"):
                    publish_output(output, destination, backup)
            self.assertEqual((destination / "index.json").read_text(), "previous registry")
            self.assertFalse(backup.exists())

    def test_interrupted_publication_recovers_persistent_backup(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            destination, backup = root / "dist", root / "backup"
            backup.mkdir()
            (backup / "index.json").write_text("previous registry")
            recover_output(destination, backup)
            self.assertEqual((destination / "index.json").read_text(), "previous registry")
            self.assertFalse(backup.exists())

    def test_successful_publication_replaces_previous_registry(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output, destination, backup = root / "new", root / "dist", root / "backup"
            output.mkdir()
            destination.mkdir()
            (output / "index.json").write_text("new registry")
            (destination / "index.json").write_text("previous registry")
            publish_output(output, destination, backup)
            self.assertEqual((destination / "index.json").read_text(), "new registry")
            self.assertFalse(backup.exists())

    def test_paths_are_relative_and_hashes_match_contents(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            file = root / "grammars/rust/parser.wasm"
            file.parent.mkdir(parents=True)
            file.write_bytes(b"example")
            self.assertEqual(file_record(file, root), {
                "path": "grammars/rust/parser.wasm",
                "size": 7,
                "sha256": hashlib.sha256(b"example").hexdigest(),
            })

    def test_inherited_queries_keep_declared_order_and_boundaries(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "base.scm").write_bytes(b"(identifier) @variable\n\n")
            (root / "extra.scm").write_bytes(b"(type_identifier) @type")
            with patch("registry.source_path", return_value=root):
                content = query_content(["c:base.scm", "cpp:extra.scm"], {"c": {}, "cpp": {}})
            self.assertEqual(content, b"(identifier) @variable\n\n(type_identifier) @type\n")

    def test_safe_paths_allow_nested_parser_directories(self):
        self.assertEqual(safe_path("grammars/interface"), Path("grammars/interface"))
        for value in ("/absolute", "../escape", "src/../../escape", "src\\escape"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                safe_path(value)
