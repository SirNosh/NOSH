#!/usr/bin/env node
// Packs nosh-runtime-<version>.tgz and its .sha256, the release assets the nosh-harness launcher installs, as
// scripts/package-npm.mjs and the release.yml checksum step do in TypeScript. The package holds nosh.exe behind the JS
// bin stub packaging/nosh.mjs, with the Pi sidecar's host.mjs, pi-package/ and the record schemas that
// Installation::find() looks for above the executable. Pi itself is an npm dependency, hoisted beside the package by
// the launcher's `npm install --prefix`. Installing and running the package is scripts/rehearse-runtime.mjs's job.
// Usage: node scripts/package-runtime.mjs [--output <dir>]   (default release/npm)
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { cpSync, existsSync, mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const rustRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const args = process.argv.slice(2);
const output = resolve(option("--output") ?? join(rustRoot, "release", "npm"));
const EXACT = /^\d+\.\d+\.\d+$/;
// From the TypeScript tree's apps/tui/UPSTREAM.md ("Upstream MIT license"), verbatim: the Rust tree has no copy yet.
const OPENCODE_LICENSE_SOURCE = "Source: [LICENSE](https://github.com/anomalyco/opencode/blob/b3f1a96c6dd7adeb28b36dd11add1998fc84d67b/LICENSE)";
const OPENCODE_LICENSE = `MIT License

Copyright (c) 2025 opencode

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.`;

// The runtime version is the nosh crate's, so the TUI footer and package.json agree; the launcher accepts only x.y.z.
const metadata = cargoMetadata(["--no-deps"]);
const nosh = metadata.packages.find((unit) => unit.name === "nosh");
const version = nosh.version;
if (!EXACT.test(version)) throw new Error(`Runtime version ${version} must be x.y.z: the nosh-harness launcher installs only nosh-runtime-<x.y.z>.tgz`);

// The C runtime is linked statically so nosh.exe runs on a Windows without the VC++ redistributable (Node links its
// own statically too). The flag is set for this build only; test builds keep the default.
const build = spawnSync("cargo", ["build", "--release", "--locked", "-p", "nosh"], { cwd: rustRoot, stdio: "inherit", windowsHide: true, env: { ...process.env, RUSTFLAGS: "-C target-feature=+crt-static" } });
if (build.status !== 0) throw new Error(`Build failed: ${build.error?.message ?? build.status}`);
const exe = join(metadata.target_directory, "release", "nosh.exe");
if (!existsSync(exe)) throw new Error(`Missing ${exe}`);
// Import names are ASCII in the PE import table, so the DLL's name anywhere in the file means a dynamic C runtime.
if (readFileSync(exe).toString("latin1").toLowerCase().includes("vcruntime140")) throw new Error("nosh.exe links the Visual C++ runtime DLL (VCRUNTIME140.dll); build with -C target-feature=+crt-static (RUSTFLAGS)");

const host = readJson("sidecar/pi-host/package.json");
const pi = readJson("pi-package/package.json");
const lock = readJson("sidecar/pi-host/package-lock.json");
for (const [name, pinned] of Object.entries(host.dependencies)) {
  if (!EXACT.test(pinned)) throw new Error(`sidecar/pi-host/package.json pins ${name}@${pinned}; the runtime needs exact versions (host.mjs checks PI_VERSION)`);
}

const staging = join(output, "package");
rmSync(staging, { recursive: true, force: true });
mkdirSync(staging, { recursive: true });
cpSync(exe, join(staging, "bin", "nosh.exe"));
cpSync(join(rustRoot, "packaging", "nosh.mjs"), join(staging, "bin", "nosh.mjs"));
cpSync(join(rustRoot, "sidecar", "pi-host", "host.mjs"), join(staging, "sidecar", "pi-host", "host.mjs"));
copyTree("pi-package");
copyTree(join("conformance", "schemas", "json"));
for (const file of ["README.md", "LICENSE"]) cpSync(join(rustRoot, file), join(staging, file));
writeFileSync(join(staging, "THIRD_PARTY_NOTICES.md"), notices());
writeFileSync(join(staging, "package.json"), `${JSON.stringify({
  name: "nosh-runtime", version, description: "Networked Orchestrated Science Harness: a local research harness and terminal UI", license: nosh.license,
  repository: { type: pi.repository.type, url: pi.repository.url }, homepage: pi.homepage.replace("#pi-package", ""),
  bin: { nosh: "bin/nosh.mjs" }, os: ["win32"], cpu: ["x64"], engines: host.engines, dependencies: host.dependencies,
}, null, 2)}\n`);

const name = npm(["pack", "--pack-destination", relative(staging, output) || "."], staging).trim().split(/\r?\n/).at(-1);
if (name !== `nosh-runtime-${version}.tgz`) throw new Error(`npm pack wrote ${name}, expected nosh-runtime-${version}.tgz`);
const tgz = join(output, name), bytes = readFileSync(tgz);
// The byte format of the published TypeScript assets (pwsh Set-Content); the launcher reads the first token.
writeFileSync(`${tgz}.sha256`, `${createHash("sha256").update(bytes).digest("hex")}  ${name}\r\n`);

// Windows' own tar (bsdtar) lists the archive from stdin: it reads path arguments in the ANSI code page, where a
// Cyrillic --output directory becomes "?".
const tar = spawnSync(process.platform === "win32" ? join(process.env.SystemRoot ?? "C:\\Windows", "System32", "tar.exe") : "tar", ["-tzf", "-"], { input: bytes, encoding: "utf8", windowsHide: true });
if (tar.status !== 0) throw new Error(`tar -tzf ${tgz} failed: ${tar.stderr || tar.error?.message}`);
const entries = tar.stdout.split(/\r?\n/).filter(Boolean);
const schemas = readdirSync(join(rustRoot, "conformance", "schemas", "json")).map((file) => `package/conformance/schemas/json/${file}`);
for (const path of ["package/package.json", "package/bin/nosh.mjs", "package/bin/nosh.exe", "package/sidecar/pi-host/host.mjs", "package/pi-package/package.json",
  "package/pi-package/skills/nosh-control/SKILL.md", "package/LICENSE", "package/THIRD_PARTY_NOTICES.md", ...schemas]) {
  if (!entries.includes(path)) throw new Error(`nosh-runtime is missing ${path}`);
}
const shippedSchemas = entries.filter((path) => path.startsWith("package/conformance/schemas/json/"));
if (shippedSchemas.length !== schemas.length) throw new Error(`nosh-runtime contains ${shippedSchemas.find((path) => !schemas.includes(path))}`);
for (const path of entries) {
  const segments = path.split("/"), file = segments.at(-1);
  if (segments.some((segment) => ["node_modules", "target", ".git", ".nosh"].includes(segment)) || /\.(?:map|pdb|tsbuildinfo)$/.test(file)
    || /\.(test|spec)(?:\.d)?\.[cm]?[jt]sx?$/.test(file) || /^\.env(?:\..*)?$/.test(file) || ["timeline.md", "project.md"].includes(file)) throw new Error(`nosh-runtime contains ${path}`);
}
process.stdout.write(`Packed ${tgz}\nChecksum ${tgz}.sha256\nInstall: npm install -g ${tgz}\n`);

/**
 * THIRD_PARTY_NOTICES.md in the format of the TypeScript scripts/generate-notices.mjs: the crates compiled into
 * nosh.exe and the Pi tree npm installs with the package, grouped by license, then the OpenCode notice in full.
 */
function notices() {
  const groups = {};
  const add = (license, name, version, homepage) => {
    const entry = ((groups[license] ??= {})[name] ??= { versions: [], homepage });
    if (!entry.versions.includes(version)) entry.versions.push(version);
  };
  // Normal dependencies of the nosh crate on the release target, through the workspace crates.
  const full = cargoMetadata(["--filter-platform", "x86_64-pc-windows-msvc"]);
  const units = new Map(full.packages.map((unit) => [unit.id, unit]));
  const nodes = new Map(full.resolve.nodes.map((node) => [node.id, node]));
  const members = new Set(full.workspace_members);
  const pending = [full.packages.find((unit) => unit.name === "nosh" && members.has(unit.id)).id], seen = new Set();
  while (pending.length) {
    const id = pending.pop();
    if (seen.has(id)) continue;
    seen.add(id);
    const unit = units.get(id);
    if (!members.has(id)) add(unit.license ?? `See license file (${unit.license_file})`, unit.name, unit.version, unit.homepage ?? unit.repository);
    for (const dependency of nodes.get(id).deps) if (dependency.dep_kinds.some(({ kind }) => kind === null)) pending.push(dependency.pkg);
  }
  // Homepages come from the sidecar's installed packages (npm ci); platform packages that were not installed have none.
  for (const [key, entry] of Object.entries(lock.packages)) {
    if (!key) continue;
    const manifest = join(rustRoot, "sidecar", "pi-host", key, "package.json");
    add(entry.license, key.slice(key.lastIndexOf("node_modules/") + "node_modules/".length), entry.version, existsSync(manifest) ? JSON.parse(readFileSync(manifest, "utf8")).homepage : undefined);
  }
  const lines = ["# Third-party notices", "", "Generated from the production dependency graphs in `Cargo.lock` (compiled into `nosh.exe`) and `sidecar/pi-host/package-lock.json` (installed from npm with the package). NOSH source is Apache-2.0; dependencies retain their own licenses.", ""];
  for (const license of Object.keys(groups).sort()) {
    lines.push(`## ${license}`, "");
    for (const [name, { versions, homepage }] of Object.entries(groups[license]).sort(([left], [right]) => left.localeCompare(right))) {
      lines.push(`- ${name} ${versions.sort((left, right) => left.localeCompare(right, "en", { numeric: true })).join(", ")}${homepage ? ` — ${homepage}` : ""}`);
    }
    lines.push("");
  }
  lines.push("## OpenCode terminal UI adaptations", "", "Theme values, border characters, layout, and interaction adaptations are based on OpenCode (MIT, Copyright (c) 2025 opencode).", "", OPENCODE_LICENSE_SOURCE, "", "```text", OPENCODE_LICENSE, "```", "");
  return `${lines.join("\n")}\n`;
}

function cargoMetadata(extra) {
  const result = spawnSync("cargo", ["metadata", "--format-version", "1", "--locked", ...extra], { cwd: rustRoot, encoding: "utf8", windowsHide: true, maxBuffer: 1 << 30 });
  if (result.status !== 0) throw new Error(`cargo metadata failed: ${result.stderr || result.error?.message}`);
  return JSON.parse(result.stdout);
}

/** npm's own CLI next to this Node, as the launcher's runNpm finds it, else `npm` on PATH; returns stdout. */
function npm(npmArgs, cwd) {
  const base = dirname(process.execPath);
  const cli = [join(base, "node_modules", "npm", "bin", "npm-cli.js"), join(base, "..", "lib", "node_modules", "npm", "bin", "npm-cli.js")].find((path) => existsSync(path));
  const result = cli
    ? spawnSync(process.execPath, [cli, ...npmArgs], { cwd, encoding: "utf8", windowsHide: true })
    : spawnSync(process.platform === "win32" ? "npm.cmd" : "npm", npmArgs.map((arg) => `"${arg}"`), { cwd, encoding: "utf8", windowsHide: true, shell: true });
  if (result.status !== 0) throw new Error(`npm ${npmArgs[0]} failed:\n${result.stderr || result.stdout || result.error?.message}`);
  return result.stdout;
}

/** Copies a source directory into the staging directory, refusing links as the TypeScript package-release.mjs does. */
function copyTree(path) {
  for (const entry of readdirSync(join(rustRoot, path), { withFileTypes: true })) {
    const child = join(path, entry.name);
    if (entry.isSymbolicLink()) throw new Error(`Refusing symlink/reparse path: ${join(rustRoot, child)}`);
    if (entry.isDirectory()) copyTree(child);
    else cpSync(join(rustRoot, child), join(staging, child));
  }
}

function readJson(path) {
  return JSON.parse(readFileSync(join(rustRoot, path), "utf8"));
}

function option(flag) {
  const index = args.indexOf(flag);
  return index >= 0 ? args[index + 1] : undefined;
}
