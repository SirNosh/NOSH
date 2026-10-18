import { createId } from "@nosh/core";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { NoshDaemon } from "./daemon.js";

describe("daemon job launch", () => {
  it("rejects malformed scope IDs before persisting a Job that could never leave starting", () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-job-scope-"));
    const daemon = new NoshDaemon({ dataDirectory: join(directory, "data"), bootstrapToken: "test" });
    try {
      const project = daemon.initializeProject({ path: join(directory, "repository"), createRepository: true, workingTitle: "Job fixture" });
      const spec = { jobId: createId("job"), projectId: project.projectId, missionId: null, directionId: null, autoresearchId: null, runId: "run_live", experimentId: "exp_live", commitSha: "0".repeat(40), workingDirectory: project.repositoryRoot, runner: "native" as const, distribution: null, command: [process.execPath, "-e", "0"], checkpointCommand: null, environmentLockHash: `sha256:${"0".repeat(64)}`, evaluationContractHash: `sha256:${"0".repeat(64)}`, timeoutSeconds: 30, usesGpu: false };
      expect(() => daemon.startJob(spec)).toThrow();
      expect(daemon.jobs.list().filter((job) => job.jobId === spec.jobId)).toEqual([]);
    } finally { daemon.stop(); rmSync(directory, { recursive: true, force: true }); }
  });
});
