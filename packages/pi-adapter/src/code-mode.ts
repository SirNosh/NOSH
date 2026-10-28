/**
 * nosh_code: the model writes a short JavaScript program that calls its own tools, so independent calls run
 * together, dependent calls chain, and large results are filtered before they reach the model's context. The program
 * runs in a QuickJS interpreter inside a worker thread (code-mode-worker.ts) and can reach nothing but the tool
 * functions passed in here, each of which the caller runs through the normal, checked tool implementation.
 */
import { Worker } from "node:worker_threads";
import type { WorkerMessage, WorkerReply } from "./code-mode-worker.js";

export type CodeLimits = { maxCalls: number; cpuMs: number; memoryBytes: number; stackBytes: number; maxLogChars: number };
export const CODE_LIMITS: CodeLimits = { maxCalls: 50, cpuMs: 5_000, memoryBytes: 64 * 1024 * 1024, stackBytes: 1024 * 1024, maxLogChars: 12_000 };
export const MAX_CODE_CHARS = 20_000;
export type ProgramResult = { ok: boolean; value?: unknown; error?: string; logs: string[]; calls: number };
/** Runs one tool call for the program; `index` is the call's position, for event ids. Throws to fail the call. */
export type ProgramHost = (name: string, args: unknown[], index: number) => Promise<unknown>;

export function runProgram(code: string, functions: readonly string[], host: ProgramHost, signal: AbortSignal, limits: CodeLimits = CODE_LIMITS): Promise<ProgramResult> {
  if (code.length > MAX_CODE_CHARS) return Promise.resolve({ ok: false, error: `Program exceeds ${MAX_CODE_CHARS} characters`, logs: [], calls: 0 });
  if (signal.aborted) return Promise.resolve({ ok: false, error: "Program cancelled", logs: [], calls: 0 });
  const worker = new Worker(new URL("./code-mode-worker.js", import.meta.url), {
    workerData: { code, functions: [...functions], cpuMs: limits.cpuMs, memoryBytes: limits.memoryBytes, stackBytes: limits.stackBytes },
    // A second fence beside QuickJS's own memory limit.
    resourceLimits: { maxOldGenerationSizeMb: 128 },
  });
  const logs: string[] = []; let logChars = 0; let calls = 0; let finished = false;
  return new Promise<ProgramResult>((resolve) => {
    const finish = (result: Omit<ProgramResult, "logs" | "calls">) => {
      if (finished) return; finished = true;
      signal.removeEventListener("abort", cancel);
      void worker.terminate();
      resolve({ ...result, logs, calls });
    };
    const cancel = () => finish({ ok: false, error: "Program cancelled" });
    signal.addEventListener("abort", cancel, { once: true });
    const reply = (message: WorkerReply) => { if (!finished) worker.postMessage(message); };
    worker.on("message", (message: WorkerMessage) => {
      if (message.type === "done") return message.ok ? finish({ ok: true, value: JSON.parse(message.json) }) : finish({ ok: false, error: message.error });
      if (message.type === "log") {
        if (logChars < limits.maxLogChars) { const text = message.text.slice(0, limits.maxLogChars - logChars); logs.push(text); logChars += text.length; }
        return;
      }
      if (!functions.includes(message.name)) return reply({ id: message.id, ok: false, json: `Unknown tool function ${message.name}` });
      if (calls >= limits.maxCalls) return reply({ id: message.id, ok: false, json: `Program exceeded ${limits.maxCalls} tool calls` });
      const index = calls++;
      let args: unknown[];
      try { const parsed: unknown = JSON.parse(message.argsJson); if (!Array.isArray(parsed)) throw new Error("arguments must be a list"); args = parsed; }
      catch (error) { return reply({ id: message.id, ok: false, json: `Invalid arguments: ${error instanceof Error ? error.message : String(error)}` }); }
      host(message.name, args, index).then(
        (value) => reply({ id: message.id, ok: true, json: JSON.stringify(value ?? null) }),
        (error: unknown) => reply({ id: message.id, ok: false, json: error instanceof Error ? error.message : String(error) }),
      );
    });
    worker.on("error", (error) => finish({ ok: false, error: `Program failed: ${error.message}` }));
    worker.on("exit", () => finish({ ok: false, error: "Program stopped unexpectedly" }));
  });
}
