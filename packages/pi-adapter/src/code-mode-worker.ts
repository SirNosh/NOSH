/**
 * Worker thread that runs one nosh_code program in a QuickJS (WASM) interpreter. The guest sees only `tools.*`,
 * `console.log`, and the JavaScript standard library; every tool call is a message to the main thread, which runs
 * the real, checked tool implementation. Running here keeps the daemon's event loop free while the program computes,
 * and lets the main thread stop a runaway program with worker.terminate().
 */
import { parentPort, workerData } from "node:worker_threads";
import { newQuickJSWASMModuleFromVariant, type QuickJSDeferredPromise } from "quickjs-emscripten-core";

type WorkerInput = { code: string; functions: string[]; cpuMs: number; memoryBytes: number; stackBytes: number };
export type WorkerReply = { id: number; ok: boolean; json: string };
export type WorkerMessage = { type: "call"; id: number; name: string; argsJson: string } | { type: "log"; text: string } | { type: "done"; ok: true; json: string; cpuMs: number } | { type: "done"; ok: false; error: string; cpuMs: number };

const port = parentPort!;
const input = workerData as WorkerInput;
const post = (message: WorkerMessage) => port.postMessage(message);
const QuickJS = await newQuickJSWASMModuleFromVariant(import("@jitl/quickjs-wasmfile-release-sync"));
const runtime = QuickJS.newRuntime();
runtime.setMemoryLimit(input.memoryBytes);
runtime.setMaxStackSize(input.stackBytes);
// Only time spent executing inside the interpreter counts against the CPU limit; waiting on tools does not.
let spentMs = 0; let enteredAt = 0;
const enter = () => { enteredAt = performance.now(); };
const leave = () => { if (enteredAt) spentMs += performance.now() - enteredAt; enteredAt = 0; };
runtime.setInterruptHandler(() => enteredAt > 0 && spentMs + performance.now() - enteredAt > input.cpuMs);
const vm = runtime.newContext();
const waiting = new Map<number, QuickJSDeferredPromise>();
let nextId = 0;
let settled = false;

const callFunction = vm.newFunction("__call", (nameHandle, argsHandle) => {
  const id = nextId++; const deferred = vm.newPromise(); waiting.set(id, deferred);
  post({ type: "call", id, name: vm.getString(nameHandle), argsJson: vm.getString(argsHandle) });
  return deferred.handle;
});
const logFunction = vm.newFunction("__log", (textHandle) => { post({ type: "log", text: vm.getString(textHandle) }); });
vm.setProp(vm.global, "__call", callFunction); vm.setProp(vm.global, "__log", logFunction);
callFunction.dispose(); logFunction.dispose();

function drive(): void {
  enter(); const jobs = runtime.executePendingJobs(); leave();
  if (jobs.error) { const error = vm.dump(jobs.error); jobs.error.dispose(); finish({ type: "done", ok: false, error: describe(error), cpuMs: Math.round(spentMs) }); return; }
  // The guest has no timers: with no tool call outstanding, an unsettled program can never make progress.
  setImmediate(() => { if (!settled && !waiting.size) finish({ type: "done", ok: false, error: "The program is waiting on something that can never finish (no tool call is outstanding).", cpuMs: Math.round(spentMs) }); });
}
function finish(message: WorkerMessage): void { if (settled) return; settled = true; post(message); }
function describe(error: unknown): string { return error && typeof error === "object" && "message" in error ? `${(error as { name?: string }).name ?? "Error"}: ${String((error as { message: unknown }).message)}` : String(error); }

port.on("message", (reply: WorkerReply) => {
  const deferred = waiting.get(reply.id); if (!deferred || settled) return;
  waiting.delete(reply.id);
  const value = vm.newString(reply.json);
  if (reply.ok) deferred.resolve(value); else deferred.reject(value);
  value.dispose(); deferred.dispose();
  drive();
});

// The prelude closes over the host functions, then removes them from the global object.
const prelude = `const { tools, console } = ((call, log) => {
  const invoke = async (name, args) => { let json; try { json = await call(name, JSON.stringify(args)); } catch (message) { throw new Error(String(message)); } return JSON.parse(json); };
  const format = (value) => typeof value === "string" ? value : JSON.stringify(value);
  return {
    tools: Object.freeze(Object.fromEntries(${JSON.stringify(input.functions)}.map((name) => [name, (...args) => invoke(name, args)]))),
    console: Object.freeze({ log: (...values) => log(values.map(format).join(" ")) }),
  };
})(__call, __log);
delete globalThis.__call; delete globalThis.__log;`;
try {
  enter();
  const evaluated = vm.evalCode(`${prelude}\n(async () => { const __value = await (async () => {\n${input.code}\n})(); return __value === undefined ? "null" : JSON.stringify(__value); })()`);
  leave();
  const program = vm.unwrapResult(evaluated);
  const outcome = vm.resolvePromise(program); program.dispose();
  drive();
  const result = await outcome;
  if (result.error) { const error = vm.dump(result.error); result.error.dispose(); finish({ type: "done", ok: false, error: describe(error), cpuMs: Math.round(spentMs) }); }
  else { const json = vm.getString(result.value); result.value.dispose(); finish({ type: "done", ok: true, json, cpuMs: Math.round(spentMs) }); }
} catch (error) {
  leave();
  finish({ type: "done", ok: false, error: describe(error instanceof Error ? error : vm.dump(error as never)), cpuMs: Math.round(spentMs) });
}
