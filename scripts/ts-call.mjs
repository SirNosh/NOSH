#!/usr/bin/env node
// Calls into a built TypeScript package for Rust differential tests. Reads one JSON request on stdin:
//   { module: "packages/jobs/dist/index.js", construct?: { name, args }, calls: [{ method, args, waitUntil? }] }
// With `construct`, each call is a method of one instance of that export; otherwise each call is a
// module-level function. `waitUntil: { path: [...], in: [...values], timeoutMs }` repeats the call
// every 100 ms until the result's field at `path` is one of `in` (the process stays alive meanwhile,
// so event-driven work such as a Job's exit is observed). Prints a JSON array with one { ok } or
// { error, invalid } per call. The TypeScript tree is NOSH_TS_ROOT, else the parent of this worktree.
import { readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const tsRoot = resolve(process.env.NOSH_TS_ROOT ?? resolve(dirname(fileURLToPath(import.meta.url)), "..", ".."));
const request = JSON.parse(readFileSync(0, "utf8"));
const module = await import(pathToFileURL(join(tsRoot, request.module)).href);
const target = request.construct ? new module[request.construct.name](...request.construct.args) : module;

const output = [];
for (const call of request.calls) {
  const invoke = () => target[call.method](...(call.args ?? []));
  try {
    let result = await invoke();
    if (call.waitUntil) {
      const deadline = Date.now() + (call.waitUntil.timeoutMs ?? 30_000);
      const field = () => call.waitUntil.path.reduce((node, key) => node?.[key], result);
      while (!call.waitUntil.in.includes(field())) {
        if (Date.now() > deadline) throw new Error(`waitUntil timed out with ${JSON.stringify(field())}`);
        await new Promise((done) => setTimeout(done, 100));
        result = await invoke();
      }
    }
    output.push({ ok: result ?? null });
  } catch (error) {
    output.push({ error: error.message, invalid: error.name === "ZodError" });
  }
}
target.close?.();
process.stdout.write(JSON.stringify(output));
// Detached children (Jobs) must not keep this process alive.
process.exit(0);
