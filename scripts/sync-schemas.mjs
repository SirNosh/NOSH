#!/usr/bin/env node
// Copies the TypeScript wire JSON Schemas and golden fixtures into conformance/schemas,
// recording the source commit in conformance/schemas/SOURCE.json.
// --check exits 1 if the vendored copy differs from the TypeScript tree (line endings ignored).
// The TypeScript tree is --ts-root, else NOSH_TS_ROOT, else the parent of this worktree.
import { existsSync, mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const rustRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const args = process.argv.slice(2);
const check = args.includes("--check");
const tsRoot = resolve(option("--ts-root") ?? process.env.NOSH_TS_ROOT ?? resolve(rustRoot, ".."));
const vendored = join(rustRoot, "conformance", "schemas");
const sources = [
  ["packages/wire/src/schemas", "json"],
  ["fixtures/schemas", "fixtures"],
];

if (check) {
  let differences = 0;
  for (const [from, to] of sources) {
    const theirs = listFiles(join(tsRoot, from));
    const ours = listFiles(join(vendored, to));
    if (!theirs.length) {
      console.log(`missing in TypeScript tree: ${from}`);
      differences += 1;
    }
    for (const file of new Set([...theirs, ...ours])) {
      const a = theirs.includes(file) ? text(join(tsRoot, from, file)) : null;
      const b = ours.includes(file) ? text(join(vendored, to, file)) : null;
      if (a === b) continue;
      console.log(`${a === null ? "removed upstream" : b === null ? "added upstream" : "changed"}: ${from}/${file}`);
      differences += 1;
    }
  }
  if (differences) {
    console.log(`${differences} difference(s); run node scripts/sync-schemas.mjs and port the changes`);
    process.exit(1);
  }
  console.log("vendored schemas and fixtures match the TypeScript tree");
} else {
  for (const [from, to] of sources) {
    const files = listFiles(join(tsRoot, from));
    if (!files.length) throw new Error(`no files under ${join(tsRoot, from)}`);
    rmSync(join(vendored, to), { recursive: true, force: true });
    for (const file of files) {
      const target = join(vendored, to, file);
      mkdirSync(dirname(target), { recursive: true });
      writeFileSync(target, text(join(tsRoot, from, file)));
    }
    console.log(`copied ${files.length} files from ${from}`);
  }
  const head = spawnSync("git", ["-C", tsRoot, "rev-parse", "HEAD"], { encoding: "utf8", windowsHide: true });
  if (head.status !== 0) throw new Error(head.stderr);
  writeFileSync(join(vendored, "SOURCE.json"), `${JSON.stringify({ commit: head.stdout.trim() }, null, 2)}\n`);
}

function option(flag) {
  const index = args.indexOf(flag);
  return index >= 0 ? args[index + 1] : undefined;
}
function listFiles(dir, prefix = "") {
  if (!existsSync(dir)) return [];
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) =>
    entry.isDirectory()
      ? listFiles(join(dir, entry.name), `${prefix}${entry.name}/`)
      : [`${prefix}${entry.name}`],
  );
}
function text(path) {
  return readFileSync(path, "utf8").replace(/\r\n/g, "\n");
}
