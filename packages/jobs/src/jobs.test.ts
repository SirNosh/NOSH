import { mkdtempSync, rmSync, writeFileSync, openSync, closeSync, ftruncateSync, writeSync } from "node:fs";
import { EventEmitter, once } from "node:events";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { JobSupervisor, type JobSpec } from "./index.js";

function spec(directory: string, jobId = "job_test1"): JobSpec {
  return { jobId, projectId: "prj_test", missionId: null, directionId: null, autoresearchId: null, runId: "run_test", experimentId: "exp_test", commitSha: "1234567", workingDirectory: directory, runner: "native", distribution: null, command: [process.execPath, "-e", "console.log('started'); setTimeout(() => console.log('done'), 2000)"], checkpointCommand: null, environmentLockHash: "sha256:test", evaluationContractHash: "sha256:test", timeoutSeconds: 10, usesGpu: false };
}

describe("JobSupervisor", () => {
  it("reads only a bounded tail from large sparse files and coalesces telemetry", async () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-tail-"));
    const jobs = new JobSupervisor(directory);
    try {
      const record = jobs.start({ ...spec(directory), command: ["nosh-definitely-missing-binary"] });
      await new Promise((resolve) => setImmediate(resolve));
      const fd = openSync(record.stdoutPath, "w");
      try { ftruncateSync(fd, 128 * 1024 * 1024); writeSync(fd, Buffer.from("last bytes"), 0, 10, 128 * 1024 * 1024 - 10); } finally { closeSync(fd); }
      expect(jobs.tail(record.jobId, "stdout", 10)).toBe("last bytes");
      expect(jobs.tail(record.jobId, "stdout", 0)).toBe("");
      expect(() => jobs.tail(record.jobId, "stdout", 1024 * 1024 + 1)).toThrow();
      writeFileSync(record.stdoutPath, "short");
      expect(jobs.tail(record.jobId, "stdout", 10)).toBe("short");
      const first = jobs.resourceSnapshot(record.jobId);
      expect(jobs.resourceSnapshot(record.jobId)).toBe(first);
      expect(await first).toMatchObject({ residentBytes: null, gpu: null });
    } finally { jobs.close(); rmSync(directory, { recursive: true, force: true }); }
  });
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

  it("persists asynchronous native launch failures as terminal failed jobs", async () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-jobs-launch-failure-"));
    const events = new EventEmitter();
    try {
      const jobs = new JobSupervisor(directory, (job) => { if (job.state === "failed") events.emit("failed", job); });
      const failure = once(events, "failed");
      const started = jobs.start({ ...spec(directory), jobId: "job_missingbinary", command: ["nosh-definitely-missing-binary"] });
      if (started.state !== "failed") await failure;
      expect(jobs.get("job_missingbinary")).toMatchObject({ state: "failed", failureReason: expect.any(String) });
      jobs.close();
    } finally {
      rmSync(directory, { recursive: true, force: true });
    }
  });

  it("terminates the native process tree when checkpointing fails and preserves failed state", () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-jobs-checkpoint-failure-"));
    try {
      const jobs = new JobSupervisor(directory);
      const started = jobs.start({ ...spec(directory), jobId: "job_checkpointfailure", command: [process.execPath, "-e", "setInterval(() => undefined, 1000)"], checkpointCommand: [process.execPath, "-e", "process.exit(1)"] });
      expect(started.state).toBe("running");
      expect(jobs.checkpoint(started.jobId)).toMatchObject({ state: "failed", failureReason: "checkpoint_failed" });
      expect(jobs.get(started.jobId)).toMatchObject({ state: "failed", failureReason: "checkpoint_failed" });
      jobs.close();
    } finally {
      rmSync(directory, { recursive: true, force: true });
    }
  });
});
