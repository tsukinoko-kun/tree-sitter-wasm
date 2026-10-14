"""Validate registry entries and build the Pages artifact. Python 3.12+."""

import argparse
import base64
from contextlib import contextmanager
import fcntl
import gzip
import hashlib
import json
from pathlib import Path, PurePosixPath
import platform
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parent.parent
CACHE = ROOT / ".cache"
QUERY_KINDS = {"highlights", "injections", "locals", "tags", "indents"}


def require(condition, message):
    if not condition:
        raise ValueError(message)


@contextmanager
def registry_lock(path):
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a") as file:
        try:
            fcntl.flock(file, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise ValueError("Another build or query sync is running in this repository. "
                             "Wait for it to finish before retrying.") from error
        yield


def safe_path(value):
    require(isinstance(value, str) and value, f"Invalid relative path: {value!r}")
    path = PurePosixPath(value)
    require(not path.is_absolute() and ".." not in path.parts and "\\" not in value,
            f"Unsafe relative path: {value!r}")
    return path


def load_toml(path):
    with path.open("rb") as file:
        return tomllib.load(file)


def load_registry(root=ROOT):
    config = load_toml(root / "registry.toml")
    require(config.get("schema_version") == 1, "Unsupported registry schema_version")
    names = config.get("grammars")
    require(isinstance(names, list) and names, "registry.toml needs a nonempty grammars list")
    require(all(isinstance(name, str) and re.fullmatch(r"[a-z][a-z0-9-]*", name)
                for name in names), "Invalid grammar name in registry.toml")
    require(len(names) == len(set(names)), "Duplicate grammar name in registry.toml")
    actual = {path.parent.name for path in (root / "grammars").glob("*/grammar.toml")}
    require(actual == set(names),
            f"Registry entries do not match directories: missing={set(names) - actual}, "
            f"unlisted={actual - set(names)}")
    tool = config.get("tree-sitter", {})
    require(isinstance(tool.get("version"), str)
            and re.fullmatch(r"\d+\.\d+\.\d+", tool["version"]), "Invalid Tree-sitter version")
    require(isinstance(tool.get("sha256"), dict) and tool["sha256"], "Missing CLI checksums")
    require(all(isinstance(value, str) and re.fullmatch(r"[0-9a-f]{64}", value)
                for value in tool["sha256"].values()), "Invalid CLI SHA-256")
    integrity = config.get("web-tree-sitter", {}).get("integrity")
    require(isinstance(integrity, str) and re.fullmatch(r"sha512-[A-Za-z0-9+/]{86}==", integrity),
            "Missing or invalid web-tree-sitter integrity hash")
    entries = {}
    alias_owners = {}
    for name in names:
        directory = root / "grammars" / name
        entry = load_toml(directory / "grammar.toml")
        require(entry.get("name") == name, f"{name}: name must match directory")
        aliases = entry.get("aliases")
        require(isinstance(aliases, list) and aliases
                and all(isinstance(alias, str) and re.fullmatch(r"[a-z0-9][a-z0-9_+#.-]*", alias)
                        for alias in aliases),
                f"{name}: aliases must be a nonempty list of lowercase fence labels")
        require(name in aliases, f"{name}: aliases must include the registry name {name!r}")
        require(len(aliases) == len(set(aliases)), f"{name}: duplicate alias")
        for alias in aliases:
            require(alias not in alias_owners,
                    f"{name}: alias {alias!r} already belongs to {alias_owners.get(alias)!r}")
            alias_owners[alias] = name
        require(isinstance(entry.get("repository"), str) and re.fullmatch(
            r"https://github\.com/[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", entry["repository"]),
            f"{name}: repository must be a GitHub HTTPS URL")
        require(isinstance(entry.get("revision"), str)
                and re.fullmatch(r"[0-9a-f]{40}", entry["revision"]),
                f"{name}: revision must be a full commit SHA")
        safe_path(entry.get("path", "."))
        require(isinstance(entry.get("language"), str)
                and re.fullmatch(r"[a-z][a-z0-9_]*", entry["language"]),
                f"{name}: missing or invalid parser language name")
        require(isinstance(entry.get("license"), str) and entry["license"],
                f"{name}: missing SPDX license")
        safe_path(entry.get("license_file", "LICENSE"))
        for key in ("scope", "injection_regex"):
            if key in entry:
                require(isinstance(entry[key], str) and entry[key], f"{name}: invalid {key}")
        require((directory / "LICENSE").is_file(), f"{name}: missing LICENSE")
        files = entry.get("files", {})
        for key in ("extensions", "names"):
            values = files.get(key, [])
            require(isinstance(values, list)
                    and all(isinstance(value, str) and value and "/" not in value
                            and "\\" not in value for value in values),
                    f"{name}: files.{key} must be a list of file suffixes or names")
        for key in ("first_line_regex", "content_regex"):
            if key in files:
                require(isinstance(files[key], str) and files[key], f"{name}: invalid files.{key}")
        queries = entry.get("queries", {})
        sources = entry.get("query-sources", {})
        require(isinstance(queries, dict) and set(queries) <= QUERY_KINDS,
                f"{name}: unsupported query kind")
        require(set(queries) == set(sources), f"{name}: query-sources must match queries")
        for kind, filename in queries.items():
            safe_path(filename)
            require(filename == f"{kind}.scm", f"{name}: query filename must be {kind}.scm")
            require((directory / filename).is_file(), f"{name}: missing query {filename}")
            require((directory / filename).stat().st_size > 0, f"{name}: empty query {filename}")
            require(isinstance(sources[kind], list) and sources[kind],
                    f"{name}: missing source list for {kind}")
            for source in sources[kind]:
                require(isinstance(source, str) and ":" in source,
                        f"{name}: invalid query source {source!r}")
                dependency, path = source.split(":", 1)
                require(dependency in names, f"{name}: unknown query dependency {dependency}")
                safe_path(path)
        entries[name] = entry
    return config, entries


def select(entries, name):
    if name is None:
        return entries
    require(name in entries, f"Unknown grammar {name!r}. Choose from: {', '.join(entries)}")
    return {name: entries[name]}


def download(url, destination):
    print(f"Downloading {url}", flush=True)
    request = urllib.request.Request(url, headers={"User-Agent": "tree-sitter-wasm-registry"})
    with urllib.request.urlopen(request, timeout=120) as response, destination.open("wb") as file:
        shutil.copyfileobj(response, file)


def cli_path(config):
    systems = {"Darwin": "macos", "Linux": "linux"}
    machines = {"arm64": "arm64", "aarch64": "arm64", "x86_64": "x64", "AMD64": "x64"}
    target = f"{systems.get(platform.system())}-{machines.get(platform.machine())}"
    tool = config["tree-sitter"]
    require(target in tool["sha256"], f"Unsupported build platform: {platform.system()} {platform.machine()}")
    directory = CACHE / "tools" / tool["version"] / target
    binary = directory / "tree-sitter"
    if binary.is_file():
        return binary
    directory.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=directory) as temporary:
        archive = Path(temporary) / "cli.gz"
        download(f"https://github.com/tree-sitter/tree-sitter/releases/download/"
                 f"v{tool['version']}/tree-sitter-{target}.gz", archive)
        require(hashlib.sha256(archive.read_bytes()).hexdigest() == tool["sha256"][target],
                f"CLI checksum mismatch for {target}")
        extracted = Path(temporary) / "tree-sitter"
        with gzip.open(archive, "rb") as source, extracted.open("wb") as output:
            shutil.copyfileobj(source, output)
        extracted.chmod(0o755)
        extracted.replace(binary)
    return binary


def source_path(entry):
    repo = entry["repository"].removeprefix("https://github.com/")
    directory = CACHE / "sources" / repo / entry["revision"]
    if directory.is_dir():
        return directory
    directory.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=directory.parent) as temporary:
        temporary = Path(temporary)
        archive = temporary / "source.tar.gz"
        download(f"https://codeload.github.com/{repo}/tar.gz/{entry['revision']}", archive)
        unpacked = temporary / "unpacked"
        with tarfile.open(archive, "r:gz") as source:
            source.extractall(unpacked, filter="data")
        roots = list(unpacked.iterdir())
        require(len(roots) == 1 and roots[0].is_dir(), f"Unexpected source archive for {repo}")
        roots[0].rename(directory)
    return directory


def runtime_path(config):
    version = config["tree-sitter"]["version"]
    directory = CACHE / "tools" / "web-tree-sitter" / version
    if directory.is_dir():
        return directory / "web-tree-sitter.js"
    directory.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=directory.parent) as temporary:
        temporary = Path(temporary)
        archive = temporary / "runtime.tgz"
        download(f"https://registry.npmjs.org/web-tree-sitter/-/web-tree-sitter-{version}.tgz", archive)
        integrity = "sha512-" + base64.b64encode(hashlib.sha512(archive.read_bytes()).digest()).decode()
        require(integrity == config["web-tree-sitter"]["integrity"], "Runtime checksum mismatch")
        with tarfile.open(archive, "r:gz") as source:
            source.extractall(temporary / "unpacked", filter="data")
        package = temporary / "unpacked/package"
        require((package / "web-tree-sitter.js").is_file(), "Runtime package is missing JavaScript entry")
        package.rename(directory)
    return directory / "web-tree-sitter.js"


def license_content(entry):
    filename = entry.get("license_file", "LICENSE")
    file = source_path(entry) / filename
    require(file.is_file(), f"{entry['name']}: missing upstream license {filename!r}")
    return file.read_bytes()


def query_content(sources, entries):
    parts = []
    for source in sources:
        dependency, path = source.split(":", 1)
        file = source_path(entries[dependency]) / path
        require(file.is_file(), f"Missing upstream query: {source}")
        parts.append(file.read_bytes().rstrip(b"\n") + b"\n")
    return b"\n".join(parts)


def file_record(path, output):
    content = path.read_bytes()
    return {"path": path.relative_to(output).as_posix(),
            "sha256": hashlib.sha256(content).hexdigest(), "size": len(content)}


def sync_queries(entries, selected):
    # Fetch everything before replacing any vendored file.
    updates = {}
    for name, entry in selected.items():
        directory = ROOT / "grammars" / name
        updates[directory / "LICENSE"] = license_content(entry)
        for kind, filename in entry.get("queries", {}).items():
            updates[directory / filename] = query_content(entry["query-sources"][kind], entries)
    for path, content in updates.items():
        path.write_bytes(content)
    print(f"Synced queries and licenses for {len(selected)} grammars.")


def build_language(name, entry, entries, cli, output):
    print(f"Building {name} at {entry['revision']}", flush=True)
    source = source_path(entry) / entry.get("path", ".")
    parser = source / "src/parser.c"
    require(parser.is_file(), f"{name}: source does not contain a generated src/parser.c")
    abi = re.search(r"^#define LANGUAGE_VERSION (\d+)$", parser.read_text(), re.MULTILINE)
    require(abi is not None, f"{name}: cannot read parser ABI version")
    grammar = json.loads((source / "src/grammar.json").read_text())
    require(grammar["name"] == entry["language"], f"{name}: source parser language does not match entry")
    directory = output / "grammars" / name
    directory.mkdir(parents=True)
    wasm = directory / "parser.wasm"
    subprocess.run([str(cli), "build", "--wasm", "--output", str(wasm), str(source)],
                   cwd=source, check=True)
    require(wasm.read_bytes()[:8] == b"\0asm\x01\0\0\0", f"{name}: invalid WASM output")
    local = ROOT / "grammars" / name
    require((local / "LICENSE").read_bytes() == license_content(entry),
            f"{name}: LICENSE differs from pinned upstream license")
    for kind, filename in entry.get("queries", {}).items():
        expected = query_content(entry["query-sources"][kind], entries)
        require((local / filename).read_bytes() == expected,
                f"{name}: {filename} differs from pinned upstream query-sources")
        shutil.copyfile(local / filename, directory / filename)
    for filename in ("grammar.toml", "LICENSE"):
        shutil.copyfile(local / filename, directory / filename)
    # Inherited query copyright notices must travel with the bundled queries.
    dependencies = {source.split(":", 1)[0]
                    for sources in entry.get("query-sources", {}).values() for source in sources} - {name}
    licenses = {}
    for dependency in sorted(dependencies):
        destination = directory / f"LICENSE-{dependency}"
        require((ROOT / "grammars" / dependency / "LICENSE").read_bytes()
                == license_content(entries[dependency]),
                f"{name}: inherited LICENSE-{dependency} differs from pinned upstream license")
        shutil.copyfile(ROOT / "grammars" / dependency / "LICENSE", destination)
        licenses[dependency] = file_record(destination, output)
    return {
        "repository": entry["repository"], "revision": entry["revision"],
        "language": entry["language"], "abi_version": int(abi[1]),
        "aliases": entry["aliases"],
        **{key: entry[key] for key in ("scope", "injection_regex") if key in entry},
        "files": entry.get("files", {}),
        "wasm": file_record(wasm, output),
        "metadata": file_record(directory / "grammar.toml", output),
        "license": {"spdx": entry["license"], **file_record(directory / "LICENSE", output)},
        "dependency_licenses": licenses,
        "queries": {kind: file_record(directory / filename, output)
                    for kind, filename in entry.get("queries", {}).items()},
    }


def recover_output(destination, backup):
    if backup.exists():
        if destination.exists():
            # The previous process published successfully but did not remove its backup.
            shutil.rmtree(backup)
        else:
            backup.rename(destination)


def publish_output(output, destination, backup):
    recover_output(destination, backup)
    if destination.exists():
        destination.rename(backup)
    try:
        output.rename(destination)
    except BaseException:
        # Restore on rename failure or Python interruption. A killed process leaves
        # the backup outside the temporary build directory for the next run.
        if backup.exists():
            backup.rename(destination)
        raise
    if backup.exists():
        shutil.rmtree(backup)


def build(config, entries, selected):
    CACHE.mkdir(exist_ok=True)
    destination = ROOT / "dist"
    backup = CACHE / "previous-dist"
    recover_output(destination, backup)
    require(shutil.which("node"), "Missing node. Install Node.js 22 or newer.")
    version = subprocess.check_output(["node", "--version"], text=True).strip()
    require(int(version.lstrip("v").split(".")[0]) >= 22, f"Node.js 22+ required; found {version}")
    cli = cli_path(config)
    runtime = runtime_path(config)
    with tempfile.TemporaryDirectory(prefix="build-", dir=CACHE) as temporary:
        output = Path(temporary) / "dist"
        output.mkdir()
        languages = {name: build_language(name, entry, entries, cli, output)
                     for name, entry in selected.items()}
        shutil.copyfile(ROOT / "registry.toml", output / "registry.toml")
        (output / ".nojekyll").touch()
        index = {"schema_version": config["schema_version"],
                 "tree_sitter_version": config["tree-sitter"]["version"],
                 "registry": file_record(output / "registry.toml", output), "languages": languages}
        (output / "index.json").write_text(json.dumps(index, indent=2) + "\n")
        subprocess.run(["node", str(ROOT / "scripts/check-wasm.mjs"), str(output), str(runtime)],
                       cwd=ROOT, check=True)
        publish_output(output, destination, backup)
    print(f"Built {len(selected)} grammars. Serve dist/ to use index.json.", flush=True)


def main(command):
    descriptions = {"build": "Build pinned grammars",
                    "validate": "Validate registry metadata and local files without network access",
                    "sync": "Replace vendored queries and licenses from pinned upstream commits"}
    parser = argparse.ArgumentParser(description=descriptions[command])
    parser.add_argument("grammar", nargs="?", help="Select one grammar instead of all")
    arguments = parser.parse_args()
    try:
        config, entries = load_registry()
        selected = select(entries, arguments.grammar)
        if command == "build":
            with registry_lock(CACHE / "registry.lock"):
                build(config, entries, selected)
        elif command == "sync":
            with registry_lock(CACHE / "registry.lock"):
                sync_queries(entries, selected)
        else:
            print(f"Validated {len(selected)} grammars.")
    except (ValueError, KeyError, TypeError, OSError, tarfile.TarError,
            urllib.error.URLError, subprocess.CalledProcessError) as error:
        print(f"{command} failed: {error}", file=sys.stderr)
        sys.exit(1)
