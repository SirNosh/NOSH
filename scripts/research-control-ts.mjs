#!/usr/bin/env node
// Runs ResearchControl operations against the TypeScript daemon's research control
// (apps/noshd/dist/research-control.js), for crates/nosh-research/tests/research_control.rs.
// Reads { database, project, ops } on stdin, where `project` is the registered Project and each op
// is { method, args } (a ResearchControl method), { fn, args } (an exported function; those
// named in WITH_RESEARCH take the ResearchControl first), or { store, args } (an EventStore
// method, to seed events the control only reads), or { result } / { failed }: an earlier run's
// outcome, recorded without running anything. `{ "$ref": [index, ...path] }` in args
// reads an earlier op's result (negative indices count back from the current op).
// Prints { results: [{ ok, published } | { error }], state } with the store's final contents.
// The TypeScript tree is NOSH_TS_ROOT, else the worktree's parent; it must be built.
import { readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const tsRoot = resolve(process.env.NOSH_TS_ROOT ?? resolve(dirname(fileURLToPath(import.meta.url)), "..", ".."));
const load = (path) => import(pathToFileURL(join(tsRoot, path)).href);
const control = await load("apps/noshd/dist/research-control.js");
const { EventStore } = await load("packages/persistence/dist/index.js");

const WITH_RESEARCH = new Set(["daemonTaskFacts", "missionOutcomes"]);
const ENTITY_TYPES = ["mission", "direction", "autoresearch", "graph_proposal", "handoff", "handoff_decision", "logical_ownership"];

const request = JSON.parse(readFileSync(0, "utf8"));
const store = new EventStore(request.database);
const published = [];
const research = new control.ResearchControl(() => store, () => request.project, (event) => published.push(event));
const results = [];

const resolveRefs = (value) => {
  if (Array.isArray(value)) return value.map(resolveRefs);
  if (value && typeof value === "object") {
    if (Array.isArray(value.$ref)) {
      const [index, ...path] = value.$ref;
      const result = results[index < 0 ? results.length + index : index];
      if (!result || !("ok" in result)) throw new Error(`$ref ${JSON.stringify(value.$ref)} names a failed op`);
      return path.reduce((object, key) => object?.[key], result.ok);
    }
    return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, resolveRefs(item)]));
  }
  return value;
};

for (const op of request.ops) {
  // An earlier run's outcome standing in for an op, so a replay's `$ref`s resolve.
  if ("result" in op) { results.push({ ok: op.result, published: [] }); continue; }
  if ("failed" in op) { results.push({ error: op.failed }); continue; }
  const before = published.length;
  try {
    const args = resolveRefs(op.args ?? []);
    let value;
    if (op.store) value = store[op.store](...args);
    else if (op.fn === "taskCommandRuns") value = control.taskCommandRuns(research)(...args);
    else if (op.fn) value = WITH_RESEARCH.has(op.fn) ? control[op.fn](research, ...args) : control[op.fn](...args);
    else value = research[op.method](...args);
    results.push({ ok: JSON.parse(JSON.stringify(value ?? null)), published: published.slice(before).map((event) => event.type) });
  } catch (error) {
    results.push({ error: error instanceof Error ? error.message : String(error) });
  }
}

const projectId = request.project.projectId;
const state = {
  events: store.replay(projectId),
  projections: Object.fromEntries(ENTITY_TYPES.map((type) => [type, store.projections(projectId, type)])),
  intents: store.operationIntents(projectId),
};
process.stdout.write(JSON.stringify({ results, state }));
