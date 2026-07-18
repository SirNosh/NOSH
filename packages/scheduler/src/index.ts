import type { GraphNode, VersionedDag } from "@nosh/graph";
import { sha256, type JsonValue } from "@nosh/wire";

export type Resources = { agents: number; gpuJobs: number; gpuSeconds: number; modelTokens: number; diskBytes: number };
export type SchedulingContext = { paused: boolean; approvals: Set<string>; workspaceConflicts: Set<string>; availableRoles: Set<string>; available: Resources };

export class Scheduler {
  private activeMissionByProject = new Map<string, string>();
  private allocation = new Map<string, number>();

  activateMission(projectId: string, missionId: string): void {
    const active = this.activeMissionByProject.get(projectId);
    if (active && active !== missionId) throw new Error("Version 1 allows one active Mission per Project");
    this.activeMissionByProject.set(projectId, missionId);
  }
  releaseMission(projectId: string, missionId: string): void { if (this.activeMissionByProject.get(projectId) === missionId) this.activeMissionByProject.delete(projectId); }

  frontier(graph: VersionedDag, context: SchedulingContext, now = Date.now()): GraphNode[] {
    if (context.paused || context.available.agents < 1) return [];
    return graph.runnable().filter((node) => !context.workspaceConflicts.has(node.id) && (context.availableRoles.has(node.type) || context.availableRoles.has("general_worker"))).sort((left, right) => score(right, this.allocation.get(right.id) ?? 0, now) - score(left, this.allocation.get(left.id) ?? 0, now));
  }
  recordAllocation(nodeId: string): void { this.allocation.set(nodeId, (this.allocation.get(nodeId) ?? 0) + 1); }
  restoreAllocations(distribution: Record<string, number>): void { for (const [nodeId, count] of Object.entries(distribution)) if (Number.isInteger(count) && count >= 0) this.allocation.set(nodeId, count); }
  distribution(): Record<string, number> { return Object.fromEntries(this.allocation); }
}

export type AttemptInput = {
  projectId: string; missionId: string | null; directionId: string | null; nodeId: string; parentExperimentId: string | null;
  taskType: string; hypothesisFamilyTags: string[]; commandClass: string; inScopeFiles: string[]; inputArtifactHashes: string[];
  diffHash: string | null; evaluationContractHash: string | null; intendedDecision: string; expectedOutputType: string;
};
export type ProgressDelta = { graphTransition?: string; artifactHash?: string; evidenceId?: string; uncertaintyClosed?: string; metricMeasured?: string; blockerResolved?: string; defectClosed?: string; decisionId?: string; claimTransition?: string; negativeHypothesisId?: string };
export type FocusAlarm = { nodeId: string; level: "reanchor" | "differentiate" | "bound" | "review" | "rotate" | "redirect" | "repair" | "escalate"; reasons: string[]; inputs: JsonValue };

export class FocusGovernor {
  private attempts = new Map<string, Array<{ fingerprint: string; progress: boolean; at: number; reason: string | null }>>();

  fingerprint(input: AttemptInput): string {
    return sha256({ ...input, hypothesisFamilyTags: [...input.hypothesisFamilyTags].sort(), inScopeFiles: [...input.inScopeFiles].sort(), inputArtifactHashes: [...input.inputArtifactHashes].sort() });
  }

  record(input: AttemptInput, delta: ProgressDelta, overrideReason: "replication" | "transient_retry" | "controlled_seed" | null = null): { fingerprint: string; progress: boolean; alarm: FocusAlarm | null } {
    const fingerprint = this.fingerprint(input); const progress = Object.values(delta).some(Boolean); const history = this.attempts.get(input.nodeId) ?? [];
    history.push({ fingerprint, progress, at: Date.now(), reason: overrideReason }); this.attempts.set(input.nodeId, history);
    const recent = history.slice(-4); const identical = recent.filter((attempt) => attempt.fingerprint === fingerprint && !attempt.reason).length;
    const noProgress = recent.filter((attempt) => !attempt.progress).length;
    const reasons: string[] = [];
    if (identical >= 2) reasons.push("repeated_attempt_fingerprint");
    if (noProgress >= 4) reasons.push("four_actions_without_durable_progress");
    if (history.length >= 3 && history.slice(-3).every((attempt) => !attempt.progress)) reasons.push("three_failed_or_nonprogressing_attempts");
    if (!reasons.length) return { fingerprint, progress, alarm: null };
    const level: FocusAlarm["level"] = identical >= 2 ? "differentiate" : noProgress >= 4 ? "review" : "reanchor";
    return { fingerprint, progress, alarm: { nodeId: input.nodeId, level, reasons, inputs: { identical, noProgress, window: recent.length, fingerprint } } };
  }

  clear(nodeId: string, proof: { newFingerprint?: string; reviewId?: string; graphVersion?: number; redirectedNodeId?: string }): void {
    if (!proof.newFingerprint && !proof.reviewId && !proof.graphVersion && !proof.redirectedNodeId) throw new Error("A Director cannot clear a focus alarm by reflection alone");
    this.attempts.delete(nodeId);
  }
}

export class BudgetLedger {
  private used: Resources = { agents: 0, gpuJobs: 0, gpuSeconds: 0, modelTokens: 0, diskBytes: 0 };
  constructor(readonly limits: Resources) {}
  consume(delta: Partial<Resources>): Resources { const next = { ...this.used }; for (const key of Object.keys(next) as Array<keyof Resources>) next[key] += delta[key] ?? 0; for (const key of Object.keys(next) as Array<keyof Resources>) if (next[key] > this.limits[key]) throw new Error(`Budget exceeded: ${key}`); this.used = next; return { ...this.used }; }
  remaining(): Resources { return { agents: this.limits.agents - this.used.agents, gpuJobs: this.limits.gpuJobs - this.used.gpuJobs, gpuSeconds: this.limits.gpuSeconds - this.used.gpuSeconds, modelTokens: this.limits.modelTokens - this.used.modelTokens, diskBytes: this.limits.diskBytes - this.used.diskBytes }; }
}

function score(node: GraphNode, allocations: number, now: number): number { const ageHours = Math.max(0, now - Date.parse(node.createdAt)) / 3_600_000; return node.criticalWeight * 100 + node.priority * 10 + Math.min(ageHours, 72) - allocations * 30; }
