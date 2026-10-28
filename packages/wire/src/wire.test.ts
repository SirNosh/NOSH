import { readFileSync } from "node:fs";
import { join } from "node:path";
import { createId } from "@nosh/core";
import { describe, expect, it } from "vitest";
import { eventBudgetTokens, parseModelSelection, canonicalJson, eventEnvelopeSchema, missionNodeSchema, runtimeInstructionSchema, runtimeInstructionTemplateSchema, schemaUri, sha256, taskWorkspaceSchema, validateRecord } from "./index.js";

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

describe("generated JSON Schemas", () => {
  it("are non-empty for every record (zod-to-json-schema must run against the same Zod major)", async () => {
    const { readdirSync, readFileSync } = await import("node:fs");
    const directory = new URL("./schemas/", import.meta.url);
    const files = readdirSync(directory).filter((file) => file.endsWith(".schema.json"));
    expect(files.length).toBeGreaterThan(40);
    for (const file of files) {
      const document = JSON.parse(readFileSync(new URL(file, directory), "utf8")) as { definitions?: Record<string, { properties?: object; anyOf?: unknown[] }> };
      const definition = Object.values(document.definitions ?? {})[0];
      expect(definition?.properties ?? definition?.anyOf, file).toBeTruthy();
    }
  });
});

describe("supervisor episode types", () => {
  it("are valid THREAD_STEP refs for every worker and director role", async () => {
    const { episodeTypeForRole, runtimeInstructionSchema } = await import("./index.js");
    for (const role of ["general_worker", "librarian_researcher", "reviewer", "mission_director", "research_director", "nosh"]) {
      const result = runtimeInstructionSchema.safeParse({ $schema: schemaUri("runtime-instruction"), schemaVersion: 1, instructionId: createId("ins"), projectId: createId("prj"), idempotencyKey: "supervisor-step-0001", proposedByAgentId: null, issuedAt: "2026-10-01T00:00:00.000Z", operation: "THREAD_STEP", threadId: createId("thr"), objective: "Run one bounded step", expectedEpisodeType: episodeTypeForRole(role), inputRefs: [], skillIds: [] });
      expect(result.success, `${role}: ${JSON.stringify(result.error?.issues)}`).toBe(true);
    }
  });
});

describe("model selection setting", () => {
  it("parses provider/id[:thinkingLevel] and rejects anything else", () => {
    expect(parseModelSelection("openai-codex/gpt-6-luna:low")).toEqual({ provider: "openai-codex", id: "gpt-6-luna", thinkingLevel: "low" });
    expect(parseModelSelection("openai-codex/gpt-6-luna")).toEqual({ provider: "openai-codex", id: "gpt-6-luna" });
    for (const bad of ["gpt-6-luna", "a/b:turbo", "a/b c"]) expect(() => parseModelSelection(bad)).toThrow();
  });
});

describe("validation messages", () => {
  it("name the expected shape of a missing field so one correction can fix it", () => {
    const result = validateRecord(schemaUri("episode-draft"), { $schema: schemaUri("episode-draft"), schemaVersion: 1, episodeType: "episode_general-worker", summary: "s", facts: [{ evidenceRefs: [], confidence: "high" }], decisions: [], artifactIds: [], evidenceIds: [], changedFiles: [], unresolvedQuestions: [], recommendedNextActions: [] });
    expect(result.ok).toBe(false);
    expect(JSON.stringify(result)).toContain("missing; expected string");
  });
  it("defaults an omitted fact confidence to medium instead of voiding the Episode", () => {
    const defaulted = validateRecord(schemaUri("episode-draft"), { $schema: schemaUri("episode-draft"), schemaVersion: 1, episodeType: "episode_general-worker", summary: "s", facts: [{ statement: "x", evidenceRefs: [] }], decisions: [], artifactIds: [], evidenceIds: [], changedFiles: [], unresolvedQuestions: [], recommendedNextActions: [] });
    expect(defaulted).toMatchObject({ ok: true, value: { facts: [{ statement: "x", confidence: "medium" }] } });
  });
  it("rejects a REVISE whose criteria all pass with no defect, and keeps a grounded one", () => {
    const passing = JSON.parse(readFileSync(join(import.meta.dirname, "..", "..", "..", "fixtures", "schemas", "review-verdict", "minimal.valid.json"), "utf8")) as Record<string, unknown> & { criteria: Array<Record<string, unknown>> };
    const ungrounded = validateRecord(schemaUri("review-verdict"), { ...passing, verdict: "REVISE" });
    expect(ungrounded.ok).toBe(false);
    expect(JSON.stringify(ungrounded)).toContain("the verdict is PASS");
    expect(validateRecord(schemaUri("review-verdict"), { ...passing, verdict: "REVISE", criteria: passing.criteria.map((criterion) => ({ ...criterion, status: "INCONCLUSIVE" })) }).ok).toBe(true);
    expect(validateRecord(schemaUri("review-verdict"), { ...passing, verdict: "BLOCKED" }).ok).toBe(true);
  });
});

describe("budget tokens", () => {
  it("weigh cache reads at 10% from raw counts, and fall back to the provider total", () => {
    expect(eventBudgetTokens({ modelTokens: 100, inputTokens: 20, outputTokens: 10, cacheReadTokens: 60, cacheWriteTokens: 10 })).toBe(46);
    expect(eventBudgetTokens({ modelTokens: 46, providerTotalTokens: 100, inputTokens: 20, outputTokens: 10, cacheReadTokens: 60, cacheWriteTokens: 10 })).toBe(46);
    expect(eventBudgetTokens({ modelTokens: 42 })).toBe(42);
    expect(eventBudgetTokens(null)).toBe(0);
  });
});

describe("bibliographic dates", () => {
  it("accept a year, a year and month, or a full date, and reject anything else", async () => {
    const { readFileSync } = await import("node:fs");
    const full = JSON.parse(readFileSync(new URL("../../../fixtures/schemas/librarian-completion/full.valid.json", import.meta.url), "utf8")) as { sources: Array<Record<string, unknown>> };
    const withDate = (publicationDate: string | null) => ({ ...full, sources: [{ ...full.sources[0], publicationDate }] });
    for (const date of ["2016", "2016-12", "2016-12-05", null]) expect(validateRecord(schemaUri("librarian-completion"), withDate(date) as never).ok, String(date)).toBe(true);
    for (const date of ["2016-13", "Dec 2016", "16", "2016-12-32"]) expect(validateRecord(schemaUri("librarian-completion"), withDate(date) as never).ok, date).toBe(false);
  });
});
