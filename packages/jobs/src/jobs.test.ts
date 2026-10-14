import { mkdtempSync, rmSync, writeFileSync, openSync, closeSync, ftruncateSync, writeSync, readFileSync, existsSync } from "node:fs";
import { EventEmitter, once } from "node:events";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { describe, expect, it } from "vitest";
// Native Jobs run through the compiled exit-wrapper.js next to index.js, so the supervisor is exercised from dist (built by `pnpm test`).
import type * as Jobs from "./index.js";
const { JobSupervisor } = await import(new URL("../dist/index.js", import.meta.url).href) as typeof Jobs;
type JobSpec = Jobs.JobSpec;
type JobRecord = Jobs.JobRecord;

// The test Job must outlive the setup before recover(): hosted Windows runners have taken over 8 s for it. Tests cancel it.
function spec(directory: string, jobId = "job_test1"): JobSpec {
  return { jobId, projectId: "prj_test", missionId: null, directionId: null, autoresearchId: null, runId: "run_test", experimentId: "exp_test", commitSha: "1234567", workingDirectory: directory, runner: "native", distribution: null, command: [process.execPath, "-e", "console.log('started'); setTimeout(() => console.log('done'), 120000)"], checkpointCommand: null, environmentLockHash: "sha256:test", evaluationContractHash: "sha256:test", timeoutSeconds: 10, usesGpu: false };
}

// Exits with the code written to ./release, so a test controls exactly when the Job ends relative to recover().
const releasable = [process.execPath, "-e", "const fs = require('fs'); console.log('started'); setInterval(() => { if (fs.existsSync('release')) process.exit(Number(fs.readFileSync('release', 'utf8'))); }, 50)"];
const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));
const alive = (pid: number) => { try { process.kill(pid, 0); return true; } catch { return false; } };
// A supervisor whose onChange resolves `settled` with the first terminal record it writes.
function watched(directory: string): { jobs: Jobs.JobSupervisor; settled: Promise<JobRecord> } {
  const events = new EventEmitter();
  const settled = once(events, "settled").then(([job]) => job as JobRecord);
  return { jobs: new JobSupervisor(directory, (job) => { if (["completed", "failed", "cancelled", "lost"].includes(job.state)) events.emit("settled", job); }), settled };
}
// WSL is not installed on hosted CI runners; the WSL runner test needs a working distribution.
const wslDistribution = (() => {
  if (process.platform !== "win32") return null;
  const listed = spawnSync("wsl.exe", ["-l", "-q"], { encoding: "utf16le", windowsHide: true, timeout: 30_000 });
  const distribution = listed.status === 0 ? listed.stdout.replace(/\0/g, "").split(/\r?\n/)[0]?.trim() : "";
  return distribution && spawnSync("wsl.exe", ["-d", distribution, "--exec", "true"], { windowsHide: true, timeout: 30_000 }).status === 0 ? distribution : null;
})();

describe("JobSupervisor", { timeout: 20_000 }, () => {
  it("reads only a bounded tail from large sparse files and coalesces telemetry", async () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-tail-"));
    const { jobs, settled } = watched(directory);
    try {
      const record = jobs.start({ ...spec(directory), command: ["nosh-definitely-missing-binary"] });
      // The exit wrapper reports the missing binary; wait until that failure is recorded and the wrapper has released the log.
      if (record.state !== "failed") await settled;
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
    } finally { jobs.close(); rmSync(directory, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 }); }
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
      rmSync(directory, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 });
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
      rmSync(directory, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 });
    }
  });

  it.each([[0, "completed", null], [3, "failed", "exit_3"]] as const)("records exit status %i of a Job that exits after daemon restart recovery", async (code, state, failureReason) => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-jobs-recovered-exit-"));
    const { jobs: second, settled } = watched(directory);
    try {
      const first = new JobSupervisor(directory);
      first.start({ ...spec(directory), timeoutSeconds: 0, command: releasable });
      first.close();
      expect(second.recover()[0]).toMatchObject({ state: "running", recoveredAfterRestart: true });
      writeFileSync(join(directory, "release"), String(code));
      expect(await settled).toMatchObject({ state, exitCode: code, failureReason });
      expect(second.get("job_test1")).toMatchObject({ state, exitCode: code, failureReason });
      expect(second.tail("job_test1", "stdout")).toContain("started");
    } finally { second.close(); rmSync(directory, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 }); }
  });

  it("records the exit status of a Job that exited while no supervisor was running", async () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-jobs-exited-offline-"));
    try {
      const first = new JobSupervisor(directory);
      const started = first.start({ ...spec(directory), timeoutSeconds: 0, command: [process.execPath, "-e", "console.log('started'); setTimeout(() => process.exit(0), 300)"] });
      first.close();
      while (alive(started.windowsPid!)) await sleep(100);
      const second = new JobSupervisor(directory);
      expect(second.recover()[0]).toMatchObject({ state: "completed", exitCode: 0, failureReason: null });
      expect(second.tail("job_test1", "stdout")).toContain("started");
      second.close();
    } finally { rmSync(directory, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 }); }
  });

  it("re-arms the timeout of a recovered Job and terminates its whole process tree", async () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-jobs-recovered-timeout-"));
    const { jobs: second, settled } = watched(directory);
    try {
      const first = new JobSupervisor(directory);
      first.start({ ...spec(directory), timeoutSeconds: 3, command: [process.execPath, "-e", "require('fs').writeFileSync('child.pid', String(process.pid)); setInterval(() => undefined, 1000)"] });
      // On a slow runner the deadline may already have passed at recover(); the command must have recorded its PID before it is killed.
      while (!existsSync(join(directory, "child.pid"))) await sleep(50);
      first.close();
      second.recover();
      expect(await settled).toMatchObject({ state: "cancelled", failureReason: "timeout" });
      const child = Number(readFileSync(join(directory, "child.pid"), "utf8"));
      for (let attempt = 0; attempt < 50 && alive(child); attempt++) await sleep(100);
      expect(alive(child)).toBe(false);
    } finally { second.close(); rmSync(directory, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 }); }
  });

  it.skipIf(process.platform !== "win32")("reports the resident memory of the Job command rather than its exit wrapper", async () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-jobs-resident-"));
    const jobs = new JobSupervisor(directory);
    try {
      const started = jobs.start({ ...spec(directory), command: [process.execPath, "-e", "const held = Buffer.alloc(256 * 1024 * 1024, 1); console.log('ready'); setInterval(() => held[0]++, 1000)"] });
      while (!jobs.tail(started.jobId, "stdout").includes("ready")) await sleep(50);
      expect((await jobs.resourceSnapshot(started.jobId)).residentBytes).toBeGreaterThan(200 * 1024 * 1024);
      jobs.cancel(started.jobId);
    } finally { jobs.close(); rmSync(directory, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 }); }
  });

  it.skipIf(!wslDistribution)("records the exit status of a WSL Job after daemon restart", { timeout: 60_000 }, async () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-jobs-wsl-exit-"));
    const { jobs: second, settled } = watched(directory);
    try {
      const jobId = `job_wsl${Date.now()}`;
      const first = new JobSupervisor(directory);
      const started = first.start({ ...spec(directory), jobId, runner: "wsl2", distribution: wslDistribution, timeoutSeconds: 0, command: ["sh", "-c", "sleep 3; exit 4"] });
      // Until WSL has started the Job its pid file does not exist, so recovery would rightly report the Job as lost.
      while (spawnSync("wsl.exe", ["-d", wslDistribution!, "--exec", "test", "-s", started.wslPidFile!], { windowsHide: true }).status !== 0) await sleep(100);
      first.close();
      second.recover();
      expect(await settled).toMatchObject({ state: "failed", exitCode: 4, failureReason: "exit_4" });
    } finally { second.close(); rmSync(directory, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 }); }
  });
});
