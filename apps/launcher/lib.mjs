// The npm package `nosh-harness` is only this launcher. It is published once and never changes:
// the harness itself ships as GitHub release assets and installs into the per-user state directory.
//
// Contract with the harness (keep stable across releases):
//   1. each release carries `nosh-runtime-<x.y.z>.tgz` and `nosh-runtime-<x.y.z>.tgz.sha256`;
//   2. the installed `nosh-runtime` package names its CLI in `bin.nosh`;
//   3. `<cli> stop --if-idle` exits 0 after stopping noshd, 2 while work is active, 3 when noshd is not running.
import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import { closeSync, existsSync, mkdirSync, openSync, readFileSync, readdirSync, renameSync, rmSync, statSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";

export const RELEASES_URL = "https://api.github.com/repos/SirNosh/NOSH/releases/latest";
const RUNTIME_ASSET = /^nosh-runtime-(\d+\.\d+\.\d+)\.tgz$/;
const DAY_MS = 24 * 60 * 60 * 1000;

/** Same state root as the harness CLI (apps/cli/src/main.ts), so config, credentials, and Projects are shared. */
export function harnessRoot(env = process.env) {
  return join(resolve(env.LOCALAPPDATA || env.APPDATA || join(homedir(), ".nosh"), "NOSH"), "harness");
}

export function readCurrent(root) {
  const path = join(root, "current.json");
  return existsSync(path) ? JSON.parse(readFileSync(path, "utf8")) : null;
}

function writeJson(path, value) {
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(`${path}.tmp`, `${JSON.stringify(value, null, 2)}\n`);
  renameSync(`${path}.tmp`, path);
}

export function compareVersions(a, b) {
  const left = a.split(".").map(Number), right = b.split(".").map(Number);
  for (let index = 0; index < 3; index += 1) if (left[index] !== right[index]) return left[index] > right[index] ? 1 : -1;
  return 0;
}

export async function latestRelease(fetchImpl = fetch, timeoutMs = 10_000) {
  const response = await fetchImpl(RELEASES_URL, { headers: { accept: "application/vnd.github+json", "user-agent": "nosh-harness-launcher" }, signal: AbortSignal.timeout(timeoutMs) });
  if (!response.ok) throw new Error(`GitHub releases answered ${response.status}`);
  const release = await response.json();
  const assets = release.assets ?? [];
  const runtime = assets.find((asset) => RUNTIME_ASSET.test(asset.name));
  if (!runtime) throw new Error(`Release ${release.tag_name} has no nosh-runtime package`);
  const checksum = assets.find((asset) => asset.name === `${runtime.name}.sha256`);
  if (!checksum) throw new Error(`Release ${release.tag_name} has no checksum for ${runtime.name}`);
  return { version: RUNTIME_ASSET.exec(runtime.name)[1], name: runtime.name, url: runtime.browser_download_url, checksumUrl: checksum.browser_download_url };
}

async function download(fetchImpl, url) {
  const response = await fetchImpl(url, { headers: { "user-agent": "nosh-harness-launcher" }, signal: AbortSignal.timeout(300_000) });
  if (!response.ok) throw new Error(`Download of ${url} answered ${response.status}`);
  return Buffer.from(await response.arrayBuffer());
}

export function verifySha256(bytes, checksumText) {
  const expected = checksumText.trim().split(/\s+/)[0]?.toLowerCase();
  const actual = createHash("sha256").update(bytes).digest("hex");
  if (expected !== actual) throw new Error(`Checksum mismatch: expected ${expected}, got ${actual}`);
}

/** npm's own CLI next to this Node, so no shell parses paths; falls back to `npm` on PATH. */
export function runNpm(args, cwd) {
  const base = dirname(process.execPath);
  const cli = [join(base, "node_modules", "npm", "bin", "npm-cli.js"), join(base, "..", "lib", "node_modules", "npm", "bin", "npm-cli.js")].find((path) => existsSync(path));
  const result = cli
    ? spawnSync(process.execPath, [cli, ...args], { cwd, encoding: "utf8", windowsHide: true })
    : spawnSync(process.platform === "win32" ? "npm.cmd" : "npm", args.map((arg) => `"${arg}"`), { cwd, encoding: "utf8", windowsHide: true, shell: true });
  if (result.status !== 0) throw new Error(`npm ${args[0]} failed:\n${result.stderr || result.stdout || result.error?.message}`);
}

/** Installs one release into versions/<version>; the running version is never touched. Returns the CLI entry. */
async function installRelease(root, release, { fetchImpl = fetch, npm = runNpm } = {}) {
  const target = join(root, "versions", release.version);
  const entryOf = () => {
    const packageDirectory = join(target, "node_modules", "nosh-runtime");
    const { bin } = JSON.parse(readFileSync(join(packageDirectory, "package.json"), "utf8"));
    return join(packageDirectory, typeof bin === "string" ? bin : bin.nosh);
  };
  if (existsSync(join(target, ".complete"))) return entryOf();
  rmSync(target, { recursive: true, force: true });
  mkdirSync(target, { recursive: true });
  try {
    const bytes = await download(fetchImpl, release.url);
    verifySha256(bytes, (await download(fetchImpl, release.checksumUrl)).toString("utf8"));
    const archive = join(target, release.name);
    writeFileSync(archive, bytes);
    npm(["install", "--prefix", target, archive, "--omit=dev", "--no-audit", "--no-fund"], target);
    rmSync(archive, { force: true });
    const entry = entryOf();
    const smoke = spawnSync(process.execPath, [entry, "help"], { encoding: "utf8", windowsHide: true });
    if (smoke.status !== 0 || !smoke.stdout.includes("nosh setup")) throw new Error(`NOSH ${release.version} did not start:\n${smoke.stderr || smoke.stdout}`);
    writeFileSync(join(target, ".complete"), "");
    return entry;
  } catch (error) {
    rmSync(target, { recursive: true, force: true });
    throw error;
  }
}

function withLock(root, action) {
  const lock = join(root, "update.lock");
  mkdirSync(root, { recursive: true });
  // A lock left by a crashed update stops mattering after an hour.
  if (existsSync(lock) && Date.now() - statSync(lock).mtimeMs > 60 * 60 * 1000) rmSync(lock, { force: true });
  let handle;
  try { handle = openSync(lock, "wx"); } catch { throw new Error(`Another NOSH update is running (lock: ${lock})`); }
  closeSync(handle);
  return action().finally(() => rmSync(lock, { force: true }));
}

/**
 * Installs the latest release and switches to it. With a version already installed it first asks that
 * version to stop noshd only if idle, and restarts noshd on the new version if it had been running.
 * Returns the process exit code: 0 done or up to date, 2 postponed because work is active.
 */
export function update(root, { fetchImpl = fetch, npm = runNpm, log = (line) => process.stdout.write(`${line}\n`) } = {}) {
  return withLock(root, async () => {
    const current = readCurrent(root);
    const release = await latestRelease(fetchImpl);
    if (current && compareVersions(release.version, current.version) <= 0) { log(`NOSH ${current.version} is up to date.`); return 0; }
    log(`Installing NOSH ${release.version} from GitHub releases...`);
    const entry = await installRelease(root, release, { fetchImpl, npm });
    let wasRunning = false;
    if (current) {
      const stop = spawnSync(process.execPath, [current.entry, "stop", "--if-idle"], { stdio: "inherit", windowsHide: true });
      if (stop.status === 2) { log(`NOSH ${release.version} is installed but not active yet. Run \`nosh update\` again once that work is done.`); return 2; }
      if (stop.status !== 0 && stop.status !== 3) throw new Error(`Could not stop noshd (exit ${stop.status}); still on NOSH ${current.version}`);
      wasRunning = stop.status === 0;
    }
    writeJson(join(root, "current.json"), { version: release.version, entry });
    writeJson(join(root, "last-check.json"), { checkedAt: Date.now(), latest: release.version });
    for (const version of readdirSync(join(root, "versions"))) {
      // Another open terminal may still run an old version; pruning is best effort.
      if (version !== release.version && version !== current?.version) try { rmSync(join(root, "versions", version), { recursive: true, force: true }); } catch { /* retried on the next update */ }
    }
    if (wasRunning && spawnSync(process.execPath, [entry, "start"], { stdio: "inherit", windowsHide: true }).status !== 0) log("noshd did not restart; run `nosh start`.");
    log(current ? `Updated NOSH ${current.version} -> ${release.version}.` : `Installed NOSH ${release.version}.`);
    return 0;
  });
}

/** At most once a day, tells the user a newer release exists. Never throws and never slows a command by more than ~2 s. */
export async function maybeNotify(root, { fetchImpl = fetch, now = Date.now(), log = (line) => process.stderr.write(`${line}\n`) } = {}) {
  try {
    const current = readCurrent(root);
    const statePath = join(root, "last-check.json");
    if (!current || (existsSync(statePath) && now - JSON.parse(readFileSync(statePath, "utf8")).checkedAt < DAY_MS)) return;
    const release = await latestRelease(fetchImpl, 2_000);
    writeJson(statePath, { checkedAt: now, latest: release.version });
    if (compareVersions(release.version, current.version) > 0) log(`NOSH ${release.version} is available (you have ${current.version}). Run \`nosh update\`.`);
  } catch { /* offline or rate-limited: try again next time */ }
}
