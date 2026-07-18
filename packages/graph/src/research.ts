import { sha256, type JsonValue } from "@nosh/wire";
import { VersionedDag, type GraphNode } from "./index.js";

export type Experiment = {
  experimentId: string; parentExperimentId: string | null; ideaFingerprint: string; attemptFingerprint: string; evaluationContractHash: string;
  branch: string; evaluatedCommit: string | null; depth: number; state: "proposed" | "preparing" | "ready" | "running" | "evaluating" | "accepted" | "rejected" | "inconclusive" | "failed" | "cancelled";
  reviewId: string | null; reviewerAgentId: string | null; guardrailsPass: boolean | null;
};

export class ExperimentTree {
  private readonly experiments = new Map<string, Experiment>();
  private readonly reviewers = new Set<string>();
  constructor(readonly evaluationContractHash: string, readonly maximumDepth: number, readonly maximumChildrenPerParent: number) {}

  add(experiment: Experiment): Experiment {
    if (experiment.evaluationContractHash !== this.evaluationContractHash) throw new Error("Evaluation contract is frozen");
    if (this.experiments.has(experiment.experimentId)) throw new Error("Experiment already exists");
    if ([...this.experiments.values()].some((existing) => existing.ideaFingerprint === experiment.ideaFingerprint)) throw new Error("Duplicate experiment idea fingerprint");
    if (experiment.parentExperimentId === null) {
      if (this.experiments.size) throw new Error("Experiment tree has exactly one root");
      if (experiment.depth !== 0) throw new Error("Root depth must be zero");
    } else {
      const parent = this.required(experiment.parentExperimentId);
      if (!parent.evaluatedCommit || !["accepted", "rejected", "inconclusive"].includes(parent.state)) throw new Error("Parent must be evaluated before adding a child");
      if (experiment.depth !== parent.depth + 1 || experiment.depth > this.maximumDepth) throw new Error("Experiment depth violates the tree contract");
      if ([...this.experiments.values()].filter((child) => child.parentExperimentId === parent.experimentId).length >= this.maximumChildrenPerParent) throw new Error("Parent child limit exceeded");
    }
    this.experiments.set(experiment.experimentId, { ...experiment }); return { ...experiment };
  }

  evaluate(experimentId: string, commit: string, state: "accepted" | "rejected" | "inconclusive" | "failed", review: { reviewId: string; reviewerAgentId: string; verdict: string } | null, guardrailsPass: boolean): Experiment {
    const experiment = this.required(experimentId);
    if (experiment.evaluatedCommit && experiment.evaluatedCommit !== commit) throw new Error("Evaluated experiment commit is immutable");
    if (state === "accepted" && (review?.verdict !== "PASS" || !guardrailsPass)) throw new Error("Acceptance requires passing Review and guardrails");
    if (review && this.reviewers.has(review.reviewerAgentId)) throw new Error("Each material round requires a fresh Reviewer session");
    experiment.evaluatedCommit = commit; experiment.state = state; experiment.reviewId = review?.reviewId ?? null; experiment.reviewerAgentId = review?.reviewerAgentId ?? null; experiment.guardrailsPass = guardrailsPass;
    if (review) this.reviewers.add(review.reviewerAgentId);
    return { ...experiment };
  }

  get(experimentId: string): Experiment { return { ...this.required(experimentId) }; }
  all(): Experiment[] { return [...this.experiments.values()].map((experiment) => ({ ...experiment })); }
  frontier(): Experiment[] { return this.all().filter((experiment) => experiment.state === "accepted" && !this.all().some((child) => child.parentExperimentId === experiment.experimentId && child.state === "accepted")); }
  shapeWarnings(): string[] {
    const all = this.all(); const warnings: string[] = [];
    const root = all.find((experiment) => experiment.parentExperimentId === null);
    if (root && all.filter((experiment) => experiment.parentExperimentId === root.experimentId).length > this.maximumChildrenPerParent) warnings.push("flat_fan");
    const longest = Math.max(0, ...all.map((experiment) => experiment.depth));
    if (longest >= 4 && all.filter((experiment) => experiment.parentExperimentId && all.filter((child) => child.parentExperimentId === experiment.parentExperimentId).length > 1).length === 0) warnings.push("unsupported_noodle");
    return warnings;
  }
  private required(id: string): Experiment { const experiment = this.experiments.get(id); if (!experiment) throw new Error(`Unknown experiment ${id}`); return experiment; }
}

export type DirectionState = "draft" | "proposed" | "active" | "paused" | "blocked" | "reviewing" | "closed" | "rejected" | "stopped";
export class DirectionEngine {
  state: DirectionState = "draft";
  private baselineAccepted = false;
  private readonly autoresearch = new Map<string, { decisionQuestion: string; familyTags: string[]; scope: string[]; fingerprint: string; terminal: boolean }>();
  readonly graph: VersionedDag;
  constructor(readonly directionId: string, readonly evaluationContract: JsonValue, nodes: GraphNode[], readonly maximumAutoresearchExecutions: number) { this.graph = new VersionedDag(directionId, nodes); }
  evaluationContractHash(): string { return sha256(this.evaluationContract); }
  propose(): void { this.move("proposed"); }
  activate(): void { this.move("active"); }
  pause(): void { this.move("paused"); }
  acceptBaseline(contractHash: string, reviewVerdict: string, immutableCommit: string): void { if (contractHash !== this.evaluationContractHash() || reviewVerdict !== "PASS" || immutableCommit.length < 7) throw new Error("Baseline acceptance gate failed"); this.baselineAccepted = true; }
  startAutoresearch(input: { id: string; decisionQuestion: string; familyTags: string[]; scope: string[]; evaluationContractHash: string; reviewerApprovedDuplicate?: boolean }): void {
    if (this.state !== "active" || !this.baselineAccepted) throw new Error("Direction must be active with an accepted baseline");
    if (input.evaluationContractHash !== this.evaluationContractHash()) throw new Error("Autoresearch cannot change the evaluation contract");
    if (this.autoresearch.size >= this.maximumAutoresearchExecutions) throw new Error("Direction Autoresearch budget exhausted");
    const fingerprint = sha256({ decisionQuestion: input.decisionQuestion.trim().toLowerCase(), familyTags: [...input.familyTags].sort(), scope: [...input.scope].sort(), evaluationContractHash: input.evaluationContractHash });
    if ([...this.autoresearch.values()].some((execution) => execution.fingerprint === fingerprint) && !input.reviewerApprovedDuplicate) throw new Error("Autoresearch execution is not meaningfully separate");
    this.autoresearch.set(input.id, { decisionQuestion: input.decisionQuestion, familyTags: [...input.familyTags], scope: [...input.scope], fingerprint, terminal: false });
  }
  finishAutoresearch(id: string): void { const execution = this.autoresearch.get(id); if (!execution) throw new Error("Unknown Autoresearch execution"); execution.terminal = true; }
  close(input: { disposition: "supported" | "refuted" | "inconclusive"; reviewVerdict: string; evidenceIds: string[]; openBlockingDefects: string[] }): void {
    if (!this.baselineAccepted || !this.graph.completionReady() || [...this.autoresearch.values()].some((execution) => !execution.terminal) || !input.evidenceIds.length || input.openBlockingDefects.length || input.reviewVerdict !== "PASS") throw new Error("Direction closure gates are not satisfied");
    this.state = "closed";
  }
  private move(next: DirectionState): void { const legal: Record<DirectionState, DirectionState[]> = { draft: ["proposed", "stopped"], proposed: ["active", "stopped"], active: ["paused", "blocked", "reviewing", "stopped"], paused: ["active", "stopped"], blocked: ["active", "stopped"], reviewing: ["active", "closed", "rejected"], closed: [], rejected: [], stopped: [] }; if (!legal[this.state].includes(next)) throw new Error(`Illegal Direction transition ${this.state} -> ${next}`); this.state = next; }
}

export type MissionState = "draft" | "planning" | "awaiting_approval" | "running" | "pausing" | "paused" | "reviewing" | "blocked" | "stopping" | "completed" | "stopped" | "failed";
export class MissionEngine {
  state: MissionState = "draft";
  readonly graph: VersionedDag;
  constructor(readonly projectId: string, readonly missionId: string, nodes: GraphNode[]) { this.graph = new VersionedDag(missionId, nodes); }
  transition(next: MissionState): void { const legal: Record<MissionState, MissionState[]> = { draft: ["planning"], planning: ["awaiting_approval"], awaiting_approval: ["running"], running: ["pausing", "reviewing", "blocked", "stopping", "failed"], pausing: ["paused"], paused: ["running", "stopping"], reviewing: ["running", "completed"], blocked: ["running"], stopping: ["stopped"], completed: [], stopped: [], failed: [] }; if (!legal[this.state].includes(next)) throw new Error(`Illegal Mission transition ${this.state} -> ${next}`); this.state = next; }
  requestCompletion(input: { exitProofsPass: boolean; artifactHashesResolve: boolean; unresolvedClaims: string[]; openBlockingDefects: string[]; budgetsReconciled: boolean }): void {
    if (this.state !== "running" || !this.graph.completionReady() || !input.exitProofsPass || !input.artifactHashesResolve || input.unresolvedClaims.length || input.openBlockingDefects.length || !input.budgetsReconciled) throw new Error("Mission completion packet failed deterministic gates");
    this.state = "reviewing";
  }
  applyCompletionReview(verdict: string, reviewerAgentId: string, directorAgentId: string): void { if (this.state !== "reviewing") throw new Error("Mission is not in final Review"); if (reviewerAgentId === directorAgentId) throw new Error("Completion Reviewer must be fresh and independent"); this.state = verdict === "PASS" ? "completed" : "running"; }
}
