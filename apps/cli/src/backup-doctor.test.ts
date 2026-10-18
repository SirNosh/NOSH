import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { copyProjectJobs, createProjectBackup, hashFiles, EventStore, restoreBackup, verifyBackup } from "@nosh/persistence";
import { gitDoctorChecks } from "./backup-doctor.js";

describe("CLI backup and doctor helpers", () => {
  it("copies only selected daemon Jobs and hashes their records and logs", () => {
    const root = mkdtempSync(join(tmpdir(), "nosh-cli-backup-"));
    try {
      const dataDirectory = join(root, "data");
      const destination = join(root, "backup");
      const selectedJob = join(dataDirectory, "jobs", "job_selected");
      const otherJob = join(dataDirectory, "jobs", "job_other");
      mkdirSync(selectedJob, { recursive: true }); mkdirSync(otherJob, { recursive: true });
      writeFileSync(join(selectedJob, "job.json"), JSON.stringify({ jobId: "job_selected", projectId: "prj_selected", state: "completed" }), "utf8");
      writeFileSync(join(selectedJob, "stdout.log"), "selected stdout\n", "utf8"); writeFileSync(join(selectedJob, "stderr.log"), "selected stderr\n", "utf8");
      writeFileSync(join(otherJob, "job.json"), JSON.stringify({ jobId: "job_other", projectId: "prj_other", state: "completed" }), "utf8");
      writeFileSync(join(otherJob, "stdout.log"), "other stdout\n", "utf8"); writeFileSync(join(otherJob, "stderr.log"), "other stderr\n", "utf8");

      copyProjectJobs(dataDirectory, destination, "prj_selected");

      for (const name of ["job.json", "stdout.log", "stderr.log"]) expect(existsSync(join(destination, "jobs", "job_selected", name))).toBe(true);
      expect(existsSync(join(destination, "jobs", "job_other"))).toBe(false);
      const hashes = hashFiles(destination);
      for (const name of ["job.json", "stdout.log", "stderr.log"]) {
        const path = join(destination, "jobs", "job_selected", name);
        const bytes = readFileSync(path);
        expect(hashes).toContainEqual({ path: `jobs/job_selected/${name}`, sha256: createHash("sha256").update(bytes).digest("hex"), bytes: bytes.length });
      }
    } finally { rmSync(root, { recursive: true, force: true }); }
  });

  it("fails loudly for malformed Job data", () => {
    const root = mkdtempSync(join(tmpdir(), "nosh-cli-backup-invalid-"));
    try {
      const job = join(root, "data", "jobs", "job_invalid"); mkdirSync(job, { recursive: true }); writeFileSync(join(job, "job.json"), "{", "utf8");
      expect(() => copyProjectJobs(join(root, "data"), join(root, "backup"), "prj_selected")).toThrow("malformed job.json");
    } finally { rmSync(root, { recursive: true, force: true }); }
  });

  it("checks Git first and never uses the caller cwd as a worktree root", () => {
    const calls: Array<[string, string[]]> = [];
    const run = (command: string, args: string[]) => { calls.push([command, args]); return { ok: true, stdout: "git version 2.50.0\n" }; };
    expect(gitDoctorChecks(undefined, run)[1]).toEqual(["Git worktrees", true, "not checked; no current Project"]);
    expect(calls).toEqual([["git", ["--version"]]]);

    const repositoryRoot = join(tmpdir(), "registered-project-root"); calls.length = 0;
    const checks = gitDoctorChecks(repositoryRoot, (command, args) => { calls.push([command, args]); return { ok: true, stdout: command === "git" && args[0] === "--version" ? "git version 2.50.0\n" : `${repositoryRoot} abc\n` }; });
    expect(calls).toEqual([["git", ["--version"]], ["git", ["-C", repositoryRoot, "worktree", "list"]]]);
    expect(checks[1]).toEqual(["Git worktrees", true, "1 available"]);
  });

  it("verifies content-addressed artifacts and restores project/data state", async () => {
    const root = mkdtempSync(join(tmpdir(), "nosh-cli-restore-"));
    try {
      const project = join(root, "project"); const data = join(root, "data"); const databasePath = join(data, "registered.sqlite");
      mkdirSync(join(project, ".nosh", "artifacts", "objects"), { recursive: true }); mkdirSync(data, { recursive: true });
      const git = (...args: string[]) => execFileSync("git", ["-C", project, ...args], { encoding: "utf8" });
      git("init", "-b", "main"); git("config", "user.email", "test@nosh.invalid"); git("config", "user.name", "NOSH test");
      writeFileSync(join(project, ".gitignore"), ".nosh/\n"); git("add", "."); git("commit", "-m", "fixture");
      const artifact = Buffer.from("artifact payload"); const digest = createHash("sha256").update(artifact).digest("hex");
      writeFileSync(join(project, ".nosh", "artifacts", "objects", digest), artifact);
      writeFileSync(join(project, ".nosh", "artifacts", "manifest.json"), JSON.stringify([{ artifactId: "art_fixture", version: 1, storedPath: `objects/${digest}`, contentHash: `sha256:${digest}`, sizeBytes: artifact.length }]));
      writeFileSync(join(project, ".nosh", "project.json"), "restored");
      new EventStore(databasePath).close();
      mkdirSync(join(data, "jobs", "selected"), { recursive: true }); writeFileSync(join(data, "jobs", "selected", "job.json"), JSON.stringify({ projectId: "prj", state: "completed" }));
      mkdirSync(join(data, "jobs", "other"), { recursive: true }); writeFileSync(join(data, "jobs", "other", "job.json"), JSON.stringify({ projectId: "other" }));
      const { path: backup } = await createProjectBackup(data, { projectId: "prj", repositoryRoot: project, databasePath, registeredAt: new Date().toISOString() });
      expect(verifyBackup(backup).project.projectId).toBe("prj");
      writeFileSync(join(backup, ".nosh", "artifacts", "objects", digest), "tampered");
      expect(() => verifyBackup(backup)).toThrow("integrity check failed");
      writeFileSync(join(backup, ".nosh", "artifacts", "objects", digest), artifact);
      const extra = join(project, ".nosh", "artifacts", "objects", "a".repeat(64)); writeFileSync(extra, "extra");
      writeFileSync(join(project, ".nosh", "project.json"), "mutated");
      mkdirSync(join(data, "jobs", "old"), { recursive: true }); writeFileSync(join(data, "jobs", "old", "job.json"), JSON.stringify({ projectId: "prj" }));
      expect(() => restoreBackup(project, data, backup, "prj", databasePath, false)).toThrow("while noshd is running");
      expect(readFileSync(join(project, ".nosh", "project.json"), "utf8")).toBe("mutated");
      restoreBackup(project, data, backup, "prj", databasePath, true);
      expect(readFileSync(join(data, "jobs", "selected", "job.json"), "utf8")).toContain('"state":"completed"');
      expect(existsSync(join(data, "jobs", "old"))).toBe(false);
      expect(existsSync(join(data, "jobs", "other"))).toBe(true);
      expect(existsSync(extra)).toBe(false);
      expect(readFileSync(join(project, ".nosh", "project.json"), "utf8")).toBe("restored");
    } finally { rmSync(root, { recursive: true, force: true }); }
  });
});
