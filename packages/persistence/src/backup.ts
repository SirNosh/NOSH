import { createHash } from "node:crypto";
import { chmodSync, cpSync, existsSync, lstatSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, renameSync, rmSync, statSync, writeFileSync } from "node:fs";
import { basename, dirname, isAbsolute, join, parse, relative, resolve, sep } from "node:path";
import { tmpdir } from "node:os";
import { spawnSync } from "node:child_process";
import { backupDatabase } from "./database.js";
import { HostRegistry, type RegisteredProject } from "./host-registry.js";

type JobIdentity = { projectId?: unknown };

export type BackupManifest = {
  format: "nosh-backup-v1";
  createdAt: string;
  project: { projectId: string; repositoryRoot: string };
  sqliteSchemaVersion: number;
  includesLargeArtifacts: boolean;
  gitBundle: { hasRefs: boolean; headCommit: string | null };
  files: Array<{ path: string; sha256: string; bytes: number }>;
};

export type BackupSummary = Pick<BackupManifest, "createdAt" | "project" | "sqliteSchemaVersion" | "includesLargeArtifacts"> & { backupId: string; path: string };
type RestorePlan = { version: 1; state?: "pending" | "committed"; project: RegisteredProject; backupId: string; backupDirectory: string; scheduledAt: string };
type GitHead = { version: 1; state: "symbolic" | "detached" | "unborn"; ref: string | null; commit: string | null };
type RestoreSwap = { destination: string; staged: string | null; rollback: string | null; movedRollback: boolean; movedStaged: boolean };
class RestoreCommittedMaintenanceError extends Error {
  constructor(error: unknown, operation = "rollback-data cleanup") {
    super(`Restore committed, but ${operation} failed: ${error instanceof Error ? error.message : String(error)}`);
    this.name = "RestoreCommittedMaintenanceError";
  }
}
function removeCommittedRestorePlan(path: string): void {
  try { rmSync(path, { force: true }); } catch (error) { throw new RestoreCommittedMaintenanceError(error, "scheduled restore plan removal"); }
}

const backupUnits = [".nosh/contracts", ".nosh/events", ".nosh/sessions", ".nosh/project.json", ".nosh/schema-lock.json", ".nosh/artifacts", "docs/paper.md", "docs/paper.bib", "docs/figures"];
const databaseSidecarSuffixes = ["-wal", "-shm", "-journal"] as const;
export async function createProjectBackup(dataDirectory: string, project: RegisteredProject): Promise<BackupSummary> {
  const dataRoot = resolve(dataDirectory);
  const backupsRoot = join(dataRoot, "backups");
  assertNoLinkedAncestors(project.repositoryRoot, project.repositoryRoot, "Project repository");
  assertGitClean(project.repositoryRoot);
  const gitHead = captureGitHead(project.repositoryRoot);
  if (gitHead.commit) gitTreeEntries(project.repositoryRoot, gitHead.commit);
  assertNoLinkedAncestors(dataRoot, backupsRoot, "Backup root");
  assertNoLinkedAncestors(dirname(project.databasePath), project.databasePath, "Project database");
  const databaseStat = lstatSync(project.databasePath, { throwIfNoEntry: false });
  if (!databaseStat || databaseStat.isSymbolicLink() || !databaseStat.isFile() || databaseStat.nlink > 1) throw new Error("Project database must already exist as an unlinked regular file");
  mkdirSync(backupsRoot, { recursive: true });
  assertNoLinkedAncestors(dataRoot, backupsRoot, "Backup root");
  const backupId = `${project.projectId}-${new Date().toISOString().replaceAll(":", "-")}`;
  const destination = join(backupsRoot, backupId);
  const staging = `${destination}.creating`;
  assertNoLinkedAncestors(dataRoot, staging, "Backup staging");
  if (existsSync(destination) || existsSync(staging)) throw new Error(`Backup destination already exists: ${backupId}`);
  mkdirSync(staging, { recursive: true });
  try {
    const sqliteSchemaVersion = await backupDatabase(project.databasePath, join(staging, "nosh.sqlite"));
    for (const unit of backupUnits) copyIfPresent(project.repositoryRoot, staging, unit);
    copyProjectJobs(dataRoot, staging, project.projectId);
    const initialRefs = runGit(project.repositoryRoot, ["for-each-ref", "--format=%(objectname) %(refname)"]);
    if (!initialRefs.ok) throw new Error("Backup Git ref snapshot failed");
    const initialRefMap = refsFrom(initialRefs.stdout);
    assertGitHeadConsistency(gitHead, initialRefMap, "Project Git HEAD");
    const hasRefs = initialRefMap.size > 0;
    const hasBundle = hasRefs || gitHead.commit !== null;
    let bundleRefs = "";
    let bundleCommits = "";
    if (hasBundle) {
      const bundle = join(staging, "git.bundle");
      const bundleArgs = ["bundle", "create", bundle, ...(hasRefs ? ["--all"] : []), ...(gitHead.commit ? ["HEAD"] : [])];
      if (!runGit(project.repositoryRoot, bundleArgs).ok) throw new Error("Backup Git bundle creation failed");
      const snapshot = snapshotBundle(bundle);
      bundleRefs = snapshot.refs;
      bundleCommits = [...new Set([...snapshot.commits.split(/\r?\n/).filter(Boolean), ...(gitHead.commit ? [gitHead.commit] : [])])].join("\n");
      if (hasRefs && !bundleRefs.trim()) throw new Error("Backup Git bundle contains no restorable refs");
    }
    writeFileSync(join(staging, "git-refs.txt"), bundleRefs, "utf8");
    writeFileSync(join(staging, "required-commits.txt"), bundleCommits, "utf8");
    writeJsonAtomic(join(staging, "git-bundle.json"), { version: 1, hasRefs, headCommit: gitHead.commit });
    writeJsonAtomic(join(staging, "git-head.json"), gitHead);
    const manifest: BackupManifest = {
      format: "nosh-backup-v1", createdAt: new Date().toISOString(),
      project: { projectId: project.projectId, repositoryRoot: project.repositoryRoot }, sqliteSchemaVersion,
      includesLargeArtifacts: existsSync(join(staging, ".nosh", "artifacts")), gitBundle: { hasRefs, headCommit: gitHead.commit }, files: hashFiles(staging),
    };
    writeJsonAtomic(join(staging, "manifest.json"), manifest);
    verifyBackup(staging);
    renameSync(staging, destination);
    return { backupId, path: destination, createdAt: manifest.createdAt, project: manifest.project, sqliteSchemaVersion, includesLargeArtifacts: manifest.includesLargeArtifacts };
  } catch (error) {
    rmSync(staging, { recursive: true, force: true });
    throw error;
  }
}

export function listProjectBackups(dataDirectory: string, projectId: string): BackupSummary[] {
  const dataRoot = resolve(dataDirectory);
  const root = join(dataRoot, "backups");
  assertNoLinkedAncestors(dataRoot, root, "Backup root");
  if (!existsSync(root)) return [];
  return readdirSync(root, { withFileTypes: true }).filter((entry) => entry.isDirectory()).flatMap((entry) => {
    const path = join(root, entry.name);
    try {
      const manifest = verifyBackup(path);
      return manifest.project.projectId === projectId ? [{ backupId: entry.name, path, createdAt: manifest.createdAt, project: manifest.project, sqliteSchemaVersion: manifest.sqliteSchemaVersion, includesLargeArtifacts: manifest.includesLargeArtifacts }] : [];
    } catch { return []; }
  }).sort((left, right) => right.createdAt.localeCompare(left.createdAt));
}

export function backupPath(dataDirectory: string, backupId: string): string {
  if (backupId === "." || backupId === ".." || !/^[A-Za-z0-9._-]+$/.test(backupId)) throw new Error("Backup ID is invalid");
  const dataRoot = resolve(dataDirectory);
  const root = join(dataRoot, "backups");
  assertNoLinkedAncestors(dataRoot, root, "Backup root");
  const path = resolve(root, backupId);
  assertNoLinkedAncestors(dataRoot, path, "Backup path");
  if (!contained(root, path) || !existsSync(path) || lstatSync(path).isSymbolicLink() || !statSync(path).isDirectory()) throw new Error("Backup does not exist");
  return path;
}

export function verifyBackup(backupDirectory: string): BackupManifest {
  const root = resolve(backupDirectory);
  assertNoLinkedAncestors(root, root, "Backup root");
  if (!existsSync(root) || lstatSync(root).isSymbolicLink() || !statSync(root).isDirectory()) throw new Error("Backup root is missing, symlinked, or not a directory");
  assertRegularContainedFile(root, "manifest.json", "Backup manifest");
  const manifest = readBackupManifest(root);
  const declared = [...manifest.files].sort((left, right) => left.path.localeCompare(right.path));
  const paths = new Set<string>();
  for (const entry of declared) {
    if (!entry.path || entry.path === "manifest.json" || !/^[a-f0-9]{64}$/.test(entry.sha256) || !Number.isInteger(entry.bytes) || entry.bytes < 0 || paths.has(entry.path)) throw new Error(`Malformed backup file entry: ${entry.path}`);
    paths.add(entry.path);
    const path = resolve(root, entry.path);
    if (!contained(root, path) || !existsSync(path) || !statSync(path).isFile() || lstatSync(path).isSymbolicLink()) throw new Error(`Backup file is missing, symlinked, or escapes backup root: ${entry.path}`);
  }
  const actual = hashFiles(root).filter((entry) => entry.path !== "manifest.json");
  if (actual.length !== declared.length) throw new Error("Backup inventory does not match manifest");
  for (let index = 0; index < declared.length; index += 1) {
    const expected = declared[index]!;
    const observed = actual[index]!;
    if (expected.path !== observed.path || expected.bytes !== observed.bytes || expected.sha256 !== observed.sha256) throw new Error(`Backup integrity check failed: ${expected.path}`);
  }
  verifyArtifactObjects(root);
  for (const name of ["git-bundle.json", "git-head.json", "git-refs.txt", "required-commits.txt"]) assertRegularContainedFile(root, name, "Backup Git metadata");
  const bundleMetadataPath = join(root, "git-bundle.json");
  const bundleMetadata = JSON.parse(readFileSync(bundleMetadataPath, "utf8")) as { version?: unknown; hasRefs?: unknown; headCommit?: unknown };
  const gitHead = readGitHead(root);
  if (bundleMetadata.version !== 1 || typeof bundleMetadata.hasRefs !== "boolean" || (bundleMetadata.headCommit !== null && (typeof bundleMetadata.headCommit !== "string" || !safeGitObjectId(bundleMetadata.headCommit))) || bundleMetadata.hasRefs !== manifest.gitBundle.hasRefs || bundleMetadata.headCommit !== manifest.gitBundle.headCommit || gitHead.commit !== manifest.gitBundle.headCommit) throw new Error("Backup Git bundle metadata is inconsistent with the manifest");
  const refs = refsFrom(readFileSync(join(root, "git-refs.txt"), "utf8"));
  if (manifest.gitBundle.hasRefs !== Boolean(refs.size)) throw new Error("Backup Git bundle ref metadata is inconsistent with the ref inventory");
  assertGitHeadConsistency(gitHead, refs, "Backup Git HEAD");
  const commits = new Set(readRequiredCommits(root));
  if (gitHead.commit && !commits.has(gitHead.commit)) throw new Error("Backup HEAD commit is absent from required commits");
  const bundlePath = join(root, "git.bundle");
  const hasBundle = manifest.gitBundle.hasRefs || gitHead.commit !== null;
  if (hasBundle) {
    verifyBundle(bundlePath, commits);
  } else {
    if (commits.size) throw new Error("Unborn backup must not require Git commits");
    if (existsSync(bundlePath)) throw new Error("Unborn backup must not contain a Git bundle");
  }
  return manifest;
}

export function scheduleProjectRestore(dataDirectory: string, project: RegisteredProject, backupId: string): void {
  const backupDirectory = backupPath(dataDirectory, backupId);
  const manifest = verifyBackup(backupDirectory);
  if (manifest.project.projectId !== project.projectId || !samePath(manifest.project.repositoryRoot, project.repositoryRoot)) throw new Error("Backup Project identity or repository root does not match the registered Project");
  const dataRoot = resolve(dataDirectory);
  const plansRoot = join(dataRoot, "restore-plans");
  assertNoLinkedAncestors(dataRoot, plansRoot, "Restore plan root");
  mkdirSync(plansRoot, { recursive: true });
  assertNoLinkedAncestors(dataRoot, plansRoot, "Restore plan root");
  writeJsonAtomic(join(plansRoot, `${project.projectId}.json`), {
    version: 1, state: "pending", project, backupId, backupDirectory, scheduledAt: new Date().toISOString(),
  } satisfies RestorePlan);
}

export function applyScheduledRestores(dataDirectory: string): number {
  const dataRoot = resolve(dataDirectory);
  const plansRoot = join(dataRoot, "restore-plans");
  assertNoLinkedAncestors(dataRoot, plansRoot, "Restore plan root");
  if (!existsSync(plansRoot)) return 0;
  const registry = new HostRegistry(join(dataRoot, "host.sqlite"));
  try {
    let restored = 0;
    for (const entry of readdirSync(plansRoot, { withFileTypes: true })) {
      if (!entry.isFile() || !entry.name.endsWith(".json")) continue;
      const path = join(plansRoot, entry.name);
      const plan = JSON.parse(readFileSync(path, "utf8")) as RestorePlan;
      if (plan.version !== 1 || (plan.state !== undefined && plan.state !== "pending" && plan.state !== "committed") || !plan.project?.projectId || !plan.project.databasePath || !plan.project.repositoryRoot || !plan.backupId || !plan.backupDirectory) throw new Error(`Restore plan is malformed: ${entry.name}`);
      if (plan.state === "committed") {
        removeCommittedRestorePlan(path);
        restored += 1;
        continue;
      }
      const project = registry.list().find((candidate) => candidate.projectId === plan.project.projectId);
      if (!project || !samePath(project.repositoryRoot, plan.project.repositoryRoot) || !samePath(project.databasePath, plan.project.databasePath)) throw new Error(`Restore plan Project identity is invalid: ${entry.name}`);
      const backupDirectory = backupPath(dataRoot, plan.backupId);
      if (resolve(plan.backupDirectory) !== backupDirectory) throw new Error(`Restore plan backup identity is invalid: ${entry.name}`);
      try {
        restoreBackup(project.repositoryRoot, dataRoot, backupDirectory, project.projectId, project.databasePath, true, {
          onCommit: () => writeJsonAtomic(path, { ...plan, state: "committed" } satisfies RestorePlan),
        });
      } catch (error) {
        if (error instanceof RestoreCommittedMaintenanceError) {
          try { removeCommittedRestorePlan(path); } catch (planCleanupError) { throw new RestoreCommittedMaintenanceError(new AggregateError([error, planCleanupError], "Rollback-data and scheduled-plan cleanup failed")); }
        }
        throw error;
      }
      removeCommittedRestorePlan(path);
      restored += 1;
    }
    return restored;
  } finally {
    registry.close();
  }
}

/** Restore only after complete preflight; each replaced path is staged then swapped with rollback. */
export function restoreBackup(projectRoot: string, dataDirectory: string, backupDirectory: string, projectId: string, databasePath: string, daemonStopped: boolean, options?: { onPhase?(phase: string): void; onCommit?(): void }): BackupManifest {
  const manifest = verifyBackup(backupDirectory);
  const targetRoot = resolve(projectRoot);
  const dataRoot = resolve(dataDirectory);
  const dbDestination = resolve(databasePath);
  if (!daemonStopped) throw new Error("Refusing restore while noshd is running; stop the daemon first");
  if (manifest.project.projectId !== projectId) throw new Error("Backup belongs to a different Project");
  if (!samePath(manifest.project.repositoryRoot, targetRoot)) throw new Error("Backup repository root does not match the registered restore target");
  if (!existsSync(targetRoot) || !statSync(targetRoot).isDirectory()) throw new Error("Project repository root does not exist");
  assertNoLinkedAncestors(targetRoot, targetRoot, "Restore target root");
  assertNoLinkedAncestors(dataRoot, dataRoot, "Restore data root");
  assertNoLinkedAncestors(dirname(dbDestination), dbDestination, "Restore database");
  assertGitClean(targetRoot);
  const originalHead = captureGitHead(targetRoot);
  const originalTrackedPaths = trackedPaths(targetRoot);
  const currentRefs = gitRefs(targetRoot, "Current Git ref snapshot");
  assertGitHeadConsistency(originalHead, currentRefs, "Current Git HEAD");
  const expectedRefs = refsFrom(readFileSync(join(backupDirectory, "git-refs.txt"), "utf8"));
  const desiredHead = readGitHead(backupDirectory);
  assertGitHeadConsistency(desiredHead, expectedRefs, "Backup Git HEAD");
  const commits = readRequiredCommits(backupDirectory);
  const backupJobs = jobsIn(join(backupDirectory, "jobs"), projectId, "Backup");
  const currentJobs = jobsIn(join(dataRoot, "jobs"), projectId, "Current");
  const otherJobs = otherJobFingerprints(join(dataRoot, "jobs"), projectId, "Current");
  if (backupJobs.some((jobId) => otherJobs.has(jobId))) throw new Error("Backup Project job collides with an unrelated Project job");
  assertRegularContainedFile(backupDirectory, "nosh.sqlite", "Backup database");

  const staged: RestoreSwap[] = [];
  const stage = (source: string | null, destination: string): void => {
    if (staged.some((entry) => samePath(entry.destination, destination))) throw new Error(`Restore has duplicate destination: ${destination}`);
    staged.push({ destination, staged: source, rollback: null, movedRollback: false, movedStaged: false });
  };
  let refsMutated = false;
  let headMutated = false;
  let filesystemMutated = false;
  let rollbackEligible = true;
  try {
    for (const unit of backupUnits) {
      const destination = resolve(targetRoot, unit);
      if (!contained(targetRoot, destination)) throw new Error(`Restore path escapes allowed root: ${unit}`);
      assertNoLinkedAncestors(targetRoot, destination, `Restore destination ${unit}`);
      const source = join(backupDirectory, unit);
      if (!existsSync(source)) {
        if (existsSync(destination)) stage(null, destination);
        continue;
      }
      rejectSymlinks(source);
      const adjacent = adjacentPath(destination, "restore");
      mkdirSync(dirname(adjacent), { recursive: true });
      assertNoLinkedAncestors(targetRoot, adjacent, `Restore staging ${unit}`);
      cpSync(source, adjacent, { recursive: true, errorOnExist: true });
      stage(adjacent, destination);
    }
    for (const jobId of backupJobs) {
      const destination = resolve(dataRoot, "jobs", jobId);
      if (!contained(dataRoot, destination)) throw new Error(`Restore path escapes allowed root: jobs/${jobId}`);
      assertNoLinkedAncestors(dataRoot, destination, `Restore job destination ${jobId}`);
      const source = join(backupDirectory, "jobs", jobId);
      rejectSymlinks(source);
      const adjacent = adjacentPath(destination, "restore");
      mkdirSync(dirname(adjacent), { recursive: true });
      assertNoLinkedAncestors(dataRoot, adjacent, `Restore job staging ${jobId}`);
      cpSync(source, adjacent, { recursive: true, errorOnExist: true });
      stage(adjacent, destination);
    }
    for (const jobId of currentJobs) if (!backupJobs.includes(jobId)) stage(null, resolve(dataRoot, "jobs", jobId));
    for (const suffix of databaseSidecarSuffixes) {
      const sidecar = `${dbDestination}${suffix}`;
      if (!lstatSync(sidecar, { throwIfNoEntry: false })) continue;
      assertNoLinkedAncestors(dirname(dbDestination), sidecar, `Restore database sidecar ${suffix}`);
      stage(null, sidecar);
    }
    const dbAdjacent = adjacentPath(dbDestination, "restore-db");
    mkdirSync(dirname(dbAdjacent), { recursive: true });
    assertNoLinkedAncestors(dirname(dbDestination), dbAdjacent, "Restore database staging");
    cpSync(join(backupDirectory, "nosh.sqlite"), dbAdjacent, { errorOnExist: true });
    stage(dbAdjacent, dbDestination);

    if (manifest.gitBundle.hasRefs || desiredHead.commit !== null) importBundleObjects(targetRoot, join(backupDirectory, "git.bundle"));
    assertRequiredCommits(targetRoot, commits, "Required Git commits");
    for (const hash of expectedRefs.values()) if (!runGit(targetRoot, ["cat-file", "-e", `${hash}^{object}`]).ok) throw new Error(`Backup Git ref object is unavailable: ${hash}`);

    refsMutated = true;
    applyRefDiff(targetRoot, currentRefs, expectedRefs);
    options?.onPhase?.("after-refs");
    headMutated = true;
    materializeGitState(targetRoot, desiredHead, originalTrackedPaths);
    options?.onPhase?.("after-head-tree");
    filesystemMutated = true;
    for (const swap of staged) applyRestoreSwap(swap);
    options?.onPhase?.("after-filesystem-swap");
    verifyRestored(targetRoot, backupDirectory, manifest, expectedRefs, desiredHead, backupJobs, otherJobs, dataRoot, dbDestination, staged);
    options?.onPhase?.("before-success");
    options?.onCommit?.();
    rollbackEligible = false;
    try {
      cleanupSuccessfulRestore(staged);
    } catch (cleanupError) {
      throw new RestoreCommittedMaintenanceError(cleanupError);
    }
    return manifest;
  } catch (error) {
    if (!rollbackEligible) throw error;
    const rollbackFailures: unknown[] = [];
    const attemptRollback = (label: string, action: () => void): void => {
      try { action(); } catch (rollbackError) { rollbackFailures.push(new Error(`${label}: ${rollbackError instanceof Error ? rollbackError.message : String(rollbackError)}`)); }
    };
    if (filesystemMutated) attemptRollback("Filesystem rollback", () => rollbackRestoreSwaps(staged));
    if (headMutated) attemptRollback("Git HEAD/index/worktree rollback", () => materializeGitState(targetRoot, originalHead, trackedPaths(targetRoot)));
    if (refsMutated) attemptRollback("Git ref rollback", () => applyRefDiff(targetRoot, gitRefs(targetRoot, "Rollback Git ref snapshot"), currentRefs));
    attemptRollback("Restore staging cleanup", () => cleanupUnappliedRestoreStaging(staged));
    if (rollbackFailures.length) throw new AggregateError([error, ...rollbackFailures], "Restore failed and rollback encountered errors");
    throw error;
  }
}

export function copyProjectJobs(dataDirectory: string, destination: string, projectId: string): void {
  const jobsRoot = join(resolve(dataDirectory), "jobs");
  if (!existsSync(jobsRoot)) return;
  if (lstatSync(jobsRoot).isSymbolicLink()) throw new Error("Cannot back up jobs through a symlink");
  const matching = jobsIn(jobsRoot, projectId, "Cannot back up");
  if (!matching.length) return;
  mkdirSync(join(destination, "jobs"), { recursive: true });
  for (const jobId of matching) { const source = join(jobsRoot, jobId); rejectSymlinks(source); cpSync(source, join(destination, "jobs", jobId), { recursive: true, errorOnExist: true }); }
}

export function hashFiles(root: string): Array<{ path: string; sha256: string; bytes: number }> {
  const result: Array<{ path: string; sha256: string; bytes: number }> = [];
  const visit = (directory: string): void => {
    if (lstatSync(directory).isSymbolicLink()) throw new Error(`Symlinked backup path is forbidden: ${directory}`);
    for (const entry of readdirSync(directory, { withFileTypes: true })) {
      const path = join(directory, entry.name); const stat = lstatSync(path);
      if (stat.isSymbolicLink()) throw new Error(`Symlinked backup path is forbidden: ${path}`);
      if (stat.isDirectory()) visit(path);
      else if (stat.isFile()) { const bytes = readFileSync(path); result.push({ path: relative(root, path).replaceAll("\\", "/"), sha256: createHash("sha256").update(bytes).digest("hex"), bytes: bytes.length }); }
      else throw new Error(`Unsupported backup file type: ${path}`);
    }
  };
  visit(root);
  return result.sort((left, right) => left.path.localeCompare(right.path));
}

function readBackupManifest(root: string): BackupManifest {
  const manifestPath = join(root, "manifest.json");
  if (!existsSync(manifestPath)) throw new Error("Backup manifest.json is missing");
  const manifest = JSON.parse(readFileSync(manifestPath, "utf8")) as BackupManifest;
  if (manifest.format !== "nosh-backup-v1" || typeof manifest.createdAt !== "string" || !manifest.createdAt || typeof manifest.project?.projectId !== "string" || !manifest.project.projectId || typeof manifest.project.repositoryRoot !== "string" || !manifest.project.repositoryRoot || !Array.isArray(manifest.files) || !Number.isInteger(manifest.sqliteSchemaVersion) || typeof manifest.includesLargeArtifacts !== "boolean" || typeof manifest.gitBundle?.hasRefs !== "boolean" || (manifest.gitBundle.headCommit !== null && (typeof manifest.gitBundle.headCommit !== "string" || !safeGitObjectId(manifest.gitBundle.headCommit)))) throw new Error("Unsupported or malformed backup manifest");
  return manifest;
}

function jobsIn(root: string, projectId: string, label: string, excludedPaths: ReadonlySet<string> = new Set()): string[] {
  if (!existsSync(root)) return [];
  if (lstatSync(root).isSymbolicLink()) throw new Error(`${label} jobs root is symlinked`);
  const jobs: string[] = [];
  for (const entry of readdirSync(root, { withFileTypes: true })) {
    if (excludedPaths.has(resolve(root, entry.name))) continue;
    if (entry.isSymbolicLink()) throw new Error(`${label} Job ${entry.name}: symlinks are forbidden`);
    if (!entry.isDirectory()) continue;
    const recordPath = join(root, entry.name, "job.json");
    if (!existsSync(recordPath) || lstatSync(recordPath).isSymbolicLink()) throw new Error(`${label} Job ${entry.name}: missing or symlinked job.json`);
    let record: JobIdentity;
    try { record = JSON.parse(readFileSync(recordPath, "utf8")) as JobIdentity; } catch { throw new Error(`${label} Job ${entry.name}: malformed job.json`); }
    if (!record || typeof record.projectId !== "string") throw new Error(`${label} Job ${entry.name}: malformed job.json`);
    if (record.projectId === projectId) jobs.push(entry.name);
  }
  return jobs;
}

function copyIfPresent(sourceRoot: string, destinationRoot: string, unit: string): void {
  const source = join(sourceRoot, unit);
  assertNoLinkedAncestors(sourceRoot, source, "Backup source");
  const sourceStat = lstatSync(source, { throwIfNoEntry: false });
  if (!sourceStat) return;
  if (sourceStat.isSymbolicLink()) throw new Error(`Symlinked backup source is forbidden: ${source}`);
  const target = join(destinationRoot, unit);
  mkdirSync(dirname(target), { recursive: true });
  rejectSymlinks(source);
  cpSync(source, target, { recursive: true });
}

function verifyArtifactObjects(root: string): void {
  const manifestPath = join(root, ".nosh", "artifacts", "manifest.json");
  if (!existsSync(manifestPath)) return;
  assertNoLinkedAncestors(root, manifestPath, "Artifact manifest");
  const records = JSON.parse(readFileSync(manifestPath, "utf8")) as unknown;
  if (!Array.isArray(records)) throw new Error("Artifact manifest is malformed");
  const identities = new Set<string>();
  for (const record of records) {
    if (!record || typeof record !== "object" || Array.isArray(record)) throw new Error("Artifact manifest record is malformed");
    const artifact = record as { artifactId?: unknown; version?: unknown; storedPath?: unknown; contentHash?: unknown; sizeBytes?: unknown };
    const { artifactId, version, storedPath, contentHash, sizeBytes } = artifact;
    if (typeof artifactId !== "string" || typeof version !== "number" || !Number.isInteger(version) || version < 1 || typeof storedPath !== "string" || typeof contentHash !== "string" || !/^sha256:[a-f0-9]{64}$/.test(contentHash) || typeof sizeBytes !== "number" || !Number.isInteger(sizeBytes) || sizeBytes < 0) throw new Error("Artifact manifest record is malformed");
    const identity = `${artifactId}:${version}`; if (identities.has(identity)) throw new Error(`Ambiguous artifact version: ${identity}`); identities.add(identity);
    const artifactRoot = join(root, ".nosh", "artifacts");
    const path = isAbsolute(storedPath) ? resolve(storedPath) : resolve(artifactRoot, storedPath);
    if (!contained(artifactRoot, path)) throw new Error(`Artifact path escapes artifact root: ${artifactId}`);
    assertNoLinkedAncestors(root, path, `Artifact ${artifactId}`);
    if (!existsSync(path) || lstatSync(path).isSymbolicLink() || !statSync(path).isFile()) throw new Error(`Artifact object is missing or linked: ${artifactId}`);
    const bytes = readFileSync(path); const hash = `sha256:${createHash("sha256").update(bytes).digest("hex")}`;
    if (bytes.length !== sizeBytes || hash !== contentHash) throw new Error(`Artifact object integrity failed: ${artifactId}`);
  }
}

function contained(root: string, candidate: string): boolean {
  const relativePath = relative(resolve(root), resolve(candidate));
  return relativePath === "" || (relativePath !== ".." && !relativePath.startsWith(`..${sep}`) && !isAbsolute(relativePath));
}
function samePath(left: string, right: string): boolean { const normalize = (value: string): string => process.platform === "win32" ? resolve(value).toLowerCase() : resolve(value); return normalize(left) === normalize(right); }
function assertNoLinkedAncestors(allowedRoot: string, candidate: string, label: string): void {
  const root = resolve(allowedRoot);
  const target = resolve(candidate);
  if (!contained(root, target)) throw new Error(`${label} escapes its allowed root`);
  assertUnlinkedPathFromFilesystemRoot(root, label);
  let path = root;
  for (const segment of relative(root, target).split(sep).filter(Boolean)) {
    path = join(path, segment);
    if (!inspectUnlinked(path, label)) break;
  }
}
function assertUnlinkedPathFromFilesystemRoot(target: string, label: string): void {
  const parsed = parse(resolve(target));
  let path = parsed.root;
  if (!inspectUnlinked(path, label)) return;
  for (const segment of relative(parsed.root, resolve(target)).split(sep).filter(Boolean)) {
    path = join(path, segment);
    if (!inspectUnlinked(path, label)) break;
  }
}
function inspectUnlinked(path: string, label: string): boolean {
  const stat = lstatSync(path, { throwIfNoEntry: false });
  if (!stat) return false;
  if (stat.isSymbolicLink()) throw new Error(`${label} traverses a symlink or reparse point: ${path}`);
  return true;
}
function assertRegularContainedFile(root: string, name: string, label: string): string {
  const path = resolve(root, name);
  if (!contained(root, path) || !existsSync(path) || lstatSync(path).isSymbolicLink() || !statSync(path).isFile()) throw new Error(`${label} is missing, linked, or escapes backup root: ${name}`);
  return path;
}
function verifyRestored(targetRoot: string, backupRoot: string, manifest: BackupManifest, expectedRefs: Map<string, string>, expectedHead: GitHead, expectedJobs: string[], otherJobs: Map<string, string>, dataRoot: string, databasePath: string, swaps: RestoreSwap[]): void {
  const observedRefs = gitRefs(targetRoot, "Restored Git ref snapshot");
  if (!sameRefs(observedRefs, expectedRefs)) throw new Error("Restored Git refs do not match backup");
  const observedHead = captureGitHead(targetRoot);
  assertGitHeadConsistency(observedHead, observedRefs, "Restored Git HEAD");
  if (!sameGitHead(observedHead, expectedHead)) throw new Error("Restored Git HEAD does not match backup");
  verifyMaterializedGitTree(targetRoot, expectedHead, swaps.flatMap((swap) => [swap.staged, swap.rollback]).filter((path): path is string => path !== null && contained(targetRoot, path)));
  for (const unit of backupUnits) assertSameOptionalPathBytes(join(backupRoot, unit), join(targetRoot, unit), `Restored unit ${unit}`);
  // Ignore only rollback paths owned by this restore, not arbitrary hidden Jobs.
  const rollbackPaths = new Set(swaps.flatMap((swap) => swap.rollback ? [resolve(swap.rollback)] : []));
  const observedJobs = jobsIn(join(dataRoot, "jobs"), manifest.project.projectId, "Restored", rollbackPaths);
  if (!sameStrings(observedJobs, expectedJobs)) throw new Error("Restored Project job identifiers do not match backup");
  for (const jobId of expectedJobs) assertSamePathBytes(join(backupRoot, "jobs", jobId), join(dataRoot, "jobs", jobId), `Restored Project job ${jobId}`);
  if (!sameOtherJobFingerprints(otherJobFingerprints(join(dataRoot, "jobs"), manifest.project.projectId, "Restored", rollbackPaths), otherJobs)) throw new Error("Other Project jobs changed during restore");
  assertSamePathBytes(join(backupRoot, "nosh.sqlite"), databasePath, "Restored database");
  for (const suffix of databaseSidecarSuffixes) if (lstatSync(`${databasePath}${suffix}`, { throwIfNoEntry: false })) throw new Error(`Restored database sidecar ${suffix} is present`);
}
function assertGitClean(repositoryRoot: string, allowedUntrackedRoots: string[] = []): void {
  assertNoConfiguredGitFilters(repositoryRoot);
  const status = runGitStatus(repositoryRoot, ["status", "--porcelain=v1", "-z", "--untracked-files=all"]);
  if (!status.ok) throw new Error("Project repository is not a valid Git repository");
  for (const entry of splitNul(status.stdout)) {
    const path = entry.startsWith("?? ") ? resolve(repositoryRoot, entry.slice(3)) : null;
    if (path && allowedUntrackedRoots.some((root) => samePath(path, root) || contained(root, path))) continue;
    throw new Error("Project repository must be exactly clean, including untracked files");
  }
}
function assertNoConfiguredGitFilters(repositoryRoot: string): void {
  const filters = runGitResult(repositoryRoot, ["config", "--local", "--get-regexp", "^filter\\..*\\.(clean|smudge|process)$"], gitEnvironment());
  if (filters.status === 0 && filters.stdout.trim()) throw new Error("Project repository configures unsupported Git filter drivers");
  if (filters.status !== 0 && filters.status !== 1) throw new Error("Could not inspect Project Git filter configuration");
}
function trackedPaths(repositoryRoot: string): string[] {
  const tracked = runGit(repositoryRoot, ["ls-files", "-z"]);
  if (!tracked.ok) throw new Error("Could not capture tracked Git paths");
  return splitNul(tracked.stdout).map((path) => { checkedWorktreePath(repositoryRoot, path); return path.replaceAll("\\", "/"); }).sort();
}
function gitRefs(repositoryRoot: string, label: string): Map<string, string> {
  const result = runGit(repositoryRoot, ["for-each-ref", "--format=%(objectname) %(refname)"]);
  if (!result.ok) throw new Error(`${label} failed`);
  return refsFrom(result.stdout);
}
function assertRequiredCommits(repositoryRoot: string, commits: Iterable<string>, label: string): void {
  const required = [...commits];
  if (!required.length) return;
  const result = runGitInput(repositoryRoot, ["cat-file", "--batch-check=%(objectname) %(objecttype)"], `${required.join("\n")}\n`);
  const observed = result.stdout.split(/\r?\n/).filter(Boolean);
  if (!result.ok || observed.length !== required.length || observed.some((line, index) => line !== `${required[index]!} commit`)) throw new Error(`${label} are unavailable or have an invalid type`);
}
function readRequiredCommits(root: string): string[] {
  const commits = readFileSync(assertRegularContainedFile(root, "required-commits.txt", "Backup Git metadata"), "utf8").split(/\r?\n/).map((value) => value.trim()).filter(Boolean);
  if (new Set(commits).size !== commits.length) throw new Error("Backup required Git commits are duplicated");
  for (const commit of commits) if (!safeGitObjectId(commit)) throw new Error(`Malformed required Git commit: ${commit}`);
  return commits;
}
function otherJobFingerprints(root: string, projectId: string, label: string, excludedPaths: ReadonlySet<string> = new Set()): Map<string, string> {
  const fingerprints = new Map<string, string>();
  if (!existsSync(root)) return fingerprints;
  if (lstatSync(root).isSymbolicLink()) throw new Error(`${label} jobs root is symlinked`);
  for (const entry of readdirSync(root, { withFileTypes: true })) {
    if (excludedPaths.has(resolve(root, entry.name))) continue;
    if (entry.isSymbolicLink()) throw new Error(`${label} Job ${entry.name}: symlinks are forbidden`);
    if (!entry.isDirectory()) continue;
    const path = join(root, entry.name); const recordPath = join(path, "job.json");
    if (!existsSync(recordPath) || lstatSync(recordPath).isSymbolicLink()) throw new Error(`${label} Job ${entry.name}: missing or symlinked job.json`);
    let record: JobIdentity;
    try { record = JSON.parse(readFileSync(recordPath, "utf8")) as JobIdentity; } catch { throw new Error(`${label} Job ${entry.name}: malformed job.json`); }
    if (!record || typeof record.projectId !== "string") throw new Error(`${label} Job ${entry.name}: malformed job.json`);
    if (record.projectId !== projectId) fingerprints.set(entry.name, JSON.stringify(hashFiles(path)));
  }
  return fingerprints;
}
function sameOtherJobFingerprints(left: Map<string, string>, right: Map<string, string>): boolean {
  return [...left.keys()].sort().length === [...right.keys()].sort().length && [...left.keys()].sort().every((key) => left.get(key) === right.get(key));
}
function assertSameOptionalPathBytes(expected: string, observed: string, label: string): void {
  const expectedStat = lstatSync(expected, { throwIfNoEntry: false }); const observedStat = lstatSync(observed, { throwIfNoEntry: false });
  if (Boolean(expectedStat) !== Boolean(observedStat)) throw new Error(`${label} presence does not match backup`);
  if (expectedStat) assertSamePathBytes(expected, observed, label);
}
function assertSamePathBytes(expected: string, observed: string, label: string): void {
  const expectedStat = lstatSync(expected); const observedStat = lstatSync(observed);
  if (expectedStat.isSymbolicLink() || observedStat.isSymbolicLink()) throw new Error(`${label} is symlinked`);
  if (expectedStat.isFile() !== observedStat.isFile() || expectedStat.isDirectory() !== observedStat.isDirectory()) throw new Error(`${label} type does not match backup`);
  if (expectedStat.isFile()) {
    if (!readFileSync(expected).equals(readFileSync(observed))) throw new Error(`${label} bytes do not match backup`);
    return;
  }
  if (!expectedStat.isDirectory()) throw new Error(`${label} has an unsupported file type`);
  const expectedFiles = hashFiles(expected); const observedFiles = hashFiles(observed);
  if (expectedFiles.length !== observedFiles.length) throw new Error(`${label} file inventory does not match backup`);
  for (let index = 0; index < expectedFiles.length; index += 1) {
    const left = expectedFiles[index]!; const right = observedFiles[index]!;
    if (left.path !== right.path || left.bytes !== right.bytes || left.sha256 !== right.sha256 || !readFileSync(join(expected, left.path)).equals(readFileSync(join(observed, right.path)))) throw new Error(`${label} bytes do not match backup: ${left.path}`);
  }
}
function assertGitHeadConsistency(head: GitHead, refs: Map<string, string>, label: string): void {
  if (head.state === "symbolic") {
    if (!head.ref || !head.commit || refs.get(head.ref) !== head.commit) throw new Error(`${label} symbolic ref does not resolve to its recorded commit`);
    return;
  }
  if (head.state === "unborn") {
    if (!head.ref || head.commit !== null || refs.has(head.ref)) throw new Error(`${label} unborn ref is not absent`);
    return;
  }
  if (head.ref !== null || !head.commit) throw new Error(`${label} detached HEAD is malformed`);
}
function sameGitHead(left: GitHead, right: GitHead): boolean { return left.state === right.state && left.ref === right.ref && left.commit === right.commit; }
function sameRefs(left: Map<string, string>, right: Map<string, string>): boolean {
  const leftEntries = [...left.entries()].sort(([a], [b]) => a.localeCompare(b)); const rightEntries = [...right.entries()].sort(([a], [b]) => a.localeCompare(b));
  return leftEntries.length === rightEntries.length && leftEntries.every(([ref, hash], index) => ref === rightEntries[index]![0] && hash === rightEntries[index]![1]);
}
function sameStrings(left: string[], right: string[]): boolean { const a = [...left].sort(); const b = [...right].sort(); return a.length === b.length && a.every((value, index) => value === b[index]); }
function materializeGitState(repositoryRoot: string, head: GitHead, trackedBefore: string[]): void {
  if (head.state === "symbolic" || head.state === "unborn") {
    if (!runGit(repositoryRoot, ["symbolic-ref", "HEAD", head.ref!]).ok) throw new Error("Could not restore symbolic Git HEAD");
  } else {
    if (!runGit(repositoryRoot, ["update-ref", "--no-deref", "HEAD", head.commit!]).ok) throw new Error("Could not restore detached Git HEAD");
  }
  if (!head.commit) {
    if (!runGit(repositoryRoot, ["read-tree", "--empty"]).ok) throw new Error("Could not restore unborn Git index");
    removeTrackedWorktreePaths(repositoryRoot, trackedBefore);
    return;
  }
  materializeGitTree(repositoryRoot, head.commit, trackedBefore);
}
function materializeGitTree(repositoryRoot: string, commit: string, trackedBefore: string[]): void {
  const entries = gitTreeEntries(repositoryRoot, commit);
  removeTrackedWorktreePaths(repositoryRoot, trackedBefore);
  if (!runGit(repositoryRoot, ["read-tree", commit]).ok) throw new Error("Could not materialize Git index");
  for (const entry of entries) {
    const target = checkedWorktreePath(repositoryRoot, entry.path);
    mkdirSync(dirname(target), { recursive: true });
    assertNoLinkedAncestors(repositoryRoot, target, `Git tree path ${entry.path}`);
    if (lstatSync(target, { throwIfNoEntry: false })) rmSync(target, { recursive: true, force: true });
    writeFileSync(target, gitBlob(repositoryRoot, entry.objectId), { flag: "wx" });
    if (process.platform !== "win32") chmodSync(target, entry.mode === "100755" ? 0o755 : 0o644);
  }
}
function verifyMaterializedGitTree(repositoryRoot: string, head: GitHead, allowedUntrackedRoots: string[]): void {
  const expected = head.commit ? gitTreeEntries(repositoryRoot, head.commit) : [];
  const observed = gitIndexTreeEntries(repositoryRoot);
  if (observed.length !== expected.length || observed.some((entry, index) => entry.path !== expected[index]!.path || entry.mode !== expected[index]!.mode || entry.objectId !== expected[index]!.objectId)) throw new Error("Restored Git index does not match the selected tree");
  assertGitClean(repositoryRoot, allowedUntrackedRoots);
}
function gitIndexTreeEntries(repositoryRoot: string): Array<{ mode: "100644" | "100755"; objectId: string; path: string }> {
  const listed = runGit(repositoryRoot, ["ls-files", "--stage", "-z"]);
  if (!listed.ok) throw new Error("Could not enumerate restored Git index");
  const paths = new Set<string>();
  return splitNul(listed.stdout).map((line) => {
    const match = /^(100644|100755) ([0-9a-f]+) 0\t(.+)$/.exec(line);
    if (!match || !safeGitObjectId(match[2]!) || !checkedWorktreePath(repositoryRoot, match[3]!) || paths.has(match[3]!)) throw new Error("Restored Git index contains an unsupported or unsafe entry");
    paths.add(match[3]!);
    return { mode: match[1]! as "100644" | "100755", objectId: match[2]!, path: match[3]! };
  }).sort((left, right) => left.path.localeCompare(right.path));
}
function removeTrackedWorktreePaths(repositoryRoot: string, paths: string[]): void {
  for (const path of [...new Set(paths)].sort((left, right) => right.length - left.length || right.localeCompare(left))) {
    const target = checkedWorktreePath(repositoryRoot, path);
    if (lstatSync(target, { throwIfNoEntry: false })) {
      assertNoLinkedAncestors(repositoryRoot, target, `Git tracked path ${path}`);
      rmSync(target, { recursive: true, force: true });
    }
  }
}
function gitTreeEntries(repositoryRoot: string, commit: string): Array<{ mode: "100644" | "100755"; objectId: string; path: string }> {
  if (!safeGitObjectId(commit)) throw new Error("Git tree commit is malformed");
  const listed = runGit(repositoryRoot, ["ls-tree", "-r", "-z", "--full-tree", commit]);
  if (!listed.ok) throw new Error("Could not enumerate Git tree");
  const paths = new Set<string>();
  return splitNul(listed.stdout).map((line) => {
    const tab = line.indexOf("\t"); const [mode, kind, objectId] = line.slice(0, tab).split(" "); const path = line.slice(tab + 1);
    if (tab < 0 || kind !== "blob" || (mode !== "100644" && mode !== "100755") || !safeGitObjectId(objectId ?? "") || !checkedWorktreePath(repositoryRoot, path) || paths.has(path)) throw new Error("Git tree contains an unsupported or unsafe entry");
    paths.add(path);
    return { mode: mode as "100644" | "100755", objectId: objectId!, path };
  }).sort((left, right) => left.path.localeCompare(right.path));
}
function checkedWorktreePath(repositoryRoot: string, path: string): string {
  const normalized = path.replaceAll("\\", "/");
  if (!normalized || normalized.includes("\0") || normalized.includes(":") || /[\r\n]/.test(normalized) || normalized.startsWith("/") || normalized.split("/").some((part) => !part || part === "." || part === ".." || part.toLowerCase() === ".git")) throw new Error("Git worktree path is unsafe");
  const target = resolve(repositoryRoot, normalized);
  if (!contained(repositoryRoot, target)) throw new Error("Git worktree path escapes the repository");
  return target;
}
function gitBlob(repositoryRoot: string, objectId: string): Buffer {
  const result = runGitBytes(repositoryRoot, ["cat-file", "blob", objectId]);
  if (!result.ok) throw new Error(`Could not read Git blob ${objectId}`);
  return result.stdout;
}
function applyRefDiff(repositoryRoot: string, current: Map<string, string>, expected: Map<string, string>): void {
  const zero = zeroObjectId(repositoryRoot, [...current.values(), ...expected.values()]);
  const changes = [...new Set([...current.keys(), ...expected.keys()])].sort().flatMap((ref) => {
    const previous = current.get(ref); const next = expected.get(ref);
    if (previous === next) return [];
    return next ? [`update ${ref} ${next} ${previous ?? zero}`] : [`delete ${ref} ${previous!}`];
  });
  if (!changes.length) return;
  if (!runGitInput(repositoryRoot, ["update-ref", "--stdin"], `start\n${changes.join("\n")}\nprepare\ncommit\n`).ok) throw new Error("Could not atomically restore Git refs");
}
function zeroObjectId(repositoryRoot: string, objectIds: string[]): string {
  const lengths = new Set(objectIds.map((value) => value.length));
  if (lengths.size === 1) return "0".repeat([...lengths][0]!);
  if (lengths.size > 1) throw new Error("Git ref object formats are inconsistent");
  const format = runGit(repositoryRoot, ["rev-parse", "--show-object-format"]);
  if (!format.ok || !["sha1", "sha256"].includes(format.stdout.trim())) throw new Error("Could not determine Git object format");
  return "0".repeat(format.stdout.trim() === "sha256" ? 64 : 40);
}
function applyRestoreSwap(swap: RestoreSwap): void {
  assertUnlinkedPathFromFilesystemRoot(dirname(swap.destination), "Restore swap destination");
  const existing = lstatSync(swap.destination, { throwIfNoEntry: false });
  if (existing) {
    swap.rollback = adjacentPath(swap.destination, "rollback");
    renameSync(swap.destination, swap.rollback);
    swap.movedRollback = true;
  }
  if (swap.staged) {
    renameSync(swap.staged, swap.destination);
    swap.movedStaged = true;
  }
}
function rollbackRestoreSwaps(swaps: RestoreSwap[]): void {
  const failures: unknown[] = [];
  for (const swap of [...swaps].reverse()) try {
    if (swap.movedStaged && lstatSync(swap.destination, { throwIfNoEntry: false })) rmSync(swap.destination, { recursive: true, force: true });
    if (swap.movedRollback && swap.rollback && lstatSync(swap.rollback, { throwIfNoEntry: false })) renameSync(swap.rollback, swap.destination);
  } catch (error) { failures.push(error); }
  if (failures.length) throw new AggregateError(failures, "Filesystem rollback failed");
}
function cleanupSuccessfulRestore(swaps: RestoreSwap[]): void {
  const failures: unknown[] = [];
  for (const swap of swaps) for (const path of [swap.staged, swap.rollback]) if (path && lstatSync(path, { throwIfNoEntry: false })) try { rmSync(path, { recursive: true, force: true }); } catch (error) { failures.push(error); }
  if (failures.length) throw new AggregateError(failures, "Restore completed but could not remove rollback data");
}
function cleanupUnappliedRestoreStaging(swaps: RestoreSwap[]): void {
  const failures: unknown[] = [];
  for (const swap of swaps) if (swap.staged && lstatSync(swap.staged, { throwIfNoEntry: false })) try { rmSync(swap.staged, { recursive: true, force: true }); } catch (error) { failures.push(error); }
  if (failures.length) throw new AggregateError(failures, "Could not remove restore staging data");
}
function captureGitHead(repositoryRoot: string): GitHead {
  if (!runGit(repositoryRoot, ["rev-parse", "--git-dir"]).ok) throw new Error("Project repository is not a valid Git repository");
  const symbolic = runGit(repositoryRoot, ["symbolic-ref", "-q", "HEAD"]);
  const commitResult = runGit(repositoryRoot, ["rev-parse", "--verify", "HEAD"]);
  if (symbolic.ok) {
    const ref = symbolic.stdout.trim();
    if (!safeGitRef(ref)) throw new Error("Malformed symbolic Git HEAD");
    if (!commitResult.ok) return { version: 1, state: "unborn", ref, commit: null };
    const commit = commitResult.stdout.trim();
    if (!safeGitObjectId(commit)) throw new Error("Malformed symbolic Git HEAD commit");
    return { version: 1, state: "symbolic", ref, commit };
  }
  const commit = commitResult.stdout.trim();
  if (!commitResult.ok || !safeGitObjectId(commit)) throw new Error("Malformed detached Git HEAD");
  return { version: 1, state: "detached", ref: null, commit };
}
function readGitHead(root: string): GitHead {
  const path = join(root, "git-head.json");
  if (!contained(root, path) || !existsSync(path) || lstatSync(path).isSymbolicLink() || !statSync(path).isFile()) throw new Error("Backup Git HEAD metadata is missing, symlinked, or not a file");
  const value = JSON.parse(readFileSync(path, "utf8")) as Partial<GitHead>;
  if (value.version !== 1 || !["symbolic", "detached", "unborn"].includes(value.state ?? "") || (value.ref !== null && (typeof value.ref !== "string" || !safeGitRef(value.ref))) || (value.commit !== null && (typeof value.commit !== "string" || !safeGitObjectId(value.commit)))) throw new Error("Malformed Git HEAD metadata");
  if (value.state === "symbolic" && (typeof value.ref !== "string" || typeof value.commit !== "string") || value.state === "unborn" && (typeof value.ref !== "string" || value.commit !== null) || value.state === "detached" && (value.ref !== null || typeof value.commit !== "string")) throw new Error("Inconsistent Git HEAD metadata");
  return value as GitHead;
}
function refsFrom(input: string): Map<string, string> {
  const refs = new Map<string, string>();
  for (const line of input.split(/\r?\n/).map((value) => value.trim()).filter(Boolean)) {
    const [hash, ref, extra] = line.split(/\s+/);
    if (!hash || !ref || extra || !safeGitObjectId(hash) || !safeGitRef(ref) || refs.has(ref)) throw new Error(`Malformed Git ref: ${line}`);
    refs.set(ref, hash);
  }
  return refs;
}
function safeGitObjectId(value: string): boolean { return /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/.test(value); }
function safeGitRef(value: string): boolean { return /^refs\/[A-Za-z0-9][A-Za-z0-9._/-]*$/.test(value) && !value.includes("..") && !value.includes("//") && !value.endsWith("/") && !value.split("/").includes("."); }
function splitNul(value: string): string[] { return value.split("\0").filter(Boolean); }
function adjacentPath(destination: string, purpose: string): string { return join(dirname(destination), `.nosh-${purpose}-${basename(destination)}-${Date.now()}-${Math.random().toString(16).slice(2)}`); }
function runGit(repositoryRoot: string, args: string[]): { ok: boolean; stdout: string } {
  const result = runGitResult(repositoryRoot, args, gitEnvironment());
  return { ok: result.status === 0, stdout: result.stdout };
}
function runGitStatus(repositoryRoot: string, args: string[]): { ok: boolean; stdout: string } {
  const result = runGitResult(repositoryRoot, args, gitStatusEnvironment(repositoryRoot));
  return { ok: result.status === 0, stdout: result.stdout };
}
function runGitResult(repositoryRoot: string, args: string[], env: NodeJS.ProcessEnv): { status: number | null; stdout: string } {
  const result = spawnSync("git", ["-C", repositoryRoot, ...args], { encoding: "utf8", windowsHide: true, timeout: 60_000, maxBuffer: 16_000_000, env });
  return { status: result.status, stdout: String(result.stdout ?? "") };
}
function runGitBytes(repositoryRoot: string, args: string[]): { ok: boolean; stdout: Buffer } {
  const result = spawnSync("git", ["-C", repositoryRoot, ...args], { windowsHide: true, timeout: 60_000, maxBuffer: 64_000_000, env: gitEnvironment() });
  return { ok: result.status === 0, stdout: Buffer.isBuffer(result.stdout) ? result.stdout : Buffer.from(result.stdout ?? "") };
}
function runGitInput(repositoryRoot: string, args: string[], input: string): { ok: boolean; stdout: string } {
  const result = spawnSync("git", ["-C", repositoryRoot, ...args], { input, encoding: "utf8", windowsHide: true, timeout: 60_000, maxBuffer: 16_000_000, env: gitEnvironment() });
  return { ok: result.status === 0, stdout: String(result.stdout ?? "") };
}
function gitEnvironment(): NodeJS.ProcessEnv {
  const inherited = Object.fromEntries(Object.entries(process.env).filter(([key]) => !key.toUpperCase().startsWith("GIT_"))) as NodeJS.ProcessEnv;
  return gitEnvironmentWith(inherited, []);
}
function gitStatusEnvironment(repositoryRoot: string): NodeJS.ProcessEnv { return gitEnvironmentWith(gitProcessEnvironment(), safeGlobalCheckoutConfig(repositoryRoot)); }
function gitEnvironmentWith(inherited: NodeJS.ProcessEnv, safeCheckout: Array<[string, string]>): NodeJS.ProcessEnv {
  const nullDevice = process.platform === "win32" ? "NUL" : "/dev/null";
  const fixed: Array<[string, string]> = [["core.hooksPath", nullDevice], ["diff.external", ""], ["core.pager", "cat"], ["core.fsmonitor", "false"], ["commit.gpgSign", "false"], ["diff.renames", "false"], ["core.attributesFile", nullDevice]];
  return { ...inherited, GIT_CONFIG_NOSYSTEM: "1", GIT_CONFIG_GLOBAL: nullDevice, GIT_ATTR_NOSYSTEM: "1", GIT_OPTIONAL_LOCKS: "0", GIT_TERMINAL_PROMPT: "0", GIT_PAGER: "cat", GIT_EDITOR: "true", GIT_ASKPASS: "true", GIT_CONFIG_COUNT: String(fixed.length + safeCheckout.length), ...Object.fromEntries([...fixed, ...safeCheckout].flatMap(([key, value], index) => [[`GIT_CONFIG_KEY_${index}`, key], [`GIT_CONFIG_VALUE_${index}`, value]])) };
}
function gitProcessEnvironment(): NodeJS.ProcessEnv { return Object.fromEntries(Object.entries(process.env).filter(([key]) => !key.toUpperCase().startsWith("GIT_"))) as NodeJS.ProcessEnv; }
function safeGlobalCheckoutConfig(repositoryRoot: string): Array<[string, string]> {
  const result = runGitResult(repositoryRoot, ["config", "--global", "--get-regexp", "^core\\.(autocrlf|filemode|ignorecase|symlinks)$"], { ...gitProcessEnvironment(), GIT_CONFIG_NOSYSTEM: "1", GIT_TERMINAL_PROMPT: "0" });
  if (result.status === 1) return [];
  if (result.status !== 0) throw new Error("Could not read safe global Git checkout configuration");
  const values = new Map<string, string>();
  for (const line of result.stdout.split(/\r?\n/).filter(Boolean)) {
    const match = /^(core\.(?:autocrlf|filemode|ignorecase|symlinks))\s(.*)$/.exec(line);
    if (!match || /[\r\n]/.test(match[2]!)) throw new Error("Global Git checkout configuration is malformed");
    values.set(match[1]!, match[2]!);
  }
  return [...values.entries()];
}
function writeJsonAtomic(path: string, value: unknown): void { mkdirSync(dirname(path), { recursive: true }); const temporary = `${path}.tmp`; writeFileSync(temporary, `${JSON.stringify(value)}\n`, "utf8"); renameSync(temporary, path); }
function rejectSymlinks(root: string): void {
  if (lstatSync(root).isSymbolicLink()) throw new Error(`Symlinked backup source is forbidden: ${root}`);
  if (!statSync(root).isDirectory()) return;
  for (const entry of readdirSync(root, { withFileTypes: true })) rejectSymlinks(join(root, entry.name));
}
function verifyBundle(bundlePath: string, requiredCommits: Iterable<string> = []): void {
  const commits = [...requiredCommits];
  const objectFormat = bundleObjectFormat(bundlePath, commits);
  const verificationRepository = mkdtempSync(join(tmpdir(), "nosh-bundle-verify-"));
  try {
    if (!runGit(verificationRepository, ["init", "--bare", `--object-format=${objectFormat}`]).ok || !runGit(verificationRepository, ["bundle", "verify", resolve(bundlePath)]).ok) throw new Error("Backup Git bundle verification failed");
    if (commits.length) {
      if (!runGit(verificationRepository, ["bundle", "unbundle", resolve(bundlePath)]).ok) throw new Error("Backup Git bundle verification import failed");
      assertRequiredCommits(verificationRepository, commits, "Backup Git bundle commits");
    }
  } finally { rmSync(verificationRepository, { recursive: true, force: true }); }
}
function snapshotBundle(bundlePath: string): { refs: string; commits: string } {
  const heads = bundleHeads(bundlePath);
  verifyBundle(bundlePath, heads.map(([hash]) => hash));
  const listed = heads.flatMap(([hash, ref]) => ref === "HEAD" ? [] : [`${hash} ${ref}`]).join("\n");
  const refs = refsFrom(listed);
  const objectFormat = bundleObjectFormat(bundlePath, heads.map(([hash]) => hash));
  const snapshot = mkdtempSync(join(tmpdir(), "nosh-bundle-snapshot-"));
  try {
    if (!runGit(snapshot, ["init", "--bare", `--object-format=${objectFormat}`]).ok || !runGit(snapshot, ["bundle", "unbundle", resolve(bundlePath)]).ok) throw new Error("Backup Git bundle snapshot import failed");
    for (const [ref, hash] of refs) if (!runGit(snapshot, ["update-ref", ref, hash]).ok) throw new Error("Backup Git bundle ref materialization failed");
    const materializedRefs = runGit(snapshot, ["for-each-ref", "--format=%(objectname) %(refname)"]);
    const commits = runGit(snapshot, ["rev-list", "--all"]);
    if (!materializedRefs.ok || !commits.ok) throw new Error("Backup Git bundle snapshot enumeration failed");
    return { refs: materializedRefs.stdout, commits: commits.stdout };
  } finally { rmSync(snapshot, { recursive: true, force: true }); }
}
function bundleHeads(bundlePath: string): Array<[string, string]> {
  const heads = spawnSync("git", ["bundle", "list-heads", resolve(bundlePath)], { encoding: "utf8", windowsHide: true, timeout: 60_000, maxBuffer: 16_000_000, env: gitEnvironment() });
  if (heads.status !== 0) throw new Error("Backup Git bundle head listing failed");
  return String(heads.stdout ?? "").split(/\r?\n/).map((line) => line.trim()).filter(Boolean).map((line) => {
    const [hash, ref, extra] = line.split(/\s+/);
    if (!hash || !ref || extra || !safeGitObjectId(hash) || (ref !== "HEAD" && !safeGitRef(ref))) throw new Error("Backup Git bundle head is malformed");
    return [hash, ref];
  });
}
function bundleObjectFormat(bundlePath: string, objectIds: Iterable<string>): "sha1" | "sha256" {
  const lengths = new Set([...objectIds, ...bundleHeads(bundlePath).map(([hash]) => hash)].map((hash) => hash.length));
  if (lengths.size !== 1 || ![40, 64].includes([...lengths][0]!)) throw new Error("Backup Git bundle object format is missing or inconsistent");
  return [...lengths][0] === 64 ? "sha256" : "sha1";
}
function importBundleObjects(repositoryRoot: string, bundlePath: string): void {
  verifyBundle(bundlePath);
  if (!runGit(repositoryRoot, ["bundle", "unbundle", resolve(bundlePath)]).ok) throw new Error("Backup Git bundle import failed");
}
