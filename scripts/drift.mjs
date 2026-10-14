#!/usr/bin/env node
// Lists TypeScript changes since each Rust crate was last synced (port-map.json).
// --check exits 1 when a crate's strict path changed after its sync commit, or no longer exists.
// The TypeScript tree is --ts-root, else NOSH_TS_ROOT, else the parent of this worktree.
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const rustRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const args = process.argv.slice(2);
const check = args.includes("--check");
const tsRoot = resolve(option("--ts-root") ?? process.env.NOSH_TS_ROOT ?? resolve(rustRoot, ".."));
const map = JSON.parse(readFileSync(resolve(rustRoot, "port-map.json"), "utf8"));

console.log(`TypeScript tree ${tsRoot} at ${git("rev-parse", "--short=12", "HEAD")}`);
let failures = 0;
for (const [name, unit] of Object.entries(map.crates)) {
  for (const path of [...unit.paths, ...unit.strict]) {
    if (gitOk("cat-file", "-e", `HEAD:${path}`)) continue;
    const strict = unit.strict.includes(path);
    console.log(`${name}: ${strict ? "STRICT path" : "path"} ${path} no longer exists in the TypeScript tree`);
    if (strict && unit.synced) failures += 1;
  }
  if (!unit.synced) {
    console.log(`${name}: not started`);
    continue;
  }
  if (!gitOk("cat-file", "-e", `${unit.synced}^{commit}`)) {
    console.log(`${name}: sync commit ${unit.synced} is not in the TypeScript history`);
    failures += 1;
    continue;
  }
  const range = `${unit.synced}..HEAD`;
  const log = git("log", "--oneline", range, "--", ...unit.paths, ...unit.strict);
  if (!log) {
    console.log(`${name}: up to date with ${unit.synced.slice(0, 12)}`);
    continue;
  }
  console.log(`${name}: ${log.split("\n").length} commit(s) since ${unit.synced.slice(0, 12)}`);
  console.log(indent(log));
  console.log(indent(git("diff", "--stat", range, "--", ...unit.paths, ...unit.strict)));
  if (unit.strict.length && git("log", "--oneline", range, "--", ...unit.strict)) {
    console.log(`${name}: STRICT paths changed; port them before bumping 'synced'`);
    failures += 1;
  }
}
if (check && failures) {
  console.log(`drift check failed: ${failures} problem(s)`);
  process.exit(1);
}

function option(flag) {
  const index = args.indexOf(flag);
  return index >= 0 ? args[index + 1] : undefined;
}
function run(gitArgs) {
  return spawnSync("git", ["-C", tsRoot, ...gitArgs], { encoding: "utf8", windowsHide: true });
}
function git(...gitArgs) {
  const result = run(gitArgs);
  if (result.status !== 0) {
    process.stderr.write(`git ${gitArgs.join(" ")} failed:\n${result.stderr}`);
    process.exit(2);
  }
  return result.stdout.trim();
}
function gitOk(...gitArgs) {
  return run(gitArgs).status === 0;
}
function indent(text) {
  return text.replace(/^/gm, "    ");
}
