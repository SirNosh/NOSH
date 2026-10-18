import { describe, expect, it } from "vitest";
import { StructuredSubmissionGate, canDelegateDirectly, validateTeachback } from "./index.js";

describe("structured agent control", () => {
  it("accepts Project contracts only through the Project contract tool", () => {
    const gate = new StructuredSubmissionGate();
    const record = { $schema: "https://nosh.dev/schemas/project-contract/v1", schemaVersion: 1, templateVersion: "1.0.0", projectId: "prj_0123456789abcdef0123456789abcdef", contractVersion: 2, workingTitle: "Test", domainTags: [], northStar: { goalId: "goal_project", question: "Does it work?", contributionType: "contribution_research", decisionUse: "Decide whether to proceed" }, scope: { included: [], excluded: [] }, datasets: [], licensingConstraints: [], computeEnvelope: { maximumGpuHours: 0, maximumDiskBytes: 1, allowedHardwareClasses: ["local"] }, reproducibilityStandard: { minimumSeeds: 1, environmentLockRequired: true, immutableEvaluatedCommitRequired: true, rawLogsRetained: true }, paper: { intendedVenue: null, requiredSections: ["Abstract"], claimPolicy: "evidence_link_required" }, policies: { network: "network_user.approved", privacy: "privacy_local.first", publication: "publication_user.approved", protectedPaths: [".nosh"] }, canonicalDefaultBranch: "main", createdBy: "user", createdAt: "2026-07-22T00:00:00.000Z", approvedAt: "2026-07-22T00:01:00.000Z" };
    expect(gate.submit("nosh_project_contract_submit", "contract-1", record).ok).toBe(true);
    expect(gate.submit("nosh_response_submit", "contract-2", record).ok).toBe(false);
  });

  it("allows exactly one schema-only correction", () => {
    const gate = new StructuredSubmissionGate();
    const first = gate.submit("nosh_review_submit", "attempt-1", { $schema: "https://nosh.dev/schemas/review-verdict/v1" });
    const second = gate.submit("nosh_review_submit", "attempt-1", { $schema: "https://nosh.dev/schemas/review-verdict/v1" });
    expect(first).toMatchObject({ ok: false, retryAllowed: true });
    expect(second).toMatchObject({ ok: false, retryAllowed: false });
  });

  it("forbids recursive worker delegation", () => {
    expect(canDelegateDirectly("general_worker")).toBe(false);
    expect(canDelegateDirectly("reviewer")).toBe(false);
    expect(canDelegateDirectly("research_director")).toBe(true);
  });

  it("transfers ownership only after exact teach-back", () => {
    const goalStack = { projectGoalId: "goal_1", missionCriterionIds: ["criterion_1"], directionQuestionId: null, currentGraphNodeId: "node_2" };
    const state = { handoffId: "hnd_1", logicalOwnerId: "owner_1", goalStack, observedVersions: { graphVersion: 4 }, branchHead: "abc1234", defectIds: ["def_1"], blockerIds: [], readyNodeIds: ["node_2"] };
    expect(validateTeachback(state, { handoffId: "hnd_1", logicalOwnerId: "owner_1", understoodGoalStack: goalStack, decision: "accepted", observedVersions: { graphVersion: 4 }, observedBranchHead: "abc1234", acknowledgedDefectIds: ["def_1"], acknowledgedBlockerIds: [], selectedNextNodeId: "node_2", conflicts: [] })).toEqual({ ok: true });
    expect(validateTeachback(state, { handoffId: "hnd_1", logicalOwnerId: "owner_1", understoodGoalStack: { ...goalStack, currentGraphNodeId: null }, decision: "accepted", observedVersions: { graphVersion: 3 }, observedBranchHead: "abc1234", acknowledgedDefectIds: ["def_1"], acknowledgedBlockerIds: [], selectedNextNodeId: "node_2", conflicts: [] })).toMatchObject({ ok: false });
  });
});
