/**
 * A compact, daemon-derived picture of what a Project is doing: Missions and Directions with their graph nodes,
 * Autoresearch executions with their experiment tree, live workers, and running Jobs. Clients render it as-is
 * (the TUI's research map), so derivation lives here, next to the records it reads.
 */
import type { AgentInspection } from "@nosh/pi-adapter";
import type { JobRecord } from "@nosh/jobs";
import { schemaUri, type EventEnvelope } from "@nosh/wire";

export type MapNode = { kind: "mission" | "direction" | "autoresearch" | "node" | "experiment" | "worker" | "job"; id: string; title: string; state: string; detail: string; progress: { done: number; total: number } | null; children: MapNode[] };
export type ResearchMap = { generatedAt: string; roots: MapNode[]; workers: MapNode[] };
type Stored = { entityId: string; version: number; state: string; value: Record<string, unknown> };
type GraphNode = { id: string; title: string; type: string; state: string; attempt: number; maximumAttempts: number; lease: { ownerId?: string } | null };
type Submitted = { event: EventEnvelope; record: Record<string, unknown> };
export type ResearchMapInput = { missions: Stored[]; directions: Stored[]; autoresearch: Stored[]; submitted: Submitted[]; events: EventEnvelope[]; agents: AgentInspection[]; jobs: JobRecord[] };

const TERMINAL = new Set(["completed", "stopped", "failed", "closed", "cancelled"]);
const MAX_ROOTS = 12; const MAX_CHILDREN = 30;

export function buildResearchMap(input: ResearchMapInput): ResearchMap {
  const workers = new Map(input.agents.map((agent) => [agent.agentId, agent]));
  const runningJobs = input.jobs.filter((job) => !["completed", "failed", "cancelled", "lost"].includes(job.state));
  const jobNode = (job: JobRecord): MapNode => leaf("job", job.jobId, job.command.join(" ").slice(0, 120), job.state, job.runner);
  const nodeTree = (nodes: GraphNode[]): MapNode[] => nodes.slice(0, MAX_CHILDREN).map((node) => {
    const owner = node.lease?.ownerId ? workers.get(node.lease.ownerId) : undefined;
    const attempts = node.attempt > 1 || ["failed", "blocked"].includes(node.state) ? `attempt ${node.attempt}/${node.maximumAttempts}` : "";
    return { ...leaf("node", node.id, node.title, node.state, [node.type.replaceAll("_", " "), attempts].filter(Boolean).join(" · ")), children: owner ? [workerNode(owner)] : [] };
  });
  const experiments = experimentTree(input);
  const autoresearchNode = (execution: Stored): MapNode => {
    const value = execution.value; const children = experiments.get(execution.entityId) ?? [];
    const best = children.filter((child) => child.state === "promoted").map((child) => Number(/score (-?[\d.]+)/.exec(child.detail)?.[1])).filter(Number.isFinite);
    const detail = [`round ${String(value.currentRound ?? 1)}/${String(value.maximumRounds ?? "?")}`, `${children.length}/${String(value.maximumExperiments ?? "?")} experiments`, best.length ? `best ${Math.max(...best)}` : ""].filter(Boolean).join(" · ");
    const scoped = runningJobs.filter((job) => job.autoresearchId === execution.entityId).map(jobNode);
    return { kind: "autoresearch", id: execution.entityId, title: String(value.decisionQuestion ?? execution.entityId), state: execution.state, detail, progress: null, children: [...children, ...scoped].slice(0, MAX_CHILDREN) };
  };
  const nested = new Set<string>();
  const roots: MapNode[] = [];
  for (const mission of input.missions) {
    const nodes = (mission.value.nodes ?? []) as GraphNode[];
    roots.push({ kind: "mission", id: mission.entityId, title: String(mission.value.title ?? mission.entityId), state: mission.state, detail: `v${mission.version}`, progress: { done: nodes.filter((node) => node.state === "accepted").length, total: nodes.length }, children: [...nodeTree(nodes), ...runningJobs.filter((job) => job.missionId === mission.entityId && !job.directionId && !job.autoresearchId).map(jobNode)] });
  }
  for (const direction of input.directions) {
    const nodes = (direction.value.nodes ?? []) as GraphNode[];
    const children = input.autoresearch.filter((execution) => execution.value.directionId === direction.entityId).map((execution) => { nested.add(execution.entityId); return autoresearchNode(execution); });
    roots.push({ kind: "direction", id: direction.entityId, title: String(direction.value.question ?? direction.entityId), state: direction.state, detail: `v${direction.version}`, progress: { done: nodes.filter((node) => node.state === "accepted").length, total: nodes.length }, children: [...nodeTree(nodes), ...children, ...runningJobs.filter((job) => job.directionId === direction.entityId && !job.autoresearchId).map(jobNode)] });
  }
  for (const execution of input.autoresearch) if (!nested.has(execution.entityId)) roots.push(autoresearchNode(execution));
  // Live work first, then the most recently updated finished work.
  const updated = (node: MapNode) => String([...input.missions, ...input.directions, ...input.autoresearch].find((entry) => entry.entityId === node.id)?.value.updatedAt ?? "");
  roots.sort((left, right) => Number(TERMINAL.has(left.state)) - Number(TERMINAL.has(right.state)) || updated(right).localeCompare(updated(left)));
  return { generatedAt: new Date().toISOString(), roots: roots.slice(0, MAX_ROOTS), workers: input.agents.map(workerNode) };
}

function leaf(kind: MapNode["kind"], id: string, title: string, state: string, detail = ""): MapNode { return { kind, id, title, state, detail, progress: null, children: [] }; }

function workerNode(agent: AgentInspection): MapNode {
  const model = agent.modelId ? `${agent.modelId}${agent.thinkingLevel ? `:${agent.thinkingLevel}` : ""}` : "";
  const context = agent.contextPercent === null ? "" : `ctx ${Math.round(agent.contextPercent)}%`;
  return leaf("worker", agent.agentId, agent.role === "nosh" ? "project agent (chat)" : agent.role.replaceAll("_", " "), agent.currentTool ? `running ${agent.currentTool}` : agent.status, [model, context].filter(Boolean).join(" · "));
}

/** Experiment states from durable records: proposed → implementing → evaluating → promoted / held / rejected, or failed. */
function experimentTree(input: ResearchMapInput): Map<string, MapNode[]> {
  const byExecution = new Map<string, MapNode[]>();
  const failed = new Map(input.events.filter((event) => event.type === "autoresearch.experiment_failed").map((event) => [String((event.payload as { experimentId?: unknown }).experimentId), String((event.payload as { phase?: unknown }).phase ?? "failed")]));
  const of = (schema: string) => input.submitted.filter((entry) => entry.record.$schema === schemaUri(schema)).map((entry) => entry.record);
  const results = new Map(of("experiment-result").map((record) => [String(record.experimentId), record]));
  const runs = new Set(of("run-manifest").map((record) => String(record.experimentId)));
  const working = new Set(input.agents.map((agent) => agent.experimentId).filter(Boolean));
  for (const proposal of of("experiment-proposal")) {
    const experimentId = String(proposal.experimentId); const autoresearchId = String(proposal.autoresearchId);
    const result = results.get(experimentId) as { promotionDecision?: string; comparison?: { candidateScore?: number; improvement?: number }; guardrails?: Array<{ passed?: boolean }> } | undefined;
    const state = result ? String(result.promotionDecision) : failed.has(experimentId) ? "failed" : runs.has(experimentId) ? "evaluating" : working.has(experimentId) ? "implementing" : "proposed";
    const score = result?.comparison?.candidateScore; const delta = result?.comparison?.improvement;
    const guardrail = result?.guardrails?.some((entry) => entry.passed === false) ? "guardrail failed" : "";
    const detail = [`round ${String(proposal.round)}`, typeof score === "number" ? `score ${score}${typeof delta === "number" ? ` (${delta >= 0 ? "+" : ""}${Number(delta.toFixed(4))})` : ""}` : "", guardrail, failed.has(experimentId) ? `failed in ${failed.get(experimentId)}` : ""].filter(Boolean).join(" · ");
    const list = byExecution.get(autoresearchId) ?? []; byExecution.set(autoresearchId, list);
    if (!list.some((entry) => entry.id === experimentId)) list.push(leaf("experiment", experimentId, String(proposal.hypothesis ?? experimentId), state, detail));
  }
  return byExecution;
}
