import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { applyScheduledRestores, createProjectBackup, restoreBackup, scheduleProjectRestore, verifyBackup } from "./backup.js";
import { openDatabase } from "./database.js";
import { HostRegistry, type RegisteredProject } from "./host-registry.js";

const projectId = "prj_backup_contract";

function git(cwd: string, args: string[]): string {
  return execFileSync("git", ["-C", cwd, ...args], { encoding: "utf8" }).trim();
}
function commitAll(repo: string, message = "fixture"): void {
  git(repo, ["add", "."]);
  git(repo, ["commit", "-m", message]);
}
async function createWorkspace() {
  const root = await mkdtemp(join(tmpdir(), "nosh-backup-test-"));
  const repo = join(root, "repo");
  mkdirSync(repo, { recursive: true });
  git(repo, ["init", "-b", "main"]);
  git(repo, ["config", "user.email", "test@nosh.invalid"]);
  git(repo, ["config", "user.name", "NOSH test"]);
  const data = join(root, "data");
  mkdirSync(data, { recursive: true });
  const databasePath = join(root, "project.sqlite");
  openDatabase(databasePath).close();
  const project: RegisteredProject = { projectId, repositoryRoot: repo, databasePath, registeredAt: "2026-01-01T00:00:00.000Z" };
  return { root, repo, data, databasePath, project };
}
function addUnits(repo: string): void {
  for (const unit of ["contracts", "events", "sessions"]) mkdirSync(join(repo, ".nosh", unit), { recursive: true });
  mkdirSync(join(repo, ".nosh", "artifacts", "aa"), { recursive: true });
  mkdirSync(join(repo, "docs", "figures"), { recursive: true });
  writeFileSync(join(repo, ".nosh", "project.json"), "{\"marker\":\"backup\"}\n");
  writeFileSync(join(repo, ".nosh", "schema-lock.json"), "{}\n");
  const bytes = Buffer.from("artifact bytes\n");
  const hash = createHash("sha256").update(bytes).digest("hex");
  writeFileSync(join(repo, ".nosh", "artifacts", "aa", hash + ".json"), bytes);
  writeFileSync(join(repo, ".nosh", "artifacts", "manifest.json"), JSON.stringify([{ artifactId: "art_fixture", projectId, kind: "result", mediaType: "application/json", contentHash: `sha256:${hash}`, sizeBytes: bytes.length, version: 1, sourcePath: "source.json", storedPath: `aa/${hash}.json`, retentionClass: "accepted_evidence", createdAt: "2026-01-01T00:00:00.000Z" }]) + "\n");
  writeFileSync(join(repo, "docs", "paper.md"), "# Backup paper\n");
  writeFileSync(join(repo, "docs", "paper.bib"), "@article{backup}\n");
  writeFileSync(join(repo, "docs", "figures", "figure.txt"), "figure\n");
}
function addJob(data: string, name: string, id: string): void {
  mkdirSync(join(data, "jobs", name), { recursive: true });
  writeFileSync(join(data, "jobs", name, "job.json"), JSON.stringify({ projectId: id }));
}
function cleanup(root: string): void { rmSync(root, { recursive: true, force: true }); }
function assertNoRestoreDebris(root: string): void {
  if (!existsSync(root)) return;
  for (const entry of readdirSync(root, { withFileTypes: true })) {
    expect(entry.name).not.toMatch(/^\.nosh-(restore|rollback|restore-db)-/);
    if (entry.isDirectory()) assertNoRestoreDebris(join(root, entry.name));
  }
}

describe("project backup contracts", () => {
  it("refuses tracked dirt and then independently refuses untracked files", async () => {
    const env = await createWorkspace();
    try {
      writeFileSync(join(env.repo, "tracked.txt"), "clean\n");
      commitAll(env.repo);
      writeFileSync(join(env.repo, "tracked.txt"), "dirty\n");
      await expect(createProjectBackup(env.data, env.project)).rejects.toThrow(/clean|dirty/i);
      git(env.repo, ["checkout", "--", "tracked.txt"]);
      writeFileSync(join(env.repo, "untracked.txt"), "untracked\n");
      await expect(createProjectBackup(env.data, env.project)).rejects.toThrow(/clean|untracked/i);
      expect(existsSync(join(env.data, "backups"))).toBe(false);
    } finally { cleanup(env.root); }
  });

  it("backs up all project units, one valid artifact object, and matching jobs", async () => {
    const env = await createWorkspace();
    try {
      addUnits(env.repo); commitAll(env.repo);
      addJob(env.data, "job-match", projectId); addJob(env.data, "job-other", "prj_other");
      const summary = await createProjectBackup(env.data, env.project);
      expect(verifyBackup(summary.path).includesLargeArtifacts).toBe(true);
      expect(readFileSync(join(summary.path, "docs", "paper.md"), "utf8")).toContain("Backup paper");
      expect(existsSync(join(summary.path, "jobs", "job-match"))).toBe(true);
      expect(existsSync(join(summary.path, "jobs", "job-other"))).toBe(false);
      const manifest = JSON.parse(readFileSync(join(summary.path, ".nosh", "artifacts", "manifest.json"), "utf8")) as Array<{ storedPath: string; contentHash: string; sizeBytes: number }>;
      const object = readFileSync(join(summary.path, ".nosh", "artifacts", manifest[0]!.storedPath));
      expect(object.length).toBe(manifest[0]!.sizeBytes);
      expect(`sha256:${createHash("sha256").update(object).digest("hex")}`).toBe(manifest[0]!.contentHash);
    } finally { cleanup(env.root); }
  });

  it("records detached HEAD exactly", async () => {
    const env = await createWorkspace();
    try { writeFileSync(join(env.repo, "tracked.txt"), "one\n"); commitAll(env.repo); const head = git(env.repo, ["rev-parse", "HEAD"]); git(env.repo, ["checkout", "--detach", head]); const summary = await createProjectBackup(env.data, env.project); expect(JSON.parse(readFileSync(join(summary.path, "git-head.json"), "utf8"))).toMatchObject({ state: "detached", commit: head }); } finally { cleanup(env.root); }
  });

  it("records an unborn repository without fabricating refs or a bundle", async () => {
    const env = await createWorkspace();
    try { const summary = await createProjectBackup(env.data, env.project); expect(JSON.parse(readFileSync(join(summary.path, "git-head.json"), "utf8"))).toEqual({ version: 1, state: "unborn", ref: "refs/heads/main", commit: null }); expect(existsSync(join(summary.path, "git.bundle"))).toBe(false); } finally { cleanup(env.root); }
  });

  it("rejects identity mismatch, daemon-running restore, and dirty target before import", async () => {
    const env = await createWorkspace();
    try { writeFileSync(join(env.repo, "tracked.txt"), "one\n"); commitAll(env.repo); const summary = await createProjectBackup(env.data, env.project); expect(() => restoreBackup(env.repo, env.data, summary.path, "prj_other", env.databasePath, true)).toThrow(/different Project/i); expect(() => restoreBackup(env.repo, env.data, summary.path, projectId, env.databasePath, false)).toThrow(/daemon/i); writeFileSync(join(env.repo, "dirty.txt"), "dirty\n"); expect(() => restoreBackup(env.repo, env.data, summary.path, projectId, env.databasePath, true)).toThrow(/clean|dirty|untracked/i); } finally { cleanup(env.root); }
  });

  it("rejects corrupted manifest before scheduling or changing the target", async () => {
    const env = await createWorkspace();
    try { writeFileSync(join(env.repo, "tracked.txt"), "one\n"); commitAll(env.repo); const summary = await createProjectBackup(env.data, env.project); writeFileSync(join(summary.path, "manifest.json"), "{}\n"); expect(() => verifyBackup(summary.path)).toThrow(); expect(() => scheduleProjectRestore(env.data, env.project, summary.backupId)).toThrow(); } finally { cleanup(env.root); }
  });
  it("removes a committed restore plan without replaying its restore", async () => {
    const env = await createWorkspace();
    try {
      writeFileSync(join(env.repo, "tracked.txt"), "before\n"); commitAll(env.repo);
      const summary = await createProjectBackup(env.data, env.project);
      const registry = new HostRegistry(join(env.data, "host.sqlite"));
      try { registry.register(env.project); } finally { registry.close(); }
      scheduleProjectRestore(env.data, env.project, summary.backupId);
      const planPath = join(env.data, "restore-plans", `${projectId}.json`);
      const plan = JSON.parse(readFileSync(planPath, "utf8")) as Record<string, unknown>;
      writeFileSync(planPath, `${JSON.stringify({ ...plan, state: "committed" })}\n`, "utf8");
      writeFileSync(join(env.repo, "tracked.txt"), "after\n"); commitAll(env.repo, "after");
      const changedHead = git(env.repo, ["rev-parse", "HEAD"]);
      expect(applyScheduledRestores(env.data)).toBe(1);
      expect(existsSync(planPath)).toBe(false);
      expect(git(env.repo, ["rev-parse", "HEAD"])).toBe(changedHead);
    } finally { cleanup(env.root); }
  });
  it("restores the backed-up repository, database, artifacts, paper, and matching jobs exactly", async () => {
    const env = await createWorkspace();
    try {
      addUnits(env.repo);
      writeFileSync(join(env.repo, "tracked.txt"), "before\n");
      commitAll(env.repo);
      const initialHead = git(env.repo, ["rev-parse", "HEAD"]);
      const initialRef = git(env.repo, ["symbolic-ref", "--quiet", "HEAD"]);
      const initialPaper = readFileSync(join(env.repo, "docs", "paper.md"));
      const initialProject = readFileSync(join(env.repo, ".nosh", "project.json"));
      const initialFigure = readFileSync(join(env.repo, "docs", "figures", "figure.txt"));
      const initialDb = openDatabase(env.databasePath);
      initialDb.prepare("INSERT INTO project_sequences (project_id, last_sequence) VALUES (?, ?)").run(projectId, 7);
      initialDb.close();
      addJob(env.data, "job-match", projectId); addJob(env.data, "job-other", "prj_other");
      writeFileSync(join(env.data, "jobs", "job-match", "marker.txt"), "before\n");
      writeFileSync(join(env.data, "jobs", "job-other", "marker.txt"), "keep\n");
      const summary = await createProjectBackup(env.data, env.project);
      const artifactManifest = readFileSync(join(summary.path, ".nosh", "artifacts", "manifest.json"));
      const manifest = JSON.parse(artifactManifest.toString("utf8")) as Array<{ storedPath: string }>;
      const artifactObject = readFileSync(join(summary.path, ".nosh", "artifacts", manifest[0]!.storedPath));
      writeFileSync(join(env.repo, "docs", "paper.md"), "# Changed\n");
      writeFileSync(join(env.repo, ".nosh", "project.json"), "{\"marker\":\"changed\"}\n");
      commitAll(env.repo, "changed");
      const changedDb = openDatabase(env.databasePath);
      changedDb.prepare("UPDATE project_sequences SET last_sequence = ? WHERE project_id = ?").run(99, projectId);
      changedDb.close();
      writeFileSync(join(env.data, "jobs", "job-match", "marker.txt"), "changed\n");
      expect(git(env.repo, ["status", "--porcelain"])).toBe("");
      restoreBackup(env.repo, env.data, summary.path, projectId, env.databasePath, true);
      expect(git(env.repo, ["symbolic-ref", "--quiet", "HEAD"])).toBe(initialRef);
      expect(git(env.repo, ["rev-parse", "HEAD"])).toBe(initialHead);
      expect(git(env.repo, ["status", "--porcelain"])).toBe("");
      expect(readFileSync(join(env.repo, "docs", "paper.md"))).toEqual(initialPaper);
      expect(readFileSync(join(env.repo, ".nosh", "project.json"))).toEqual(initialProject);
      expect(readFileSync(join(env.repo, "docs", "figures", "figure.txt"))).toEqual(initialFigure);
      expect(readFileSync(join(env.repo, ".nosh", "artifacts", manifest[0]!.storedPath)).equals(artifactObject)).toBe(true);
      expect(readFileSync(join(env.repo, ".nosh", "artifacts", "manifest.json")).equals(artifactManifest)).toBe(true);
      const restoredDb = openDatabase(env.databasePath); expect(restoredDb.prepare("SELECT last_sequence FROM project_sequences WHERE project_id = ?").get(projectId)).toEqual({ last_sequence: 7 }); restoredDb.close();
      expect(readFileSync(join(env.data, "jobs", "job-match", "marker.txt"), "utf8")).toBe("before\n");
      expect(readFileSync(join(env.data, "jobs", "job-other", "marker.txt"), "utf8")).toBe("keep\n");
    } finally { cleanup(env.root); }
  });
  it("restores a detached unique HEAD commit", async () => {
    const env = await createWorkspace();
    try {
      writeFileSync(join(env.repo, "tracked.txt"), "base\n"); commitAll(env.repo);
      const base = git(env.repo, ["rev-parse", "HEAD"]); git(env.repo, ["checkout", "--detach", base]); writeFileSync(join(env.repo, "tracked.txt"), "unique\n"); commitAll(env.repo, "unique");
      const unique = git(env.repo, ["rev-parse", "HEAD"]);
      const summary = await createProjectBackup(env.data, env.project);
      writeFileSync(join(env.repo, "tracked.txt"), "later\n"); commitAll(env.repo, "later");
      restoreBackup(env.repo, env.data, summary.path, projectId, env.databasePath, true);
      expect(() => git(env.repo, ["symbolic-ref", "--quiet", "HEAD"])).toThrow(); expect(git(env.repo, ["rev-parse", "HEAD"])).toBe(unique); expect(git(env.repo, ["status", "--porcelain", "--untracked-files=all"])).toBe(""); expect(readFileSync(join(env.repo, "tracked.txt"), "utf8")).toBe("unique\n");
    } finally { cleanup(env.root); }
  });

  it("restores an unborn HEAD over a later commit without deleting unrelated state", async () => {
    const env = await createWorkspace();
    try {
      const summary = await createProjectBackup(env.data, env.project);
      writeFileSync(join(env.repo, "later.txt"), "later\n"); commitAll(env.repo);
      restoreBackup(env.repo, env.data, summary.path, projectId, env.databasePath, true);
      expect(git(env.repo, ["symbolic-ref", "--quiet", "HEAD"])).toBe("refs/heads/main"); expect(() => git(env.repo, ["rev-parse", "--verify", "HEAD"])).toThrow(); expect(existsSync(join(env.repo, "later.txt"))).toBe(false); expect(git(env.repo, ["status", "--porcelain", "--untracked-files=all"])).toBe("");
    } finally { cleanup(env.root); }
  });
  it.each(["after-filesystem-swap", "after-refs", "after-head-tree", "before-success", "on-commit"])("rolls back every reversible restore phase: %s", async (phase) => {
    const env = await createWorkspace();
    try {
      addUnits(env.repo); writeFileSync(join(env.repo, "tracked.txt"), "before\n"); commitAll(env.repo); addJob(env.data, "job-match", projectId); addJob(env.data, "job-other", "prj_other");
      const db = openDatabase(env.databasePath); db.prepare("INSERT INTO project_sequences (project_id, last_sequence) VALUES (?, ?)").run(projectId, 7); db.close();
      const summary = await createProjectBackup(env.data, env.project);
      writeFileSync(join(env.repo, "tracked.txt"), "after\n"); writeFileSync(join(env.repo, "docs", "paper.md"), "# Changed paper\n"); commitAll(env.repo, "after"); const changedDb = openDatabase(env.databasePath); changedDb.prepare("UPDATE project_sequences SET last_sequence = 99 WHERE project_id = ?").run(projectId); changedDb.close(); writeFileSync(join(env.data, "jobs", "job-match", "marker.txt"), "after\n");
      const originalHead = git(env.repo, ["rev-parse", "HEAD"]); const originalRef = git(env.repo, ["symbolic-ref", "--quiet", "HEAD"]); const originalRefs = git(env.repo, ["for-each-ref", "--format=%(objectname) %(refname)"]); const originalDb = readFileSync(env.databasePath); const originalJob = readFileSync(join(env.data, "jobs", "job-match", "marker.txt")); const originalPaper = readFileSync(join(env.repo, "docs", "paper.md"));
      const options = phase === "on-commit" ? { onCommit: () => { throw new Error("injected"); } } : { onPhase: (current: string) => { if (current === phase) throw new Error("injected"); } };
      expect(() => restoreBackup(env.repo, env.data, summary.path, projectId, env.databasePath, true, options)).toThrow("injected");
      expect(git(env.repo, ["rev-parse", "HEAD"])).toBe(originalHead); expect(git(env.repo, ["symbolic-ref", "--quiet", "HEAD"])).toBe(originalRef); expect(git(env.repo, ["for-each-ref", "--format=%(objectname) %(refname)"])).toBe(originalRefs); expect(git(env.repo, ["status", "--porcelain", "--untracked-files=all"])).toBe(""); expect(readFileSync(join(env.repo, "tracked.txt"), "utf8")).toBe("after\n"); expect(readFileSync(join(env.repo, "docs", "paper.md"))).toEqual(originalPaper); expect(readFileSync(env.databasePath)).toEqual(originalDb); expect(readFileSync(join(env.data, "jobs", "job-match", "marker.txt"))).toEqual(originalJob); expect(existsSync(join(env.data, "jobs", "job-other"))).toBe(true); assertNoRestoreDebris(env.root);
    } finally { cleanup(env.root); }
  });
});
