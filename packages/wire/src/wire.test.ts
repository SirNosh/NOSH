import { createId } from "@nosh/core";
import { describe, expect, it } from "vitest";
import { canonicalJson, eventEnvelopeSchema, missionNodeSchema, runtimeInstructionSchema, runtimeInstructionTemplateSchema, schemaUri, sha256, taskWorkspaceSchema, validateRecord } from "./index.js";

const scope = {
  projectId: createId("prj"),
  missionId: null,
  directionId: null,
  autoresearchId: null,
  experimentId: null,
  runId: null,
  jobId: null,
  agentId: null,
};

describe("wire contracts", () => {
  it("uses canonical task roles for Mission node assignments", () => {
    for (const role of ["librarian_researcher", "general_worker", "reviewer"]) expect(missionNodeSchema.shape.assignedRole.safeParse(role).success).toBe(true);
    expect(missionNodeSchema.shape.assignedRole.safeParse("unknown_role").success).toBe(false);
  });

  it("canonicalizes before hashing", () => {
    expect(canonicalJson({ b: 2, a: 1 })).toBe('{"a":1,"b":2}');
    expect(sha256({ a: 1, b: 2 })).toBe(sha256({ b: 2, a: 1 }));
  });

  it("requires a sequence for persistent events", () => {
    const event = {
      $schema: schemaUri("event"),
      schemaVersion: 1,
      eventId: createId("evt"),
      sequence: null,
      timestamp: "2026-07-17T20:00:00.000Z",
      retention: "persistent",
      type: "agent.started",
      source: "test",
      scope,
      correlationId: null,
      causationId: null,
      payload: {},
    };
    expect(eventEnvelopeSchema.safeParse(event).success).toBe(false);
  });

  it("returns stable JSON Pointer errors for strict records", () => {
    const result = validateRecord(schemaUri("task-acknowledgement"), {
      $schema: schemaUri("task-acknowledgement"),
      schemaVersion: 1,
      taskId: createId("tsk"),
      attempt: 1,
      agentId: createId("agt"),
      decision: "accepted",
      understoodObjective: "Test strict validation.",
      understoodOutputIds: [],
      understoodCriterionIds: [],
      observedLeaseId: "lease_1",
      observedStartingCommit: "1234567",
      conflicts: [],
      clarificationRequest: null,
      submittedAt: "2026-07-17T20:00:00.000Z",
      unexpected: true,
    });
    expect(result.ok).toBe(false);
    if (!result.ok) expect(result.errors.some((error) => error.pointer === "/unexpected")).toBe(true);
  });

  it("requires canonical Git object IDs and safe branches for task workspaces", () => {
    const workspace = { worktreeId: createId("wrk"), branch: "agent/task-1", startingCommit: "a".repeat(40), writeScopes: ["src"], protectedScopes: [] };
    expect(taskWorkspaceSchema.safeParse(workspace).success).toBe(true);
    expect(taskWorkspaceSchema.safeParse({ ...workspace, startingCommit: "b".repeat(64) }).success).toBe(true);
    expect(taskWorkspaceSchema.safeParse({ ...workspace, startingCommit: "A".repeat(40) }).success).toBe(false);
    expect(taskWorkspaceSchema.safeParse({ ...workspace, startingCommit: "a".repeat(39) }).success).toBe(false);
    expect(taskWorkspaceSchema.safeParse({ ...workspace, branch: "agent//task" }).success).toBe(false);
    expect(taskWorkspaceSchema.safeParse({ ...workspace, branch: "agent/../task" }).success).toBe(false);
    expect(taskWorkspaceSchema.safeParse({ ...workspace, branch: "main/.hidden" }).success).toBe(false);
    expect(taskWorkspaceSchema.safeParse({ ...workspace, branch: "main/foo.lock" }).success).toBe(false);
    expect(taskWorkspaceSchema.safeParse({ ...workspace, branch: "main." }).success).toBe(false);
  });

  it("shares thread-open bodies without weakening the instruction envelope", () => {
    const template = {
      operation: "THREAD_OPEN" as const, threadId: createId("thr"), taskId: createId("tsk"), initialAgentId: null,
      ownerScope: { missionId: null, directionId: null, autoresearchId: null, experimentId: null, graphNodeId: null },
      role: "general_worker" as const, purpose: "Exercise the shared instruction body.", executionMode: "background" as const,
      parentThreadId: null, inputRefs: [], skillIds: [], capabilities: [],
      budget: { maximumToolCalls: 1, maximumModelTokens: 1, maximumWallClockSeconds: 1 },
    };
    const instruction = {
      ...template, $schema: schemaUri("runtime-instruction"), schemaVersion: 1, instructionId: createId("ins"),
      projectId: createId("prj"), idempotencyKey: "runtime-instruction-test", proposedByAgentId: null, issuedAt: "2026-07-17T20:00:00.000Z",
    };
    expect(runtimeInstructionTemplateSchema.safeParse(template).success).toBe(true);
    expect(runtimeInstructionTemplateSchema.safeParse(instruction).success).toBe(false);
    expect(runtimeInstructionSchema.safeParse(instruction).success).toBe(true);
  });
});
