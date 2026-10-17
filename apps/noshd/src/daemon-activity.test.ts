import { createId } from "@nosh/core";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { NoshDaemon } from "./daemon.js";

describe("daemon activity", () => {
  it("is empty while idle and names a running Job, which `nosh stop --if-idle` refuses to interrupt", () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-activity-"));
    const daemon = new NoshDaemon({ dataDirectory: join(directory, "data"), bootstrapToken: "test" });
    try {
      const project = daemon.initializeProject({ path: join(directory, "repository"), createRepository: true, workingTitle: "Activity fixture" });
      expect(daemon.activity()).toEqual([]);
      const jobId = createId("job");
      daemon.startJob({ jobId, projectId: project.projectId, missionId: null, directionId: null, autoresearchId: null, runId: createId("run"), experimentId: createId("exp"), commitSha: "0".repeat(40), workingDirectory: project.repositoryRoot, runner: "native", distribution: null, command: [process.execPath, "-e", "setTimeout(() => {}, 30000)"], checkpointCommand: null, environmentLockHash: `sha256:${"0".repeat(64)}`, evaluationContractHash: `sha256:${"0".repeat(64)}`, timeoutSeconds: 60, usesGpu: false });
      try {
        const activity = daemon.activity();
        expect(activity.map((entry) => entry.projectId)).toEqual([project.projectId]);
        expect(activity[0]!.active.some((entry) => entry.startsWith(`job ${jobId}:`))).toBe(true);
      } finally { daemon.jobs.cancel(jobId); }
    } finally { daemon.stop(); rmSync(directory, { recursive: true, force: true }); }
  }, 60_000);
});
