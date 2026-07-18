import { createId } from "@nosh/core";
import { describe, expect, it } from "vitest";
import { canonicalJson, eventEnvelopeSchema, schemaUri, sha256, validateRecord } from "./index.js";

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
});
