/**
 * Deterministic director briefs (spec: directors receive compact structured state "rendered from
 * typed fields without a separate summarization LLM"; they decide semantics, the daemon executes).
 * A brief is the director's whole view of its scope: goal stack, frozen contract, graph, ledgers,
 * evidence, and the event digest since its previous cycle. No raw logs, no transcripts, no tools.
 * Values the model must not invent (cycle ID, timestamps, event range) are allocated here.
 */
import { createId } from "@nosh/core";
import { readProjectContract } from "@nosh/evidence";
import type { GraphNode } from "@nosh/graph";
import type { RegisteredProject } from "@nosh/persistence";
import { canonicalJson, episodeTypeForRole, schemaDocumentPath, schemaUri, type EventEnvelope, type JsonValue } from "@nosh/wire";
import type { DirectionProjection, MissionProjection, ResearchControl, Stored } from "./research-control.js";

const MAX_BRIEF = 14_000;
const MAX_ITEMS = 12;

export type BriefIds = { cycleId: string; submittedAt: string; consumedEventRange: { fromSequence: number; toSequence: number } };
type Record_ = Record<string, unknown>;

function clip(value: unknown, limit: number): string {
  const text = (typeof value === "string" ? value : canonicalJson(value as JsonValue)).replace(/\s+/g, " ").trim();
  return text.length > limit ? `${text.slice(0, limit - 1)}…` : text;
}
function northStar(project: RegisteredProject): string {
  try { const contract = readProjectContract(project.repositoryRoot); return `${contract.northStar.question} (decision use: ${contract.northStar.decisionUse})`; }
  catch { return "unavailable: the active Project contract could not be read"; }
}
function nodeLine(node: GraphNode): string {
  const deps = node.hardDependencies.length ? ` after ${node.hardDependencies.join(",")}` : "";
  return `- ${node.id} [${node.state}] ${clip(node.title, 120)} (${node.type}, attempt ${node.attempt}/${node.maximumAttempts}${node.required ? ", required" : ""}${deps})`;
}

/** Allocate the cycle identity and the event range consumed since this scope's previous cycle record. */
function allocate(research: ResearchControl, projectId: string, previous: Record_ | undefined): BriefIds & { events: EventEnvelope[] } {
  const prior = (previous?.consumedEventRange as { toSequence?: number } | undefined)?.toSequence ?? 0;
  const events = research.eventsAfter(projectId, prior);
  const last = events.at(-1)?.sequence ?? prior;
  return { cycleId: `cycle_${createId("cmd").slice(4)}`, submittedAt: new Date().toISOString(), consumedEventRange: { fromSequence: events.length ? prior + 1 : prior, toSequence: last }, events };
}

/** Typed digest of scoped events: counts by type plus notable failures, transitions, and verdicts. */
function digest(events: EventEnvelope[], inScope: (event: EventEnvelope) => boolean): string[] {
  const scoped = events.filter(inScope);
  if (!scoped.length) return ["- no new events in this scope since the previous cycle"];
  const counts = new Map<string, number>();
  for (const event of scoped) counts.set(event.type, (counts.get(event.type) ?? 0) + 1);
  const lines = [`- counts: ${[...counts].map(([type, count]) => `${type}×${count}`).join(", ")}`];
  const notable = scoped.filter((event) => /fail|block|error|reject|cancel|accepted|baseline|review|state_changed|^direction\.|^mission\./.test(event.type) && !/^agent\.(text_delta|tool_)/.test(event.type));
  for (const event of notable.slice(-MAX_ITEMS)) lines.push(`- #${event.sequence} ${event.type} ${clip(event.payload ?? {}, 160)}`);
  return lines;
}
function steeringLines(research: ResearchControl, projectId: string, missionId: string): string[] {
  const steers = research.eventsAfter(projectId, 0).filter((event) => event.type === "mission.steering_applied" && event.scope.missionId === missionId).slice(-5);
  return steers.length ? steers.map((event) => `- #${event.sequence}: ${clip((event.payload as { message?: unknown }).message, 600)}`) : ["- none"];
}

function verdictLines(research: ResearchControl, projectId: string, targetIds: Set<string>): string[] {
  const verdicts = research.records(projectId, "review-verdict").filter((record) => targetIds.has(String((record.target as Record_ | undefined)?.targetId ?? "")));
  const lines: string[] = [];
  for (const review of verdicts.slice(-MAX_ITEMS)) {
    const target = review.target as Record_;
    lines.push(`- ${String(review.reviewId)} ${String(review.verdict)} on ${String(target.targetId)} v${String(target.targetVersion)}: ${clip(review.summary, 140)}`);
    for (const defect of ((review.defects as Record_[] | undefined) ?? []).filter((item) => item.severity === "blocking" || item.severity === "major").slice(0, 3)) lines.push(`  - defect ${String(defect.defectId)} (${String(defect.severity)}): ${clip(defect.title, 120)}`);
  }
  return lines.length ? lines : ["- none"];
}
function finish(sections: string[], instructions: string[]): string {
  let body = sections.join("\n");
  if (body.length > MAX_BRIEF) body = `${body.slice(0, MAX_BRIEF - 40)}\n… [brief clipped at ${MAX_BRIEF} chars]`;
  return `${body}\n\n${instructions.join("\n")}`;
}

/**
 * Records are given as complete, schema-valid templates: the daemon fills identity, versions, hashes,
 * ranges, and timestamps; the director edits only judgment fields. Paraphrased field names were the
 * observed failure mode when the contract was described in prose.
 */
function templateInstructions(cycle: Record_, editable: string, role: "research_director" | "mission_director"): string[] {
  const episode = { $schema: schemaUri("episode-draft"), schemaVersion: 1, episodeType: episodeTypeForRole(role), summary: "EDIT: one-paragraph summary of this cycle's decision.", facts: [{ statement: "EDIT: a fact taken from the brief.", evidenceRefs: [], confidence: "high" }], decisions: [{ statement: "EDIT: the decision.", rationale: "EDIT: why.", evidenceRefs: [] }], artifactIds: [], evidenceIds: [], changedFiles: [], unresolvedQuestions: [], recommendedNextActions: [{ operation: "THREAD_STEP", objective: "EDIT: the next bounded action." }] };
  return [
    "Return the host terminal JSON envelope whose records are exactly these two objects. Keep every key, and keep every value that is not an EDIT placeholder unless listed as editable below. Replace each \"EDIT: ...\" string. New reference values use one lowercase prefix, exactly one underscore, then lowercase letters, digits, '.', ':' or '-' (for example uncertainty_baseline-pending).",
    `Editable judgment fields in record 1: ${editable}. Record 2 is your episode; fill facts, decisions, unresolvedQuestions, and recommendedNextActions from this brief (artifactIds/evidenceIds only with IDs listed above).`,
    `Record 1 (${schemaDocumentPath(String(cycle.$schema).split("/").at(-2)!)}):`, JSON.stringify(cycle),
    `Record 2 (${schemaDocumentPath("episode-draft")}):`, JSON.stringify(episode),
  ];
}

/** Every Autoresearch this Direction has consumed: completion, measured results, and Evidence statements. Shared by the Director brief and Direction reviewer nodes. */
export function directionAutoresearchOutcomes(research: ResearchControl, projectId: string, directionId: string): { evidenceIds: string[]; digest: string } {
  const consumed = new Set(research.eventsAfter(projectId, 0).filter((entry) => entry.type === "direction.hypothesis_resolved" && entry.scope.directionId === directionId).map((entry) => String((entry.payload as { autoresearchId?: string }).autoresearchId)));
  const completions = research.records(projectId, "autoresearch-completion-packet").filter((record) => consumed.has(String(record.autoresearchId))) as Array<Record<string, JsonValue>>;
  const evidenceIds = [...new Set(completions.flatMap((completion) => Array.isArray(completion.evidenceIds) ? completion.evidenceIds.map(String) : []))];
  const results = research.records(projectId, "experiment-result").filter((record) => consumed.has(String(record.autoresearchId))) as Array<Record<string, JsonValue>>;
  const evidence = research.records(projectId, "evidence").filter((record) => evidenceIds.includes(String(record.evidenceId))) as Array<Record<string, JsonValue>>;
  const digest = completions.length ? JSON.stringify({
    completions: completions.map((completion) => ({ autoresearchId: completion.autoresearchId, terminalReason: completion.terminalReason, decisionAnswer: completion.decisionAnswer, bestExperimentId: completion.bestExperimentId, failedHypothesisIds: completion.failedHypothesisIds })),
    results: results.map((result) => ({ experimentId: result.experimentId, evaluatedCommit: result.evaluatedCommit, metrics: result.metrics, guardrails: result.guardrails, comparison: result.comparison, reviewVerdict: result.reviewVerdict, promotionRecommendation: result.promotionRecommendation, promotionDecision: result.promotionDecision })),
    evidence: evidence.map((record) => ({ evidenceId: record.evidenceId, polarity: record.polarity, statement: record.statement, quality: record.quality })),
  }).slice(0, 12_000) : "";
  return { evidenceIds, digest };
}

export function directionBrief(input: { research: ResearchControl; project: RegisteredProject; direction: Stored<DirectionProjection>; node: GraphNode; agentId: string }): { prompt: string; ids: BriefIds } {
  const { research, project, direction, node, agentId } = input; const value = direction.value; const projectId = project.projectId;
  const previous = research.records(projectId, "research-director-cycle").filter((record) => record.directionId === direction.entityId).at(-1);
  const { events, ...ids } = allocate(research, projectId, previous);
  const nodeIds = new Set(value.nodes.map((item) => item.id).concat(direction.entityId));
  const evidence = research.records(projectId, "evidence").filter((record) => record.evaluationContractHash === value.evaluationContractHash).slice(-MAX_ITEMS);
  const failed = value.nodes.filter((item) => ["failed", "blocked"].includes(item.state));
  const sections = [
    `DIRECTION BRIEF (daemon-rendered from durable state at ${ids.submittedAt}; authoritative for this cycle)`,
    "", "Goal stack:",
    `- Project north star: ${clip(northStar(project), 400)}`,
    `- Direction ${direction.entityId} question ${value.questionId}: ${clip(value.question, 400)}`,
    `- Decision use: ${clip(value.decisionUse, 300)}`,
    `- Falsifiability: supports if ${clip(value.falsifiability.supportingOutcome, 160)}; refutes if ${clip(value.falsifiability.refutingOutcome, 160)}; inconclusive if ${clip(value.falsifiability.inconclusiveOutcome, 160)}`,
    "", `State: ${direction.state}, version ${direction.version}, graph version ${value.graphVersion}`,
    `Frozen evaluation contract ${value.evaluationContractHash}: ${clip(value.evaluationContract, 1_500)}`,
    `Baseline: ${value.acceptedBaseline ? `accepted at commit ${value.acceptedBaseline.commit} (review ${value.acceptedBaseline.reviewId})` : `not yet accepted (planned experiment ${value.plannedBaselineExperimentId})`}`,
    `Stopping rules: ${value.stoppingRules.map((rule) => `${rule.ruleId}: ${clip(rule.statement, 140)}`).join("; ") || "none"}`,
    "", "Graph nodes:", ...value.nodes.map(nodeLine),
    `Selected by the deterministic scheduler to run next: ${node.id} (${clip(node.title, 160)})`,
    "", "Failure ledger:", ...(failed.length ? failed.map(nodeLine) : ["- no failed or blocked nodes"]),
    "", "Review verdicts on this Direction:", ...verdictLines(research, projectId, nodeIds),
    "", "Resolved Autoresearch outcomes (reviewer nodes receive these as required Evidence):", directionAutoresearchOutcomes(research, projectId, direction.entityId).digest || "- none yet",
    "", "Evidence under this contract:", ...(evidence.length ? evidence.map((record) => `- ${String(record.evidenceId)} ${String(record.polarity)}: ${clip(record.statement, 160)}`) : ["- none yet"]),
    "", `Event digest for #${ids.consumedEventRange.fromSequence}..#${ids.consumedEventRange.toSequence}:`, ...digest(events, (event) => event.scope.directionId === direction.entityId),
  ];
  const cycle = { $schema: schemaUri("research-director-cycle"), schemaVersion: 1, cycleId: ids.cycleId, directionId: direction.entityId, directorAgentId: agentId, observedGraphVersion: value.graphVersion, evaluationContractHash: value.evaluationContractHash, consumedEventRange: ids.consumedEventRange, questionCheck: { questionId: value.questionId, currentDisposition: "unresolved", remainingUncertaintyIds: ["uncertainty_question-open"], currentWorkContributes: true }, acceptedFrontierExperimentIds: [], failedHypothesisIdsAdded: [], decisions: [{ action: "run_node", targetId: node.id, reason: "EDIT: why running this node now advances the question." }], createdTaskIds: [], invokedAutoresearchIds: [], graphChangeProposalIds: [], directionSuggestionIds: [], userInputRequestId: null, nextWake: { type: "event", deadline: null }, submittedAt: ids.submittedAt };
  const instructions = [
    "Your cycle: decide whether running the selected node now advances the Direction question within the frozen contract. You do not execute work, lease nodes, or read files; the daemon runs the node, its worker, Job, postflight, and independent Review after you. Base every statement only on this brief.",
    ...templateInstructions(cycle, "questionCheck.currentDisposition (unresolved | supported | refuted | inconclusive), questionCheck.remainingUncertaintyIds, questionCheck.currentWorkContributes (false halts the Direction for user attention), and decisions[].reason (decisions may also add stop_direction or request_review entries targeting IDs above)", "research_director"),
  ];
  return { prompt: finish(sections, instructions), ids };
}

export function missionBrief(input: { research: ResearchControl; project: RegisteredProject; mission: Stored<MissionProjection>; node: GraphNode; agentId: string }): { prompt: string; ids: BriefIds } {
  const { research, project, mission, node, agentId } = input; const value = mission.value; const projectId = project.projectId;
  const previous = research.records(projectId, "mission-director-cycle").filter((record) => record.missionId === mission.entityId).at(-1);
  const { events, ...ids } = allocate(research, projectId, previous);
  const nodeIds = new Set(value.nodes.map((item) => item.id).concat(mission.entityId));
  const unsatisfied = [...new Set(value.nodes.filter((item) => item.required && item.state !== "accepted").flatMap((item) => item.criterionIds))];
  const ready = value.nodes.filter((item) => item.state === "ready").map((item) => item.id);
  const failed = value.nodes.filter((item) => ["failed", "blocked"].includes(item.state));
  const sections = [
    `MISSION BRIEF (daemon-rendered from durable state at ${ids.submittedAt}; authoritative for this cycle)`,
    "", "Goal stack:",
    `- Project north star: ${clip(northStar(project), 400)}`,
    `- Mission ${mission.entityId} "${clip(value.title, 160)}": ${clip(value.objective, 500)}`,
    `- Deliverables: ${value.deliverables.map((item) => clip(item, 140)).join("; ")}`,
    `- Success criteria: ${value.successCriteria.map((item) => clip(item, 140)).join("; ")}`,
    `- Non-objectives: ${value.nonObjectives.map((item) => clip(item, 140)).join("; ") || "none"}`,
    "", `State: ${mission.state}, version ${mission.version}, graph version ${value.graphVersion}`,
    `Unsatisfied criterion IDs: ${unsatisfied.join(", ") || "none"}`, `Ready node IDs: ${ready.join(", ") || "none"}`,
    "", "Graph nodes:", ...value.nodes.map(nodeLine),
    `Selected by the deterministic scheduler to run next: ${node.id} (${clip(node.title, 160)})`,
    "", "Failure ledger:", ...(failed.length ? failed.map(nodeLine) : ["- no failed or blocked nodes"]),
    "", "User steering (most recent last; binding within the approved Mission contract):", ...steeringLines(research, projectId, mission.entityId),
    "", "Review verdicts on this Mission:", ...verdictLines(research, projectId, nodeIds),
    "", `Event digest for #${ids.consumedEventRange.fromSequence}..#${ids.consumedEventRange.toSequence}:`, ...digest(events, (event) => event.scope.missionId === mission.entityId),
  ];
  const cycle = { $schema: schemaUri("mission-director-cycle"), schemaVersion: 1, cycleId: ids.cycleId, missionId: mission.entityId, directorAgentId: agentId, observedObjectiveVersion: 1, observedGraphVersion: value.graphVersion, consumedEventRange: ids.consumedEventRange, northStarCheck: { missionObjective: value.objective, unsatisfiedCriterionIds: unsatisfied, criticalPathNodeIds: [node.id], currentWorkContributes: true }, portfolioCheck: { readyNodeIds: ready, starvedNodeIds: [], overfocusedNodeId: null, fairnessAction: "fairness_none" }, decisions: [{ action: "run_node", targetId: node.id, reason: "EDIT: why running this node now advances the objective." }], createdTaskIds: [], invokedDirectionIds: [], invokedAutoresearchIds: [], graphChangeProposalIds: [], userInputRequestId: null, budgetWarnings: [], blockerIds: [], nextWake: { type: "event", deadline: null }, submittedAt: ids.submittedAt };
  const instructions = [
    "Your cycle: decide whether running the selected node now advances the Mission objective and its unsatisfied criteria. You do not execute work, lease nodes, read files, or claim completion; the daemon runs the node, its worker, postflight, and independent Review after you. Base every statement only on this brief.",
    ...templateInstructions(cycle, "northStarCheck.criticalPathNodeIds, northStarCheck.currentWorkContributes (false halts the Mission for user attention), portfolioCheck.starvedNodeIds/overfocusedNodeId, budgetWarnings, and decisions[].reason", "mission_director"),
  ];
  return { prompt: finish(sections, instructions), ids };
}
