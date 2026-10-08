import { createId } from "@nosh/core";
import { canonicalJson, projectContractSchema, projectRootSchema, schemaLockSchema, schemaUri, schemaUris, sha256, type JsonValue } from "@nosh/wire";
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, readdirSync, realpathSync, renameSync, statSync, writeFileSync } from "node:fs";
import { basename, dirname, join, resolve } from "node:path";
import { initializePaperWorkspace } from "./paper.js";

export type ProjectInitialization = { path: string; dataDirectory: string; createRepository?: boolean; workingTitle?: string; githubRepositoryUrl?: string };
export type InitializedProject = { projectId: string; repositoryRoot: string; databasePath: string };
export type ProjectContract = ReturnType<typeof projectContractSchema.parse>;
export type RunnableCommand = { commandId: string; description: string; argv: string[]; timeoutSeconds: number };

// Executables a supervised Job can launch directly (argv, no shell). npm/npx are shell shims on Windows and are excluded.
const DIRECT_EXECUTABLES = new Set(["node", "python", "python3", "py", "pytest", "bun", "deno", "go", "cargo", "make", "Rscript", "julia"]);

/** Candidate test/evaluation commands for discovery to propose; the user approves them in the contract. Read-only and best effort. */
export function detectRunnableCommands(repositoryRoot: string): RunnableCommand[] {
  const found: RunnableCommand[] = []; const read = (name: string): string | null => { try { return readFileSync(join(repositoryRoot, name), "utf8"); } catch { return null; } };
  try {
    const scripts = (JSON.parse(read("package.json") ?? "{}") as { scripts?: Record<string, unknown> }).scripts ?? {};
    for (const [name, script] of Object.entries(scripts)) {
      if (!/^(test|eval|evaluate|bench|benchmark|reproduce)(:[\w-]+)?$/.test(name) || typeof script !== "string" || /[&|;<>`$()*]/.test(script)) continue;
      const argv = script.trim().split(/\s+/); if (!DIRECT_EXECUTABLES.has(argv[0]!)) continue;
      found.push({ commandId: `command_${name.toLowerCase().replace(/[^a-z0-9]+/g, "-")}`, description: `package.json script "${name}"`, argv, timeoutSeconds: 600 });
    }
  } catch { /* unreadable package.json proposes nothing */ }
  const tests = (() => { try { return readdirSync(join(repositoryRoot, "tests")).some((file) => /^test_.*\.py$/.test(file)); } catch { return false; } })();
  if (existsSync(join(repositoryRoot, "pytest.ini")) || /\[tool\.pytest/.test(read("pyproject.toml") ?? "") || tests) found.push({ commandId: "command_pytest", description: "pytest suite", argv: ["python", "-m", "pytest", "-q"], timeoutSeconds: 1_800 });
  const makefile = read("Makefile") ?? "";
  for (const target of ["test", "eval", "evaluate", "reproduce"]) if (new RegExp(`^${target}:`, "m").test(makefile)) found.push({ commandId: `command_make-${target}`, description: `make ${target}`, argv: ["make", target], timeoutSeconds: 1_800 });
  return found.filter((command, index) => found.findIndex((other) => other.commandId === command.commandId) === index).slice(0, 10);
}

/**
 * NOSH's runtime state (Artifacts, Mission/Autoresearch files, Pi sessions, exports, worktrees) lives under .nosh but is
 * not research content: ignore all of it except the control files that belong in Git. Without this the user's checkout
 * turns dirty as soon as work produces an Artifact, and every later task fails its clean-tree preflight.
 */
export const NOSH_GITIGNORE = "# NOSH runtime state (rewritten by NOSH; ignores itself). Only the Project's control files are tracked.\n/*\n!/project.json\n!/schema-lock.json\n!/contracts/\n";
export function ensureNoshIgnore(repositoryRoot: string): void {
  const path = join(repositoryRoot, ".nosh", ".gitignore");
  if (!existsSync(join(repositoryRoot, ".nosh"))) return;
  if (!existsSync(path) || readFileSync(path, "utf8") !== NOSH_GITIGNORE) writeFileSync(path, NOSH_GITIGNORE, "utf8");
}

export function initializeResearchProject(input: ProjectInitialization): InitializedProject {
  const requested = resolve(input.path); if (input.createRepository) { mkdirSync(requested, { recursive: true }); const initialized = git(requested, ["init"]); if (!initialized.ok) throw new Error("Git repository initialization failed"); }
  const root = realpathSync(requested); if (!statSync(root).isDirectory()) throw new Error("Project path must be a directory"); const top = git(root, ["rev-parse", "--show-toplevel"]); if (!top.ok || realpathSync(top.stdout.trim()) !== root) throw new Error("Project path must be the root of a Git repository"); configureGithubRemote(root, input.githubRepositoryUrl);
  const nosh = join(root, ".nosh"); for (const directory of ["contracts", "events", "artifacts", "sessions", "jobs"]) mkdirSync(join(nosh, directory), { recursive: true });
  ensureNoshIgnore(root);
  const metadataPath = join(nosh, "project.json"); const prior = existsSync(metadataPath) ? JSON.parse(readFileSync(metadataPath, "utf8")) as { projectId?: string } : {}; const projectId = prior.projectId ?? createId("prj"); const now = new Date().toISOString(); const branch = git(root, ["branch", "--show-current"]).stdout.trim() || "main"; const workingTitle = input.workingTitle?.trim() || basename(root);
  initializePaperWorkspace(root, workingTitle);
  const contract = projectContractSchema.parse({ $schema: schemaUri("project-contract"), schemaVersion: 1, templateVersion: "1.0.0", projectId, contractVersion: 1, workingTitle, domainTags: [], northStar: { goalId: "goal_project", question: "Pending collaborative Project discovery", contributionType: "contribution_pending", decisionUse: "Pending collaborative Project discovery" }, scope: { included: [], excluded: [] }, datasets: [], licensingConstraints: [], computeEnvelope: { maximumGpuHours: 0, maximumDiskBytes: 0, allowedHardwareClasses: ["local"] }, reproducibilityStandard: { minimumSeeds: 1, environmentLockRequired: true, immutableEvaluatedCommitRequired: true, rawLogsRetained: true }, paper: { intendedVenue: null, requiredSections: ["Abstract", "Introduction", "Related work", "Method", "Experiments", "Limitations", "Conclusion"], claimPolicy: "evidence_link_required" }, policies: { network: "network_user.approved", privacy: "privacy_local.first", publication: "publication_user.approved", protectedPaths: [".git", ".nosh"] }, canonicalDefaultBranch: branch, createdBy: "user", createdAt: now, approvedAt: null });
  const databasePath = join(resolve(input.dataDirectory), "projects", projectId, "nosh.sqlite"); writeAtomic(join(nosh, "contracts", "project.v1.json"), contract); writeAtomic(metadataPath, projectRootSchema.parse({ $schema: schemaUri("project-root"), schemaVersion: 1, projectId, activeProjectContractVersion: 1, activeProjectContractPath: ".nosh/contracts/project.v1.json", operationalDatabaseId: "database_operational", defaultBranch: branch, latestPersistentSequence: 0, updatedAt: now })); writeAtomic(join(nosh, "schema-lock.json"), schemaLockSchema.parse({ $schema: schemaUri("schema-lock"), schemaVersion: 1, noshVersion: "0.1.0", wirePackageVersion: "0.1.0", templateBundleVersion: "1.0.0", schemaDigest: sha256(schemaUris()), migrationsApplied: ["migration_1"], updatedAt: now }));
  return { projectId, repositoryRoot: root, databasePath };
}

export function readProjectContract(repositoryRoot: string): ProjectContract {
  const metadataPath = join(repositoryRoot, ".nosh", "project.json");
  const root = projectRootSchema.safeParse(readProjectJson(metadataPath, "metadata"));
  if (!root.success) throw projectReadError(metadataPath, "metadata", projectIssueSummary(root.error.issues));
  // Repository-controlled metadata must not redirect reads outside .nosh/contracts.
  if (!/^\.nosh\/contracts\/project\.v\d+\.json$/.test(root.data.activeProjectContractPath)) throw projectReadError(metadataPath, "metadata", "activeProjectContractPath must name .nosh/contracts/project.v<N>.json");
  const contractPath = join(repositoryRoot, root.data.activeProjectContractPath);
  const contract = projectContractSchema.safeParse(readProjectJson(contractPath, "active contract"));
  if (!contract.success) throw projectReadError(contractPath, "active contract", projectIssueSummary(contract.error.issues));
  return contract.data;
}

function readProjectJson(path: string, label: string): unknown {
  try { return JSON.parse(readFileSync(path, "utf8")); }
  // Parser messages quote file content; report only the failure class.
  catch (error) { throw projectReadError(path, label, (error as NodeJS.ErrnoException).code === "ENOENT" ? "file is missing" : error instanceof SyntaxError ? "file is not valid JSON" : "file could not be read"); }
}

function projectReadError(path: string, label: string, detail: string): Error {
  return new Error(`Cannot load Project ${label} at "${path}": ${detail}. Repair this file to match its NOSH schema, then retry. No older contract was selected.`);
}

function projectIssueSummary(issues: Array<{ path: Array<string | number>; message: string }>): string {
  const summary = issues.slice(0, 3).map((issue) => `${issue.path.join(".") || "(root)"}: ${issue.message}`).join("; ");
  return issues.length > 3 ? `${summary}; +${issues.length - 3} more validation issues` : summary;
}

/**
 * Commits exactly the active contract and its pointer (never other staged work, never a push) so task
 * preflight sees a clean checkout right after approval or amendment. Best effort: a failure is reported, not thrown.
 */
export function commitProjectContract(repositoryRoot: string): { committed: boolean; detail: string } {
  const contract = readProjectContract(repositoryRoot);
  const git = (args: string[]) => spawnSync("git", ["-C", repositoryRoot, "-c", `safe.directory=${repositoryRoot.replaceAll("\\", "/")}`, ...args], { encoding: "utf8", windowsHide: true, timeout: 20_000 });
  if (git(["rev-parse", "--is-inside-work-tree"]).status !== 0) return { committed: false, detail: "not a Git repository" };
  // Everything NOSH scaffolded must be committed too, or the checkout stays dirty and every Mission fails its clean-tree
  // preflight: all contract versions, the schema lock, and the paper workspace while still untracked (never a user's edits).
  const contracts = existsSync(join(repositoryRoot, ".nosh", "contracts")) ? readdirSync(join(repositoryRoot, ".nosh", "contracts")).filter((name) => /^project\.v\d+\.json$/.test(name)).map((name) => `.nosh/contracts/${name}`) : [];
  const untracked = (path: string) => existsSync(join(repositoryRoot, path)) && git(["ls-files", "--error-unmatch", "--", path]).status !== 0;
  const paths = [...new Set([".nosh/project.json", `.nosh/contracts/project.v${contract.contractVersion}.json`, ...contracts, ...[".nosh/schema-lock.json", "docs/paper.md", "docs/paper.bib"].filter((path) => path.startsWith(".nosh/") ? existsSync(join(repositoryRoot, path)) : untracked(path))])];
  if (git(["add", "--", ...paths]).status !== 0) return { committed: false, detail: "the contract files are ignored or unreadable by Git" };
  if (!git(["status", "--porcelain", "--", ...paths]).stdout.trim()) return { committed: false, detail: "already committed" };
  const identity = git(["config", "user.email"]).status === 0 ? [] : ["-c", "user.name=NOSH", "-c", "user.email=nosh@localhost.invalid"];
  const result = git([...identity, "commit", "-m", `NOSH: approve project contract v${contract.contractVersion}`, "--", ...paths]);
  return result.status === 0 ? { committed: true, detail: git(["rev-parse", "HEAD"]).stdout.trim() } : { committed: false, detail: (result.stderr || result.stdout).trim().slice(0, 300) };
}

export function approveProjectContract(repositoryRoot: string, input: unknown): ProjectContract {
  const current = readProjectContract(repositoryRoot);
  if (current.approvedAt) throw new Error("Project discovery is already complete");
  return writeApprovedSuccessor(repositoryRoot, current, input);
}

/** User-only amendment of an approved contract (e.g. declaring runnable commands); agents approve only through discovery. */
export function amendProjectContract(repositoryRoot: string, input: unknown): ProjectContract {
  const current = readProjectContract(repositoryRoot);
  if (!current.approvedAt) throw new Error("Complete Project discovery before amending the contract");
  return writeApprovedSuccessor(repositoryRoot, current, input);
}

function writeApprovedSuccessor(repositoryRoot: string, current: ProjectContract, input: unknown): ProjectContract {
  // The host records the approval moment; agents must not invent approval metadata.
  const contract = projectContractSchema.parse({ ...(input && typeof input === "object" ? input : {}), approvedAt: new Date().toISOString() });
  if (contract.projectId !== current.projectId || contract.contractVersion !== current.contractVersion + 1 || contract.createdAt !== current.createdAt || contract.createdBy !== current.createdBy || contract.canonicalDefaultBranch !== current.canonicalDefaultBranch) throw new Error("Project contract identity or version does not match the draft");
  const rootPath = join(repositoryRoot, ".nosh", "project.json"); const root = projectRootSchema.parse(JSON.parse(readFileSync(rootPath, "utf8"))); const contractPath = `.nosh/contracts/project.v${contract.contractVersion}.json`;
  writeAtomic(join(repositoryRoot, contractPath), contract); writeAtomic(rootPath, projectRootSchema.parse({ ...root, activeProjectContractVersion: contract.contractVersion, activeProjectContractPath: contractPath, updatedAt: new Date().toISOString() })); return contract;
}
function git(root: string, args: string[]): { ok: boolean; stdout: string } { const result = spawnSync("git", ["-C", root, ...args], { encoding: "utf8", windowsHide: true, timeout: 10_000 }); return { ok: result.status === 0, stdout: result.stdout ?? "" }; }
function writeAtomic(path: string, value: JsonValue): void { mkdirSync(dirname(path), { recursive: true }); const temporary = `${path}.tmp`; writeFileSync(temporary, `${canonicalJson(value)}\n`, "utf8"); renameSync(temporary, path); }
function configureGithubRemote(root: string, value?: string): void { const url = value?.trim(); if (!url) return; if (!/^https:\/\/github\.com\/[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+(?:\.git)?\/?$/.test(url)) throw new Error("GitHub repository must be a full https://github.com/owner/repository URL"); const origin = git(root, ["remote", "get-url", "origin"]); if (origin.ok && origin.stdout.trim() !== url) throw new Error("Existing Git origin does not match the GitHub repository link"); if (!origin.ok && !git(root, ["remote", "add", "origin", url]).ok) throw new Error("GitHub repository could not be configured as origin"); }
