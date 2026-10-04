#!/usr/bin/env node
import { randomBytes } from "node:crypto";
import { closeSync, existsSync, mkdirSync, openSync, readFileSync, realpathSync, renameSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join, relative, resolve } from "node:path";
import { spawn, spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { initializeResearchProject } from "@nosh/evidence";
import { createProjectBackup, HostRegistry, scheduleProjectRestore, type RegisteredProject } from "@nosh/persistence";
import { createInterface } from "node:readline/promises";
import { canonicalJson, parseModelSelection, schemaUris, type JsonValue } from "@nosh/wire";
import { gitDoctorChecks } from "./backup-doctor.js";
import { connectModelProvider, connectedProviders } from "./login.js";

type Config = { dataDirectory: string; port: number; bootstrapToken: string; currentProjectId: string | null; defaultModel?: string };
// Models NOSH is tested with, preferred as the default when the account offers them (low effort keeps research turns cheap).
const RECOMMENDED_MODELS = ["gpt-6-luna"];
const stateRoot = resolve(process.env.LOCALAPPDATA || process.env.APPDATA || join(homedir(), ".nosh"), "NOSH");
const configPath = join(stateRoot, "config.json"); const flags = new Map(process.argv.slice(2).filter((value) => value.startsWith("--")).map((value) => { const [key, ...rest] = value.split("="); return [key!, rest.join("=")]; }));
const words = process.argv.slice(2).filter((value) => !value.startsWith("--"));
try { await route(words); } catch (error) { process.stderr.write(`nosh: ${error instanceof Error ? error.message : "command failed"}\n`); process.exitCode = 1; }

async function route(args: string[]): Promise<void> {
  const [command, subcommand, argument, backupId] = args;
  if (!command || command === "help" || flags.has("--help")) return help();
  if (command === "setup") return setup();
  if (command === "login") return loginCommand(subcommand);
  if (command === "logout" && subcommand) return logoutCommand(subcommand);
  const config = loadConfig();
  if (command === "model") return model(config, subcommand, argument);
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

/** Guided first run: prerequisites, state, daemon, and the default model, with a fix for every failed check. */
async function setup(): Promise<void> {
  mkdirSync(stateRoot, { recursive: true }); const dataDirectory = join(stateRoot, "data"); mkdirSync(dataDirectory, { recursive: true });
  if (!existsSync(configPath)) saveConfig({ dataDirectory, port: 4321, bootstrapToken: randomBytes(32).toString("base64url"), currentProjectId: null });
  if (process.platform === "win32" && process.env.USERNAME) spawnSync("icacls.exe", [stateRoot, "/inheritance:r", "/grant:r", `${process.env.USERNAME}:(OI)(CI)F`], { windowsHide: true });
  process.stdout.write(`NOSH state: ${stateRoot}\n\nChecking prerequisites\n`);
  const [major, minor] = process.versions.node.split(".").map(Number) as [number, number];
  const git = run("git", ["--version"]); const bun = bunVersion();
  const checks: Array<[string, boolean, string]> = [
    ["Node >= 22.19", major > 22 || (major === 22 && minor >= 19), major > 22 || (major === 22 && minor >= 19) ? process.versions.node : `found ${process.versions.node}; install Node 22.19+ from https://nodejs.org`],
    ["Git", git.ok, git.ok ? clean(git.stdout) : "install Git from https://git-scm.com"],
    ["Bun >= 1.3 (terminal UI)", bun.ok, bun.ok ? bun.version : "install Bun from https://bun.sh, then open a new terminal"],
  ];
  for (const [name, ok, detail] of checks) process.stdout.write(`  ${ok ? "✓" : "✗"} ${name}${ok ? `  ${detail}` : `\n      → ${detail}`}\n`);
  if (!checks[0]![1] || !checks[1]![1]) throw new Error("fix the prerequisites above, then run `nosh setup` again");
  let config = loadConfig();
  // A model account comes first: a ChatGPT/Claude subscription or an API key, stored where Pi stores it.
  let connected = await connectedProviders(); let newlyConnected = false;
  if (connected.length) process.stdout.write(`  ✓ Model access  ${connected.map((provider) => provider.name).join(", ")}\n`);
  else if (process.stdin.isTTY && !flags.has("--yes")) {
    process.stdout.write("\nConnect a model account. NOSH uses it for every research session; credentials stay in Pi's store and are never shown.\n");
    while (!connected.length && await connectModelProvider()) { connected = await connectedProviders(); newlyConnected = connected.length > 0; }
  }
  process.stdout.write("\n");
  // A daemon started before this sign-in has not loaded the new credential.
  if (newlyConnected && await healthy(config)) await stop(config);
  await start(config);
  if (connected.length) config = await chooseDefaultModel(config);
  else process.stdout.write("\nNo model account connected yet. Run `nosh login` (ChatGPT/Claude subscription or API key), then `nosh model set <provider/id:level>`.\n");
  process.stdout.write(`\nReady. Next:\n  nosh open        open the terminal UI (press ctrl+o to open or create a project)\n  nosh doctor      re-run these checks any time\n  nosh model       show or change the default model${config.defaultModel ? ` (now ${config.defaultModel})` : ""}\n`);
}

async function loginCommand(argument?: string): Promise<void> {
  if (argument === "--list" || flags.has("--list")) {
    const providers = await (await import("@nosh/pi-adapter")).authProviders();
    for (const provider of providers.filter((entry) => entry.subscription || entry.apiKey)) process.stdout.write(`${provider.connected ? "●" : "○"} ${provider.id.padEnd(24)} ${[provider.subscription ? "subscription" : "", provider.apiKey ? "api key" : ""].filter(Boolean).join(" + ")}\n`);
    return;
  }
  if (!process.stdin.isTTY) throw new Error("nosh login needs an interactive terminal");
  if (await connectModelProvider()) {
    const config = existsSync(configPath) ? loadConfig() : null;
    process.stdout.write(config && await healthy(config) ? "Restart the daemon to use the new account: nosh stop, then nosh start (interrupts running tasks).\n" : "Next: nosh setup (or nosh model set <provider/id:level>)\n");
  }
}

async function logoutCommand(providerId: string): Promise<void> {
  await (await import("@nosh/pi-adapter")).logoutProvider(providerId);
  process.stdout.write(`Signed out of ${providerId}\n`);
}

/** Default model for every session that selects none (Directors, workers, reviewers, librarians). */
async function chooseDefaultModel(config: Config): Promise<Config> {
  const requested = flags.get("--model");
  if (requested) return applyDefaultModel(config, requested, true);
  if (config.defaultModel && !flags.has("--reselect")) { process.stdout.write(`\nDefault model: ${config.defaultModel} (change with \`nosh model set\`)\n`); return config; }
  const models = await availableModels(config);
  if (!models.length) { process.stdout.write("\nPi lists no authenticated models yet; after logging in, run `nosh model set <provider/id:level>`.\n"); return config; }
  if (!process.stdin.isTTY || flags.has("--yes")) { process.stdout.write(`\nNo default model chosen (non-interactive). Run \`nosh model set <provider/id:level>\`; for example ${models[0]!.provider}/${models[0]!.id}:${preferredLevel(models[0]!)}\n`); return config; }
  const prompt = createInterface({ input: process.stdin, output: process.stdout });
  try {
    // Provider first (only connected ones), then a short list for that provider with a recommended default.
    const providers = [...new Set(models.map((entry) => entry.provider))];
    let provider = providers[0]!;
    if (providers.length > 1) {
      process.stdout.write("\nWhich connected account should NOSH use by default?\n");
      providers.forEach((name, index) => process.stdout.write(`  ${String(index + 1).padStart(2)}. ${name}\n`));
      provider = providers[Math.min(providers.length, Math.max(1, Number((await prompt.question(`Account [1-${providers.length}, default 1]: `)).trim() || "1"))) - 1] ?? provider;
    }
    const choices = models.filter((entry) => entry.provider === provider);
    const recommended = Math.max(0, choices.findIndex((entry) => RECOMMENDED_MODELS.includes(entry.id)));
    process.stdout.write(`\nDefault model for NOSH sessions (${provider}):\n`);
    choices.forEach((entry, index) => process.stdout.write(`  ${String(index + 1).padStart(2)}. ${entry.id}${entry.name && entry.name !== entry.id ? `  (${entry.name})` : ""}${index === recommended ? "  ← recommended" : ""}\n`));
    const picked = choices[Math.min(choices.length, Math.max(1, Number((await prompt.question(`Model [1-${choices.length}, default ${recommended + 1}]: `)).trim() || String(recommended + 1)))) - 1] ?? choices[recommended]!;
    const levels = picked.thinkingLevels ?? [];
    const fallback = preferredLevel(picked);
    const level = levels.length ? (await prompt.question(`Thinking level [${levels.join("/")}, default ${fallback}]: `)).trim() || fallback : "";
    return await applyDefaultModel(config, `${picked.provider}/${picked.id}${level ? `:${level}` : ""}`, true);
  } finally { prompt.close(); }
}


type ListedModel = { provider: string; id: string; name?: string; thinkingLevels?: string[] };
async function availableModels(config: Config): Promise<ListedModel[]> {
  if (!await healthy(config)) return [];
  try { return ((await api(config, "/models")) as { models?: ListedModel[] }).models ?? []; } catch { return []; }
}
function preferredLevel(entry: ListedModel): string { const levels = entry.thinkingLevels ?? []; return levels.includes("low") ? "low" : levels[0] ?? ""; }

/** Validates against the models Pi lists (when reachable), saves, and restarts a running daemon only when asked. */
async function applyDefaultModel(config: Config, text: string, restart: boolean): Promise<Config> {
  const selection = parseModelSelection(text);
  const models = await availableModels(config);
  const match = models.find((entry) => entry.provider === selection.provider && entry.id === selection.id);
  if (models.length && !match) throw new Error(`${selection.provider}/${selection.id} is not an authenticated Pi model; run \`nosh model list\``);
  if (match && selection.thinkingLevel && match.thinkingLevels?.length && !match.thinkingLevels.includes(selection.thinkingLevel)) throw new Error(`${selection.provider}/${selection.id} supports thinking levels ${match.thinkingLevels.join(", ")}`);
  const next = { ...config, defaultModel: `${selection.provider}/${selection.id}${selection.thinkingLevel ? `:${selection.thinkingLevel}` : ""}` };
  saveConfig(next);
  process.stdout.write(`Default model set to ${next.defaultModel}\n`);
  if (await healthy(config)) {
    if (restart) { await stop(next); await start(next); }
    else process.stdout.write("The running daemon keeps its current default until restarted (nosh stop, then nosh start); restarting interrupts running tasks.\n");
  }
  return next;
}

async function model(config: Config, action = "show", value?: string): Promise<void> {
  if (action === "show") return void process.stdout.write(`${config.defaultModel ?? "none (Pi's global default applies; set one with `nosh model set <provider/id:level>`)"}\n`);
  if (action === "list") {
    const models = await availableModels(config);
    if (!models.length) return void process.stdout.write(await healthy(config) ? "Pi lists no authenticated models; log in with `pi` first.\n" : "Start the daemon first: nosh start\n");
    for (const entry of models) process.stdout.write(`${entry.provider}/${entry.id}${entry.thinkingLevels?.length ? `  [${entry.thinkingLevels.join(", ")}]` : ""}${`${entry.provider}/${entry.id}` === config.defaultModel?.split(":")[0] ? "  ← default" : ""}\n`);
    return;
  }
  if (action === "set" && value) { await applyDefaultModel(config, value, flags.has("--restart")); return; }
  throw new Error("Use nosh model [show|list|set <provider/id[:thinkingLevel]> [--restart]]");
}

function bunVersion(): { ok: boolean; version: string } {
  const bun = run(process.env.NOSH_BUN || "bun", ["--version"]); const version = /^(\d+)\.(\d+)\./.exec(bun.stdout.trim());
  return { ok: bun.ok && Boolean(version) && (Number(version![1]) > 1 || (Number(version![1]) === 1 && Number(version![2]) >= 3)), version: bun.stdout.trim() };
}

async function start(config: Config): Promise<void> {
  if (await healthy(config)) return void process.stdout.write(`noshd already running at ${url(config)}\n`);
  const script = fileURLToPath(new URL("../../noshd/dist/main.js", import.meta.url)); if (!existsSync(script)) throw new Error("noshd build is missing; install a NOSH distribution or run the repository build");
  mkdirSync(join(config.dataDirectory, "logs"), { recursive: true }); const log = openSync(join(config.dataDirectory, "logs", "noshd.log"), "a");
  const child = spawn(process.execPath, [script, `--data-dir=${config.dataDirectory}`, `--port=${config.port}`], { detached: true, windowsHide: true, stdio: ["ignore", log, log], env: { ...process.env, NOSH_BOOTSTRAP_TOKEN: config.bootstrapToken, ...(config.defaultModel ? { NOSH_DEFAULT_MODEL: config.defaultModel } : {}) } });
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
  if (!bunVersion().ok) throw new Error("nosh tui requires Bun >=1.3 on PATH; install Bun from https://bun.sh (daemon and scripting commands use Node)");
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
  const registration = initializeResearchProject({ path: input, dataDirectory: config.dataDirectory }); if (await healthy(config)) await api(config, "/projects", { method: "POST", body: registration }); else { const registry = new HostRegistry(join(config.dataDirectory, "host.sqlite")); try { registry.register(registration); } finally { registry.close(); } }
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
  // Match backup's realpath lookup so symlinked or differently cased paths resolve the same Project.
  const path = existsSync(resolve(selector)) ? realpathSync(resolve(selector)) : null;
  const project = (await projects(config)).find((entry) => entry.projectId === selector || (path && entry.repositoryRoot === path));
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
  const accounts = await connectedProviders().catch(() => []); checks.push(["Model access", accounts.length > 0, accounts.length ? accounts.map((provider) => provider.name).join(", ") : "none; run nosh login"]);
  const bun = bunVersion(); checks.push(["Bun (terminal UI)", bun.ok, bun.ok ? bun.version : "install Bun >=1.3 from https://bun.sh"]);
  checks.push(["Default model", Boolean(config.defaultModel), config.defaultModel ?? "none; run nosh model set <provider/id:level>"]);
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
async function api(config: Config, path: string, options: { method?: string; body?: unknown } = {}): Promise<unknown> { const response = await fetch(`${url(config)}/api${path}`, { method: options.method ?? "GET", signal: AbortSignal.timeout(300_000), headers: options.body === undefined ? {} : { "content-type": "application/json" }, ...(options.body === undefined ? {} : { body: JSON.stringify(options.body) }) }); if (!response.ok) throw new Error((await response.json().catch(() => ({})) as { error?: string }).error ?? `daemon request failed (${response.status})`); return response.json(); }
// A busy machine (test suites, model sessions) can delay /health; a false "not running" makes stop skip a live daemon.
async function healthy(config: Config): Promise<boolean> { try { return (await fetch(`${url(config)}/health`, { signal: AbortSignal.timeout(3_000) })).ok; } catch { return false; } }
function saveConfig(config: Config): void { mkdirSync(dirname(configPath), { recursive: true }); writeAtomic(configPath, config, 0o600); }
function loadConfig(): Config { if (!existsSync(configPath)) throw new Error("NOSH is not initialized; run `nosh setup`"); return JSON.parse(readFileSync(configPath, "utf8")) as Config; }
function help(): void { process.stdout.write("nosh setup [--model=provider/id:level] [--yes]\nnosh login [--list]   connect a ChatGPT/Claude subscription or an API key\nnosh logout <provider>\nnosh start|stop|status|open|tui|doctor\nnosh model [show|list|set <provider/id[:level]> [--restart]]\nnosh project list|open <path>\nnosh mission list|status <id>\nnosh job list\nnosh backup <project>\nnosh backup restore <project> <backup-id>\nnosh logs\n"); }
function writeAtomic(path: string, value: unknown, mode = 0o644): void { mkdirSync(dirname(path), { recursive: true }); const temporary = `${path}.tmp`; writeFileSync(temporary, `${canonicalJson(value as JsonValue)}\n`, { encoding: "utf8", mode }); renameSync(temporary, path); }
function run(command: string, args: string[]): { ok: boolean; stdout: string } { const result = spawnSync(command, args, { encoding: "utf8", windowsHide: true, timeout: 10_000 }); return { ok: result.status === 0, stdout: result.stdout ?? "" }; }
function clean(value: string): string { return value.replace(/[\u0000-\u001f]+/g, " ").trim().slice(0, 200); }
function url(config: Config): string { return `http://127.0.0.1:${config.port}`; }
function delay(milliseconds: number): Promise<void> { return new Promise((resolve) => setTimeout(resolve, milliseconds)); }
