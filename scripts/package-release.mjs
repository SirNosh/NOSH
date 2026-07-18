import { cpSync, existsSync, mkdirSync, readdirSync, rmSync, statSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { spawnSync } from "node:child_process";

const version = JSON.parse(await import("node:fs").then(({ readFileSync }) => readFileSync("package.json", "utf8"))).version; const target = join("release", `NOSH-Research-${version}`);
if (existsSync(target)) rmSync(target, { recursive: true }); mkdirSync(target, { recursive: true });
for (const path of ["apps", "packages", "pi-package", "scripts/install.ps1", "scripts/uninstall.ps1", "package.json", "pnpm-lock.yaml", "pnpm-workspace.yaml", ".nvmrc", "README.md", "LICENSE", "THIRD_PARTY_NOTICES.md"]) copy(path, join(target, path));
const git = spawnSync("git", ["rev-parse", "HEAD"], { encoding: "utf8", windowsHide: true }); const status = spawnSync("git", ["status", "--porcelain"], { encoding: "utf8", windowsHide: true }); writeFileSync(join(target, "BUILD-METADATA.json"), `${JSON.stringify({ version, commit: git.status === 0 ? git.stdout.trim() : null, dirty: status.status !== 0 || Boolean(status.stdout.trim()), node: process.version, builtAt: new Date().toISOString() }, null, 2)}\n`, "utf8");

function copy(source, destination) { if (!existsSync(source)) return; if (!statSync(source).isDirectory()) { mkdirSync(dirname(destination), { recursive: true }); cpSync(source, destination); return; } mkdirSync(destination, { recursive: true }); for (const entry of readdirSync(source, { withFileTypes: true })) { if (["node_modules", "dist-types"].includes(entry.name) || entry.name.endsWith(".tsbuildinfo")) continue; copy(join(source, entry.name), join(destination, entry.name)); } }
