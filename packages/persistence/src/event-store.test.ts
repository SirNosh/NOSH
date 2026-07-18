import { createId } from "@nosh/core";
import { schemaUri, type EventDraft } from "@nosh/wire";
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

describe("EventStore", () => {
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

  it("commits projection, graph version, event, and idempotency receipt atomically", () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-projection-")); const projectId = createId("prj"); const missionId = createId("mis"); const path = join(directory, "project.sqlite");
    try { const store = new EventStore(path); const first = store.mutateProjection("projection-command-0001", 0, { ...persistentDraft(projectId, "mission.created"), scope: { ...persistentDraft(projectId).scope, missionId } }, { entityType: "mission", entityId: missionId, state: "draft", value: { title: "Test" }, graph: { scopeType: "mission", version: 1, value: { nodes: [] }, rationale: "initial" } }); expect(first.projection.version).toBe(1); expect(store.graphVersions(projectId, "mission", missionId)).toHaveLength(1); expect(store.mutateProjection("projection-command-0001", 0, persistentDraft(projectId, "mission.created"), { entityType: "mission", entityId: missionId, state: "draft", value: { title: "Test" } }).replayed).toBe(true); expect(() => store.mutateProjection("projection-command-0002", 0, persistentDraft(projectId, "mission.updated"), { entityType: "mission", entityId: missionId, state: "running", value: { title: "Test" } })).toThrow("conflict"); store.close(); const recovered = new EventStore(path); expect(recovered.projection(projectId, "mission", missionId)?.state).toBe("draft"); recovered.close(); } finally { rmSync(directory, { recursive: true, force: true }); }
  });

  it("recovers durable external-operation intents without accepting key reuse", () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-intent-")); const projectId = createId("prj"); const path = join(directory, "project.sqlite");
    try { const first = new EventStore(path); const intent = first.beginOperation(projectId, "job.launch", "launch-job-1", { jobId: "job_1", command: ["node"] }); expect(intent.state).toBe("pending"); expect(first.beginOperation(projectId, "job.launch", "launch-job-1", { jobId: "job_1", command: ["node"] }).intentId).toBe(intent.intentId); expect(() => first.beginOperation(projectId, "job.launch", "launch-job-1", { jobId: "job_2" })).toThrow("different request"); first.close(); const recovered = new EventStore(path); expect(recovered.operationIntents(projectId, "pending")).toHaveLength(1); recovered.completeOperation(projectId, intent.intentId, { jobId: "job_1", verified: true }); expect(recovered.operationIntents(projectId, "pending")).toEqual([]); expect(recovered.operationIntents(projectId, "completed")[0]?.result).toEqual({ jobId: "job_1", verified: true }); recovered.close(); } finally { rmSync(directory, { recursive: true, force: true }); }
  });
});
