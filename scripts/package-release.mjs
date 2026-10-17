import { cpSync, existsSync, lstatSync, mkdirSync, readdirSync, readFileSync, renameSync, rmSync, statSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { spawnSync } from "node:child_process";

// Delete only declared compiler output directories, then force a complete rebuild.
// Incremental builds alone leave JavaScript for deleted source files behind.
const projects = JSON.parse(readFileSync("tsconfig.json", "utf8")).references.map(({ path }) => path);
for (const project of projects) {
  const config = JSON.parse(readFileSync(join(project, "tsconfig.json"), "utf8"));
  if (config.compilerOptions?.outDir !== "dist") throw new Error(`Unexpected compiler output directory: ${project}`);
  rmSync(join(project, "dist"), { recursive: true, force: true });
}
const build = spawnSync(process.execPath, [join("node_modules", "typescript", "bin", "tsc"), "--build", "--force"], { stdio: "inherit", windowsHide: true });
if (build.status !== 0) throw new Error(`Clean release build failed: ${build.error?.message ?? build.status}`);
const version = JSON.parse(readFileSync("package.json", "utf8")).version;
if (process.argv.length > 2 && (process.argv[2] !== "--output" || process.argv.length !== 4)) throw new Error("Usage: package-release.mjs [--output <directory>]");
const target = process.argv[3] ?? join("release", `NOSH-Research-${version}`);
const staging = `${target}.creating`;
const backup = `${target}.previous`;
if (existsSync(staging)) rmSync(staging, { recursive: true, force: true });
if (existsSync(backup)) rmSync(backup, { recursive: true, force: true });
mkdirSync(staging, { recursive: true });
try {
  for (const path of [
  "apps/cli",
  "apps/noshd",
  "apps/tui",
  ...projects.filter((path) => path.startsWith("./packages/")).map((path) => path.slice(2)),
  "pi-package",
  "scripts/install.ps1",
  "scripts/uninstall.ps1",
  "scripts/generate-notices.mjs",
  "scripts/generate-sbom.mjs",
  "scripts/generate-schema-fixtures.mjs",
  "scripts/package-release.mjs",
  "scripts/delegate.mjs",
  "scripts/context-audit.mjs",
  "scripts/quality-check.mjs",
  "scripts/release-smoke.mjs",
  "eslint.config.mjs",
  "knip.json",
  "package.json",
  "pnpm-lock.yaml",
  "pnpm-workspace.yaml",
  "tsconfig.json",
  "tsconfig.base.json",
  ".nvmrc",
  "README.md",
  "ARCHITECTURE.md",
  "SECURITY.md",
  "THREAT_MODEL.md",
  "LICENSE",
  "THIRD_PARTY_NOTICES.md",
  "docs",
]) copy(path, join(staging, path));
  const git = spawnSync("git", ["rev-parse", "HEAD"], { encoding: "utf8", windowsHide: true }); const status = spawnSync("git", ["status", "--porcelain"], { encoding: "utf8", windowsHide: true }); writeFileSync(join(staging, "BUILD-METADATA.json"), `${JSON.stringify({ version, commit: git.status === 0 ? git.stdout.trim() : null, dirty: status.status !== 0 || Boolean(status.stdout.trim()), node: process.version, builtAt: new Date().toISOString() }, null, 2)}\n`, "utf8");
} catch (error) {
  rmSync(staging, { recursive: true, force: true });
  throw error;
}
try {
  if (existsSync(target)) renameSync(target, backup);
  renameSync(staging, target);
} catch (error) {
  if (existsSync(staging)) rmSync(staging, { recursive: true, force: true });
  if (!existsSync(target) && existsSync(backup)) renameSync(backup, target);
  throw error;
}
if (existsSync(backup)) {
  try {
    rmSync(backup, { recursive: true, force: true });
  } catch (error) {
    throw new Error(`Release promoted successfully, but cleanup of ${backup} failed: ${error instanceof Error ? error.message : String(error)}`);
  }
}

function copy(source, destination) { if (!existsSync(source)) throw new Error(`Missing release input: ${source}`); if (lstatSync(source).isSymbolicLink()) throw new Error(`Refusing symlink/reparse path: ${source}`); if (!statSync(source).isDirectory()) { mkdirSync(dirname(destination), { recursive: true }); cpSync(source, destination); return; } mkdirSync(destination, { recursive: true }); for (const entry of readdirSync(source, { withFileTypes: true })) { if (shouldSkip(entry.name, entry.isDirectory())) continue; const child = join(source, entry.name); if (lstatSync(child).isSymbolicLink()) throw new Error(`Refusing symlink/reparse path: ${child}`); copy(child, join(destination, entry.name)); } }
function shouldSkip(name, directory) { return ["node_modules", "dist-types", "coverage", ".git", ".nosh"].includes(name) || name.endsWith(".tsbuildinfo") || name.endsWith(".map") || (!directory && /\.(test|spec)(?:\.d)?\.[cm]?[jt]sx?$/.test(name)) || (!directory && /^\.env(?:\..*)?$/.test(name)); }
