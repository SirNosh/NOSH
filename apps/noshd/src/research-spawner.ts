/**
 * Research children spawned without a human: a Mission's Research Directions and its own Autoresearch, and each
 * Direction's Autoresearch. A director supplies the judgment (question, metric, scope); the daemon checks it against
 * the Project contract, measures the baseline by running the declared command once, freezes the evaluation contract
 * from that measurement, and creates and starts the child under a durable intent, so a restart between steps resumes
 * instead of duplicating work.
 */
import { existsSync, readdirSync, statSync } from "node:fs";
import { isAbsolute, join, relative, resolve } from "node:path";
import { createId } from "@nosh/core";
import { loadNativeEvaluation, type JobRecord, type JobSpec } from "@nosh/jobs";
import type { RegisteredProject } from "@nosh/persistence";
import { sha256, type JsonValue } from "@nosh/wire";
import { declaredTaskCommands, type MissionProjection, type ResearchControl, type SpawnOrigin, type Stored } from "./research-control.js";
import { createTaskWorktree } from "./task-worktree.js";

export type JobRunner = { start(spec: JobSpec): JobRecord; get(jobId: string): JobRecord };
export type EvaluationExecution = { runner: "native" | "wsl2"; distribution?: string | null; command: string[]; resultPath: string; timeoutSeconds: number; usesGpu?: boolean };
type Metric = { name: string; objective: "maximize" | "minimize"; minimumEffect: number };
type Guardrail = { name: string; direction: "maximize" | "minimize"; threshold: number };
export type MissionSpawn = {
  nodeId: string; kind: "research_direction" | "autoresearch"; question: string; decisionUse: string | null;
  evaluation: { commandId: string; resultPath: string; primaryMetric: Metric; guardrails: Guardrail[]; target: number | null; maximumNoProgressRounds: number };
  familyTags: string[]; scope: string[];
};
export type DirectionSpawn = { nodeId: string; kind: "autoresearch"; question: string; familyTags: string[]; scope: string[] };

/** Smallest model-token allocations worth spawning; below them a child cannot finish its own reviewed closure. */
const MIN_DIRECTION_TOKENS = 300_000;
export const MIN_AUTORESEARCH_TOKENS = 150_000;
/** How long a spawned child may sit blocked or paused, with no agent working on it, before its parent stops it. */
export const CHILD_BLOCKED_MS = 10 * 60_000;
/** Share of the Mission ceiling kept for the Mission's own nodes and reviews. */
const MISSION_RESERVE = 0.3;

/** Runs one evaluation command as a supervised Job in a worktree and reads its JSON result. */
export async function runEvaluationJob(jobRunner: JobRunner, input: { projectId: string; missionId: string | null; directionId: string | null; autoresearchId: string | null; experimentId: string; commitSha: string; worktree: string; execution: EvaluationExecution; evaluationContractHash: string }): Promise<{ jobId: string; job: JobRecord; resultPath: string; metrics: JsonValue; failure: string | null }> {
  const resultPath = resolve(input.worktree, input.execution.resultPath);
  if (relative(input.worktree, resultPath).startsWith("..")) throw new Error("Evaluation result path escapes the worktree");
  const jobId = createId("job"); const timeoutSeconds = input.execution.timeoutSeconds;
  jobRunner.start({ jobId, projectId: input.projectId, missionId: input.missionId, directionId: input.directionId, autoresearchId: input.autoresearchId, runId: createId("run"), experimentId: input.experimentId, commitSha: input.commitSha, workingDirectory: input.worktree, runner: input.execution.runner, distribution: input.execution.distribution ?? null, command: input.execution.command, checkpointCommand: null, environmentLockHash: sha256({ node: process.version, platform: process.platform, arch: process.arch }), evaluationContractHash: input.evaluationContractHash, timeoutSeconds, usesGpu: input.execution.usesGpu === true });
  const terminal = ["completed", "failed", "cancelled", "lost"]; let job = jobRunner.get(jobId); const deadline = Date.now() + (timeoutSeconds + 60) * 1_000;
  while (!terminal.includes(job.state) && Date.now() < deadline) { await new Promise((done) => setTimeout(done, 500)); job = jobRunner.get(jobId); }
  if (!terminal.includes(job.state)) throw new Error(`Evaluation Job ${jobId} did not finish within its timeout`);
  let metrics: JsonValue = { error: job.failureReason ?? job.state }; let failure: string | null = job.state === "completed" ? "evaluation result file is absent" : `job ${job.state}`;
  if (job.state === "completed" && existsSync(resultPath)) {
    try { const loaded = loadNativeEvaluation(resultPath); if (loaded && typeof loaded === "object" && !Array.isArray(loaded)) { metrics = loaded; failure = null; } else failure = "evaluation result is not a JSON object"; }
    catch (error) { failure = error instanceof Error ? error.message : "evaluation result is malformed"; }
  }
  return { jobId, job, resultPath, metrics, failure };
}

/**
 * Whether argv runs the frozen evaluation's program with other flags (`node evaluate.mjs --split=test` for
 * `node evaluate.mjs --split=dev`). Experiment workers never run it: the daemon evaluates each variant with the frozen
 * command, and its other splits (held-out test) are reserved.
 */
export function isEvaluationVariant(argv: string[], evaluation: string[]): boolean {
  const program = (args: string[]) => args.filter((arg) => !arg.startsWith("-")).join("\u0000");
  return program(argv) === program(evaluation);
}

const repositoryPath = (path: string): string => path.replaceAll("\\", "/").replace(/^(\.\/)+/, "");
/**
 * The frozen evaluation's own repository files: its program (non-flag command arguments that are files in the
 * repository) and its result file. Experiments never write them, or a variant could change how it is measured.
 */
export function frozenEvaluationFiles(root: string, execution: { command: string[]; resultPath: string }): string[] {
  const program = execution.command.filter((arg) => !arg.startsWith("-") && !isAbsolute(arg) && existsSync(join(root, arg)) && statSync(join(root, arg)).isFile());
  return [...new Set([...program, execution.resultPath].map(repositoryPath))];
}
/** A scope naming only frozen evaluation files leaves the search nothing it may change. */
export function scopeIssues(scope: string[], frozen: string[]): string[] {
  if (!scope.length || !scope.every((entry) => frozen.includes(repositoryPath(entry)))) return [];
  return [`scope ${scope.join(", ")} names only the frozen evaluation (${frozen.join(", ")}), which experiments never change; name the files a variant edits instead, such as the method's source and configuration`];
}

/** Structural checks a spawn must pass before anything runs; the probe then checks it against a real evaluation. */
export function spawnIssues(project: RegisteredProject, spawn: MissionSpawn, nodeId: string, nodeType: string): string[] {
  const issues: string[] = [];
  if (spawn.nodeId !== nodeId) issues.push(`spawn.nodeId must be ${nodeId}, the node being run`);
  if (spawn.kind !== nodeType) issues.push(`spawn.kind must be ${nodeType} for this node`);
  if (!declaredTaskCommands(project).some((command) => command.commandId === spawn.evaluation.commandId)) issues.push(`spawn.evaluation.commandId ${spawn.evaluation.commandId} is not a command declared in the Project contract (declared: ${declaredTaskCommands(project).map((command) => command.commandId).join(", ") || "none"})`);
  const path = spawn.evaluation.resultPath;
  if (/^[a-zA-Z]:|^[\\/]/.test(path) || path.split(/[\\/]/).includes("..")) issues.push("spawn.evaluation.resultPath must be a relative path inside the repository");
  const command = declaredTaskCommands(project).find((entry) => entry.commandId === spawn.evaluation.commandId);
  if (command) issues.push(...scopeIssues(spawn.scope, frozenEvaluationFiles(project.repositoryRoot, { command: command.argv, resultPath: path })));
  // A guardrail threshold is a change from the measured baseline; one the baseline itself fails would refuse every experiment.
  for (const guardrail of spawn.evaluation.guardrails) if (guardrail.direction === "maximize" ? guardrail.threshold > 0 : guardrail.threshold < 0) issues.push(`guardrail ${guardrail.name}: threshold is the allowed change from the measured baseline, so for ${guardrail.direction} it must be ${guardrail.direction === "maximize" ? "zero or negative (-0.005 allows a 0.005 drop)" : "zero or positive (0.005 allows a 0.005 rise)"}`);
  return issues;
}

/** Runs the declared command once at the base commit; the measured metrics become the frozen baseline. */
export async function probeEvaluation(research: ResearchControl, jobRunner: JobRunner, project: RegisteredProject, input: { missionId: string; directionId: string | null; spawn: MissionSpawn; baseCommit: string }): Promise<{ ok: true; metrics: Record<string, number>; execution: EvaluationExecution; probeJobId: string } | { ok: false; issues: string[]; metricKeys: string[]; writtenJson: string[] }> {
  const command = declaredTaskCommands(project).find((entry) => entry.commandId === input.spawn.evaluation.commandId)!;
  const execution: EvaluationExecution = { runner: "native", distribution: null, command: command.argv, resultPath: input.spawn.evaluation.resultPath, timeoutSeconds: command.timeoutSeconds, usesGpu: false };
  const tree = createTaskWorktree(research, project, createId("tsk"), input.baseCommit); const before = jsonFiles(tree.path);
  const run = await runEvaluationJob(jobRunner, { projectId: project.projectId, missionId: input.missionId, directionId: input.directionId, autoresearchId: null, experimentId: createId("exp"), commitSha: input.baseCommit, worktree: tree.path, execution, evaluationContractHash: sha256({ probe: input.spawn.evaluation as unknown as JsonValue }) });
  const metrics = Object.fromEntries(Object.entries(run.metrics && typeof run.metrics === "object" && !Array.isArray(run.metrics) ? run.metrics : {}).filter((entry): entry is [string, number] => typeof entry[1] === "number" && Number.isFinite(entry[1])));
  const issues = run.failure ? [`probe ${command.argv.join(" ")}: ${run.failure}`] : [];
  for (const name of [input.spawn.evaluation.primaryMetric.name, ...input.spawn.evaluation.guardrails.map((guardrail) => guardrail.name)]) if (!run.failure && !(name in metrics)) issues.push(`metric ${name} is not a finite number in ${input.spawn.evaluation.resultPath}`);
  if (!issues.length) return { ok: true, metrics, execution, probeJobId: run.jobId };
  // The rejection is all the Director sees next cycle, so it names what the command really wrote. Outputs are often
  // gitignored (metrics.json), so they are found by comparing the worktree before and after the run, not Git status.
  const writtenJson = [...jsonFiles(tree.path)].filter(([path, modified]) => before.get(path) !== modified).map(([path]) => path).slice(0, 20); const metricKeys = Object.keys(metrics).slice(0, 40);
  if (run.failure === "evaluation result file is absent") issues.push(writtenJson.length ? `${input.spawn.evaluation.resultPath} does not exist; the command wrote ${writtenJson.join(", ")}: use one of these as resultPath` : `${input.spawn.evaluation.resultPath} does not exist and the command wrote no JSON file in the repository`);
  else if (!run.failure) issues.push(`numeric keys in ${input.spawn.evaluation.resultPath}: ${metricKeys.join(", ") || "none"}`);
  return { ok: false, issues, metricKeys, writtenJson };
}

/** Repository-relative JSON files and their modification times, skipping Git metadata and dependencies. */
function jsonFiles(root: string, directory = root, depth = 0, found = new Map<string, number>()): Map<string, number> {
  if (depth > 4) return found;
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    if (entry.name === ".git" || entry.name === "node_modules") continue;
    const path = join(directory, entry.name);
    if (entry.isDirectory()) jsonFiles(root, path, depth + 1, found);
    else if (entry.isFile() && entry.name.endsWith(".json")) found.set(relative(root, path).replaceAll("\\", "/"), statSync(path).mtimeMs);
  }
  return found;
}

/** The frozen evaluation contract: the director's metric judgment plus the daemon-measured baseline. */
export function freezeContract(spawn: MissionSpawn, probe: { metrics: Record<string, number>; execution: EvaluationExecution; probeJobId: string }, provenance: { missionId: string; baseCommit: string }): JsonValue {
  return {
    primaryMetric: spawn.evaluation.primaryMetric as unknown as JsonValue, baselineMetrics: probe.metrics,
    // The Director wrote each threshold as a change from a baseline it could not see; frozen, it is an absolute bound.
    guardrails: spawn.evaluation.guardrails.map((guardrail) => ({ name: guardrail.name, direction: guardrail.direction, threshold: probe.metrics[guardrail.name]! + guardrail.threshold })),
    metrics: Object.keys(probe.metrics), datasets: [`declared command ${spawn.evaluation.commandId} (${probe.execution.command.join(" ")})`], seeds: [0],
    stopConditions: { ...(spawn.evaluation.target === null ? {} : { target: spawn.evaluation.target }), maximumNoProgressRounds: spawn.evaluation.maximumNoProgressRounds },
    execution: probe.execution as unknown as JsonValue,
    provenance: { commandId: spawn.evaluation.commandId, probeJobId: probe.probeJobId, probeCommit: provenance.baseCommit, authorizedBy: "mission_approval", missionId: provenance.missionId },
  };
}

/**
 * A child's share of the Mission ceiling: a reserve stays with the Mission's own nodes, and the rest is split over the
 * spawn slots not yet given out. Below the floor the spawn is refused, never silently starved.
 */
export function allocateBudget(research: ResearchControl, projectId: string, mission: Stored<MissionProjection>, kind: "research_direction" | "autoresearch"): { ok: true; maximumModelTokens: number; maximumWallClockSeconds: number } | { ok: false; reason: string } {
  const budgets = mission.value.budgets; const use = research.missionBudgetUse(projectId, mission.entityId);
  const spawnNodes = mission.value.nodes.filter((node) => ["research_direction", "autoresearch"].includes(node.type));
  // A live child holds its whole ceiling; an ended one (stopped, failed, rejected) holds only what it spent, so a retried node can respawn.
  const given = spawnNodes.map((node) => research.spawnedChild(projectId, mission.entityId, node.id)).filter((child): child is NonNullable<typeof child> => Boolean(child)).reduce((sum, child) => {
    const ceiling = child.kind === "direction" ? Number((research.direction(projectId, child.entityId).value.budget as { maximumModelTokens?: number }).maximumModelTokens ?? 0) : research.autoresearchExecution(projectId, child.entityId).value.maximumModelTokens;
    if (!["failed", "stopped", "rejected"].includes(child.state)) return sum + ceiling;
    return sum + Math.min(ceiling, child.kind === "direction" ? research.directionBudgetUse(projectId, child.entityId).modelTokens : research.autoresearchBudgetUse(projectId, child.entityId).modelTokens);
  }, 0);
  const open = spawnNodes.filter((node) => !research.spawnedChild(projectId, mission.entityId, node.id) || ["failed", "stopped", "rejected"].includes(research.spawnedChild(projectId, mission.entityId, node.id)!.state)).length || 1;
  const pool = budgets.maximumModelTokens * (1 - MISSION_RESERVE) - given;
  const tokens = Math.floor(Math.min(pool / open, budgets.maximumModelTokens - use.modelTokens - budgets.maximumModelTokens * MISSION_RESERVE));
  const floor = kind === "research_direction" ? MIN_DIRECTION_TOKENS : MIN_AUTORESEARCH_TOKENS;
  if (tokens < floor) return { ok: false, reason: `budget_insufficient: ${tokens} model tokens available for this ${kind}, at least ${floor} needed` };
  return { ok: true, maximumModelTokens: tokens, maximumWallClockSeconds: Math.max(600, budgets.maximumWallClockSeconds - use.wallClockSeconds) };
}

/** Creates and starts a Mission-spawned child under a durable intent; replaying the same key resumes the same child. */
export function spawnMissionChild(research: ResearchControl, projectId: string, input: { mission: Stored<MissionProjection>; nodeId: string; attempt: number; spawn: MissionSpawn; evaluationContract: JsonValue; baseCommit: string; budget: { maximumModelTokens: number; maximumWallClockSeconds: number } }): string {
  const key = `child-spawn:${input.mission.entityId}:${input.nodeId}:${input.attempt}`;
  const origin: SpawnOrigin = { scopeType: "mission", scopeId: input.mission.entityId, nodeId: input.nodeId, attempt: input.attempt };
  const intent = research.beginExternalOperation(projectId, "research.child_spawn", key, { origin: origin as unknown as JsonValue, spawn: input.spawn as unknown as JsonValue, evaluationContract: input.evaluationContract, baseCommit: input.baseCommit, budget: input.budget });
  if (intent.state === "completed") return String((intent.result as { childId?: string }).childId);
  const childId = input.spawn.kind === "research_direction"
    ? research.createDirection(projectId, { question: input.spawn.question, decisionUse: input.spawn.decisionUse ?? input.spawn.question, missionId: input.mission.entityId, evaluationContract: input.evaluationContract, idempotencyKey: `${key}:create` }, { origin, baseCommit: input.baseCommit, budget: input.budget }).entityId
    : research.createAutoresearch(projectId, { decisionQuestion: input.spawn.question, missionId: input.mission.entityId, familyTags: input.spawn.familyTags, scope: input.spawn.scope, evaluationContract: input.evaluationContract, maximumModelTokens: input.budget.maximumModelTokens, maximumWallClockSeconds: input.budget.maximumWallClockSeconds, maximumGpuSeconds: 0, maximumExperiments: 6, maximumRounds: 4, idempotencyKey: `${key}:create` }, { invokedByRole: "mission_director", origin, baseCommit: input.baseCommit }).entityId;
  startChild(research, projectId, childId, key);
  research.completeExternalOperation(projectId, intent.intentId, { childId });
  return childId;
}

/** Creates and starts a Direction-spawned Autoresearch under the Direction's frozen contract. */
export function spawnDirectionAutoresearch(research: ResearchControl, projectId: string, input: { directionId: string; missionId: string | null; nodeId: string; ordinal: number; spawn: DirectionSpawn; maximumModelTokens: number }): string {
  const key = `child-spawn:${input.directionId}:${input.nodeId}:${input.ordinal}`;
  const origin: SpawnOrigin = { scopeType: "direction", scopeId: input.directionId, nodeId: input.nodeId, attempt: input.ordinal };
  const intent = research.beginExternalOperation(projectId, "research.child_spawn", key, { origin: origin as unknown as JsonValue, spawn: input.spawn as unknown as JsonValue, maximumModelTokens: input.maximumModelTokens });
  if (intent.state === "completed") return String((intent.result as { childId?: string }).childId);
  const childId = research.createAutoresearch(projectId, { decisionQuestion: input.spawn.question, directionId: input.directionId, missionId: input.missionId, familyTags: input.spawn.familyTags, scope: input.spawn.scope, maximumModelTokens: input.maximumModelTokens, maximumGpuSeconds: 0, maximumExperiments: 6, maximumRounds: 4, idempotencyKey: `${key}:create` }, { invokedByRole: "research_director", origin, baseCommit: null }).entityId;
  startChild(research, projectId, childId, key);
  research.completeExternalOperation(projectId, intent.intentId, { childId });
  return childId;
}

function startChild(research: ResearchControl, projectId: string, childId: string, key: string): void {
  if (childId.startsWith("dir_")) { const direction = research.direction(projectId, childId); if (["draft", "proposed"].includes(direction.state)) research.start(projectId, "directions", childId, direction.version, `${key}:start`); return; }
  const execution = research.autoresearchExecution(projectId, childId); if (execution.state === "draft") research.start(projectId, "autoresearch", childId, execution.version, `${key}:start`);
}
