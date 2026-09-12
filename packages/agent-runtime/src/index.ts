import { schemaUris, validateRecord, type EventEnvelope } from "@nosh/wire";

export const agentRoles = ["librarian_researcher", "general_worker", "reviewer"] as const;
export type AgentRole = (typeof agentRoles)[number];

const toolSchemas: Record<string, readonly string[]> = {
  nosh_task_acknowledge: ["task-acknowledgement"],
  nosh_progress_emit: ["progress-update"],
  nosh_response_submit: ["general-worker-completion", "librarian-completion", "task-failure", "mission-director-cycle", "research-director-cycle"],
  nosh_review_submit: ["review-verdict"],
  nosh_blocker_submit: ["blocker"],
  nosh_graph_change_propose: ["graph-change-proposal"],
  nosh_delegation_request: ["delegation-request"],
  nosh_handoff_create: ["handoff"],
  nosh_handoff_teachback: ["handoff-teachback"],
  nosh_experiment_propose: ["experiment-proposal"],
  nosh_evidence_submit: ["evidence"],
  nosh_episode_submit: ["episode-draft"],
  nosh_runtime_instruct: ["runtime-instruction"],
  nosh_project_contract_submit: ["project-contract"],
};

export class StructuredSubmissionGate {
  private readonly invalidAttempts = new Map<string, number>();

  submit(tool: string, attemptKey: string, record: unknown): { ok: true; record: unknown } | { ok: false; retryAllowed: boolean; errors: Array<{ pointer: string; code: string; message: string }> } {
    const uri = typeof record === "object" && record !== null && "$schema" in record ? String((record as { $schema: unknown }).$schema) : "";
    const names = toolSchemas[tool];
    const allowed = names?.some((name) => uri === `https://nosh.dev/schemas/${name}/v1`) ?? false;
    const result = allowed ? validateRecord(uri, record) : { ok: false as const, errors: [{ pointer: "/$schema", code: "wrong_tool", message: `Schema ${uri || "<missing>"} is not accepted by ${tool}` }] };
    if (result.ok) {
      this.invalidAttempts.delete(attemptKey);
      return { ok: true, record: result.value };
    }
    const count = (this.invalidAttempts.get(attemptKey) ?? 0) + 1;
    this.invalidAttempts.set(attemptKey, count);
    return { ok: false, retryAllowed: count === 1, errors: result.errors };
  }

  supportedSchemas(): string[] {
    return schemaUris();
  }
}

export type HandoffState = {
  handoffId: string;
  logicalOwnerId: string;
  goalStack: { projectGoalId: string; missionCriterionIds: string[]; directionQuestionId: string | null; currentGraphNodeId: string | null };
  observedVersions: Record<string, number | string>;
  branchHead: string;
  defectIds: string[];
  blockerIds: string[];
  readyNodeIds: string[];
};

export function validateTeachback(state: HandoffState, teachback: {
  handoffId: string;
  logicalOwnerId: string;
  understoodGoalStack: HandoffState["goalStack"];
  decision: string;
  observedVersions: Record<string, number | string>;
  observedBranchHead: string;
  acknowledgedDefectIds: string[];
  acknowledgedBlockerIds: string[];
  selectedNextNodeId: string | null;
  conflicts: string[];
}): { ok: true } | { ok: false; conflicts: string[] } {
  const conflicts: string[] = [];
  if (teachback.handoffId !== state.handoffId) conflicts.push("handoffId");
  if (teachback.logicalOwnerId !== state.logicalOwnerId) conflicts.push("logicalOwnerId");
  if (!sameGoalStack(teachback.understoodGoalStack, state.goalStack)) conflicts.push("understoodGoalStack");
  if (!sameVersions(teachback.observedVersions, state.observedVersions)) conflicts.push("observedVersions");
  if (teachback.observedBranchHead !== state.branchHead) conflicts.push("observedBranchHead");
  if (!sameSet(teachback.acknowledgedDefectIds, state.defectIds)) conflicts.push("acknowledgedDefectIds");
  if (!sameSet(teachback.acknowledgedBlockerIds, state.blockerIds)) conflicts.push("acknowledgedBlockerIds");
  if (teachback.selectedNextNodeId !== null && !state.readyNodeIds.includes(teachback.selectedNextNodeId)) conflicts.push("selectedNextNodeId");
  if (teachback.decision === "accepted" && teachback.conflicts.length) conflicts.push("decision");
  if ((teachback.decision === "rejected_conflict" || teachback.decision === "clarification_required") && !teachback.conflicts.length) conflicts.push("conflicts");
  if (!["accepted", "rejected_conflict", "clarification_required"].includes(teachback.decision)) conflicts.push("decision");
  return conflicts.length ? { ok: false, conflicts: [...new Set(conflicts)] } : { ok: true };
}

export function directorInbox(events: EventEnvelope[]): EventEnvelope[] {
  const relevant = new Set(["agent.progress", "agent.blocked", "agent.anomaly", "agent.completed", "review.completed", "job.completed", "budget.warning"]);
  return events.filter((event) => relevant.has(event.type));
}

export function canDelegateDirectly(role: string): boolean {
  return role === "nosh" || role === "mission_director" || role === "research_director";
}

function sameSet(left: string[], right: string[]): boolean {
  return left.length === right.length && [...left].sort().every((value, index) => value === [...right].sort()[index]);
}
function sameGoalStack(left: HandoffState["goalStack"], right: HandoffState["goalStack"]): boolean {
  return left.projectGoalId === right.projectGoalId && sameSet(left.missionCriterionIds, right.missionCriterionIds) && left.directionQuestionId === right.directionQuestionId && left.currentGraphNodeId === right.currentGraphNodeId;
}
function sameVersions(left: Record<string, number | string>, right: Record<string, number | string>): boolean {
  const leftKeys = Object.keys(left).sort(); const rightKeys = Object.keys(right).sort();
  return leftKeys.length === rightKeys.length && leftKeys.every((key, index) => key === rightKeys[index] && left[key] === right[key]);
}
