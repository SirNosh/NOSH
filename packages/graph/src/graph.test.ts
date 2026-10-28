import { describe, expect, it } from "vitest";
import { DirectionEngine, ExperimentTree, MissionEngine, VersionedDag, isLegalDirectionTransition, isLegalMissionTransition, isLegalNodeTransition, type GraphNode } from "./index.js";

const node = (id: string, dependencies: string[] = []): GraphNode => ({ id, type: "implementation", title: id, required: true, criterionIds: [], hardDependencies: dependencies, softDependencies: [], state: "pending", attempt: 0, maximumAttempts: 3, priority: 1, criticalWeight: 1, createdAt: new Date().toISOString(), lease: null });

describe("versioned research graphs", () => {
  it("rejects cycles, stale mutations, and premature completion", () => {
    const graph = new VersionedDag("mis_1", [node("a"), node("b", ["a"])]);
    expect(() => graph.apply(1, [{ type: "add_hard_edge", from: "b", to: "a" }], "bad", [])).toThrow("cycle");
    expect(() => graph.apply(0, [], "stale", [])).toThrow("Stale");
    expect(graph.completionReady()).toBe(false);
  });

  it("exports the canonical lifecycle transition authority", () => {
    expect(isLegalNodeTransition("ready", "leased")).toBe(true);
    expect(isLegalNodeTransition("ready", "working")).toBe(false);
    expect(isLegalMissionTransition("awaiting_approval", "running")).toBe(true);
    // A blocked Mission can be abandoned without first resuming it.
    expect(isLegalMissionTransition("blocked", "stopping")).toBe(true);
    expect(isLegalDirectionTransition("proposed", "active")).toBe(true);
  });

  it("keeps experiments rooted, frozen, novel, and independently reviewed", () => {
    const tree = new ExperimentTree("sha256:contract", 4, 3);
    tree.add({ experimentId: "exp_base", parentExperimentId: null, ideaFingerprint: "fp_base", attemptFingerprint: "at_base", evaluationContractHash: "sha256:contract", branch: "baseline", evaluatedCommit: null, depth: 0, state: "evaluating", reviewId: null, reviewerAgentId: null, guardrailsPass: null });
    tree.evaluate("exp_base", "1234567", "accepted", { reviewId: "rev_1", reviewerAgentId: "agt_1", verdict: "PASS" }, true);
    tree.add({ experimentId: "exp_1", parentExperimentId: "exp_base", ideaFingerprint: "fp_1", attemptFingerprint: "at_1", evaluationContractHash: "sha256:contract", branch: "ar/1", evaluatedCommit: null, depth: 1, state: "proposed", reviewId: null, reviewerAgentId: null, guardrailsPass: null });
    expect(() => tree.evaluate("exp_1", "7654321", "accepted", { reviewId: "rev_2", reviewerAgentId: "agt_1", verdict: "PASS" }, true)).toThrow("fresh Reviewer");
    expect(() => tree.add({ ...tree.get("exp_1"), experimentId: "exp_2", ideaFingerprint: "fp_1" })).toThrow("Duplicate");
  });

  it("does not let Directors bypass Direction or Mission closure gates", () => {
    const direction = new DirectionEngine("dir_1", { metric: "accuracy" }, [node("d1")], 2);
    direction.propose(); direction.activate();
    expect(() => direction.startAutoresearch({ id: "ar_1", decisionQuestion: "q", familyTags: ["f"], scope: ["src"], evaluationContractHash: direction.evaluationContractHash() })).toThrow("accepted baseline");
    const mission = new MissionEngine("prj_1", "mis_1", [node("m1")]); mission.transition("planning"); mission.transition("awaiting_approval"); mission.transition("running");
    expect(() => mission.requestCompletion({ exitProofsPass: true, artifactHashesResolve: true, unresolvedClaims: [], openBlockingDefects: [], budgetsReconciled: true })).toThrow("failed deterministic gates");
  });
});
