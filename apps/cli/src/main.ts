#!/usr/bin/env node
import { randomBytes } from "node:crypto";
import { closeSync, existsSync, mkdirSync, openSync, readFileSync, realpathSync, renameSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join, relative, resolve } from "node:path";
import { spawn, spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { initializeResearchProject } from "@nosh/evidence";
import { createProjectBackup, HostRegistry, scheduleProjectRestore, type RegisteredProject } from "@nosh/persistence";
import { canonicalJson, schemaUris, type JsonValue } from "@nosh/wire";
import { gitDoctorChecks } from "./backup-doctor.js";

type Config = { dataDirectory: string; port: number; bootstrapToken: string; currentProjectId: string | null };
const stateRoot = resolve(process.env.LOCALAPPDATA || process.env.APPDATA || join(homedir(), ".nosh"), "NOSH");
const configPath = join(stateRoot, "config.json"); const flags = new Map(process.argv.slice(2).filter((value) => value.startsWith("--")).map((value) => { const [key, ...rest] = value.split("="); return [key!, rest.join("=")]; }));
const words = process.argv.slice(2).filter((value) => !value.startsWith("--"));
try { await route(words); } catch (error) { process.stderr.write(`nosh: ${error instanceof Error ? error.message : "command failed"}\n`); process.exitCode = 1; }

async function route(args: string[]): Promise<void> {
  const [command, subcommand, argument, backupId] = args;
  if (!command || command === "help" || flags.has("--help")) return help();
  if (command === "setup") return setup();
  const config = loadConfig();
  if (command === "start") return start(config);
  if (command === "stop") return stop(config);
  if (command === "status") return status(config);
  if (command === "open" || command === "tui") return openTerminal(config);
  if (command === "doctor") return doctor(config);
  if (command === "logs") return logs(config);
  if (command === "project" && subcommand === "list") return projectList(config);
  if (command === "project" && subcommand === "open" && argument) return projectOpen(config, argument);
  if (command === "mission" && subcommand === "list") return missionList(config);
  if (command === "mission" && subcommand === "status" && argument) return missionStatus(config, argument);
  if (command === "job" && subcommand === "list") return jobList(config);
  if (command === "backup" && subcommand === "restore" && argument && backupId) return backupRestore(config, argument, backupId);
  if (command === "backup" && subcommand) return backup(config, subcommand);
  throw new Error(`Unknown command: ${args.join(" ")}; run nosh help`);
}

function setup(): void {
  mkdirSync(stateRoot, { recursive: true }); const dataDirectory = join(stateRoot, "data"); mkdirSync(dataDirectory, { recursive: true });
  if (!existsSync(configPath)) saveConfig({ dataDirectory, port: 4321, bootstrapToken: randomBytes(32).toString("base64url"), currentProjectId: null });
  if (process.platform === "win32" && process.env.USERNAME) spawnSync("icacls.exe", [stateRoot, "/inheritance:r", "/grant:r", `${process.env.USERNAME}:(OI)(CI)F`], { windowsHide: true });
  process.stdout.write(`NOSH initialized in ${stateRoot}\nRun: nosh doctor\nThen: nosh start\n`);
}

async function start(config: Config): Promise<void> {
  if (await healthy(config)) return void process.stdout.write(`noshd already running at ${url(config)}\n`);
  const script = fileURLToPath(new URL("../../noshd/dist/main.js", import.meta.url)); if (!existsSync(script)) throw new Error("noshd build is missing; install a NOSH distribution or run the repository build");
  mkdirSync(join(config.dataDirectory, "logs"), { recursive: true }); const log = openSync(join(config.dataDirectory, "logs", "noshd.log"), "a");
  const child = spawn(process.execPath, [script, `--data-dir=${config.dataDirectory}`, `--port=${config.port}`], { detached: true, windowsHide: true, stdio: ["ignore", log, log], env: { ...process.env, NOSH_BOOTSTRAP_TOKEN: config.bootstrapToken } });
  let launchError: Error | undefined;
  child.once("error", error => { launchError = error; });
  child.unref(); closeSync(log);
  process.stdout.write("Starting noshd…\n");
  // Cold Pi imports on Windows/WSL can take longer than the former six-second limit.
  const deadline = Date.now() + 90_000;
  while (Date.now() < deadline) {
    if (await healthy(config)) return void process.stdout.write(`noshd started at ${url(config)}\n`);
    if (launchError) throw new Error(`noshd launch failed: ${launchError.message}`);
    if (child.exitCode !== null || child.signalCode !== null) throw new Error(`noshd exited during startup; inspect ${join(config.dataDirectory, "logs", "noshd.log")}`);
    await delay(250);
  }
  throw new Error(`noshd did not become healthy within 90 seconds; inspect ${join(config.dataDirectory, "logs", "noshd.log")}`);
}

async function stop(config: Config): Promise<void> {
  if (!await healthy(config)) return void process.stdout.write("noshd is not running\n");
  await api(config, "/shutdown", { method: "POST", body: {} }); for (let attempt = 0; attempt < 30; attempt += 1) { await delay(200); if (!await healthy(config)) return void process.stdout.write("noshd stopped cleanly\n"); }
  throw new Error(`noshd did not stop cleanly; inspect ${join(config.dataDirectory, "logs", "noshd.log")}`);
}

async function status(config: Config): Promise<void> { process.stdout.write(await healthy(config) ? `running ${url(config)}\n` : "stopped\n"); }

async function openTerminal(config: Config): Promise<void> {
  if (!process.stdin.isTTY || !process.stdout.isTTY) throw new Error("nosh tui requires an interactive terminal; use nosh help for scripting commands");
  const executable = process.env.NOSH_BUN || "bun";
  const bun = run(executable, ["--version"]);
  const version = /^(\d+)\.(\d+)\./.exec(bun.stdout.trim());
  if (!bun.ok || !version || Number(version[1]) < 1 || Number(version[1]) === 1 && Number(version[2]) < 3) throw new Error("nosh tui requires Bun >=1.3 on PATH; install Bun from https://bun.sh (daemon and scripting commands use Node)");
  const script = fileURLToPath(new URL("../../tui/dist/main.js", import.meta.url));
  if (!existsSync(script)) throw new Error("TUI build is missing; install a NOSH distribution or run the repository build");
  await start(config);
  const code = await new Promise<number>((resolve, reject) => {
    const child = spawn(executable, [script], { stdio: "inherit", env: { ...process.env, NOSH_TUI_CONFIG: JSON.stringify({ baseUrl: url(config), bootstrapToken: config.bootstrapToken, currentProjectId: config.currentProjectId }) } });
    child.once("error", reject);
    child.once("exit", (code, signal) => resolve(code ?? (signal ? 1 : 0)));
  });
  if (code !== 0) process.exitCode = code;
}

async function projectOpen(config: Config, input: string): Promise<void> {
  const registration = initializeResearchProject({ path: input, dataDirectory: config.dataDirectory }); try { await api(config, "/projects", { method: "POST", body: registration }); } catch { const registry = new HostRegistry(join(config.dataDirectory, "host.sqlite")); try { registry.register(registration); } finally { registry.close(); } }
  saveConfig({ ...config, currentProjectId: registration.projectId }); process.stdout.write(`Opened ${registration.projectId}\nPaper: ${join(registration.repositoryRoot, "docs", "paper.md")}\n`);
}

async function projectList(config: Config): Promise<void> { const registeredProjects = await projects(config); if (!registeredProjects.length) return void process.stdout.write("No Projects registered\n"); for (const project of registeredProjects) process.stdout.write(`${project.projectId}${project.projectId === config.currentProjectId ? " *" : ""}\t${project.repositoryRoot}\n`); }
async function missionList(config: Config): Promise<void> { if (!config.currentProjectId) throw new Error("No current Project; run `nosh project open <path>`"); const result = await api(config, `/missions?projectId=${encodeURIComponent(config.currentProjectId)}`) as { missions: Array<{ entityId: string; state: string; version: number }> }; if (!result.missions.length) return void process.stdout.write("No Missions recorded for the current Project\n"); for (const mission of result.missions) process.stdout.write(`${mission.entityId}\t${mission.state}\tversion ${mission.version}\n`); }
async function missionStatus(config: Config, missionId: string): Promise<void> { if (!config.currentProjectId) throw new Error("No current Project; run `nosh project open <path>`"); const result = await api(config, `/missions/${encodeURIComponent(missionId)}?projectId=${encodeURIComponent(config.currentProjectId)}`) as { mission: { entityId: string; state: string; version: number; value: { title: string; graphVersion: number; updatedAt: string } } }; const mission = result.mission; process.stdout.write(`${mission.entityId}\nstate: ${mission.state}\nversion: ${mission.version}\ntitle: ${mission.value.title}\ngraphVersion: ${mission.value.graphVersion}\nupdatedAt: ${mission.value.updatedAt}\n`); }
async function jobList(config: Config): Promise<void> { const result = await api(config, "/jobs") as { jobs: Array<{ jobId: string; state: string; projectId: string }> }; if (!result.jobs.length) return void process.stdout.write("No jobs\n"); for (const job of result.jobs) process.stdout.write(`${job.jobId}\t${job.state}\t${job.projectId}\n`); }

async function backup(config: Config, selector: string): Promise<void> {
  const all = await projects(config);
  const path = existsSync(resolve(selector)) ? realpathSync(resolve(selector)) : null;
  const project = all.find((entry) => entry.projectId === selector || (path && entry.repositoryRoot === path));
  if (!project) throw new Error("Project is not registered");
  const created = await healthy(config)
    ? await api(config, "/backups", { method: "POST", body: { projectId: project.projectId } }) as { backup: { path: string } }
    : { backup: await createProjectBackup(config.dataDirectory, project) };
  process.stdout.write(`Backup written to ${created.backup.path}\n`);
}

async function backupRestore(config: Config, selector: string, backupId: string): Promise<void> {
  const project = (await projects(config)).find((entry) => entry.projectId === selector || entry.repositoryRoot === resolve(selector));
  if (!project) throw new Error("Project is not registered");
  if (await healthy(config)) {
    await api(config, `/backups/${encodeURIComponent(backupId)}/restore`, { method: "POST", body: { projectId: project.projectId } });
    process.stdout.write(`Restore scheduled for ${project.projectId}; noshd is shutting down for restart.\n`);
    return;
  }
  scheduleProjectRestore(config.dataDirectory, project, backupId);
  process.stdout.write(`Restore scheduled for ${project.projectId}; run nosh start to apply it.\n`);
}

async function doctor(config: Config): Promise<void> {
  const checks: Array<[string, boolean, string]> = [];
  const currentProject = config.currentProjectId ? (await projects(config)).find((project) => project.projectId === config.currentProjectId) : undefined;
  checks.push(...gitDoctorChecks(currentProject?.repositoryRoot, run));
  const pi = run(process.platform === "win32" ? "where.exe" : "which", ["pi"]);
  checks.push(["Pi CLI", pi.ok, clean(pi.stdout) || "not found"]);
  checks.push(["NOSH Pi package", existsSync(fileURLToPath(new URL("../../../pi-package/package.json", import.meta.url))), "native package metadata"]);
  checks.push(["Pi authentication", Boolean(process.env.OPENAI_API_KEY || process.env.ANTHROPIC_API_KEY || existsSync(join(homedir(), ".pi", "agent", "auth.json"))), "credential presence only; values not read"]);
  const running = await healthy(config);
  checks.push(["daemon/database", running && existsSync(join(config.dataDirectory, "host.sqlite")), running ? "healthy" : "stopped; run nosh start"]);
  if (flags.has("--wsl")) { const wsl = run("wsl.exe", ["--list", "--quiet"]); checks.push(["WSL", wsl.ok && Boolean(wsl.stdout.trim()), clean(wsl.stdout) || "not available"]); }
  if (flags.has("--gpu")) { const gpu = run("nvidia-smi", ["--query-gpu=name,driver_version", "--format=csv,noheader"]); checks.push(["NVIDIA telemetry", gpu.ok, clean(gpu.stdout) || "not available"]); }
  const registeredSchemas = schemaUris();
  checks.push(["schema/protocol", registeredSchemas.length > 10, `${registeredSchemas.length} registered schema URIs; protocol v1`]);
  checks.push(["protected storage", relative(resolve(process.env.LOCALAPPDATA || stateRoot), stateRoot).startsWith("..") === false, stateRoot]);
  for (const [name, ok, detail] of checks) process.stdout.write(`${ok ? "PASS" : "FAIL"}\t${name}\t${detail}\n`); if (checks.some(([, ok]) => !ok)) process.exitCode = 1;
}

function logs(config: Config): void { const path = join(config.dataDirectory, "logs", "noshd.log"); if (!existsSync(path)) return void process.stdout.write("No daemon log\n"); const lines = readFileSync(path, "utf8").split(/\r?\n/); process.stdout.write(`${lines.slice(-200).join("\n")}\n`); }
async function projects(config: Config): Promise<RegisteredProject[]> { try { return (await api(config, "/projects") as { projects: RegisteredProject[] }).projects; } catch { const registry = new HostRegistry(join(config.dataDirectory, "host.sqlite")); try { return registry.list(); } finally { registry.close(); } } }
async function api(config: Config, path: string, options: { method?: string; body?: unknown } = {}): Promise<unknown> { const response = await fetch(`${url(config)}/api${path}`, { method: options.method ?? "GET", headers: options.body === undefined ? {} : { "content-type": "application/json" }, ...(options.body === undefined ? {} : { body: JSON.stringify(options.body) }) }); if (!response.ok) throw new Error((await response.json() as { error?: string }).error ?? `daemon request failed (${response.status})`); return response.json(); }
async function healthy(config: Config): Promise<boolean> { try { return (await fetch(`${url(config)}/health`, { signal: AbortSignal.timeout(750) })).ok; } catch { return false; } }
function saveConfig(config: Config): void { mkdirSync(dirname(configPath), { recursive: true }); writeAtomic(configPath, config, 0o600); }
function loadConfig(): Config { if (!existsSync(configPath)) throw new Error("NOSH is not initialized; run `nosh setup`"); return JSON.parse(readFileSync(configPath, "utf8")) as Config; }
function help(): void { process.stdout.write("nosh setup|start|stop|status|open|tui|doctor\nnosh project list|open <path>\nnosh mission list|status <id>\nnosh job list\nnosh backup <project>\nnosh backup restore <project> <backup-id>\nnosh logs\n"); }
function writeAtomic(path: string, value: unknown, mode = 0o644): void { mkdirSync(dirname(path), { recursive: true }); const temporary = `${path}.tmp`; writeFileSync(temporary, `${canonicalJson(value as JsonValue)}\n`, { encoding: "utf8", mode }); renameSync(temporary, path); }
function run(command: string, args: string[]): { ok: boolean; stdout: string } { const result = spawnSync(command, args, { encoding: "utf8", windowsHide: true, timeout: 10_000 }); return { ok: result.status === 0, stdout: result.stdout ?? "" }; }
function clean(value: string): string { return value.replace(/[\u0000-\u001f]+/g, " ").trim().slice(0, 200); }
function url(config: Config): string { return `http://127.0.0.1:${config.port}`; }
function delay(milliseconds: number): Promise<void> { return new Promise((resolve) => setTimeout(resolve, milliseconds)); }
