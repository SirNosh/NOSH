#!/usr/bin/env node
// Calls the built TypeScript pi-adapter (packages/pi-adapter/dist) for nosh-agent's differential
// tests. Reads a JSON array of ops on stdin and prints one { ok } or { error, syntax } per op:
//   { fn, args, env? }          an exported function; env sets process.env for the call
//   { terminal: [text, ctx] }   parseTerminalOutput, plus { excerpt } = withJsonExcerpt on failure
//   { piError: message }        userFacingPiError(new Error(message)).message
//   { value: name }             an exported constant (a Set becomes a sorted array)
// The TypeScript tree is NOSH_TS_ROOT, else the parent of this worktree.
import { readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const tsRoot = resolve(process.env.NOSH_TS_ROOT ?? resolve(dirname(fileURLToPath(import.meta.url)), "..", ".."));
const adapter = await import(pathToFileURL(join(tsRoot, "packages/pi-adapter/dist/index.js")).href);
const ops = JSON.parse(readFileSync(0, "utf8"));

function withEnv(env, work) {
  const previous = Object.fromEntries(Object.keys(env ?? {}).map((key) => [key, process.env[key]]));
  for (const [key, value] of Object.entries(env ?? {})) { if (value === null) delete process.env[key]; else process.env[key] = value; }
  try { return work(); } finally { for (const [key, value] of Object.entries(previous)) { if (value === undefined) delete process.env[key]; else process.env[key] = value; } }
}

const output = [];
for (const op of ops) {
  try {
    if (op.terminal) {
      const [text, context] = op.terminal;
      try { output.push({ ok: adapter.parseTerminalOutput(text, context) }); }
      catch (error) { output.push({ error: error.message, syntax: error instanceof SyntaxError, excerpt: adapter.withJsonExcerpt(error, text) }); }
      continue;
    }
    if (op.piError !== undefined) { output.push({ ok: adapter.userFacingPiError(new Error(op.piError)).message }); continue; }
    if (op.value) { const value = adapter[op.value]; output.push({ ok: value instanceof Set ? [...value].sort() : value }); continue; }
    output.push({ ok: withEnv(op.env, () => adapter[op.fn](...(op.args ?? []))) ?? null });
  } catch (error) {
    output.push({ error: error instanceof Error ? error.message : String(error), syntax: error instanceof SyntaxError });
  }
}
// Lone surrogates (a UTF-16 slice through a pair) become U+FFFD, as Rust strings hold them.
process.stdout.write(JSON.stringify(output, (_key, value) => typeof value === "string" ? value.toWellFormed() : value));
