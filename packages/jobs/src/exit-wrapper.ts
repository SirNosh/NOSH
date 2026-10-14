// Runs a native Job command and records how it ended, so a supervisor restarted while the Job ran can still report its result.
// Usage: node exit-wrapper.js <exitFile> -- <command> [args...]. The command inherits this process's stdio (the Job's log files),
// so the wrapper itself never writes to stdout or stderr. It stays in the Job's process tree (not detached): cancelling the Job
// kills both. The exit record is complete before the wrapper exits, so "wrapper gone" implies "record present" unless it was killed.
import { spawn } from "node:child_process";
import { renameSync, writeFileSync } from "node:fs";

const [exitFile, separator, file, ...args] = process.argv.slice(2);
if (!exitFile || separator !== "--" || !file) process.exit(2);

let recorded = false;
function record(exitCode: number | null, signal: NodeJS.Signals | null, error?: string): void {
  if (recorded) return;
  recorded = true;
  const temporary = `${exitFile}.tmp`;
  writeFileSync(temporary, `${JSON.stringify({ exitCode, signal, finishedAt: new Date().toISOString(), ...(error === undefined ? {} : { error }) })}\n`, "utf8");
  renameSync(temporary, exitFile!);
}

const child = spawn(file, args, { stdio: "inherit", windowsHide: true });
child.once("error", (error) => { record(null, null, error.message); process.exit(1); });
child.once("exit", (code, signal) => {
  record(code, signal);
  process.exitCode = code ?? 1;
  // Re-raise a terminating signal so the supervisor observes the same outcome as an unwrapped command (exit 1 if it is ignored here).
  if (signal) process.kill(process.pid, signal);
});
