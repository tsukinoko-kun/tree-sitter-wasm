import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { createHash } from "node:crypto";
import { pathToFileURL } from "node:url";

if (!process.argv[3]) throw new Error("Usage: node scripts/check-wasm.mjs <dist> <runtime.js>");
const { Parser, Language, Query } = await import(pathToFileURL(resolve(process.argv[3])).href);

const directory = resolve(process.argv[2] ?? "dist");
const index = JSON.parse(await readFile(resolve(directory, "index.json"), "utf8"));

async function read(record) {
  const path = resolve(directory, record.path);
  if (!path.startsWith(`${directory}/`)) throw new Error(`Unsafe index path: ${record.path}`);
  const bytes = await readFile(path);
  const hash = createHash("sha256").update(bytes).digest("hex");
  if (hash !== record.sha256 || bytes.length !== record.size) {
    throw new Error(`Index checksum or size mismatch: ${record.path}`);
  }
  return bytes;
}

await Parser.init();
await read(index.registry);
for (const [name, entry] of Object.entries(index.languages)) {
  try {
    const language = await Language.load(await read(entry.wasm));
    const parser = new Parser();
    try {
      parser.setLanguage(language);
      const tree = parser.parse("");
      if (!tree) throw new Error("Parser did not produce a syntax tree");
      tree.delete();
      if (language.abiVersion !== entry.abi_version) throw new Error("Parser ABI differs from index");
      for (const [kind, record] of Object.entries(entry.queries)) {
        try {
          const query = new Query(language, (await read(record)).toString("utf8"));
          query.delete();
        } catch (cause) {
          throw new Error(`Invalid ${kind} query`, { cause });
        }
      }
      await read(entry.metadata);
      await read(entry.license);
      for (const license of Object.values(entry.dependency_licenses)) await read(license);
    } finally {
      parser.delete();
    }
    console.log(`Verified ${name}: WASM loads, parser runs, queries compile, hashes match`);
  } catch (cause) {
    throw new Error(`Verification failed for ${name}`, { cause });
  }
}
