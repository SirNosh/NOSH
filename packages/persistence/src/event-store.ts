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
export type EntityProjection<T extends JsonValue = JsonValue> = { projectId: string; entityType: string; entityId: string; version: number; state: string; value: T; updatedAt: string };
export type OperationIntent = { intentId: string; projectId: string; operationType: string; idempotencyKey: string; state: "pending" | "completed" | "failed"; request: JsonValue; result: JsonValue | null; error: string | null; createdAt: string; updatedAt: string };

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

  commandReceipt(projectId: string, idempotencyKey: string): CommandReceipt | undefined {
    const stored = this.database.prepare("SELECT result_json FROM command_receipts WHERE project_id = ? AND idempotency_key = ?").get(projectId, idempotencyKey) as { result_json: string } | undefined;
    return stored ? JSON.parse(stored.result_json) as CommandReceipt : undefined;
  }

  beginOperation(projectId: string, operationType: string, idempotencyKey: string, request: JsonValue): OperationIntent {
    if (!operationType.trim() || !idempotencyKey.trim()) throw new Error("Operation type and idempotency key are required");
    return this.database.transaction(() => {
      const prior = this.operationByKey(projectId, idempotencyKey);
      if (prior) { if (prior.operationType !== operationType || canonicalJson(prior.request) !== canonicalJson(request)) throw new Error("Operation idempotency key was reused for a different request"); return prior; }
      const now = nowUtc(); const intent: OperationIntent = { intentId: createId("cmd"), projectId, operationType, idempotencyKey, state: "pending", request, result: null, error: null, createdAt: now, updatedAt: now };
      this.database.prepare("INSERT INTO operation_intents (intent_id, project_id, operation_type, idempotency_key, state, request_json, result_json, error, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, NULL, NULL, ?, ?)").run(intent.intentId, projectId, operationType, idempotencyKey, intent.state, canonicalJson(request), now, now); return intent;
    })();
  }

  completeOperation(projectId: string, intentId: string, result: JsonValue): OperationIntent {
    return this.finishOperation(projectId, intentId, "completed", result, null);
  }

  failOperation(projectId: string, intentId: string, error: string): OperationIntent {
    return this.finishOperation(projectId, intentId, "failed", null, error || "operation_failed");
  }

  operationIntents(projectId: string, state?: OperationIntent["state"]): OperationIntent[] {
    const rows = (state ? this.database.prepare("SELECT * FROM operation_intents WHERE project_id = ? AND state = ? ORDER BY created_at").all(projectId, state) : this.database.prepare("SELECT * FROM operation_intents WHERE project_id = ? ORDER BY created_at").all(projectId)) as OperationRow[];
    return rows.map(operationIntent);
  }

  projection<T extends JsonValue = JsonValue>(projectId: string, entityType: string, entityId: string): EntityProjection<T> | undefined {
    const row = this.database.prepare("SELECT version, state, projection_json, updated_at FROM entity_projections WHERE project_id = ? AND entity_type = ? AND entity_id = ?").get(projectId, entityType, entityId) as { version: number; state: string; projection_json: string; updated_at: string } | undefined;
    return row ? { projectId, entityType, entityId, version: row.version, state: row.state, value: JSON.parse(row.projection_json) as T, updatedAt: row.updated_at } : undefined;
  }

  projections<T extends JsonValue = JsonValue>(projectId: string, entityType: string): Array<EntityProjection<T>> {
    return (this.database.prepare("SELECT entity_id, version, state, projection_json, updated_at FROM entity_projections WHERE project_id = ? AND entity_type = ? ORDER BY updated_at").all(projectId, entityType) as Array<{ entity_id: string; version: number; state: string; projection_json: string; updated_at: string }>).map((row) => ({ projectId, entityType, entityId: row.entity_id, version: row.version, state: row.state, value: JSON.parse(row.projection_json) as T, updatedAt: row.updated_at }));
  }

  mutateProjection<T extends JsonValue>(idempotencyKey: string, expectedVersion: number | null, draft: EventDraft, input: { entityType: string; entityId: string; state: string; value: T; graph?: { scopeType: string; version: number; value: JsonValue; rationale: string } }): { event: EventEnvelope; projection: EntityProjection<T>; replayed: boolean } {
    const parsed = eventDraftSchema.parse(draft); if (parsed.retention !== "persistent") throw new Error("Projection mutations require a persistent event");
    return this.database.transaction(() => {
      const prior = this.database.prepare("SELECT result_json FROM command_receipts WHERE project_id = ? AND idempotency_key = ?").get(parsed.scope.projectId, idempotencyKey) as { result_json: string } | undefined;
      if (prior) return { ...(JSON.parse(prior.result_json) as { event: EventEnvelope; projection: EntityProjection<T> }), replayed: true };
      const current = this.projection(parsed.scope.projectId, input.entityType, input.entityId); const actual = current?.version ?? 0; if (expectedVersion !== actual) throw new Error(`Optimistic concurrency conflict: expected ${expectedVersion}, current ${actual}`);
      const event = this.appendPersistent(parsed); const updatedAt = event.timestamp; const projection: EntityProjection<T> = { projectId: parsed.scope.projectId, entityType: input.entityType, entityId: input.entityId, version: actual + 1, state: input.state, value: input.value, updatedAt };
      this.database.prepare(`INSERT INTO entity_projections (project_id, entity_type, entity_id, version, state, projection_json, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT(project_id, entity_type, entity_id) DO UPDATE SET version = excluded.version, state = excluded.state, projection_json = excluded.projection_json, updated_at = excluded.updated_at`).run(projection.projectId, projection.entityType, projection.entityId, projection.version, projection.state, canonicalJson(projection.value), projection.updatedAt);
      if (input.graph) { const latest = this.database.prepare("SELECT COALESCE(MAX(version), 0) AS version FROM graph_versions WHERE project_id = ? AND scope_type = ? AND scope_id = ?").get(parsed.scope.projectId, input.graph.scopeType, input.entityId) as { version: number }; if (latest.version + 1 !== input.graph.version) throw new Error(`Graph version conflict: expected ${latest.version + 1}`); this.database.prepare("INSERT INTO graph_versions (project_id, scope_type, scope_id, version, graph_json, rationale, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)").run(parsed.scope.projectId, input.graph.scopeType, input.entityId, input.graph.version, canonicalJson(input.graph.value), input.graph.rationale, updatedAt); }
      const receipt = { event, projection }; this.database.prepare("INSERT INTO command_receipts (project_id, idempotency_key, result_json, created_at) VALUES (?, ?, ?, ?)").run(parsed.scope.projectId, idempotencyKey, canonicalJson(receipt), updatedAt); return { ...receipt, replayed: false };
    })();
  }

  graphVersions(projectId: string, scopeType: string, scopeId: string): Array<{ version: number; value: JsonValue; rationale: string; createdAt: string }> { return (this.database.prepare("SELECT version, graph_json, rationale, created_at FROM graph_versions WHERE project_id = ? AND scope_type = ? AND scope_id = ? ORDER BY version").all(projectId, scopeType, scopeId) as Array<{ version: number; graph_json: string; rationale: string; created_at: string }>).map((row) => ({ version: row.version, value: JSON.parse(row.graph_json) as JsonValue, rationale: row.rationale, createdAt: row.created_at })); }

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

  private operationByKey(projectId: string, idempotencyKey: string): OperationIntent | undefined {
    const row = this.database.prepare("SELECT * FROM operation_intents WHERE project_id = ? AND idempotency_key = ?").get(projectId, idempotencyKey) as OperationRow | undefined; return row ? operationIntent(row) : undefined;
  }

  private finishOperation(projectId: string, intentId: string, state: "completed" | "failed", result: JsonValue | null, error: string | null): OperationIntent {
    return this.database.transaction(() => { const row = this.database.prepare("SELECT * FROM operation_intents WHERE project_id = ? AND intent_id = ?").get(projectId, intentId) as OperationRow | undefined; if (!row) throw new Error("Unknown operation intent"); const current = operationIntent(row); if (current.state !== "pending") { if (current.state !== state || canonicalJson(current.result) !== canonicalJson(result) || current.error !== error) throw new Error("Operation intent already has a different terminal result"); return current; } const now = nowUtc(); this.database.prepare("UPDATE operation_intents SET state = ?, result_json = ?, error = ?, updated_at = ? WHERE project_id = ? AND intent_id = ?").run(state, result === null ? null : canonicalJson(result), error, now, projectId, intentId); return { ...current, state, result, error, updatedAt: now }; })();
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

type OperationRow = { intent_id: string; project_id: string; operation_type: string; idempotency_key: string; state: OperationIntent["state"]; request_json: string; result_json: string | null; error: string | null; created_at: string; updated_at: string };
function operationIntent(row: OperationRow): OperationIntent { return { intentId: row.intent_id, projectId: row.project_id, operationType: row.operation_type, idempotencyKey: row.idempotency_key, state: row.state, request: JSON.parse(row.request_json) as JsonValue, result: row.result_json ? JSON.parse(row.result_json) as JsonValue : null, error: row.error, createdAt: row.created_at, updatedAt: row.updated_at }; }
