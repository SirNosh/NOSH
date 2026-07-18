import { describe, expect, it } from "vitest";
import { StructuredSubmissionGate, canDelegateDirectly, validateTeachback } from "./index.js";

describe("structured agent control", () => {
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
    const state = { handoffId: "hnd_1", logicalOwnerId: "owner_1", observedVersions: { graphVersion: 4 }, branchHead: "abc1234", defectIds: ["def_1"], blockerIds: [], readyNodeIds: ["node_2"] };
    expect(validateTeachback(state, { handoffId: "hnd_1", logicalOwnerId: "owner_1", decision: "accepted", observedVersions: { graphVersion: 4 }, observedBranchHead: "abc1234", acknowledgedDefectIds: ["def_1"], acknowledgedBlockerIds: [], selectedNextNodeId: "node_2", conflicts: [] })).toEqual({ ok: true });
    expect(validateTeachback(state, { handoffId: "hnd_1", logicalOwnerId: "owner_1", decision: "accepted", observedVersions: { graphVersion: 3 }, observedBranchHead: "abc1234", acknowledgedDefectIds: ["def_1"], acknowledgedBlockerIds: [], selectedNextNodeId: "node_2", conflicts: [] })).toMatchObject({ ok: false });
  });
});
