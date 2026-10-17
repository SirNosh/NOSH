#!/usr/bin/env node
// Runs domain operations against the TypeScript graph, scheduler, skills, episodes, threads, and
// agent-runtime packages, for crates/nosh-research/tests/domain.rs. Reads { ops: [...] } on stdin
// (already expanded by the Rust test) and prints one { ok } or { error, invalid } per op. Objects
// created by an op live under its `name`; `{ "$ref": [index, ...path] }` reads an earlier result
// (negative indices count back). The TypeScript tree is NOSH_TS_ROOT, else the worktree's parent.
import { readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const tsRoot = resolve(process.env.NOSH_TS_ROOT ?? resolve(dirname(fileURLToPath(import.meta.url)), "..", ".."));
const load = (name) => import(pathToFileURL(join(tsRoot, "packages", name, "dist", "index.js")).href);
const [graph, scheduler, skills, episodes, threads, agentRuntime] = await Promise.all(["graph", "scheduler", "skills", "episodes", "threads", "agent-runtime"].map(load));

const objects = new Map();
const get = (name) => { const object = objects.get(name); if (!object) throw new Error(`no object ${name}`); return object; };
const keep = (name, object) => { objects.set(name, object); return null; };
const context = (c) => ({ paused: c.paused, approvals: new Set(), workspaceConflicts: new Set(c.workspaceConflicts), availableRoles: new Set(c.availableRoles), available: { agents: c.available.agents, gpuJobs: 0, gpuSeconds: 0, modelTokens: 0, diskBytes: 0 } });

const handlers = {
  "dag.new": (a) => keep(a.name, new graph.VersionedDag(a.scopeId, a.nodes, a.rationale, a.initialVersion)),
  "dag.apply": (a) => get(a.name).apply(a.baseVersion, a.operations, a.rationale, a.evidenceIds),
  "dag.transition": (a) => get(a.name).transition(a.nodeId, a.next),
  "dag.lease": (a) => get(a.name).lease(a.nodeId, a.lease),
  "dag.releaseExpired": (a) => get(a.name).releaseExpired(a.now),
  "dag.current": (a) => get(a.name).current(),
  "dag.versions": (a) => get(a.name).versions(),
  "dag.runnable": (a) => get(a.name).runnable(),
  "dag.completionReady": (a) => get(a.name).completionReady(),
  "dag.criticalPath": (a) => get(a.name).criticalPath(),
  "dag.diff": (a) => get(a.name).diff(a.from, a.to),
  "legal.mission": (a) => graph.isLegalMissionTransition(a.current, a.next),
  "legal.direction": (a) => graph.isLegalDirectionTransition(a.current, a.next),
  "tree.new": (a) => keep(a.name, new graph.ExperimentTree(a.hash, a.maximumDepth, a.maximumChildren)),
  "tree.add": (a) => get(a.name).add(a.experiment),
  "tree.evaluate": (a) => get(a.name).evaluate(a.id, a.commit, a.state, a.review, a.guardrailsPass),
  "tree.get": (a) => get(a.name).get(a.id),
  "tree.all": (a) => get(a.name).all(),
  "tree.frontier": (a) => get(a.name).frontier(),
  "tree.shapeWarnings": (a) => get(a.name).shapeWarnings(),
  "scheduler.new": (a) => keep(a.name, new scheduler.Scheduler()),
  "scheduler.activate": (a) => get(a.name).activateMission(a.projectId, a.missionId),
  "scheduler.release": (a) => get(a.name).releaseMission(a.projectId, a.missionId),
  "scheduler.frontier": (a) => get(a.name).frontier(get(a.dag), context(a.context), a.now),
  "scheduler.record": (a) => get(a.name).recordAllocation(a.nodeId),
  "scheduler.restore": (a) => get(a.name).restoreAllocations(a.distribution),
  "scheduler.distribution": (a) => get(a.name).distribution(),
  "focus.new": (a) => keep(a.name, new scheduler.FocusGovernor()),
  "focus.fingerprint": (a) => get(a.name).fingerprint(a.input),
  "focus.record": (a) => get(a.name).record(a.input, a.delta, a.override ?? null),
  "focus.clear": (a) => get(a.name).clear(a.nodeId, a.proof),
  "skills.new": (a) => keep(a.name, new skills.SkillRegistry()),
  "skills.add": (a) => get(a.name).add(a.input),
  "skills.get": (a) => get(a.name).get(a.skillId),
  "skills.list": (a) => get(a.name).list(),
  "skills.applicable": (a) => get(a.name).applicable(a.skillId, a.thread),
  compileProgram: (a) => skills.compileProgram(a.input),
  validateState: (a) => skills.validateState(a.schema, a.values),
  createEpisode: (a) => episodes.createEpisode(a.input),
  fallbackEpisodeDraft: (a) => episodes.fallbackEpisodeDraft(a.episodeType, a.summary),
  renderEpisode: (a) => episodes.renderEpisode(a.episode),
  episodeIntegrity: (a) => episodes.episodeIntegrity(a.episode),
  createThread: (a) => threads.createThread(a.instruction),
  ensureCanStep: (a) => threads.ensureCanStep(a.thread),
  renderThreadContext: (a) => threads.renderThreadContext(a.thread, a.episodes, a.extraRefs, a.skillPrompts),
  "gate.new": (a) => keep(a.name, new agentRuntime.StructuredSubmissionGate()),
  "gate.submit": (a) => get(a.name).submit(a.tool, a.key, a.record),
  "gate.supported": (a) => get(a.name).supportedSchemas(),
  validateTeachback: (a) => agentRuntime.validateTeachback(a.state, a.teachback),
};

const request = JSON.parse(readFileSync(0, "utf8"));
const results = [];
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
}
// Lone surrogates (a clipped emoji) cannot cross into Rust; compare their well-formed form.
process.stdout.write(JSON.stringify(results, (_key, value) => (typeof value === "string" ? value.toWellFormed() : value)));

function substitute(value) {
  if (Array.isArray(value)) return value.map(substitute);
  if (!value || typeof value !== "object") return value;
  if (Array.isArray(value.$ref)) {
    const [index, ...path] = value.$ref;
    return path.reduce((node, key) => node?.[key], results[index < 0 ? results.length + index : index]?.ok);
  }
  return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, substitute(item)]));
}
