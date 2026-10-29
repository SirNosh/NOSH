#!/usr/bin/env node
// Rehearses installing nosh-runtime-<version>.tgz the way users get it, in throwaway state, as the TypeScript
// release.yml "Install the runtime package and run it" step and apps/launcher/launcher.test.mjs do for the TypeScript
// runtime. Phase R0 installs the tgz with the launcher's own `npm install --prefix` and runs the installed daemon.
// Phase A installs and updates it through the real nosh-harness launcher code, and phase B moves a TypeScript 0.1.30
// install onto it. The launcher's GitHub release API is answered by an in-process fake; nothing is published. Phase B
// downloads the real 0.1.30 assets from the GitHub release.
// Usage: node scripts/rehearse-runtime.mjs [--tgz <path>] [--launcher <dir>]
//   --tgz       default release/npm/nosh-runtime-<version>.tgz, the version from the staging package.json
//   --launcher  a directory holding lib.mjs and nosh.mjs (e.g. $NOSH_TS_ROOT/apps/launcher); default nosh-harness@1.0.0 from npm
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { createHash, randomBytes } from "node:crypto";
import { closeSync, existsSync, mkdirSync, mkdtempSync, openSync, readdirSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { homedir, tmpdir } from "node:os";
import { dirname, join, resolve, sep } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const rustRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const args = process.argv.slice(2);
const LAUNCHER = "nosh-harness@1.0.0";
const TS_BASELINE = "0.1.30";
const ACTIVE = ["queued", "starting", "running", "checkpointing", "finishing"];
// R-A11's Job: the daemon accepts only NOSH ids (`<prefix>_` and 32 lowercase hex digits), as the TypeScript isNoshId does.
const JOB = `job_${"0".repeat(32)}`, RUN = `run_${"0".repeat(32)}`;
// The state directory of R0 and A. The space, the ü and the Д exercise quoting and wide-character paths in the stub, the
// launcher, the sidecar and job-wrapper spawns; the Д lies outside the ANSI code page of an en-US Windows (CI's 1252).
const STATE_PREFIX = "nosh rehearsal üД-";
const piDependencies = readJson(join(rustRoot, "sidecar", "pi-host", "package.json")).dependencies;
const tgz = resolve(option("--tgz") ?? join(rustRoot, "release", "npm", `nosh-runtime-${readJson(join(rustRoot, "release", "npm", "package", "package.json")).version}.tgz`));
const version = /nosh-runtime-(\d+\.\d+\.\d+)\.tgz$/.exec(tgz)?.[1];
assert.ok(version, `${tgz} is not named nosh-runtime-<x.y.z>.tgz, so the launcher would not install it`);
const tgzBytes = readFileSync(tgz), sha256Text = readFileSync(`${tgz}.sha256`, "utf8");

// Isolation comes first. The launcher runs its children with this process's environment, so a real `stop --if-idle`
// would stop the user's daemon: state, Pi's agent directory and the home directory move to throwaway directories
// before any launcher call. npm keeps its real cache (it would otherwise follow LOCALAPPDATA); npm refuses to print a
// cache path its log redaction would mask, such as one holding a UUID, and then its default location is kept. Git
// commits in the test repositories need an identity.
if (!process.env.npm_config_cache) {
  try {
    process.env.npm_config_cache = npm(["config", "get", "cache"], rustRoot).trim();
  } catch {
    const windows = process.platform === "win32";
    process.env.npm_config_cache = join((windows && process.env.LOCALAPPDATA) || homedir(), windows ? "npm-cache" : ".npm");
  }
}
Object.assign(process.env, { GIT_AUTHOR_NAME: "NOSH rehearsal", GIT_COMMITTER_NAME: "NOSH rehearsal", GIT_AUTHOR_EMAIL: "rehearsal@nosh.invalid", GIT_COMMITTER_EMAIL: "rehearsal@nosh.invalid" });
const scratch = [], states = [];
const summary = [];
let launcher;
try {
  launcher = await loadLauncher();
  launcher.verifySha256(tgzBytes, sha256Text);
  assert.equal(launcher.compareVersions(version, TS_BASELINE), 1, `NOSH ${version} is not above TypeScript ${TS_BASELINE} (Rust prereleases are 0.2.x): nosh update would keep TypeScript installs on TypeScript and move this runtime's users back to it`);
  summary.push(await phaseR0(), await phaseA(), await phaseB());
} finally {
  for (const state of states) await stopAndRemove(state);
  for (const directory of scratch) rmSync(directory, { recursive: true, force: true, maxRetries: 10, retryDelay: 500 });
}
process.stdout.write(`rehearsal passed\n${summary.map((line) => `  ${line}\n`).join("")}`);

/** A fresh state directory (its long path, so process paths compare) that LOCALAPPDATA, APPDATA and HOME now name. */
function isolate(prefix) {
  const state = realpathSync.native(mkdtempSync(join(tmpdir(), prefix)));
  Object.assign(process.env, { LOCALAPPDATA: state, APPDATA: state, USERPROFILE: state, HOME: state, PI_CODING_AGENT_DIR: join(state, "pi-agent") });
  return state;
}

// R0: the layout, the hoisted Pi install and the installed daemon, without the launcher's smoke test (which needs
// the CLI's `help`). /api/models answers only when the sidecar loaded Pi from where npm put it.
async function phaseR0() {
  const state = isolate(STATE_PREFIX);
  scratch.push(state);
  const prefix = join(state, "r0");
  mkdirSync(prefix);
  launcher.runNpm(["install", "--prefix", prefix, tgz, "--omit=dev", "--no-audit", "--no-fund"], prefix);
  const runtime = join(prefix, "node_modules", "nosh-runtime");
  for (const path of [join(runtime, "bin", "nosh.mjs"), join(runtime, "bin", "nosh.exe"), join(runtime, "sidecar", "pi-host", "host.mjs")]) assert.ok(existsSync(path), `npm install --prefix did not install ${path}`);
  hoisted(prefix);
  const port = await freePort(), logPath = join(prefix, "daemon.log");
  const log = openSync(logPath, "a");
  const daemon = spawn(join(runtime, "bin", "nosh.exe"), ["daemon", `--data-dir=${join(prefix, "data")}`, `--port=${port}`], { stdio: ["ignore", log, log], windowsHide: true, env: { ...process.env, NOSH_BOOTSTRAP_TOKEN: "rehearsal", NOSH_NODE: process.execPath } });
  closeSync(log);
  const exited = new Promise((resolve) => daemon.once("exit", resolve));
  try {
    await until(async () => daemon.exitCode !== null || await healthy(port), 90_000, () => `noshd was not healthy within 90 s; daemon.log:\n${readFileSync(logPath, "utf8")}`);
    assert.equal(daemon.exitCode, null, `noshd exited with ${daemon.exitCode} before it was healthy; daemon.log:\n${readFileSync(logPath, "utf8")}`);
    const models = await http(port, "/api/models");
    assert.equal(models.status, 200, `GET /api/models answered ${models.status}; daemon.log:\n${readFileSync(logPath, "utf8")}`);
    assert.ok(Array.isArray((await models.json()).models), "GET /api/models has no models array");
    const shutdown = await http(port, "/api/shutdown", { method: "POST", json: {} });
    assert.equal(shutdown.status, 202);
    assert.deepEqual(await shutdown.json(), { stopping: true });
    assert.equal(await within(exited, 10_000, "noshd did not exit within 10 s of /api/shutdown"), 0);
    assert.ok(readFileSync(logPath, "utf8").includes(`noshd listening on 127.0.0.1:${port}`), "daemon.log has no listening line");
  } finally {
    if (daemon.exitCode === null) {
      await http(port, "/api/shutdown", { method: "POST", json: {} }).catch(() => {});
      await within(exited, 10_000, "").catch(() => {
        process.stderr.write("rehearsal: noshd ignored /api/shutdown after phase R0; killing it\n");
        daemon.kill();
        return within(exited, 10_000, "").catch(() => {});
      });
    }
  }
  return `R0 ${version}: npm install --prefix layout, hoisted ${Object.keys(piDependencies).join(", ")}, installed daemon served /api/models and shut down cleanly`;
}

// A: install, update with a running daemon, postponement while a Job runs, the .complete fast path, pruning and a
// refused checksum, all through the launcher's update() and nosh.mjs.
async function phaseA() {
  const state = isolate(STATE_PREFIX);
  states.push(state);
  const root = launcher.harnessRoot(process.env);
  const lines = [], log = (line) => lines.push(line);
  const [v1, v2, v3] = [1, 2, 3].map((step) => { const [major, minor, patch] = version.split(".").map(Number); return `${major}.${minor}.${patch + step}`; });
  const entry = (v) => join(root, "versions", v, "node_modules", "nosh-runtime", "bin", "nosh.mjs");

  let fetchImpl = release(version, tgzBytes, sha256Text);
  assert.equal(await launcher.update(root, { fetchImpl, log }), 0, "R-A1");
  assert.deepEqual(lines.splice(0), [`Installing NOSH ${version} from GitHub releases...`, `Installed NOSH ${version}.`]);
  assert.deepEqual(fetchImpl.downloads, { [fetchImpl.url]: 1, [`${fetchImpl.url}.sha256`]: 1 });
  assert.deepEqual(launcher.readCurrent(root), { version, entry: entry(version) });
  assert.ok(existsSync(join(root, "versions", version, ".complete")));
  assert.deepEqual(readJson(join(root, "versions", version, "package.json")), { dependencies: { "nosh-runtime": `file:nosh-runtime-${version}.tgz` } });
  assert.ok(!existsSync(join(root, "versions", version, `nosh-runtime-${version}.tgz`)), "the downloaded archive was not removed");
  hoisted(join(root, "versions", version));
  const check = readJson(join(root, "last-check.json"));
  assert.equal(typeof check.checkedAt, "number");
  assert.equal(check.latest, version);
  assert.ok(!existsSync(join(root, "update.lock")));

  let result = cli("--version");
  assert.equal(result.status, 0, "R-A2");
  assert.equal(result.stdout, `NOSH ${version} (nosh-harness launcher ${readJson(join(launcher.directory, "package.json")).version})\n`);
  result = cli("help");
  assert.equal(result.status, 0, "R-A3");
  assert.ok(result.stdout.includes("nosh setup"), result.stdout);
  result = cli("stop", "--if-idle");
  assert.equal(result.status, 3, "R-A4");
  assert.equal(result.stdout, "noshd is not running\n");
  result = cli("rehearsal-unknown");
  assert.equal(result.status, 1, "R-A5");
  assert.equal(result.stderr, "nosh: NOSH is not initialized; run `nosh setup`\n");
  const port = await writeConfig(state);
  result = cli("rehearsal-unknown");
  assert.equal(result.status, 1, "R-A6");
  assert.equal(result.stderr, "nosh: Unknown command: rehearsal-unknown; run nosh help\n");

  // Pipes and a timeout: a daemon holding the CLI's inherited stdout would keep this call waiting.
  result = cli("start");
  if (result.error?.code === "ETIMEDOUT") throw new Error("nosh start returned only when its output pipe closed: the daemon inherited the CLI's stdout/stderr (see §6.2)");
  assert.equal(result.status, 0, `R-A7: ${result.stderr}`);
  assert.ok(result.stdout.includes("Starting noshd…\n") && result.stdout.includes(`noshd started at http://127.0.0.1:${port}\n`), result.stdout);
  assert.equal((await http(port, "/health")).status, 200);
  assert.equal((await http(port, "/api/models")).status, 200);
  result = cli("status");
  assert.equal(result.status, 0, "R-A8");
  assert.equal(result.stdout, `running http://127.0.0.1:${port}\n`);

  // The old CLI's `stop --if-idle` and the new CLI's `start` write to this console.
  assert.equal(await launcher.update(root, { fetchImpl: release(v1, ...repack(state, v1)), log }), 0, "R-A9");
  assert.deepEqual(lines.splice(0), [`Installing NOSH ${v1} from GitHub releases...`, `Updated NOSH ${version} -> ${v1}.`]);
  assert.equal(launcher.readCurrent(root).version, v1);
  assert.equal((await http(port, "/health")).status, 200);
  daemonUnder(state, join(root, "versions", v1));
  assert.deepEqual(readdirSync(join(root, "versions")).sort(launcher.compareVersions), [version, v1]);

  const project = join(state, "project");
  repository(project);
  result = cli("project", "open", project);
  assert.equal(result.status, 0, `R-A10: ${result.stderr}`);
  const projectId = /^Opened (prj_[a-z0-9]+)\nPaper: .*docs[\\/]paper\.md\n$/.exec(result.stdout)?.[1];
  assert.ok(projectId, result.stdout);
  const job = await http(port, "/api/jobs", { method: "POST", json: {
    jobId: JOB, projectId, missionId: null, directionId: null, autoresearchId: null, runId: RUN, experimentId: null, commitSha: git(project, "rev-parse", "HEAD"),
    workingDirectory: project, runner: "native", distribution: null, command: [process.execPath, "-e", "setTimeout(() => {}, 600000)"], checkpointCommand: null,
    environmentLockHash: "sha256:rehearsal", evaluationContractHash: "sha256:rehearsal", timeoutSeconds: 0, usesGpu: false,
  } });
  assert.equal(job.status, 201, `R-A11: ${await job.text()}`);
  const jobState = async () => (await (await http(port, `/api/jobs/${JOB}?projectId=${projectId}`)).json()).job?.state;
  await until(async () => await jobState() === "running", 30_000, () => `${JOB} did not reach running within 30 s`);
  result = cli("stop", "--if-idle");
  assert.equal(result.status, 2, "R-A12");
  assert.equal(result.stdout, "");
  assert.equal(result.stderr, `noshd has active work; finish or stop it first:\n  ${projectId}: job ${JOB}:running\n`);
  assert.equal((await http(port, "/health")).status, 200);

  const second = repack(state, v2);
  assert.equal(await launcher.update(root, { fetchImpl: release(v2, ...second), log }), 2, "R-A13");
  assert.equal(lines.splice(0).at(-1), `NOSH ${v2} is installed but not active yet. Run \`nosh update\` again once that work is done.`);
  assert.equal(launcher.readCurrent(root).version, v1);
  assert.ok(existsSync(join(root, "versions", v2, ".complete")));
  const cancel = await http(port, `/api/jobs/${JOB}/cancel`, { method: "POST", json: { projectId, idempotencyKey: "rehearsal-cancel" } });
  assert.ok(cancel.ok, `R-A14: ${cancel.status} ${await cancel.text()}`);
  await until(async () => !ACTIVE.includes(await jobState()), 30_000, () => `${JOB} did not end within 30 s of its cancel`);

  fetchImpl = release(v2, ...second);
  assert.equal(await launcher.update(root, { fetchImpl, log }), 0, "R-A15");
  assert.deepEqual(fetchImpl.downloads, {}, "the .complete fast path downloaded again");
  assert.deepEqual(lines.splice(0), [`Installing NOSH ${v2} from GitHub releases...`, `Updated NOSH ${v1} -> ${v2}.`]);
  assert.equal(launcher.readCurrent(root).version, v2);
  assert.equal((await http(port, "/health")).status, 200);
  daemonUnder(state, join(root, "versions", v2));
  // A leftover <version> directory means a process from it is still alive.
  assert.deepEqual(readdirSync(join(root, "versions")).sort(launcher.compareVersions), [v1, v2]);
  assert.equal(await launcher.update(root, { fetchImpl: release(v2, ...second), log }), 0, "R-A16");
  assert.deepEqual(lines.splice(0), [`NOSH ${v2} is up to date.`]);
  await assert.rejects(launcher.update(root, { fetchImpl: release(v3, repack(state, v3)[0], `${"f".repeat(64)}  x\r\n`), log }), /Checksum mismatch/, "R-A17");
  assert.equal(launcher.readCurrent(root).version, v2);
  assert.ok(!existsSync(join(root, "versions", v3)) && !existsSync(join(root, "update.lock")));

  const stop = stopToFile(state);
  assert.equal(stop.status, 0, "R-A18");
  assert.ok(stop.output.includes("noshd stopped cleanly"), stop.output);
  await until(async () => !await healthy(port), 6_000, () => "noshd still answers /health 6 s after nosh stop");
  return `A  ${version} -> ${v1} -> ${v2} through ${launcher.name}: install, help, stop --if-idle 3/2/0, start with pipes, update with restart, postponed update, .complete fast path, prune, refused checksum`;
}

// B: a TypeScript install updated to the Rust runtime continues its config and Project registry (the cutover path).
async function phaseB() {
  const state = isolate("nosh-rehearsal-b-");
  states.push(state);
  const root = launcher.harnessRoot(process.env);
  const lines = [], log = (line) => lines.push(line);
  const url = `https://github.com/SirNosh/NOSH/releases/download/v${TS_BASELINE}/nosh-runtime-${TS_BASELINE}.tgz`;
  const baseline = async (target, init) => target === launcher.RELEASES_URL
    ? Response.json({ tag_name: `v${TS_BASELINE}`, assets: [{ name: `nosh-runtime-${TS_BASELINE}.tgz`, browser_download_url: url }, { name: `nosh-runtime-${TS_BASELINE}.tgz.sha256`, browser_download_url: `${url}.sha256` }] })
    : [url, `${url}.sha256`].includes(target) ? fetch(target, init) : new Response("not found", { status: 404 });
  assert.equal(await launcher.update(root, { fetchImpl: baseline, log }), 0, "R-B1");
  assert.equal(launcher.readCurrent(root).version, TS_BASELINE);
  assert.ok(launcher.readCurrent(root).entry.endsWith(join("apps", "cli", "dist", "main.js")));

  const port = await writeConfig(state);
  assert.equal(cli({ stdio: "inherit" }, "start").status, 0, "R-B2");
  assert.equal((await http(port, "/health")).status, 200);
  const project = join(state, "project");
  repository(project);
  let result = cli("project", "open", project);
  assert.equal(result.status, 0, `R-B3: ${result.stderr}`);
  const projectId = /^Opened (prj_[a-z0-9]+)\n/.exec(result.stdout)?.[1];
  assert.ok(projectId, result.stdout);

  lines.length = 0;
  assert.equal(await launcher.update(root, { fetchImpl: release("999.0.0", ...repack(state, "999.0.0")), log }), 0, "R-B4");
  assert.deepEqual(lines, ["Installing NOSH 999.0.0 from GitHub releases...", `Updated NOSH ${TS_BASELINE} -> 999.0.0.`]);
  assert.equal((await http(port, "/health")).status, 200);
  daemonUnder(state, join(root, "versions", "999.0.0"));
  result = cli("project", "list");
  assert.equal(result.status, 0, "R-B5");
  assert.equal(result.stdout, `${projectId} *\t${realpathSync.native(project)}\n`);
  result = cli("mission", "list");
  assert.equal(result.status, 0, "R-B6");
  assert.equal(result.stdout, "No Missions recorded for the current Project\n");
  const stop = stopToFile(state);
  assert.equal(stop.status, 0, "R-B7");
  assert.ok(stop.output.includes("noshd stopped cleanly"), stop.output);
  return `B  TypeScript ${TS_BASELINE} -> 999.0.0: the Rust daemon took over the TypeScript config and Project registry`;
}

/** The launcher's lib.mjs exports, from --launcher or from the published nosh-harness package. */
async function loadLauncher() {
  let directory = option("--launcher"), name = directory;
  if (!directory) {
    const work = mkdtempSync(join(tmpdir(), "nosh-launcher-"));
    scratch.push(work);
    const packed = npm(["pack", LAUNCHER, "--pack-destination", work], work).trim().split(/\r?\n/).at(-1);
    mkdirSync(join(work, "extracted"));
    untar(readFileSync(join(work, packed)), join(work, "extracted"));
    directory = join(work, "extracted", "package");
    name = LAUNCHER;
  }
  directory = resolve(directory);
  return { directory, name, ...(await import(pathToFileURL(join(directory, "lib.mjs")).href)) };
}

/** The launcher's fake GitHub: the release API names this version's two assets, which it serves and counts. */
function release(target, bytes, checksum) {
  const name = `nosh-runtime-${target}.tgz`, url = `https://rehearsal.invalid/${name}`;
  const assets = { [url]: bytes, [`${url}.sha256`]: Buffer.from(checksum) };
  const fetchImpl = async (requested) => {
    if (requested === launcher.RELEASES_URL) return Response.json({ tag_name: `rust-v${target}`, assets: [{ name, browser_download_url: url }, { name: `${name}.sha256`, browser_download_url: `${url}.sha256` }] });
    if (!(requested in assets)) return new Response("not found", { status: 404 });
    fetchImpl.downloads[requested] = (fetchImpl.downloads[requested] ?? 0) + 1;
    return new Response(assets[requested]);
  };
  return Object.assign(fetchImpl, { url, downloads: {} });
}

/** The real package under another version, as [bytes, .sha256 text]. */
function repack(state, target) {
  const work = mkdtempSync(join(state, "repack-"));
  untar(tgzBytes, work);
  const manifest = join(work, "package", "package.json");
  writeFileSync(manifest, `${JSON.stringify({ ...readJson(manifest), version: target }, null, 2)}\n`);
  launcher.runNpm(["pack", join(work, "package"), "--pack-destination", work], work);
  const bytes = readFileSync(join(work, `nosh-runtime-${target}.tgz`));
  return [bytes, `${createHash("sha256").update(bytes).digest("hex")}  nosh-runtime-${target}.tgz\r\n`];
}

/** The sidecar's Pi dependencies sit beside nosh-runtime in `<prefix>/node_modules`, at their pinned versions. */
function hoisted(prefix) {
  for (const [name, pinned] of Object.entries(piDependencies)) {
    const manifest = join(prefix, "node_modules", ...name.split("/"), "package.json");
    assert.ok(existsSync(manifest), `${name} is not hoisted beside nosh-runtime (${manifest})`);
    assert.equal(readJson(manifest).version, pinned, `${name} is not installed at ${pinned}`);
  }
}

/** `config.json` as `nosh setup` writes it (sorted keys), on a free port; never `setup` itself, which takes port 4321. */
async function writeConfig(state) {
  const port = await freePort();
  mkdirSync(join(state, "NOSH"), { recursive: true });
  writeFileSync(join(state, "NOSH", "config.json"), `${JSON.stringify({ bootstrapToken: randomBytes(32).toString("base64url"), currentProjectId: null, dataDirectory: join(state, "NOSH", "data"), port })}\n`);
  return port;
}

/** `node <launcher>/nosh.mjs <args>`; only for commands that cannot leave a daemon holding the pipes, and R-A7. */
function cli(...cliArgs) {
  const options = typeof cliArgs[0] === "object" ? cliArgs.shift() : {};
  return spawnSync(process.execPath, [join(launcher.directory, "nosh.mjs"), ...cliArgs], { encoding: "utf8", timeout: 120_000, ...options });
}

/** `nosh stop` with its output in a file, so a daemon it leaves running holds no pipe of ours. */
function stopToFile(state) {
  const path = join(state, `stop-${Date.now()}.log`);
  const output = openSync(path, "a");
  const { status } = spawnSync(process.execPath, [join(launcher.directory, "nosh.mjs"), "stop"], { stdio: ["ignore", output, output], timeout: 120_000, env: { ...process.env, LOCALAPPDATA: state, APPDATA: state, USERPROFILE: state, HOME: state } });
  closeSync(output);
  return { status, output: readFileSync(path, "utf8") };
}

/** Stops whatever a phase left running under `state`, then removes it. */
async function stopAndRemove(state) {
  const config = join(state, "NOSH", "config.json");
  const port = existsSync(config) ? readJson(config).port : null;
  if (port && await healthy(port)) {
    stopToFile(state);
    await until(async () => !await healthy(port), 6_000, () => "").catch(() => {});
  }
  // Last resort: processes started from this state (daemon, job wrappers, sidecars) that outlived `nosh stop`.
  for (const { ProcessId } of processes(state)) spawnSync("taskkill", ["/T", "/F", "/PID", String(ProcessId)], { windowsHide: true });
  rmSync(state, { recursive: true, force: true, maxRetries: 10, retryDelay: 500 });
}

/** The single nosh.exe daemon started from `state` runs from under `directory`. */
function daemonUnder(state, directory) {
  const daemons = processes(state).filter(({ Name, CommandLine }) => Name.toLowerCase() === "nosh.exe" && /\sdaemon(\s|$)/.test(CommandLine ?? ""));
  assert.equal(daemons.length, 1, `expected one noshd under ${state}, found ${JSON.stringify(daemons)}`);
  assert.ok(daemons[0].ExecutablePath.toLowerCase().startsWith(`${directory.toLowerCase()}${sep}`), `noshd runs ${daemons[0].ExecutablePath}, not from ${directory}`);
}

/** nosh.exe and node.exe processes whose executable or command line names `state` (Win32_Process). */
function processes(state) {
  const script = "[Console]::OutputEncoding = [Text.Encoding]::UTF8; @(Get-CimInstance Win32_Process -Filter \"Name='nosh.exe' or Name='node.exe'\" | Select-Object ProcessId, Name, ExecutablePath, CommandLine) | ConvertTo-Json -Compress";
  const result = spawnSync("powershell.exe", ["-NoProfile", "-NonInteractive", "-EncodedCommand", Buffer.from(script, "utf16le").toString("base64")], { encoding: "utf8", windowsHide: true });
  if (result.status !== 0) throw new Error(`Win32_Process query failed: ${result.stderr}`);
  const needle = state.toLowerCase();
  return [].concat(result.stdout.trim() ? JSON.parse(result.stdout) : []).filter(({ ExecutablePath, CommandLine }) => `${ExecutablePath ?? ""}\n${CommandLine ?? ""}`.toLowerCase().includes(needle));
}

function repository(directory) {
  mkdirSync(directory, { recursive: true });
  git(directory, "init");
  writeFileSync(join(directory, "README.md"), "# NOSH rehearsal\n");
  git(directory, "add", "-A");
  git(directory, "commit", "-m", "init");
}

function git(cwd, ...gitArgs) {
  const result = spawnSync("git", gitArgs, { cwd, encoding: "utf8", windowsHide: true });
  if (result.status !== 0) throw new Error(`git ${gitArgs.join(" ")} failed: ${result.stderr}`);
  return result.stdout.trim();
}

/**
 * Extracts a .tgz into `destination` with Windows' own tar (bsdtar), giving it no path: bsdtar reads its arguments in
 * the ANSI code page, where a Cyrillic user name in TEMP becomes "?". The archive comes on stdin; the destination is
 * the working directory.
 */
function untar(bytes, destination) {
  const result = spawnSync(process.platform === "win32" ? join(process.env.SystemRoot ?? "C:\\Windows", "System32", "tar.exe") : "tar", ["-xzf", "-"], { cwd: destination, input: bytes, encoding: "utf8", windowsHide: true });
  if (result.status !== 0) throw new Error(`tar -xzf into ${destination} failed: ${result.stderr || result.error?.message}`);
}

/** npm's own CLI next to this Node, as the launcher's runNpm finds it, else `npm` on PATH; returns stdout. */
function npm(npmArgs, cwd) {
  const base = dirname(process.execPath);
  const npmCli = [join(base, "node_modules", "npm", "bin", "npm-cli.js"), join(base, "..", "lib", "node_modules", "npm", "bin", "npm-cli.js")].find((path) => existsSync(path));
  const result = npmCli
    ? spawnSync(process.execPath, [npmCli, ...npmArgs], { cwd, encoding: "utf8", windowsHide: true })
    : spawnSync(process.platform === "win32" ? "npm.cmd" : "npm", npmArgs.map((arg) => `"${arg}"`), { cwd, encoding: "utf8", windowsHide: true, shell: true });
  if (result.status !== 0) throw new Error(`npm ${npmArgs[0]} failed:\n${result.stderr || result.stdout || result.error?.message}`);
  return result.stdout;
}

function http(port, path, { json, ...init } = {}) {
  return fetch(`http://127.0.0.1:${port}${path}`, { ...init, ...(json && { headers: { "content-type": "application/json" }, body: JSON.stringify(json) }), signal: AbortSignal.timeout(10_000) });
}

async function healthy(port) {
  try { return (await http(port, "/health")).ok; } catch { return false; }
}

function freePort() {
  return new Promise((resolve, reject) => {
    const server = createServer().once("error", reject).listen(0, "127.0.0.1", () => { const { port } = server.address(); server.close(() => resolve(port)); });
  });
}

async function until(check, ms, message) {
  for (const deadline = Date.now() + ms; !(await check());) {
    if (Date.now() > deadline) throw new Error(message());
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
}

function within(promise, ms, message) {
  let timer;
  return Promise.race([promise, new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(message)), ms); })]).finally(() => clearTimeout(timer));
}

function readJson(path) {
  return JSON.parse(readFileSync(path, "utf8"));
}

function option(flag) {
  const index = args.indexOf(flag);
  return index >= 0 ? args[index + 1] : undefined;
}
