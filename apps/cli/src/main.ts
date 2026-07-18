#!/usr/bin/env node
import { createHash, randomBytes } from "node:crypto";
import { closeSync, cpSync, existsSync, mkdirSync, openSync, readFileSync, readdirSync, realpathSync, renameSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join, relative, resolve } from "node:path";
import { spawn, spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import qr from "qrcode-terminal";
import { createDeviceKeys, DeviceRegistry, encryptVault } from "@nosh/crypto";
import { initializeResearchProject } from "@nosh/evidence";
import { backupDatabase, HostRegistry, type RegisteredProject } from "@nosh/persistence";
import { canonicalJson, schemaUris, type EventEnvelope, type JsonValue } from "@nosh/wire";

type Config = { dataDirectory: string; port: number; bootstrapToken: string; remoteVaultPath: string; currentProjectId: string | null };
const stateRoot = resolve(process.env.LOCALAPPDATA || process.env.APPDATA || join(homedir(), ".nosh"), "NOSH");
const configPath = join(stateRoot, "config.json"); const flags = new Map(process.argv.slice(2).filter((value) => value.startsWith("--")).map((value) => { const [key, ...rest] = value.split("="); return [key!, rest.join("=")]; }));
const words = process.argv.slice(2).filter((value) => !value.startsWith("--"));
let localSession: { token: string; expiresAt: number } | null = null;

try { await route(words); } catch (error) { process.stderr.write(`nosh: ${error instanceof Error ? error.message : "command failed"}\n`); process.exitCode = 1; }

async function route(args: string[]): Promise<void> {
  const [command, subcommand, argument] = args;
  if (!command || command === "help" || flags.has("--help")) return help();
  if (command === "setup") return setup();
  const config = loadConfig();
  if (command === "start") return start(config);
  if (command === "stop") return stop(config);
  if (command === "status") return status(config);
  if (command === "open") return openBrowser(config);
  if (command === "doctor") return doctor(config);
  if (command === "logs") return logs(config);
  if (command === "project" && subcommand === "list") return projectList(config);
  if (command === "project" && subcommand === "open" && argument) return projectOpen(config, argument);
  if (command === "mission" && subcommand === "list") return missionList(config);
  if (command === "mission" && subcommand === "status" && argument) return missionStatus(config, argument);
  if (command === "job" && subcommand === "list") return jobList(config);
  if (command === "remote" && subcommand === "setup") return remoteSetup(config);
  if (command === "remote" && subcommand === "status") return remoteStatus(config);
  if (command === "remote" && subcommand === "pair") return remotePair(config);
  if (command === "remote" && subcommand === "revoke" && argument) return remoteRevoke(config, argument);
  if (command === "backup" && subcommand) return backup(config, subcommand);
  throw new Error("unknown or incomplete command; run `nosh help`");
}

function setup(): void {
  mkdirSync(stateRoot, { recursive: true }); const dataDirectory = join(stateRoot, "data"); mkdirSync(dataDirectory, { recursive: true });
  if (!existsSync(configPath)) saveConfig({ dataDirectory, port: 4321, bootstrapToken: randomBytes(32).toString("base64url"), remoteVaultPath: join(stateRoot, "remote.vault.json"), currentProjectId: null });
  if (process.platform === "win32" && process.env.USERNAME) spawnSync("icacls.exe", [stateRoot, "/inheritance:r", "/grant:r", `${process.env.USERNAME}:(OI)(CI)F`], { windowsHide: true });
  process.stdout.write(`NOSH initialized in ${stateRoot}\nRun: nosh doctor\nThen: nosh start\n`);
}

async function start(config: Config): Promise<void> {
  if (await healthy(config)) return void process.stdout.write(`noshd already running at ${url(config)}\n`);
  const script = fileURLToPath(new URL("../../noshd/dist/main.js", import.meta.url)); if (!existsSync(script)) throw new Error("noshd build is missing; install a NOSH distribution or run the repository build");
  mkdirSync(join(config.dataDirectory, "logs"), { recursive: true }); const log = openSync(join(config.dataDirectory, "logs", "noshd.log"), "a");
  const child = spawn(process.execPath, [script, `--data-dir=${config.dataDirectory}`, `--port=${config.port}`, `--remote-vault=${config.remoteVaultPath}`], { detached: true, windowsHide: true, stdio: ["ignore", log, log], env: { ...process.env, NOSH_BOOTSTRAP_TOKEN: config.bootstrapToken } }); child.unref(); closeSync(log);
  writeAtomic(join(config.dataDirectory, "noshd.pid.json"), { pid: child.pid, executable: process.execPath, script, startedAt: new Date().toISOString() }, 0o600);
  for (let attempt = 0; attempt < 30; attempt += 1) { await delay(200); if (await healthy(config)) return void process.stdout.write(`noshd started at ${url(config)}\n`); }
  throw new Error(`noshd did not become healthy; inspect ${join(config.dataDirectory, "logs", "noshd.log")}`);
}

async function stop(config: Config): Promise<void> {
  if (!await healthy(config)) return void process.stdout.write("noshd is not running\n");
  await api(config, "/shutdown", { method: "POST", body: {} }); for (let attempt = 0; attempt < 30; attempt += 1) { await delay(200); if (!await healthy(config)) return void process.stdout.write("noshd stopped cleanly\n"); }
  throw new Error("noshd did not stop cleanly; its PID file was left intact for diagnosis");
}

async function status(config: Config): Promise<void> { process.stdout.write(await healthy(config) ? `running ${url(config)}\n` : "stopped\n"); }
function openBrowser(config: Config): void { const target = url(config); const result = process.platform === "win32" ? spawn("cmd.exe", ["/d", "/s", "/c", "start", "", target], { detached: true, stdio: "ignore" }) : spawn(process.platform === "darwin" ? "open" : "xdg-open", [target], { detached: true, stdio: "ignore" }); result.unref(); }

async function projectOpen(config: Config, input: string): Promise<void> {
  const registration = initializeResearchProject({ path: input, dataDirectory: config.dataDirectory }); try { await api(config, "/projects", { method: "POST", body: registration }); } catch { const registry = new HostRegistry(join(config.dataDirectory, "host.sqlite")); try { registry.register(registration); } finally { registry.close(); } }
  saveConfig({ ...config, currentProjectId: registration.projectId }); process.stdout.write(`Opened ${registration.projectId}\nPaper: ${join(registration.repositoryRoot, "docs", "paper.md")}\n`);
}

async function projectList(config: Config): Promise<void> { const registeredProjects = await projects(config); if (!registeredProjects.length) return void process.stdout.write("No Projects registered\n"); for (const project of registeredProjects) process.stdout.write(`${project.projectId}${project.projectId === config.currentProjectId ? " *" : ""}\t${project.repositoryRoot}\n`); }
async function missionList(config: Config): Promise<void> { const events = await currentEvents(config); const missions = new Map<string, EventEnvelope>(); for (const event of events) if (event.scope.missionId) missions.set(event.scope.missionId, event); if (!missions.size) return void process.stdout.write("No Missions recorded for the current Project\n"); for (const [id, event] of missions) process.stdout.write(`${id}\t${event.type}\tsequence ${event.sequence}\n`); }
async function missionStatus(config: Config, missionId: string): Promise<void> { const events = (await currentEvents(config)).filter((event) => event.scope.missionId === missionId); if (!events.length) throw new Error("Mission not found in the current Project"); process.stdout.write(`${missionId}\nlatest: ${events.at(-1)!.type}\nsequence: ${events.at(-1)!.sequence}\nevents: ${events.length}\n`); }
async function jobList(config: Config): Promise<void> { const result = await api(config, "/jobs") as { jobs: Array<{ jobId: string; state: string; projectId: string }> }; if (!result.jobs.length) return void process.stdout.write("No jobs\n"); for (const job of result.jobs) process.stdout.write(`${job.jobId}\t${job.state}\t${job.projectId}\n`); }

async function remoteSetup(config: Config): Promise<void> {
  const relayUrl = requiredFlag("--relay-url"); const channelId = requiredFlag("--channel"); const adminToken = requiredFlag("--admin-token"); if (!/^[-_a-z0-9]{16,128}$/.test(channelId)) throw new Error("--channel must contain 16-128 lowercase routing characters");
  const accountId = `acc_${randomBytes(16).toString("hex")}`; const hostKeys = await createDeviceKeys(); const accountKey = randomBytes(32).toString("base64url"); const configuration = { relayUrl, channelId, adminToken, accountId, accountKey, keyVersion: 1, hostKeys, registry: new DeviceRegistry().snapshot() };
  mkdirSync(dirname(config.remoteVaultPath), { recursive: true }); writeAtomic(config.remoteVaultPath, await encryptVault(configuration as unknown as JsonValue, config.bootstrapToken), 0o600); process.stdout.write(`Remote vault created for ${accountId}. Restart noshd to connect.\n`);
}
async function remoteStatus(config: Config): Promise<void> { process.stdout.write(`${JSON.stringify(await api(config, "/remote"), null, 2)}\n`); }
async function remotePair(config: Config): Promise<void> { const capability = flags.get("--capability"); const code = flags.get("--code"); if (capability && code) { const permissions = ["mission.pause", "mission.resume", "mission.stop", "mission.steer", "agent.message", "agent.safe_pause", "agent.cancel", "job.checkpoint", "job.cancel", "review.respond", "artifact.preview.request", "thread.message", "thread.stop"]; const approved = await api(config, "/remote/pairings/approve", { method: "POST", body: { capability, verificationCode: code, permissions } }); return void process.stdout.write(`Approved ${JSON.stringify(approved)}\n`); } const [pairing, remote] = await Promise.all([api(config, "/remote/pairings", { method: "POST", body: {} }) as Promise<{ capability: string; verificationCode: string; expiresAt: string }>, api(config, "/remote") as Promise<{ relayUrl: string; channelId: string }>]); const payload = JSON.stringify({ protocol: "nosh-pair-v1", relayUrl: remote.relayUrl, channelId: remote.channelId, capability: pairing.capability, expiresAt: pairing.expiresAt }); const image = await new Promise<string>((resolve) => qr.generate(payload, { small: true }, resolve)); process.stdout.write(`${image}\nVerification code: ${pairing.verificationCode}\nExpires: ${pairing.expiresAt}\nAfter the browser presents its keys, approve with:\nnosh remote pair --capability=${pairing.capability} --code=${pairing.verificationCode}\n`); }
async function remoteRevoke(config: Config, deviceId: string): Promise<void> { const result = await api(config, "/remote/revoke", { method: "POST", body: { deviceId } }) as { rekeyedDeviceIds: string[]; keyVersion: number }; process.stdout.write(`Revoked ${deviceId}; rotated to key v${result.keyVersion} and securely rekeyed ${result.rekeyedDeviceIds.length} remaining device(s).\n`); }

async function backup(config: Config, selector: string): Promise<void> {
  const all = await projects(config); const path = existsSync(resolve(selector)) ? realpathSync(resolve(selector)) : null; const project = all.find((entry) => entry.projectId === selector || (path && entry.repositoryRoot === path)); if (!project) throw new Error("Project is not registered");
  const destination = join(config.dataDirectory, "backups", `${project.projectId}-${new Date().toISOString().replaceAll(":", "-")}`); mkdirSync(destination, { recursive: true }); const schemaVersion = await backupDatabase(project.databasePath, join(destination, "nosh.sqlite"));
  for (const item of [".nosh/contracts", ".nosh/events", ".nosh/sessions", ".nosh/jobs", ".nosh/project.json", ".nosh/schema-lock.json", ".nosh/artifacts/manifest.json", "docs/paper.md", "docs/paper.bib", "docs/figures"]) copyIfPresent(project.repositoryRoot, destination, item);
  const refs = run("git", ["-C", project.repositoryRoot, "show-ref"]); writeFileSync(join(destination, "git-refs.txt"), refs.stdout, "utf8"); const commits = run("git", ["-C", project.repositoryRoot, "rev-list", "--all"]); writeFileSync(join(destination, "required-commits.txt"), commits.stdout, "utf8");
  const manifest = { format: "nosh-backup-v1", createdAt: new Date().toISOString(), project: { projectId: project.projectId, repositoryRoot: project.repositoryRoot }, sqliteSchemaVersion: schemaVersion, includesLargeArtifacts: false, files: hashFiles(destination) }; writeAtomic(join(destination, "manifest.json"), manifest); process.stdout.write(`Backup written to ${destination}\n`);
}

async function doctor(config: Config): Promise<void> {
  const checks: Array<[string, boolean, string]> = []; checks.push(["Windows", process.platform === "win32", process.platform]); const wsl = run("wsl.exe", ["--list", "--quiet"]); checks.push(["WSL2", wsl.ok && Boolean(wsl.stdout.trim()), clean(wsl.stdout) || "not available"]); const pi = run(process.platform === "win32" ? "where.exe" : "which", ["pi"]); checks.push(["Pi CLI", pi.ok, clean(pi.stdout) || "not found"]); checks.push(["NOSH Pi package", existsSync(fileURLToPath(new URL("../../../pi-package/package.json", import.meta.url))), "native package metadata"]); checks.push(["Pi authentication", Boolean(process.env.OPENAI_API_KEY || process.env.ANTHROPIC_API_KEY || existsSync(join(homedir(), ".pi", "agent", "auth.json"))), "credential presence only; values not read"]); checks.push(["daemon/database", await healthy(config) && existsSync(join(config.dataDirectory, "host.sqlite")), await healthy(config) ? "healthy" : "stopped"]); const git = run("git", ["worktree", "list"]); checks.push(["Git worktrees", git.ok, clean(git.stdout) || "unavailable"]); const gpu = run("nvidia-smi", ["--query-gpu=name,driver_version", "--format=csv,noheader"]); checks.push(["NVIDIA telemetry", gpu.ok, clean(gpu.stdout) || "not applicable"]); let relay = "not configured"; let relayOk = true; if (existsSync(config.remoteVaultPath)) { try { const state = await api(config, "/remote") as { configured?: boolean; relayUrl?: string }; relayOk = Boolean(state.configured); relay = state.relayUrl ?? "configured but disconnected"; } catch { relayOk = false; relay = "vault present; daemon unavailable"; } } checks.push(["relay/device keys", relayOk, relay]); const registeredSchemas = schemaUris(); checks.push(["schema/protocol", registeredSchemas.length > 10, `${registeredSchemas.length} registered schema URIs; protocol v1`]); checks.push(["protected storage", relative(resolve(process.env.LOCALAPPDATA || stateRoot), stateRoot).startsWith("..") === false, stateRoot]);
  for (const [name, ok, detail] of checks) process.stdout.write(`${ok ? "PASS" : "FAIL"}\t${name}\t${detail}\n`); if (checks.some(([, ok]) => !ok)) process.exitCode = 1;
}

function logs(config: Config): void { const path = join(config.dataDirectory, "logs", "noshd.log"); if (!existsSync(path)) return void process.stdout.write("No daemon log\n"); const lines = readFileSync(path, "utf8").split(/\r?\n/); process.stdout.write(`${lines.slice(-200).join("\n")}\n`); }
async function projects(config: Config): Promise<RegisteredProject[]> { try { return (await api(config, "/projects") as { projects: RegisteredProject[] }).projects; } catch { const registry = new HostRegistry(join(config.dataDirectory, "host.sqlite")); try { return registry.list(); } finally { registry.close(); } } }
async function currentEvents(config: Config): Promise<EventEnvelope[]> { if (!config.currentProjectId) throw new Error("No current Project; run `nosh project open <path>`"); return (await api(config, `/events?projectId=${encodeURIComponent(config.currentProjectId)}&after=0`) as { events: EventEnvelope[] }).events; }
async function api(config: Config, path: string, options: { method?: string; body?: unknown } = {}): Promise<unknown> { if (!localSession || localSession.expiresAt <= Date.now() + 5_000) { const exchanged = await fetch(`${url(config)}/api/session`, { method: "POST", headers: { authorization: `Bearer ${config.bootstrapToken}` } }); if (!exchanged.ok) throw new Error("daemon session exchange failed"); const value = await exchanged.json() as { token: string; expiresAt: string }; localSession = { token: value.token, expiresAt: Date.parse(value.expiresAt) }; } const response = await fetch(`${url(config)}/api${path}`, { method: options.method ?? "GET", headers: { authorization: `Bearer ${localSession.token}`, ...(options.body === undefined ? {} : { "content-type": "application/json" }) }, ...(options.body === undefined ? {} : { body: JSON.stringify(options.body) }) }); if (!response.ok) throw new Error((await response.json() as { error?: string }).error ?? `daemon request failed (${response.status})`); return response.json(); }
async function healthy(config: Config): Promise<boolean> { try { return (await fetch(`${url(config)}/health`, { signal: AbortSignal.timeout(750) })).ok; } catch { return false; } }
function loadConfig(): Config { if (!existsSync(configPath)) throw new Error("NOSH is not initialized; run `nosh setup`"); return JSON.parse(readFileSync(configPath, "utf8")) as Config; }
function saveConfig(config: Config): void { mkdirSync(dirname(configPath), { recursive: true }); writeAtomic(configPath, config, 0o600); }
function writeAtomic(path: string, value: unknown, mode = 0o644): void { mkdirSync(dirname(path), { recursive: true }); const temporary = `${path}.tmp`; writeFileSync(temporary, `${canonicalJson(value as JsonValue)}\n`, { encoding: "utf8", mode }); renameSync(temporary, path); }
function copyIfPresent(root: string, destination: string, item: string): void { const source = resolve(root, item); if (relative(root, source).startsWith("..") || !existsSync(source)) return; const target = join(destination, item); mkdirSync(dirname(target), { recursive: true }); cpSync(source, target, { recursive: true, errorOnExist: true }); }
function hashFiles(root: string): Array<{ path: string; sha256: string; bytes: number }> { const result: Array<{ path: string; sha256: string; bytes: number }> = []; const visit = (directory: string) => { for (const entry of readdirSync(directory, { withFileTypes: true })) { const path = join(directory, entry.name); if (entry.isDirectory()) visit(path); else { const bytes = readFileSync(path); result.push({ path: relative(root, path).replaceAll("\\", "/"), sha256: createHash("sha256").update(bytes).digest("hex"), bytes: bytes.length }); } } }; visit(root); return result.sort((a, b) => a.path.localeCompare(b.path)); }
function run(command: string, args: string[]): { ok: boolean; stdout: string } { const result = spawnSync(command, args, { encoding: "utf8", windowsHide: true, timeout: 10_000 }); return { ok: result.status === 0, stdout: result.stdout ?? "" }; }
function clean(value: string): string { return value.replace(/[\u0000-\u001f]+/g, " ").trim().slice(0, 200); }
function url(config: Config): string { return `http://127.0.0.1:${config.port}`; }
function requiredFlag(name: string): string { const value = flags.get(name); if (!value) throw new Error(`${name}=... is required`); return value; }
function delay(milliseconds: number): Promise<void> { return new Promise((resolve) => setTimeout(resolve, milliseconds)); }
function help(): void { process.stdout.write("nosh setup|start|stop|status|open|doctor\nnosh project list|open <path>\nnosh mission list|status <id>\nnosh job list\nnosh remote setup|status|pair|revoke <device>\nnosh backup <project>\nnosh logs\n"); }
