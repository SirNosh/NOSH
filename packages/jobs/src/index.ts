import { canonicalJson, sha256, type JsonValue } from "@nosh/wire";
import { closeSync, existsSync, mkdirSync, openSync, readFileSync, readdirSync, renameSync, statSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { spawn, spawnSync, type ChildProcess } from "node:child_process";

export type JobState = "queued" | "starting" | "running" | "checkpointing" | "finishing" | "completed" | "failed" | "cancelled" | "lost";
export type JobSpec = {
  jobId: string; projectId: string; missionId: string | null; directionId: string | null; autoresearchId: string | null; runId: string; experimentId: string; commitSha: string; workingDirectory: string;
  runner: "native" | "wsl2"; distribution: string | null; command: string[]; checkpointCommand: string[] | null;
  environmentLockHash: string; evaluationContractHash: string; timeoutSeconds: number; usesGpu: boolean;
};
export type JobRecord = JobSpec & {
  state: JobState; commandDigest: string; windowsPid: number | null; processFingerprint: string | null; wslPidFile: string | null;
  stdoutPath: string; stderrPath: string; startedAt: string | null; finishedAt: string | null; exitCode: number | null; failureReason: string | null; recoveredAfterRestart: boolean;
};

export class JobSupervisor {
  private readonly children = new Map<string, ChildProcess>();
  private readonly timers = new Map<string, NodeJS.Timeout>();

  constructor(private readonly dataDirectory: string, private readonly onChange: (record: JobRecord) => void = () => undefined) {
    mkdirSync(join(dataDirectory, "jobs"), { recursive: true });
  }

  start(spec: JobSpec): JobRecord {
    validateSpec(spec);
    if (existsSync(this.recordPath(spec.jobId))) { const prior = this.get(spec.jobId); if (canonicalJson(jobSpec(prior) as unknown as JsonValue) !== canonicalJson({ ...spec, workingDirectory: resolve(spec.workingDirectory) } as unknown as JsonValue)) throw new Error(`Job ${spec.jobId} already exists with a different specification`); return prior; }
    const directory = join(this.dataDirectory, "jobs", spec.jobId);
    mkdirSync(directory, { recursive: true });
    const record: JobRecord = {
      ...spec, workingDirectory: resolve(spec.workingDirectory), state: "starting", commandDigest: sha256(spec.command), windowsPid: null, processFingerprint: null,
      wslPidFile: spec.runner === "wsl2" ? `/tmp/nosh-${spec.jobId}.pid` : null, stdoutPath: join(directory, "stdout.log"), stderrPath: join(directory, "stderr.log"),
      startedAt: new Date().toISOString(), finishedAt: null, exitCode: null, failureReason: null, recoveredAfterRestart: false,
    };
    this.save(record);
    const stdout = openSync(record.stdoutPath, "a");
    const stderr = openSync(record.stderrPath, "a");
    const launch = commandFor(record);
    let child: ChildProcess;
    try {
      child = spawn(launch.file, launch.args, { cwd: record.workingDirectory, detached: true, windowsHide: true, stdio: ["ignore", stdout, stderr] });
    } catch (error) {
      closeSync(stdout); closeSync(stderr);
      record.state = "failed"; record.failureReason = error instanceof Error ? error.message : "launch_failed"; record.finishedAt = new Date().toISOString(); this.save(record);
      return record;
    }
    closeSync(stdout); closeSync(stderr);
    if (!child.pid) throw new Error("Runner did not return a process ID");
    record.windowsPid = child.pid;
    record.processFingerprint = processFingerprint(child.pid);
    record.state = "running";
    this.children.set(record.jobId, child);
    child.once("exit", (code, signal) => this.finish(record.jobId, code, signal));
    child.unref();
    this.save(record);
    if (record.timeoutSeconds > 0) {
      const timer = setTimeout(() => void this.cancel(record.jobId, "timeout"), record.timeoutSeconds * 1000);
      timer.unref(); this.timers.set(record.jobId, timer);
    }
    return record;
  }

  list(): JobRecord[] {
    return readdirSync(join(this.dataDirectory, "jobs"), { withFileTypes: true }).filter((entry) => entry.isDirectory() && existsSync(this.recordPath(entry.name))).map((entry) => this.get(entry.name));
  }

  get(jobId: string): JobRecord {
    return JSON.parse(readFileSync(this.recordPath(jobId), "utf8")) as JobRecord;
  }

  recover(): JobRecord[] {
    return this.list().map((record) => {
      if (!["starting", "running", "checkpointing", "finishing"].includes(record.state)) return record;
      const alive = record.runner === "wsl2" ? wslAlive(record) : nativeAlive(record);
      if (!alive) {
        record.state = "lost"; record.failureReason = "process_identity_not_recoverable"; record.finishedAt = new Date().toISOString();
      } else {
        record.state = "running"; record.recoveredAfterRestart = true; this.watchRecovered(record.jobId);
      }
      this.save(record);
      return record;
    });
  }

  checkpoint(jobId: string): JobRecord {
    const record = this.get(jobId);
    if (record.state !== "running" || !record.checkpointCommand?.length) throw new Error("Running job does not declare a checkpoint command");
    record.state = "checkpointing"; this.save(record);
    const command = record.runner === "wsl2" ? { file: "wsl.exe", args: [...distributionArgs(record), "--exec", ...record.checkpointCommand] } : { file: record.checkpointCommand[0]!, args: record.checkpointCommand.slice(1) };
    const result = spawnSync(command.file, command.args, { cwd: record.workingDirectory, windowsHide: true, timeout: 60_000 });
    record.state = result.status === 0 ? "running" : "failed";
    if (result.status !== 0) { record.failureReason = "checkpoint_failed"; record.finishedAt = new Date().toISOString(); }
    this.save(record); return record;
  }

  cancel(jobId: string, reason = "cancelled_by_user"): JobRecord {
    const record = this.get(jobId);
    if (["completed", "failed", "cancelled", "lost"].includes(record.state)) return record;
    if (record.runner === "wsl2" && record.wslPidFile) {
      const script = 'pid=$(cat "$1" 2>/dev/null) || exit 0; kill -TERM -- -"$pid" 2>/dev/null || true';
      spawnSync("wsl.exe", [...distributionArgs(record), "--exec", "sh", "-c", script, "nosh", record.wslPidFile], { windowsHide: true, timeout: 10_000 });
    } else if (record.windowsPid) {
      if (process.platform === "win32") spawnSync("taskkill.exe", ["/PID", String(record.windowsPid), "/T", "/F"], { windowsHide: true });
      else { try { process.kill(-record.windowsPid, "SIGTERM"); } catch { /* already stopped */ } }
    }
    record.state = "cancelled"; record.failureReason = reason; record.finishedAt = new Date().toISOString(); this.save(record); return record;
  }

  tail(jobId: string, stream: "stdout" | "stderr", maximumBytes = 64 * 1024): string {
    const record = this.get(jobId);
    const path = stream === "stdout" ? record.stdoutPath : record.stderrPath;
    if (!existsSync(path)) return "";
    const bytes = readFileSync(path);
    return bytes.subarray(Math.max(0, bytes.length - maximumBytes)).toString("utf8");
  }

  resourceSnapshot(jobId: string): { elapsedSeconds: number; residentBytes: number | null; gpu: JsonValue | null; outputStalledSeconds: number } {
    const record = this.get(jobId);
    const elapsedSeconds = record.startedAt ? Math.max(0, Math.floor((Date.now() - Date.parse(record.startedAt)) / 1000)) : 0;
    let residentBytes: number | null = null;
    if (record.windowsPid && process.platform === "win32") {
      const result = spawnSync("powershell.exe", ["-NoProfile", "-Command", `(Get-Process -Id ${record.windowsPid} -ErrorAction Stop).WorkingSet64`], { encoding: "utf8", windowsHide: true });
      const value = Number(result.stdout.trim()); if (Number.isFinite(value)) residentBytes = value;
    }
    const newest = Math.max(existsSync(record.stdoutPath) ? statSync(record.stdoutPath).mtimeMs : 0, existsSync(record.stderrPath) ? statSync(record.stderrPath).mtimeMs : 0);
    return { elapsedSeconds, residentBytes, gpu: gpuSnapshot(), outputStalledSeconds: newest ? Math.floor((Date.now() - newest) / 1000) : elapsedSeconds };
  }

  close(): void {
    for (const timer of this.timers.values()) clearTimeout(timer);
    this.timers.clear();
  }

  private finish(jobId: string, code: number | null, signal: NodeJS.Signals | null): void {
    const record = this.get(jobId);
    if (record.state === "cancelled") return;
    record.state = "finishing"; this.save(record);
    record.exitCode = code; record.finishedAt = new Date().toISOString(); record.state = code === 0 ? "completed" : "failed"; record.failureReason = code === 0 ? null : signal ? `signal_${signal}` : `exit_${code}`;
    this.save(record); this.children.delete(jobId);
    const timer = this.timers.get(jobId); if (timer) clearTimeout(timer); this.timers.delete(jobId);
  }

  private watchRecovered(jobId: string): void {
    const timer = setInterval(() => {
      const record = this.get(jobId);
      if (record.state !== "running") { clearInterval(timer); this.timers.delete(jobId); return; }
      const alive = record.runner === "wsl2" ? wslAlive(record) : nativeAlive(record);
      if (!alive) { record.state = "lost"; record.failureReason = "exit_unobserved_after_restart"; record.finishedAt = new Date().toISOString(); this.save(record); clearInterval(timer); this.timers.delete(jobId); }
    }, 2_000);
    timer.unref(); this.timers.set(jobId, timer);
  }

  private recordPath(jobId: string): string { return join(this.dataDirectory, "jobs", jobId, "job.json"); }
  private save(record: JobRecord): void {
    const path = this.recordPath(record.jobId); mkdirSync(resolve(path, ".."), { recursive: true }); const temporary = `${path}.tmp`; writeFileSync(temporary, `${canonicalJson(record)}\n`, "utf8"); renameSync(temporary, path); this.onChange({ ...record });
  }
}

function jobSpec(record: JobRecord): JobSpec { return { jobId: record.jobId, projectId: record.projectId, missionId: record.missionId, directionId: record.directionId, autoresearchId: record.autoresearchId, runId: record.runId, experimentId: record.experimentId, commitSha: record.commitSha, workingDirectory: record.workingDirectory, runner: record.runner, distribution: record.distribution, command: [...record.command], checkpointCommand: record.checkpointCommand ? [...record.checkpointCommand] : null, environmentLockHash: record.environmentLockHash, evaluationContractHash: record.evaluationContractHash, timeoutSeconds: record.timeoutSeconds, usesGpu: record.usesGpu }; }

export function loadNativeEvaluation(path: string): JsonValue {
  const value = JSON.parse(readFileSync(path, "utf8")) as JsonValue;
  assertFinite(value); return value;
}

export function loadWandbSummary(directory: string): { runId: string | null; metrics: JsonValue } {
  const metadataPath = join(directory, "wandb-metadata.json");
  return { runId: existsSync(metadataPath) ? (JSON.parse(readFileSync(metadataPath, "utf8")) as { id?: string }).id ?? null : null, metrics: loadNativeEvaluation(join(directory, "wandb-summary.json")) };
}

export function tensorBoardReference(logDirectory: string): { logDirectory: string; eventFiles: string[] } {
  const root = resolve(logDirectory);
  return { logDirectory: root, eventFiles: readdirSync(root).filter((name) => name.startsWith("events.out.tfevents")).sort() };
}

function validateSpec(spec: JobSpec): void {
  if (!/^job_[a-z0-9]+$/.test(spec.jobId)) throw new Error("Invalid Job ID");
  if (!spec.command.length || spec.command.some((part) => part.includes("\0"))) throw new Error("Job command must be a non-empty argument array");
  if (!existsSync(spec.workingDirectory) || !statSync(spec.workingDirectory).isDirectory()) throw new Error("Job working directory does not exist");
  if (spec.runner === "wsl2" && !spec.distribution) throw new Error("WSL2 jobs require a distribution");
}

function commandFor(record: JobRecord): { file: string; args: string[] } {
  if (record.runner === "native") return { file: record.command[0]!, args: record.command.slice(1) };
  const script = 'umask 077; echo "$$" > "$1"; shift; exec setsid "$@"';
  return { file: "wsl.exe", args: [...distributionArgs(record), "--exec", "sh", "-c", script, "nosh", record.wslPidFile!, ...record.command] };
}

function distributionArgs(record: Pick<JobRecord, "distribution">): string[] { return record.distribution ? ["-d", record.distribution] : []; }

function nativeAlive(record: JobRecord): boolean {
  if (!record.windowsPid || !record.processFingerprint) return false;
  return processFingerprint(record.windowsPid) === record.processFingerprint;
}

function wslAlive(record: JobRecord): boolean {
  if (!record.wslPidFile) return false;
  const script = 'pid=$(cat "$1" 2>/dev/null) || exit 1; kill -0 -- -"$pid" 2>/dev/null';
  return spawnSync("wsl.exe", [...distributionArgs(record), "--exec", "sh", "-c", script, "nosh", record.wslPidFile], { windowsHide: true, timeout: 10_000 }).status === 0;
}

function processFingerprint(pid: number): string | null {
  if (process.platform === "win32") {
    const result = spawnSync("powershell.exe", ["-NoProfile", "-Command", `(Get-Process -Id ${pid} -ErrorAction Stop).StartTime.ToUniversalTime().ToString('O')`], { encoding: "utf8", windowsHide: true });
    return result.status === 0 ? result.stdout.trim() : null;
  }
  try { process.kill(pid, 0); return readFileSync(`/proc/${pid}/stat`, "utf8").split(" ")[21] ?? null; } catch { return null; }
}

function gpuSnapshot(): JsonValue | null {
  const result = spawnSync("nvidia-smi", ["--query-gpu=uuid,utilization.gpu,memory.used,temperature.gpu", "--format=csv,noheader,nounits"], { encoding: "utf8", windowsHide: true, timeout: 3_000 });
  if (result.status !== 0) return null;
  return result.stdout.trim().split(/\r?\n/).filter(Boolean).map((line) => { const [uuid, utilization, memoryMiB, temperatureC] = line.split(",").map((value) => value?.trim()); return { uuid: uuid ?? "", utilizationPercent: Number(utilization), memoryMiB: Number(memoryMiB), temperatureC: Number(temperatureC) }; });
}

function assertFinite(value: JsonValue): void {
  if (typeof value === "number" && !Number.isFinite(value)) throw new Error("Metrics contain NaN or infinity");
  if (Array.isArray(value)) value.forEach(assertFinite);
  else if (value && typeof value === "object") Object.values(value).forEach(assertFinite);
}
