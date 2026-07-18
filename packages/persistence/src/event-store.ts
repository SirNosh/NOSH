import { createId, nowUtc } from "@nosh/core";
import { canonicalJson, eventDraftSchema, eventEnvelopeSchema, sha256, type EventDraft, type EventEnvelope, type JsonValue } from "@nosh/wire";
import type Database from "better-sqlite3";
import { openDatabase } from "./database.js";

export type Snapshot = {
  projectId: string;
  sequence: number;
  checksum: string;
  state: JsonValue;
  createdAt: string;
};

export type CommandReceipt = {
  event: EventEnvelope;
};

export class EventStore {
  readonly database: Database.Database;

  constructor(databasePath: string) {
    this.database = openDatabase(databasePath);
  }

  append(draft: EventDraft): EventEnvelope {
    const parsed = eventDraftSchema.parse(draft);
    if (parsed.retention === "ephemeral") {
      return eventEnvelopeSchema.parse({
        ...parsed,
        eventId: createId("evt"),
        sequence: null,
        timestamp: nowUtc(),
      });
    }

    return this.database.transaction(() => this.appendPersistent(parsed))();
  }

  appendIdempotent(idempotencyKey: string, draft: EventDraft): { receipt: CommandReceipt; replayed: boolean } {
    const parsed = eventDraftSchema.parse(draft);
    if (parsed.retention !== "persistent") {
      throw new Error("Idempotent commands may only append persistent events");
    }

    return this.database.transaction(() => {
      const stored = this.database
        .prepare("SELECT result_json FROM command_receipts WHERE project_id = ? AND idempotency_key = ?")
        .get(parsed.scope.projectId, idempotencyKey) as { result_json: string } | undefined;
      if (stored) {
        return { receipt: JSON.parse(stored.result_json) as CommandReceipt, replayed: true };
      }

      const receipt = { event: this.appendPersistent(parsed) };
      this.database
        .prepare("INSERT INTO command_receipts (project_id, idempotency_key, result_json, created_at) VALUES (?, ?, ?, ?)")
        .run(parsed.scope.projectId, idempotencyKey, canonicalJson(receipt), nowUtc());
      return { receipt, replayed: false };
    })();
  }

  replay(projectId: string, afterSequence = 0): EventEnvelope[] {
    return (this.database
      .prepare("SELECT event_json FROM events WHERE project_id = ? AND sequence > ? ORDER BY sequence")
      .all(projectId, afterSequence) as Array<{ event_json: string }>)
      .map(({ event_json }) => eventEnvelopeSchema.parse(JSON.parse(event_json)));
  }

  currentSequence(projectId: string): number {
    const row = this.database.prepare("SELECT last_sequence FROM project_sequences WHERE project_id = ?").get(projectId) as
      | { last_sequence: number }
      | undefined;
    return row?.last_sequence ?? 0;
  }

  writeSnapshot(projectId: string, sequence: number, state: JsonValue): Snapshot {
    const currentSequence = this.currentSequence(projectId);
    if (sequence > currentSequence) {
      throw new Error(`Cannot snapshot sequence ${sequence} beyond current sequence ${currentSequence}`);
    }

    const snapshot: Snapshot = {
      projectId,
      sequence,
      checksum: sha256(state),
      state,
      createdAt: nowUtc(),
    };
    this.database
      .prepare(
        `INSERT INTO snapshots (project_id, sequence, checksum, snapshot_json, created_at)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(project_id, sequence) DO UPDATE SET checksum = excluded.checksum, snapshot_json = excluded.snapshot_json, created_at = excluded.created_at`,
      )
      .run(snapshot.projectId, snapshot.sequence, snapshot.checksum, canonicalJson(snapshot.state), snapshot.createdAt);
    return snapshot;
  }

  latestSnapshot(projectId: string): Snapshot | undefined {
    const row = this.database
      .prepare("SELECT sequence, checksum, snapshot_json, created_at FROM snapshots WHERE project_id = ? ORDER BY sequence DESC LIMIT 1")
      .get(projectId) as { sequence: number; checksum: string; snapshot_json: string; created_at: string } | undefined;
    if (!row) return undefined;
    const state = JSON.parse(row.snapshot_json) as JsonValue;
    if (sha256(state) !== row.checksum) throw new Error(`Snapshot checksum mismatch for ${projectId} at sequence ${row.sequence}`);
    return { projectId, sequence: row.sequence, checksum: row.checksum, state, createdAt: row.created_at };
  }

  close(): void {
    this.database.close();
  }

  private appendPersistent(draft: EventDraft): EventEnvelope {
    const projectId = draft.scope.projectId;
    const previous = this.currentSequence(projectId);
    const sequence = previous + 1;
    const event = eventEnvelopeSchema.parse({
      ...draft,
      eventId: createId("evt"),
      sequence,
      timestamp: nowUtc(),
    });

    if (previous === 0) {
      this.database.prepare("INSERT INTO project_sequences (project_id, last_sequence) VALUES (?, ?)").run(projectId, sequence);
    } else {
      this.database.prepare("UPDATE project_sequences SET last_sequence = ? WHERE project_id = ?").run(sequence, projectId);
    }
    this.database
      .prepare("INSERT INTO events (project_id, sequence, event_id, event_type, timestamp, event_json) VALUES (?, ?, ?, ?, ?, ?)")
      .run(projectId, sequence, event.eventId, event.type, event.timestamp, canonicalJson(event));
    this.database
      .prepare(
        `INSERT INTO projection_heads (project_id, sequence, updated_at) VALUES (?, ?, ?)
         ON CONFLICT(project_id) DO UPDATE SET sequence = excluded.sequence, updated_at = excluded.updated_at`,
      )
      .run(projectId, sequence, event.timestamp);
    return event;
  }
}
