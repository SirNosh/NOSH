import { createId } from "@nosh/core";
import { generalWorkerCompletionSchema, librarianCompletionSchema, schemaUri, type EventDraft } from "@nosh/wire";
import Database from "better-sqlite3";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { EventStore } from "./event-store.js";

function persistentDraft(projectId: string, type = "agent.started"): EventDraft {
  return {
    $schema: schemaUri("event"),
    schemaVersion: 1,
    retention: "persistent",
    type,
    source: "test",
    scope: { projectId, missionId: null, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: null },
    correlationId: null,
    causationId: null,
    payload: { marker: type },
  };
}

function terminalRecord(kind: "general-worker-completion" | "librarian-completion") {
  if (kind === "general-worker-completion") return generalWorkerCompletionSchema.parse({
    $schema: schemaUri(kind), schemaVersion: 1, taskOutcome: "completed", workPerformed: [],
    codeChanges: { startingCommit: "a".repeat(40), endingCommit: "a".repeat(40), changedPaths: [], diffArtifactId: null, branch: "main" },
    commands: [], criteria: [], scientificImpact: { claimIds: [], evidenceIds: [], interpretation: "No scientific changes" },
    deviations: [], newRisks: [], unresolvedItems: [], suggestedNextActions: [], readyForDeterministicPostflight: true, readyForReview: true,
  });
  return librarianCompletionSchema.parse({
    $schema: schemaUri(kind), schemaVersion: 1, taskOutcome: "completed", researchQuestion: "Review prior work",
    searchCoverage: { databases: [], queries: [], dateRange: { from: null, to: null }, language: [], inclusionCriteria: [], exclusionCriteria: [] },
    sources: [], findings: [], contradictions: [], evaluationDifferences: [], knowledgeGaps: [], candidateClaimEffects: [],
    bibliographyArtifactId: "art_" + "a".repeat(32), reportArtifactId: "art_" + "b".repeat(32), readyForReview: true,
  });
}

describe("EventStore", () => {
  it("bootstraps recent history then pages every new delta in order", () => {
    const store = new EventStore(":memory:"); const projectId = createId("prj");
    try {
      expect(store.replayPage(projectId, 0, 3, true)).toEqual({ events: [], nextCursor: 0, hasMore: false });
      store.append(persistentDraft(projectId));
      expect(store.replayPage(projectId, 0, 3).events.map(e => e.sequence)).toEqual([1]);
      for (let i = 0; i < 9; i++) store.append(persistentDraft(projectId));
      store.append(persistentDraft(createId("prj")));
      const recent = store.replayPage(projectId, 0, 3, true);
      expect(recent.events.map(e => e.sequence)).toEqual([8, 9, 10]);
      expect(recent).toMatchObject({ nextCursor: 10, hasMore: false });
      for (let i = 0; i < 4; i++) store.append(persistentDraft(projectId));
      const delta = store.replayPage(projectId, recent.nextCursor, 3);
      expect(delta.events.map(e => e.sequence)).toEqual([11, 12, 13]);
      expect(delta.hasMore).toBe(true);
      expect(store.replayPage(projectId, delta.nextCursor, 3).events.map(e => e.sequence)).toEqual([14]);
      expect(store.replay(projectId)).toHaveLength(14);
      expect(() => store.replayPage(projectId, 10, 3, true)).toThrow("initial zero cursor");
    } finally { store.close(); }
  });
  it("pages durable events with stable cursors and bounded validation", () => {
    const store = new EventStore(":memory:");
    const projectId = createId("prj");
    try {
      for (let i = 0; i < 7; i++) store.append(persistentDraft(projectId));
      store.append(persistentDraft(createId("prj")));
      const first = store.replayPage(projectId, 0, 3);
      expect(first.events.map((event) => event.sequence)).toEqual([1, 2, 3]);
      expect(first).toMatchObject({ nextCursor: 3, hasMore: true });
      expect(store.replayPage(projectId, 3, 3)).toMatchObject({ nextCursor: 6, hasMore: true });
      expect(store.replayPage(projectId, 6, 3)).toMatchObject({ nextCursor: 7, hasMore: false });
      expect(store.replayPage(projectId, 7, 3)).toEqual({ events: [], nextCursor: 7, hasMore: false });
      for (const limit of [0, -1, 1001, 1.5, Infinity]) expect(() => store.replayPage(projectId, 0, limit)).toThrow();
      expect(() => store.replayPage(projectId, -1)).toThrow();
    } finally { store.close(); }
  });
  it("replays persistent events exactly once across a restart", () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-event-store-"));
    const databasePath = join(directory, "project.sqlite");
    const projectId = createId("prj");
    try {
      const first = new EventStore(databasePath);
      const one = first.append(persistentDraft(projectId));
      const receipt = first.appendIdempotent("idempotency-key-0001", persistentDraft(projectId, "job.started"));
      const duplicate = first.appendIdempotent("idempotency-key-0001", persistentDraft(projectId, "job.started"));
      first.writeSnapshot(projectId, 2, { missions: [] });
      first.close();

      const recovered = new EventStore(databasePath);
      expect(one.sequence).toBe(1);
      expect(receipt.receipt.event.sequence).toBe(2);
      expect(duplicate.replayed).toBe(true);
      expect(recovered.replay(projectId)).toHaveLength(2);
      expect(recovered.latestSnapshot(projectId)?.sequence).toBe(2);
      recovered.close();
    } finally {
      rmSync(directory, { recursive: true, force: true });
    }
  });

  it("never persists ephemeral events", () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-event-store-"));
    const projectId = createId("prj");
    try {
      const store = new EventStore(join(directory, "project.sqlite"));
      const event = store.append({ ...persistentDraft(projectId, "agent.text_delta"), retention: "ephemeral" });
      expect(event.sequence).toBeNull();
      expect(store.replay(projectId)).toEqual([]);
      store.close();
    } finally {
      rmSync(directory, { recursive: true, force: true });
    }
  });

  it("permits only one terminal schema per task tool across retrying store handles", () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-terminal-submission-"));
    const projectId = createId("prj");
    const taskId = createId("tsk");
    try {
      const databasePath = join(directory, "project.sqlite");
      const firstStore = new EventStore(databasePath);
      const secondStore = new EventStore(databasePath);
      const first = firstStore.appendTerminalSubmission(taskId, "nosh_response_submit", schemaUri("general-worker-completion"), terminalRecord("general-worker-completion"), { ...persistentDraft(projectId, "record.submitted"), correlationId: `task:${taskId}`, payload: terminalRecord("general-worker-completion") });
      const retry = secondStore.appendTerminalSubmission(taskId, "nosh_response_submit", schemaUri("general-worker-completion"), terminalRecord("general-worker-completion"), { ...persistentDraft(projectId, "record.submitted"), correlationId: `task:${taskId}`, payload: terminalRecord("general-worker-completion") });
      expect(retry).toMatchObject({ replayed: true, receipt: { event: { eventId: first.receipt.event.eventId } } });
      expect(() => secondStore.appendTerminalSubmission(taskId, "nosh_response_submit", schemaUri("librarian-completion"), terminalRecord("librarian-completion"), { ...persistentDraft(projectId, "record.submitted"), correlationId: `task:${taskId}`, payload: terminalRecord("librarian-completion") })).toThrow("already has terminal");
      expect(firstStore.replay(projectId)).toHaveLength(1);
      secondStore.close();
      firstStore.close();
    } finally {
      rmSync(directory, { recursive: true, force: true });
    }
  });

  it("arbitrates concurrent cross-schema terminal submissions at the task-tool boundary", async () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-terminal-race-")); const projectId = createId("prj"); const taskId = createId("tsk"); const path = join(directory, "project.sqlite");
    try {
      const left = new EventStore(path); const right = new EventStore(path);
      const outcomes = await Promise.allSettled([
        Promise.resolve().then(() => left.appendTerminalSubmission(taskId, "nosh_response_submit", schemaUri("general-worker-completion"), terminalRecord("general-worker-completion"), { ...persistentDraft(projectId, "record.submitted"), correlationId: `task:${taskId}`, payload: terminalRecord("general-worker-completion") })),
        Promise.resolve().then(() => right.appendTerminalSubmission(taskId, "nosh_response_submit", schemaUri("librarian-completion"), terminalRecord("librarian-completion"), { ...persistentDraft(projectId, "record.submitted"), correlationId: `task:${taskId}`, payload: terminalRecord("librarian-completion") })),
      ]);
      expect(outcomes.filter((outcome) => outcome.status === "fulfilled")).toHaveLength(1);
      expect(outcomes.filter((outcome) => outcome.status === "rejected")).toHaveLength(1);
      expect(left.replay(projectId)).toHaveLength(1);
      left.close(); right.close();
    } finally { rmSync(directory, { recursive: true, force: true }); }
  });
  it("migrates legacy schema-specific terminal rows to one deterministic task-tool authority row", () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-terminal-migration-")); const path = join(directory, "project.sqlite"); const projectId = createId("prj"); const taskId = createId("tsk");
    try {
      const legacy = new Database(path);
      legacy.exec("CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL); CREATE TABLE terminal_submissions (project_id TEXT NOT NULL, task_id TEXT NOT NULL, tool_name TEXT NOT NULL, schema_uri TEXT NOT NULL, record_hash TEXT NOT NULL, receipt_json TEXT NOT NULL, created_at TEXT NOT NULL, PRIMARY KEY (project_id, task_id, tool_name, schema_uri));");
      legacy.prepare("INSERT INTO schema_migrations (version, applied_at) VALUES (?, ?)").run(1, "2025-01-01T00:00:00.000Z"); legacy.prepare("INSERT INTO schema_migrations (version, applied_at) VALUES (?, ?)").run(2, "2025-01-01T00:00:00.000Z"); legacy.prepare("INSERT INTO schema_migrations (version, applied_at) VALUES (?, ?)").run(3, "2025-01-01T00:00:00.000Z"); legacy.prepare("INSERT INTO schema_migrations (version, applied_at) VALUES (?, ?)").run(4, "2025-01-01T00:00:00.000Z");
      legacy.prepare("INSERT INTO terminal_submissions VALUES (?, ?, ?, ?, ?, ?, ?)").run(projectId, taskId, "nosh_response_submit", schemaUri("librarian-completion"), "sha256:later", "{}", "2025-01-02T00:00:00.000Z");
      legacy.prepare("INSERT INTO terminal_submissions VALUES (?, ?, ?, ?, ?, ?, ?)").run(projectId, taskId, "nosh_response_submit", schemaUri("general-worker-completion"), "sha256:earlier", "{}", "2025-01-01T00:00:00.000Z");
      legacy.close();
      const store = new EventStore(path);
      expect(store.database.prepare("SELECT schema_uri, record_hash FROM terminal_task_tools WHERE project_id = ? AND task_id = ? AND tool_name = ?").get(projectId, taskId, "nosh_response_submit")).toEqual({ schema_uri: schemaUri("general-worker-completion"), record_hash: "sha256:earlier" });
      store.close();
    } finally { rmSync(directory, { recursive: true, force: true }); }
  });
  it("commits projection companion authority records in the same transaction", () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-projection-records-")); const projectId = createId("prj"); const missionId = createId("mis"); const path = join(directory, "project.sqlite");
    try {
      const store = new EventStore(path);
      const result = store.mutateProjection("projection-records-0001", 0, { ...persistentDraft(projectId, "mission.created"), scope: { ...persistentDraft(projectId).scope, missionId } }, { entityType: "mission", entityId: missionId, state: "draft", value: { title: "Test" }, records: [{ ...persistentDraft(projectId, "record.submitted"), scope: { ...persistentDraft(projectId).scope, missionId } }] });
      expect(result.records).toHaveLength(1);
      expect(result.records[0]?.sequence).toBe(result.event.sequence! + 1);
      expect(store.replay(projectId)).toHaveLength(2);
      expect(store.mutateProjection("projection-records-0001", 0, { ...persistentDraft(projectId, "mission.created"), scope: { ...persistentDraft(projectId).scope, missionId } }, { entityType: "mission", entityId: missionId, state: "draft", value: { title: "Test" }, records: [{ ...persistentDraft(projectId, "record.submitted"), scope: { ...persistentDraft(projectId).scope, missionId } }] }).records).toHaveLength(1);
      store.close();
    } finally { rmSync(directory, { recursive: true, force: true }); }
  });
  it("commits projection, graph version, event, and idempotency receipt atomically", () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-projection-")); const projectId = createId("prj"); const missionId = createId("mis"); const path = join(directory, "project.sqlite");
    try { const store = new EventStore(path); const first = store.mutateProjection("projection-command-0001", 0, { ...persistentDraft(projectId, "mission.created"), scope: { ...persistentDraft(projectId).scope, missionId } }, { entityType: "mission", entityId: missionId, state: "draft", value: { title: "Test" }, graph: { scopeType: "mission", version: 1, value: { nodes: [] }, rationale: "initial" } }); expect(first.projection.version).toBe(1); expect(store.graphVersions(projectId, "mission", missionId)).toHaveLength(1); expect(store.mutateProjection("projection-command-0001", 0, { ...persistentDraft(projectId, "mission.created"), scope: { ...persistentDraft(projectId).scope, missionId } }, { entityType: "mission", entityId: missionId, state: "draft", value: { title: "Test" }, graph: { scopeType: "mission", version: 1, value: { nodes: [] }, rationale: "initial" } }).replayed).toBe(true); expect(() => store.mutateProjection("projection-command-0002", 0, persistentDraft(projectId, "mission.updated"), { entityType: "mission", entityId: missionId, state: "running", value: { title: "Test" } })).toThrow("conflict"); store.close(); const recovered = new EventStore(path); expect(recovered.projection(projectId, "mission", missionId)?.state).toBe("draft"); recovered.close(); } finally { rmSync(directory, { recursive: true, force: true }); }
  });

  it("recovers durable external-operation intents without accepting key reuse", () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-intent-")); const projectId = createId("prj"); const path = join(directory, "project.sqlite");
    try { const first = new EventStore(path); const intent = first.beginOperation(projectId, "job.launch", "launch-job-1", { jobId: "job_1", command: ["node"] }); expect(intent.state).toBe("pending"); expect(first.beginOperation(projectId, "job.launch", "launch-job-1", { jobId: "job_1", command: ["node"] }).intentId).toBe(intent.intentId); expect(() => first.beginOperation(projectId, "job.launch", "launch-job-1", { jobId: "job_2" })).toThrow("different request"); first.close(); const recovered = new EventStore(path); expect(recovered.operationIntents(projectId, "pending")).toHaveLength(1); recovered.completeOperation(projectId, intent.intentId, { jobId: "job_1", verified: true }); expect(recovered.operationIntents(projectId, "pending")).toEqual([]); expect(recovered.operationIntents(projectId, "completed")[0]?.result).toEqual({ jobId: "job_1", verified: true }); recovered.close(); } finally { rmSync(directory, { recursive: true, force: true }); }
  });
});
