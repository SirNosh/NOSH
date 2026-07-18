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
});
