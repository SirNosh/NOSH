import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";

const planPath = process.argv.slice(2).find((value) => !value.startsWith("--"));
const dryRun = process.argv.includes("--dry-run");
if (!planPath) fail("Usage: node scripts/delegate.mjs <plan.json> [--dry-run]");

const plan = JSON.parse(readFileSync(planPath === "-" ? 0 : resolve(planPath), "utf8").replace(/^\uFEFF/, ""));
const compiled = compile(plan);
if (dryRun) {
  process.stdout.write(`${JSON.stringify(compiled, null, 2)}\n`);
  process.exit(0);
}

const baseUrl = process.env.NOSH_DAEMON_URL?.replace(/\/$/, "");
const token = process.env.NOSH_BOOTSTRAP_TOKEN;
if (!baseUrl || !token) fail("NOSH_DAEMON_URL and NOSH_BOOTSTRAP_TOKEN are required");

await Promise.all(compiled.open.map(post));
const steps = Promise.all(compiled.step.map(post));
const joined = post(compiled.await);
let stepResults; let awaitResult;
try {
  [stepResults, awaitResult] = await Promise.all([steps, joined]);
} catch (error) {
  await Promise.allSettled(compiled.stop.map(post)); throw error;
}
const stopResults = await Promise.all(compiled.stop.map(post));
process.stdout.write(`${JSON.stringify({ threads: compiled.open.map((item) => item.threadId), stepResults, awaitResult, stopResults }, null, 2)}\n`);

async function post(instruction) {
  const response = await fetch(`${baseUrl}/runtime/instructions`, { method: "POST", headers: { authorization: `Bearer ${token}`, "content-type": "application/json" }, body: JSON.stringify(instruction) });
  const result = await response.json(); if (!response.ok) throw new Error(`Runtime instruction failed (${response.status}): ${JSON.stringify(result)}`); return result;
}

function compile(value) {
  if (!record(value) || !validId(value.projectId, "prj") || typeof value.idempotencyKey !== "string" || value.idempotencyKey.length < 16 || value.idempotencyKey.length > 120 || typeof value.issuedAt !== "string" || !Number.isFinite(Date.parse(value.issuedAt)) || !Array.isArray(value.tasks) || !value.tasks.length || value.tasks.length > 100) fail("Plan requires projectId, a 16-120 character idempotencyKey, an ISO issuedAt, and 1-100 tasks");
  const issuedAt = new Date(value.issuedAt).toISOString(); const scope = ownerScope(value.ownerScope); const open = []; const step = []; const stop = [];
  for (const [index, raw] of value.tasks.entries()) {
    if (!record(raw) || typeof raw.objective !== "string" || !raw.objective.trim() || raw.objective.length > 20_000) fail(`Task ${index} requires an objective of 1-20000 characters`);
    const role = raw.role ?? "general_worker"; if (!["nosh", "mission_director", "research_director", "librarian_researcher", "general_worker", "reviewer"].includes(role)) fail(`Task ${index} has an invalid role`);
    const threadId = id("thr", `${value.projectId}:${value.idempotencyKey}:thread:${index}`); const taskId = id("tsk", `${value.projectId}:${value.idempotencyKey}:task:${index}`); const common = { projectId: value.projectId, issuedAt, proposedByAgentId: null };
    open.push({ $schema: schema("runtime-instruction"), schemaVersion: 1, instructionId: id("ins", `${value.idempotencyKey}:open:${index}`), ...common, idempotencyKey: `${value.idempotencyKey}:open:${index}`, operation: "THREAD_OPEN", threadId, taskId, initialAgentId: null, ownerScope: scope, role, purpose: raw.purpose ?? raw.objective, executionMode: "background", parentThreadId: null, inputRefs: refs(raw.inputRefs), skillIds: ids(raw.skillIds, "skl"), capabilities: refs(raw.capabilities), budget: budget(raw.budget) });
    step.push({ $schema: schema("runtime-instruction"), schemaVersion: 1, instructionId: id("ins", `${value.idempotencyKey}:step:${index}`), ...common, idempotencyKey: `${value.idempotencyKey}:step:${index}`, operation: "THREAD_STEP", threadId, objective: raw.objective, expectedEpisodeType: raw.expectedEpisodeType ?? `episode_${role}`, inputRefs: refs(raw.inputRefs), skillIds: ids(raw.skillIds, "skl") });
    stop.push({ $schema: schema("runtime-instruction"), schemaVersion: 1, instructionId: id("ins", `${value.idempotencyKey}:stop:${index}`), ...common, idempotencyKey: `${value.idempotencyKey}:stop:${index}`, operation: "STOP", threadId, programId: null, reason: "Delegated task reached its boundary" });
  }
  const awaitInstruction = { $schema: schema("runtime-instruction"), schemaVersion: 1, instructionId: id("ins", `${value.idempotencyKey}:await`), projectId: value.projectId, idempotencyKey: `${value.idempotencyKey}:await`, proposedByAgentId: null, issuedAt, operation: "THREAD_AWAIT", threadIds: open.map((item) => item.threadId), policy: value.awaitPolicy ?? "all", minimumCount: value.awaitPolicy === "minimum_count" ? value.minimumCount ?? open.length : null, deadline: value.awaitPolicy === "deadline" ? value.deadline ?? null : null };
  return { open, step, await: awaitInstruction, stop };
}

function ownerScope(value) { const input = record(value) ? value : {}; return { missionId: nullableId(input.missionId, "mis"), directionId: nullableId(input.directionId, "dir"), autoresearchId: nullableId(input.autoresearchId, "ar"), experimentId: nullableId(input.experimentId, "exp"), graphNodeId: typeof input.graphNodeId === "string" ? input.graphNodeId : null }; }
function budget(value) { const input = record(value) ? value : {}; return { maximumToolCalls: positiveInt(input.maximumToolCalls, 100), maximumModelTokens: positiveInt(input.maximumModelTokens, 60_000), maximumWallClockSeconds: positiveInt(input.maximumWallClockSeconds, 7_200) }; }
function refs(value) { if (value === undefined) return []; if (!Array.isArray(value) || value.some((item) => typeof item !== "string")) fail("Reference lists must contain only strings"); return [...new Set(value)]; }
function ids(value, prefix) { const values = refs(value); if (values.some((item) => !validId(item, prefix))) fail(`Expected ${prefix}_ IDs`); return values; }
function nullableId(value, prefix) { if (value === undefined || value === null) return null; if (!validId(value, prefix)) fail(`Expected ${prefix}_ ID`); return value; }
function positiveInt(value, fallback) { if (value === undefined) return fallback; if (!Number.isInteger(value) || value < 1) fail("Budgets must be positive integers"); return value; }
function validId(value, prefix) { return typeof value === "string" && new RegExp(`^${prefix}_[0-9a-f]{32}$`).test(value); }
function id(prefix, seed) { return `${prefix}_${createHash("sha256").update(seed).digest("hex").slice(0, 32)}`; }
function schema(name) { return `https://nosh.dev/schemas/${name}/v1`; }
function record(value) { return typeof value === "object" && value !== null && !Array.isArray(value); }
function fail(message) { throw new Error(message); }
