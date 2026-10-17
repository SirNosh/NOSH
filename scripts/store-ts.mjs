#!/usr/bin/env node
// Runs store operations against the TypeScript EventStore and HostRegistry, for the Rust
// differential tests (crates/nosh-store/tests/differential.rs). Reads one JSON request on stdin:
//   { eventStore, host, prior: [results...], ops: [{ op, ...args }] }
// and prints a JSON array with one { ok } or { error, invalid } per op. An argument of the form
// { "$ref": [index, ...path] } is replaced by that field of an earlier result (prior + this run;
// a negative index counts back from the current op).
// The TypeScript tree is NOSH_TS_ROOT, else the parent of this worktree; it must be built.
import { readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const tsRoot = resolve(process.env.NOSH_TS_ROOT ?? resolve(dirname(fileURLToPath(import.meta.url)), "..", ".."));
const { EventStore, HostRegistry } = await import(pathToFileURL(join(tsRoot, "packages", "persistence", "dist", "index.js")).href);

const request = JSON.parse(readFileSync(0, "utf8"));
const results = [...request.prior];
let store = new EventStore(request.eventStore);
let host = new HostRegistry(request.host);

const handlers = {
  append: (a) => store.append(a.draft),
  appendIdempotent: (a) => store.appendIdempotent(a.key, a.draft),
  appendTerminalSubmission: (a) => store.appendTerminalSubmission(a.taskId, a.toolName, a.schemaUri, a.record, a.draft),
  replay: (a) => store.replay(a.projectId, a.after),
  replayPage: (a) => store.replayPage(a.projectId, a.after, a.limit, a.recent),
  currentSequence: (a) => store.currentSequence(a.projectId),
  commandReceipt: (a) => store.commandReceipt(a.projectId, a.key) ?? null,
  terminalTurn: (a) => store.terminalTurn(a.projectId, a.turnId, a.context),
  saveTerminalTurn: (a) => (store.saveTerminalTurn(a.projectId, a.turnId, a.receipt), null),
  beginOperation: (a) => store.beginOperation(a.projectId, a.operationType, a.key, a.request),
  completeOperation: (a) => store.completeOperation(a.projectId, a.intentId, a.result),
  failOperation: (a) => store.failOperation(a.projectId, a.intentId, a.error),
  operationIntents: (a) => store.operationIntents(a.projectId, a.state ?? undefined),
  projection: (a) => store.projection(a.projectId, a.entityType, a.entityId) ?? null,
  projections: (a) => store.projections(a.projectId, a.entityType),
  mutateProjection: (a) => store.mutateProjection(a.key, a.expectedVersion, a.draft, a.input),
  graphVersions: (a) => store.graphVersions(a.projectId, a.scopeType, a.scopeId),
  writeSnapshot: (a) => store.writeSnapshot(a.projectId, a.sequence, a.state),
  latestSnapshot: (a) => store.latestSnapshot(a.projectId) ?? null,
  register: (a) => host.register({ projectId: a.projectId, repositoryRoot: a.repositoryRoot, databasePath: a.databasePath }),
  list: () => host.list(),
  exec: (a) => (store.database.exec(a.sql), null),
  reopen: () => {
    store.close();
    host.close();
    store = new EventStore(request.eventStore);
    host = new HostRegistry(request.host);
    return null;
  },
};

const output = [];
for (const op of request.ops) {
  const handler = handlers[op.op];
  if (!handler) throw new Error(`unknown op ${op.op}`);
  let result;
  try {
    result = { ok: handler(substitute(op)) ?? null };
  } catch (error) {
    result = { error: error.message, invalid: error.name === "ZodError" };
  }
  results.push(result);
  output.push(result);
}
store.close();
host.close();
process.stdout.write(JSON.stringify(output));

function substitute(value) {
  if (Array.isArray(value)) return value.map(substitute);
  if (!value || typeof value !== "object") return value;
  if (Array.isArray(value.$ref)) {
    const [index, ...path] = value.$ref;
    return path.reduce((node, key) => node?.[key], results[index < 0 ? results.length + index : index]?.ok);
  }
  return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, substitute(item)]));
}
