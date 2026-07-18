import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { JobSupervisor, type JobSpec } from "./index.js";

function spec(directory: string, jobId = "job_test1"): JobSpec {
  return { jobId, projectId: "prj_test", missionId: null, directionId: null, autoresearchId: null, runId: "run_test", experimentId: "exp_test", commitSha: "1234567", workingDirectory: directory, runner: "native", distribution: null, command: [process.execPath, "-e", "console.log('started'); setTimeout(() => console.log('done'), 2000)"], checkpointCommand: null, environmentLockHash: "sha256:test", evaluationContractHash: "sha256:test", timeoutSeconds: 10, usesGpu: false };
}

describe("JobSupervisor", () => {
  it("persists process identity and append-only logs independently of the caller", async () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-jobs-"));
    try {
      const first = new JobSupervisor(directory);
      const started = first.start(spec(directory));
      expect(started.state).toBe("running");
      expect(first.start(spec(directory)).windowsPid).toBe(started.windowsPid);
      expect(() => first.start({ ...spec(directory), command: [process.execPath, "-e", "process.exit(2)"] })).toThrow("different specification");
      first.close();
      const recovered = new JobSupervisor(directory).recover()[0];
      expect(recovered?.recoveredAfterRestart).toBe(true);
      await new Promise((resolve) => setTimeout(resolve, 100));
      const second = new JobSupervisor(directory);
      expect(second.tail("job_test1", "stdout")).toContain("started");
      second.cancel("job_test1");
      await new Promise((resolve) => setTimeout(resolve, 500));
    } finally { rmSync(directory, { recursive: true, force: true, maxRetries: 5, retryDelay: 100 }); }
  });
});
