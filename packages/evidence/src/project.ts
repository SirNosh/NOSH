import { createId } from "@nosh/core";
import { canonicalJson, projectContractSchema, projectRootSchema, schemaLockSchema, schemaUri, schemaUris, sha256, type JsonValue } from "@nosh/wire";
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, realpathSync, renameSync, statSync, writeFileSync } from "node:fs";
import { basename, dirname, join, resolve } from "node:path";
import { initializePaperWorkspace } from "./paper.js";

export type ProjectInitialization = { path: string; dataDirectory: string; createRepository?: boolean; workingTitle?: string; githubRepositoryUrl?: string };
export type InitializedProject = { projectId: string; repositoryRoot: string; databasePath: string };
export type ProjectContract = ReturnType<typeof projectContractSchema.parse>;

export function initializeResearchProject(input: ProjectInitialization): InitializedProject {
  const requested = resolve(input.path); if (input.createRepository) { mkdirSync(requested, { recursive: true }); const initialized = git(requested, ["init"]); if (!initialized.ok) throw new Error("Git repository initialization failed"); }
  const root = realpathSync(requested); if (!statSync(root).isDirectory()) throw new Error("Project path must be a directory"); const top = git(root, ["rev-parse", "--show-toplevel"]); if (!top.ok || realpathSync(top.stdout.trim()) !== root) throw new Error("Project path must be the root of a Git repository"); configureGithubRemote(root, input.githubRepositoryUrl);
  const nosh = join(root, ".nosh"); for (const directory of ["contracts", "events", "artifacts", "sessions", "jobs"]) mkdirSync(join(nosh, directory), { recursive: true });
  const metadataPath = join(nosh, "project.json"); const prior = existsSync(metadataPath) ? JSON.parse(readFileSync(metadataPath, "utf8")) as { projectId?: string } : {}; const projectId = prior.projectId ?? createId("prj"); const now = new Date().toISOString(); const branch = git(root, ["branch", "--show-current"]).stdout.trim() || "main"; const workingTitle = input.workingTitle?.trim() || basename(root);
  initializePaperWorkspace(root, workingTitle);
  const contract = projectContractSchema.parse({ $schema: schemaUri("project-contract"), schemaVersion: 1, templateVersion: "1.0.0", projectId, contractVersion: 1, workingTitle, domainTags: [], northStar: { goalId: "goal_project", question: "Pending collaborative Project discovery", contributionType: "contribution_pending", decisionUse: "Pending collaborative Project discovery" }, scope: { included: [], excluded: [] }, datasets: [], licensingConstraints: [], computeEnvelope: { maximumGpuHours: 0, maximumDiskBytes: 0, allowedHardwareClasses: ["local"] }, reproducibilityStandard: { minimumSeeds: 1, environmentLockRequired: true, immutableEvaluatedCommitRequired: true, rawLogsRetained: true }, paper: { intendedVenue: null, requiredSections: ["Abstract", "Introduction", "Related work", "Method", "Experiments", "Limitations", "Conclusion"], claimPolicy: "evidence_link_required" }, policies: { network: "network_user.approved", privacy: "privacy_local.first", publication: "publication_user.approved", protectedPaths: [".git", ".nosh"] }, canonicalDefaultBranch: branch, createdBy: "user", createdAt: now, approvedAt: null });
  const databasePath = join(resolve(input.dataDirectory), "projects", projectId, "nosh.sqlite"); writeAtomic(join(nosh, "contracts", "project.v1.json"), contract); writeAtomic(metadataPath, projectRootSchema.parse({ $schema: schemaUri("project-root"), schemaVersion: 1, projectId, activeProjectContractVersion: 1, activeProjectContractPath: ".nosh/contracts/project.v1.json", operationalDatabaseId: "database_operational", defaultBranch: branch, latestPersistentSequence: 0, updatedAt: now })); writeAtomic(join(nosh, "schema-lock.json"), schemaLockSchema.parse({ $schema: schemaUri("schema-lock"), schemaVersion: 1, noshVersion: "0.1.0", wirePackageVersion: "0.1.0", templateBundleVersion: "1.0.0", schemaDigest: sha256(schemaUris()), migrationsApplied: ["migration_1"], updatedAt: now }));
  return { projectId, repositoryRoot: root, databasePath };
}

export function readProjectContract(repositoryRoot: string): ProjectContract {
  const root = projectRootSchema.parse(JSON.parse(readFileSync(join(repositoryRoot, ".nosh", "project.json"), "utf8")));
  return projectContractSchema.parse(JSON.parse(readFileSync(join(repositoryRoot, root.activeProjectContractPath), "utf8")));
}

export function approveProjectContract(repositoryRoot: string, input: unknown): ProjectContract {
  const current = readProjectContract(repositoryRoot); const contract = projectContractSchema.parse(input);
  if (current.approvedAt) throw new Error("Project discovery is already complete");
  if (!contract.approvedAt) throw new Error("Approved Project contract requires approvedAt");
  if (contract.projectId !== current.projectId || contract.contractVersion !== current.contractVersion + 1 || contract.createdAt !== current.createdAt || contract.createdBy !== current.createdBy || contract.canonicalDefaultBranch !== current.canonicalDefaultBranch) throw new Error("Project contract identity or version does not match the draft");
  const rootPath = join(repositoryRoot, ".nosh", "project.json"); const root = projectRootSchema.parse(JSON.parse(readFileSync(rootPath, "utf8"))); const contractPath = `.nosh/contracts/project.v${contract.contractVersion}.json`;
  writeAtomic(join(repositoryRoot, contractPath), contract); writeAtomic(rootPath, projectRootSchema.parse({ ...root, activeProjectContractVersion: contract.contractVersion, activeProjectContractPath: contractPath, updatedAt: new Date().toISOString() })); return contract;
}

function git(root: string, args: string[]): { ok: boolean; stdout: string } { const result = spawnSync("git", ["-C", root, ...args], { encoding: "utf8", windowsHide: true, timeout: 10_000 }); return { ok: result.status === 0, stdout: result.stdout ?? "" }; }
function writeAtomic(path: string, value: JsonValue): void { mkdirSync(dirname(path), { recursive: true }); const temporary = `${path}.tmp`; writeFileSync(temporary, `${canonicalJson(value)}\n`, "utf8"); renameSync(temporary, path); }
function configureGithubRemote(root: string, value?: string): void { const url = value?.trim(); if (!url) return; if (!/^https:\/\/github\.com\/[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+(?:\.git)?\/?$/.test(url)) throw new Error("GitHub repository must be a full https://github.com/owner/repository URL"); const origin = git(root, ["remote", "get-url", "origin"]); if (origin.ok && origin.stdout.trim() !== url) throw new Error("Existing Git origin does not match the GitHub repository link"); if (!origin.ok && !git(root, ["remote", "add", "origin", url]).ok) throw new Error("GitHub repository could not be configured as origin"); }
