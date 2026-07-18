import { describe, expect, it } from "vitest";
import { FocusGovernor, Scheduler, type AttemptInput } from "./index.js";

const attempt: AttemptInput = { projectId: "prj_1", missionId: "mis_1", directionId: null, nodeId: "node_minor", parentExperimentId: null, taskType: "implementation", hypothesisFamilyTags: ["same"], commandClass: "test", inScopeFiles: ["a.ts"], inputArtifactHashes: [], diffHash: null, evaluationContractHash: null, intendedDecision: "validate", expectedOutputType: "report" };

describe("deterministic scheduling controls", () => {
  it("detects repetition without model calls and requires a concrete reset proof", () => {
    const governor = new FocusGovernor();
    expect(governor.record(attempt, {}).alarm).toBeNull();
    expect(governor.record(attempt, {}).alarm?.reasons).toContain("repeated_attempt_fingerprint");
    expect(() => governor.clear("node_minor", {})).toThrow("reflection alone");
    governor.clear("node_minor", { redirectedNodeId: "node_critical" });
  });
  it("enforces one active Mission per Project", () => { const scheduler = new Scheduler(); scheduler.activateMission("prj_1", "mis_1"); expect(() => scheduler.activateMission("prj_1", "mis_2")).toThrow("one active Mission"); });
});
