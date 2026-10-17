//! The per-project event store, as `EventStore` in `packages/persistence/src/event-store.ts`.
//!
//! Records are `serde_json::Value`s shaped exactly like the TypeScript objects, because they are
//! stored as canonical JSON that both implementations read back and hash.

use crate::database::open_database;
use crate::{Error, Result, rejected};
use nosh_wire::{
    IdPrefix, canonical_json, create_id, is_task_terminal_record, now_utc, sha256, validate_event,
    validate_event_draft,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;

/// `Number.MAX_SAFE_INTEGER`: replay cursors beyond it are invalid in TypeScript.
const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

/// An idempotent or terminal append: the stored receipt and whether it already existed.
#[derive(Debug, Clone, PartialEq)]
pub struct Appended {
    pub receipt: Value,
    pub replayed: bool,
}

/// A bounded replay page (`replayPage`).
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub events: Vec<Value>,
    pub next_cursor: i64,
    pub has_more: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GraphInput {
    pub scope_type: String,
    pub version: i64,
    pub value: Value,
    pub rationale: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectionInput {
    pub entity_type: String,
    pub entity_id: String,
    pub state: String,
    pub value: Value,
    pub graph: Option<GraphInput>,
    /// Companion event drafts appended in the same transaction.
    pub records: Vec<Value>,
    /// Overrides what the idempotency intent hash covers.
    pub intent: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectionMutation {
    pub event: Value,
    pub projection: Value,
    pub records: Vec<Value>,
    pub replayed: bool,
}

pub struct EventStore {
    connection: Connection,
    /// Parsed events per project. The log is append-only, so each row is parsed once.
    parsed: RefCell<HashMap<String, Vec<Value>>>,
}

impl EventStore {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(EventStore {
            connection: open_database(path)?,
            parsed: RefCell::default(),
        })
    }

    pub fn append(&self, draft: &Value) -> Result<Value> {
        let parsed = validate_event_draft(draft)?;
        if parsed["retention"] == "ephemeral" {
            let mut event = object(parsed);
            event.insert("eventId".into(), json!(create_id(IdPrefix::Evt)));
            event.insert("sequence".into(), Value::Null);
            event.insert("timestamp".into(), json!(now_utc()));
            return Ok(validate_event(&Value::Object(event))?);
        }
        self.transaction(|| self.append_persistent(&parsed, false))
    }

    pub fn append_idempotent(&self, idempotency_key: &str, draft: &Value) -> Result<Appended> {
        let parsed = validate_event_draft(draft)?;
        if parsed["retention"] != "persistent" {
            return rejected("Idempotent commands may only append persistent events");
        }
        if parsed["type"] == "record.submitted" && is_task_terminal_record(&parsed["payload"]) {
            return rejected("Terminal record.submitted events require appendTerminalSubmission");
        }
        let intent_hash = sha256(&parsed);
        let project_id = project_of(&parsed);
        self.transaction(|| {
            if let Some(receipt) = self.command_receipt(project_id, idempotency_key)? {
                let same = match receipt.get("intentHash").and_then(Value::as_str).filter(|hash| !hash.is_empty()) {
                    Some(hash) => hash == intent_hash,
                    None => same_draft(&receipt["event"], &parsed),
                };
                if !same {
                    return rejected("Idempotency key was reused for a different command intent");
                }
                return Ok(Appended { receipt, replayed: true });
            }
            let receipt = json!({ "event": self.append_persistent(&parsed, false)?, "intentHash": intent_hash });
            self.connection
                .prepare_cached("INSERT INTO command_receipts (project_id, idempotency_key, result_json, created_at) VALUES (?, ?, ?, ?)")?
                .execute(params![project_id, idempotency_key, canonical_json(&receipt), now_utc()])?;
            Ok(Appended { receipt, replayed: false })
        })
    }

    pub fn append_terminal_submission(
        &self,
        task_id: &str,
        tool_name: &str,
        schema_uri: &str,
        record: &Value,
        draft: &Value,
    ) -> Result<Appended> {
        let parsed = validate_event_draft(draft)?;
        if parsed["retention"] != "persistent" {
            return rejected("Terminal submissions must be persistent");
        }
        let canonical = parsed["type"] == "record.submitted"
            && canonical_json(&parsed["payload"]) == canonical_json(record)
            && is_task_terminal_record(record)
            && record.get("$schema").and_then(Value::as_str) == Some(schema_uri);
        if !canonical {
            return rejected(
                "Terminal submission must be a canonical record.submitted task-terminal payload",
            );
        }
        let record_hash = sha256(record);
        let project_id = project_of(&parsed);
        self.transaction(|| {
            let ambiguity: Option<i64> = self
                .connection
                .prepare_cached("SELECT terminal_count FROM terminal_task_legacy_ambiguities WHERE project_id = ? AND task_id = ?")?
                .query_row([project_id, task_id], |row| row.get(0))
                .optional()?;
            if let Some(count) = ambiguity {
                return rejected(format!("Task {task_id} has {count} legacy terminal submissions and requires manual resolution"));
            }
            let prior: Option<(String, String, String, String)> = self
                .connection
                .prepare_cached("SELECT tool_name, schema_uri, record_hash, receipt_json FROM terminal_task_authority WHERE project_id = ? AND task_id = ?")?
                .query_row([project_id, task_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
                .optional()?;
            if let Some((prior_tool, prior_schema, prior_hash, receipt)) = prior {
                if prior_tool != tool_name || prior_schema != schema_uri || prior_hash != record_hash {
                    return rejected(format!("Task {task_id} already has terminal {prior_schema} through {prior_tool}"));
                }
                return Ok(Appended { receipt: serde_json::from_str(&receipt)?, replayed: true });
            }
            let receipt = json!({ "event": self.append_persistent(&parsed, true)? });
            self.connection
                .prepare_cached("INSERT INTO terminal_task_authority (project_id, task_id, tool_name, schema_uri, record_hash, receipt_json, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)")?
                .execute(params![project_id, task_id, tool_name, schema_uri, record_hash, canonical_json(&receipt), now_utc()])?;
            Ok(Appended { receipt, replayed: false })
        })
    }

    /// Persistent events after `after_sequence`, in order.
    pub fn replay(&self, project_id: &str, after_sequence: i64) -> Result<Vec<Value>> {
        self.with_events(project_id, after_sequence, |events, start| {
            events[start..].to_vec()
        })
    }

    /// Bounded transport replay. With `recent`, the last `limit` events (cursor must be 0).
    pub fn replay_page(
        &self,
        project_id: &str,
        after_sequence: i64,
        limit: i64,
        recent: bool,
    ) -> Result<Page> {
        if !(0..=MAX_SAFE_INTEGER).contains(&after_sequence) {
            return rejected("Invalid replay cursor");
        }
        if !(1..=1000).contains(&limit) {
            return rejected("Replay limit must be between 1 and 1000");
        }
        if recent && after_sequence != 0 {
            return rejected("Recent replay requires an initial zero cursor");
        }
        let limit = usize::try_from(limit).expect("checked range");
        self.with_events(project_id, after_sequence, |all, start| {
            if recent {
                let events = all[start.max(all.len().saturating_sub(limit))..].to_vec();
                let next_cursor = events.last().map_or(0, sequence);
                return Page {
                    events,
                    next_cursor,
                    has_more: false,
                };
            }
            let end = all.len().min(start + limit);
            let events = all[start..end].to_vec();
            let next_cursor = events.last().map_or(after_sequence, sequence);
            Page {
                events,
                next_cursor,
                has_more: all.len() - start > limit,
            }
        })
    }

    pub fn current_sequence(&self, project_id: &str) -> Result<i64> {
        let row = self
            .connection
            .prepare_cached("SELECT last_sequence FROM project_sequences WHERE project_id = ?")?
            .query_row([project_id], |row| row.get(0))
            .optional()?;
        Ok(row.unwrap_or(0))
    }

    pub fn command_receipt(
        &self,
        project_id: &str,
        idempotency_key: &str,
    ) -> Result<Option<Value>> {
        let stored: Option<String> = self
            .connection
            .prepare_cached("SELECT result_json FROM command_receipts WHERE project_id = ? AND idempotency_key = ?")?
            .query_row([project_id, idempotency_key], |row| row.get(0))
            .optional()?;
        Ok(stored.map(|json| serde_json::from_str(&json)).transpose()?)
    }

    /// The host-owned turn journal entry for `turn_id`, created open on first use. Replaying with
    /// a different context is a conflict.
    pub fn terminal_turn(&self, project_id: &str, turn_id: &str, context: &Value) -> Result<Value> {
        self.transaction(|| {
            let hash = sha256(context);
            let row: Option<(String, String)> = self
                .connection
                .prepare_cached("SELECT context_hash, receipt_json FROM terminal_turn_receipts WHERE project_id = ? AND turn_id = ?")?
                .query_row([project_id, turn_id], |row| Ok((row.get(0)?, row.get(1)?)))
                .optional()?;
            if let Some((stored_hash, receipt)) = row {
                if stored_hash != hash {
                    return rejected("Terminal turn context replay conflict");
                }
                return Ok(serde_json::from_str(&receipt)?);
            }
            let receipt = json!({ "failures": 0, "closed": false, "records": null, "results": [], "status": "open" });
            self.connection.prepare_cached("INSERT INTO terminal_turn_receipts VALUES (?, ?, ?, ?)")?.execute(params![project_id, turn_id, hash, canonical_json(&receipt)])?;
            Ok(receipt)
        })
    }

    pub fn save_terminal_turn(
        &self,
        project_id: &str,
        turn_id: &str,
        receipt: &Value,
    ) -> Result<()> {
        let changed = self
            .connection
            .prepare_cached("UPDATE terminal_turn_receipts SET receipt_json = ? WHERE project_id = ? AND turn_id = ?")?
            .execute(params![canonical_json(receipt), project_id, turn_id])?;
        if changed != 1 {
            return rejected("Terminal turn receipt is missing");
        }
        Ok(())
    }

    /// Records a durable intent before an external effect; the same key and request return it again.
    pub fn begin_operation(
        &self,
        project_id: &str,
        operation_type: &str,
        idempotency_key: &str,
        request: &Value,
    ) -> Result<Value> {
        if js_trim_is_empty(operation_type) || js_trim_is_empty(idempotency_key) {
            return rejected("Operation type and idempotency key are required");
        }
        self.transaction(|| {
            let prior = self.query_operations("SELECT * FROM operation_intents WHERE project_id = ? AND idempotency_key = ?", &[project_id, idempotency_key])?.pop();
            if let Some(prior) = prior {
                if prior["operationType"] != operation_type || canonical_json(&prior["request"]) != canonical_json(request) {
                    return rejected("Operation idempotency key was reused for a different request");
                }
                return Ok(prior);
            }
            let now = now_utc();
            let intent_id = create_id(IdPrefix::Cmd);
            self.connection
                .prepare_cached("INSERT INTO operation_intents (intent_id, project_id, operation_type, idempotency_key, state, request_json, result_json, error, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, NULL, NULL, ?, ?)")?
                .execute(params![intent_id, project_id, operation_type, idempotency_key, "pending", canonical_json(request), now, now])?;
            Ok(json!({ "intentId": intent_id, "projectId": project_id, "operationType": operation_type, "idempotencyKey": idempotency_key, "state": "pending", "request": request, "result": null, "error": null, "createdAt": now, "updatedAt": now }))
        })
    }

    pub fn complete_operation(
        &self,
        project_id: &str,
        intent_id: &str,
        result: &Value,
    ) -> Result<Value> {
        self.finish_operation(
            project_id,
            intent_id,
            "completed",
            result.clone(),
            Value::Null,
        )
    }

    pub fn fail_operation(&self, project_id: &str, intent_id: &str, error: &str) -> Result<Value> {
        let error = if error.is_empty() {
            "operation_failed"
        } else {
            error
        };
        self.finish_operation(project_id, intent_id, "failed", Value::Null, json!(error))
    }

    /// Intents for a project, oldest first, optionally only those in `state`.
    pub fn operation_intents(&self, project_id: &str, state: Option<&str>) -> Result<Vec<Value>> {
        match state {
            Some(state) => self.query_operations("SELECT * FROM operation_intents WHERE project_id = ? AND state = ? ORDER BY created_at", &[project_id, state]),
            None => self.query_operations("SELECT * FROM operation_intents WHERE project_id = ? ORDER BY created_at", &[project_id]),
        }
    }

    pub fn projection(
        &self,
        project_id: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<Option<Value>> {
        let row: Option<(i64, String, String, String)> = self
            .connection
            .prepare_cached("SELECT version, state, projection_json, updated_at FROM entity_projections WHERE project_id = ? AND entity_type = ? AND entity_id = ?")?
            .query_row([project_id, entity_type, entity_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
            .optional()?;
        row.map(|(version, state, value, updated_at)| {
            projection_value(
                project_id,
                entity_type,
                entity_id,
                version,
                &state,
                serde_json::from_str(&value)?,
                &updated_at,
            )
        })
        .transpose()
    }

    /// Every projection of `entity_type`, least recently updated first.
    pub fn projections(&self, project_id: &str, entity_type: &str) -> Result<Vec<Value>> {
        let mut statement = self.connection.prepare_cached("SELECT entity_id, version, state, projection_json, updated_at FROM entity_projections WHERE project_id = ? AND entity_type = ? ORDER BY updated_at")?;
        let rows = statement.query_map([project_id, entity_type], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (entity_id, version, state, value, updated_at) = row?;
            out.push(projection_value(
                project_id,
                entity_type,
                &entity_id,
                version,
                &state,
                serde_json::from_str(&value)?,
                &updated_at,
            )?);
        }
        Ok(out)
    }

    /// Atomically appends `draft`, writes the projection at `expected_version + 1`, optionally a
    /// graph version, and companion records, under an idempotency key.
    pub fn mutate_projection(
        &self,
        idempotency_key: &str,
        expected_version: Option<i64>,
        draft: &Value,
        input: &ProjectionInput,
    ) -> Result<ProjectionMutation> {
        let parsed = validate_event_draft(draft)?;
        if parsed["retention"] != "persistent" {
            return rejected("Projection mutations require a persistent event");
        }
        let project_id = project_of(&parsed);
        let records = input
            .records
            .iter()
            .map(|record| {
                let parsed_record = validate_event_draft(record)?;
                if parsed_record["retention"] != "persistent"
                    || parsed_record["scope"]["projectId"] != project_id
                {
                    return rejected(
                        "Projection companion records must be persistent and project-scoped",
                    );
                }
                Ok(parsed_record)
            })
            .collect::<Result<Vec<_>>>()?;
        let graph = input.graph.as_ref().map_or(Value::Null, |g| json!({ "scopeType": g.scope_type, "version": g.version, "value": g.value, "rationale": g.rationale }));
        let intent = input.intent.clone().filter(|intent| !intent.is_null()).unwrap_or_else(|| {
            json!({ "draft": parsed, "expectedVersion": expected_version, "input": { "entityType": input.entity_type, "entityId": input.entity_id, "state": input.state, "value": input.value, "graph": graph, "records": records } })
        });
        let intent_hash = sha256(&intent);
        self.transaction(|| {
            if let Some(receipt) = self.command_receipt(project_id, idempotency_key)? {
                if receipt.get("intentHash").and_then(Value::as_str) != Some(intent_hash.as_str()) {
                    let legacy = receipt.get("intentHash").and_then(Value::as_str).is_none_or(str::is_empty);
                    return rejected(if legacy { "Legacy projection receipt has no verifiable intent hash" } else { "Idempotency key was reused for a different projection intent" });
                }
                let records = receipt.get("records").and_then(Value::as_array).cloned().unwrap_or_default();
                return Ok(ProjectionMutation { event: receipt["event"].clone(), projection: receipt["projection"].clone(), records, replayed: true });
            }
            let actual = self.projection(project_id, &input.entity_type, &input.entity_id)?.map_or(0, |p| p["version"].as_i64().unwrap_or(0));
            if expected_version != Some(actual) {
                let expected = expected_version.map_or_else(|| "null".to_owned(), |v| v.to_string());
                return rejected(format!("Optimistic concurrency conflict: expected {expected}, current {actual}"));
            }
            let event = self.append_persistent(&parsed, false)?;
            let updated_at = event["timestamp"].as_str().expect("validated events have a timestamp").to_owned();
            let projection = projection_value(project_id, &input.entity_type, &input.entity_id, actual + 1, &input.state, input.value.clone(), &updated_at)?;
            self.connection
                .prepare_cached("INSERT INTO entity_projections (project_id, entity_type, entity_id, version, state, projection_json, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT(project_id, entity_type, entity_id) DO UPDATE SET version = excluded.version, state = excluded.state, projection_json = excluded.projection_json, updated_at = excluded.updated_at")?
                .execute(params![project_id, input.entity_type, input.entity_id, actual + 1, input.state, canonical_json(&input.value), updated_at])?;
            if let Some(graph) = &input.graph {
                let latest: i64 = self
                    .connection
                    .prepare_cached("SELECT COALESCE(MAX(version), 0) FROM graph_versions WHERE project_id = ? AND scope_type = ? AND scope_id = ?")?
                    .query_row([project_id, &graph.scope_type, &input.entity_id], |row| row.get(0))?;
                if latest + 1 != graph.version {
                    return rejected(format!("Graph version conflict: expected {}", latest + 1));
                }
                self.connection
                    .prepare_cached("INSERT INTO graph_versions (project_id, scope_type, scope_id, version, graph_json, rationale, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)")?
                    .execute(params![project_id, graph.scope_type, input.entity_id, graph.version, canonical_json(&graph.value), graph.rationale, updated_at])?;
            }
            let appended = records.iter().map(|record| self.append_persistent(record, false)).collect::<Result<Vec<_>>>()?;
            let receipt = json!({ "event": event, "projection": projection, "records": appended, "intentHash": intent_hash });
            self.connection
                .prepare_cached("INSERT INTO command_receipts (project_id, idempotency_key, result_json, created_at) VALUES (?, ?, ?, ?)")?
                .execute(params![project_id, idempotency_key, canonical_json(&receipt), updated_at])?;
            Ok(ProjectionMutation { event, projection, records: appended, replayed: false })
        })
    }

    /// Graph versions for a scope, oldest first, as `{ version, value, rationale, createdAt }`.
    pub fn graph_versions(
        &self,
        project_id: &str,
        scope_type: &str,
        scope_id: &str,
    ) -> Result<Vec<Value>> {
        let mut statement = self.connection.prepare_cached("SELECT version, graph_json, rationale, created_at FROM graph_versions WHERE project_id = ? AND scope_type = ? AND scope_id = ? ORDER BY version")?;
        let rows = statement.query_map([project_id, scope_type, scope_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (version, value, rationale, created_at) = row?;
            let value: Value = serde_json::from_str(&value)?;
            out.push(json!({ "version": version, "value": value, "rationale": rationale, "createdAt": created_at }));
        }
        Ok(out)
    }

    pub fn write_snapshot(&self, project_id: &str, sequence: i64, state: &Value) -> Result<Value> {
        let current = self.current_sequence(project_id)?;
        if sequence > current {
            return rejected(format!(
                "Cannot snapshot sequence {sequence} beyond current sequence {current}"
            ));
        }
        let checksum = sha256(state);
        let created_at = now_utc();
        self.connection
            .prepare_cached("INSERT INTO snapshots (project_id, sequence, checksum, snapshot_json, created_at) VALUES (?, ?, ?, ?, ?) ON CONFLICT(project_id, sequence) DO UPDATE SET checksum = excluded.checksum, snapshot_json = excluded.snapshot_json, created_at = excluded.created_at")?
            .execute(params![project_id, sequence, checksum, canonical_json(state), created_at])?;
        Ok(
            json!({ "projectId": project_id, "sequence": sequence, "checksum": checksum, "state": state, "createdAt": created_at }),
        )
    }

    /// The newest snapshot, verified against its checksum.
    pub fn latest_snapshot(&self, project_id: &str) -> Result<Option<Value>> {
        let row: Option<(i64, String, String, String)> = self
            .connection
            .prepare_cached("SELECT sequence, checksum, snapshot_json, created_at FROM snapshots WHERE project_id = ? ORDER BY sequence DESC LIMIT 1")?
            .query_row([project_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
            .optional()?;
        let Some((sequence, checksum, state, created_at)) = row else {
            return Ok(None);
        };
        let state: Value = serde_json::from_str(&state)?;
        if sha256(&state) != checksum {
            return rejected(format!(
                "Snapshot checksum mismatch for {project_id} at sequence {sequence}"
            ));
        }
        Ok(Some(
            json!({ "projectId": project_id, "sequence": sequence, "checksum": checksum, "state": state, "createdAt": created_at }),
        ))
    }

    /// Runs `body` in a transaction, or in a savepoint when one is already open (better-sqlite3's
    /// nested `transaction()`), rolling back on error.
    fn transaction<T>(&self, body: impl FnOnce() -> Result<T>) -> Result<T> {
        let nested = !self.connection.is_autocommit();
        let (begin, commit, rollback) = if nested {
            (
                "SAVEPOINT nosh",
                "RELEASE nosh",
                "ROLLBACK TO nosh; RELEASE nosh",
            )
        } else {
            ("BEGIN", "COMMIT", "ROLLBACK")
        };
        self.connection.execute_batch(begin)?;
        let outcome =
            body().and_then(|value| Ok(self.connection.execute_batch(commit).map(|()| value)?));
        if outcome.is_err() && (nested || !self.connection.is_autocommit()) {
            // The original error matters more than a failed rollback.
            let _ = self.connection.execute_batch(rollback);
        }
        outcome
    }

    /// Calls `f` with the project's parsed events and the index of the first one after `after`.
    fn with_events<R>(
        &self,
        project_id: &str,
        after: i64,
        f: impl FnOnce(&[Value], usize) -> R,
    ) -> Result<R> {
        let mut cache = self.parsed.borrow_mut();
        let cached = cache.entry(project_id.to_owned()).or_default();
        let newest = cached.last().map_or(0, sequence);
        let mut statement = self.connection.prepare_cached(
            "SELECT event_json FROM events WHERE project_id = ? AND sequence > ? ORDER BY sequence",
        )?;
        let tail = statement
            .query_map(params![project_id, newest], |row| row.get::<_, String>(0))?
            .map(|json| -> Result<Value> { Ok(validate_event(&serde_json::from_str(&json?)?)?) })
            .collect::<Result<Vec<_>>>()?;
        let combined;
        let events: &[Value] = if tail.is_empty() {
            cached
        } else if !self.connection.is_autocommit() {
            // Rows read inside an open transaction may still roll back, so they never enter the cache.
            combined = [cached.as_slice(), &tail].concat();
            &combined
        } else {
            cached.extend(tail);
            cached
        };
        let start = events.partition_point(|event| sequence(event) <= after);
        Ok(f(events, start))
    }

    fn append_persistent(&self, draft: &Value, allow_terminal_submission: bool) -> Result<Value> {
        if draft["type"] == "record.submitted"
            && is_task_terminal_record(&draft["payload"])
            && !allow_terminal_submission
        {
            return rejected("Terminal record.submitted events require appendTerminalSubmission");
        }
        let project_id = project_of(draft);
        let previous = self.current_sequence(project_id)?;
        let next = previous + 1;
        let mut event = object(draft.clone());
        event.insert("eventId".into(), json!(create_id(IdPrefix::Evt)));
        event.insert("sequence".into(), json!(next));
        event.insert("timestamp".into(), json!(now_utc()));
        let event = validate_event(&Value::Object(event))?;
        if previous == 0 {
            self.connection
                .prepare_cached(
                    "INSERT INTO project_sequences (project_id, last_sequence) VALUES (?, ?)",
                )?
                .execute(params![project_id, next])?;
        } else {
            self.connection
                .prepare_cached(
                    "UPDATE project_sequences SET last_sequence = ? WHERE project_id = ?",
                )?
                .execute(params![next, project_id])?;
        }
        self.connection
            .prepare_cached("INSERT INTO events (project_id, sequence, event_id, event_type, timestamp, event_json) VALUES (?, ?, ?, ?, ?, ?)")?
            .execute(params![project_id, next, event["eventId"].as_str(), event["type"].as_str(), event["timestamp"].as_str(), canonical_json(&event)])?;
        self.connection
            .prepare_cached("INSERT INTO projection_heads (project_id, sequence, updated_at) VALUES (?, ?, ?) ON CONFLICT(project_id) DO UPDATE SET sequence = excluded.sequence, updated_at = excluded.updated_at")?
            .execute(params![project_id, next, event["timestamp"].as_str()])?;
        Ok(event)
    }

    fn query_operations(&self, sql: &str, args: &[&str]) -> Result<Vec<Value>> {
        let mut statement = self.connection.prepare_cached(sql)?;
        let rows = statement.query_map(rusqlite::params_from_iter(args), |row| {
            Ok((
                row.get::<_, String>("intent_id")?,
                row.get::<_, String>("project_id")?,
                row.get::<_, String>("operation_type")?,
                row.get::<_, String>("idempotency_key")?,
                row.get::<_, String>("state")?,
                row.get::<_, String>("request_json")?,
                row.get::<_, Option<String>>("result_json")?,
                row.get::<_, Option<String>>("error")?,
                row.get::<_, String>("created_at")?,
                row.get::<_, String>("updated_at")?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (
                intent_id,
                project_id,
                operation_type,
                idempotency_key,
                state,
                request,
                result,
                error,
                created_at,
                updated_at,
            ) = row?;
            let request: Value = serde_json::from_str(&request)?;
            let result: Value = match result.filter(|json| !json.is_empty()) {
                Some(json) => serde_json::from_str(&json)?,
                None => Value::Null,
            };
            out.push(json!({ "intentId": intent_id, "projectId": project_id, "operationType": operation_type, "idempotencyKey": idempotency_key, "state": state, "request": request, "result": result, "error": error, "createdAt": created_at, "updatedAt": updated_at }));
        }
        Ok(out)
    }

    fn finish_operation(
        &self,
        project_id: &str,
        intent_id: &str,
        state: &str,
        result: Value,
        error: Value,
    ) -> Result<Value> {
        self.transaction(|| {
            let Some(current) = self.query_operations("SELECT * FROM operation_intents WHERE project_id = ? AND intent_id = ?", &[project_id, intent_id])?.pop() else {
                return rejected("Unknown operation intent");
            };
            if current["state"] != "pending" {
                if current["state"] != state || canonical_json(&current["result"]) != canonical_json(&result) || current["error"] != error {
                    return rejected("Operation intent already has a different terminal result");
                }
                return Ok(current);
            }
            let now = now_utc();
            let result_json = if result.is_null() { None } else { Some(canonical_json(&result)) };
            self.connection
                .prepare_cached("UPDATE operation_intents SET state = ?, result_json = ?, error = ?, updated_at = ? WHERE project_id = ? AND intent_id = ?")?
                .execute(params![state, result_json, error.as_str(), now, project_id, intent_id])?;
            let mut finished = object(current);
            finished.insert("state".into(), json!(state));
            finished.insert("result".into(), result);
            finished.insert("error".into(), error);
            finished.insert("updatedAt".into(), json!(now));
            Ok(Value::Object(finished))
        })
    }
}

fn projection_value(
    project_id: &str,
    entity_type: &str,
    entity_id: &str,
    version: i64,
    state: &str,
    value: Value,
    updated_at: &str,
) -> Result<Value> {
    Ok(
        json!({ "projectId": project_id, "entityType": entity_type, "entityId": entity_id, "version": version, "state": state, "value": value, "updatedAt": updated_at }),
    )
}

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => unreachable!("validated drafts and events are objects"),
    }
}

fn project_of(draft: &Value) -> &str {
    draft["scope"]["projectId"]
        .as_str()
        .expect("validated drafts have a project scope")
}

fn sequence(event: &Value) -> i64 {
    event["sequence"].as_i64().unwrap_or(0)
}

/// Whether the event that a legacy receipt recorded came from this draft (`sameDraft`).
fn same_draft(event: &Value, draft: &Value) -> bool {
    let fields = [
        "$schema",
        "schemaVersion",
        "retention",
        "type",
        "source",
        "scope",
        "correlationId",
        "causationId",
        "payload",
    ];
    let picked: Map<String, Value> = fields
        .iter()
        .filter_map(|key| {
            event
                .get(*key)
                .map(|value| ((*key).to_owned(), value.clone()))
        })
        .collect();
    canonical_json(&Value::Object(picked)) == canonical_json(draft)
}

/// `!value.trim()`: JavaScript trims WhiteSpace and LineTerminator characters, which is Unicode
/// White_Space without U+0085 and with U+FEFF.
fn js_trim_is_empty(value: &str) -> bool {
    value
        .chars()
        .all(|c| (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}')
}

impl From<Page> for Value {
    fn from(page: Page) -> Self {
        json!({ "events": page.events, "nextCursor": page.next_cursor, "hasMore": page.has_more })
    }
}

impl From<Appended> for Value {
    fn from(appended: Appended) -> Self {
        json!({ "receipt": appended.receipt, "replayed": appended.replayed })
    }
}

impl From<ProjectionMutation> for Value {
    fn from(mutation: ProjectionMutation) -> Self {
        json!({ "event": mutation.event, "projection": mutation.projection, "records": mutation.records, "replayed": mutation.replayed })
    }
}

impl Error {
    /// Whether this is a wire-validation failure rather than a store rule.
    pub fn is_invalid(&self) -> bool {
        matches!(self, Error::Invalid(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(project_id: &str, event_type: &str) -> Value {
        json!({
            "$schema": nosh_wire::EVENT_SCHEMA, "schemaVersion": 1, "retention": "persistent", "type": event_type, "source": "test",
            "scope": { "projectId": project_id, "missionId": null, "directionId": null, "autoresearchId": null, "experimentId": null, "runId": null, "jobId": null, "agentId": null },
            "correlationId": null, "causationId": null, "payload": { "marker": event_type }
        })
    }

    fn store() -> (tempfile::TempDir, EventStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = EventStore::open(&dir.path().join("project.sqlite")).unwrap();
        (dir, store)
    }

    fn types(events: &[Value]) -> Vec<&str> {
        events.iter().map(|e| e["type"].as_str().unwrap()).collect()
    }

    /// Port of "replays from a parse-once cache that sees new appends, cursors, and never a rolled-back row".
    #[test]
    fn cache_never_keeps_a_rolled_back_row() {
        let (_dir, store) = store();
        let project = create_id(IdPrefix::Prj);
        store.append(&draft(&project, "agent.started")).unwrap();
        store.append(&draft(&project, "agent.started")).unwrap();
        assert_eq!(store.replay(&project, 0).unwrap().len(), 2);
        store.append(&draft(&project, "agent.completed")).unwrap();
        assert_eq!(
            types(&store.replay(&project, 0).unwrap()),
            ["agent.started", "agent.started", "agent.completed"]
        );
        assert_eq!(store.replay(&project, 2).unwrap()[0]["sequence"], 3);
        assert!(store.replay(&project, 3).unwrap().is_empty());

        let outcome: Result<()> = store.transaction(|| {
            store.append(&draft(&project, "agent.failed"))?;
            assert_eq!(store.replay(&project, 0)?.len(), 4);
            rejected("roll back")
        });
        assert_eq!(outcome.unwrap_err().to_string(), "roll back");
        assert_eq!(
            types(&store.replay(&project, 0).unwrap()),
            ["agent.started", "agent.started", "agent.completed"]
        );
        store.append(&draft(&project, "agent.failed")).unwrap();
        let sequences: Vec<i64> = store
            .replay(&project, 0)
            .unwrap()
            .iter()
            .map(sequence)
            .collect();
        assert_eq!(sequences, [1, 2, 3, 4]);
    }

    /// Port of "permits only one terminal schema per task tool across retrying store handles" and
    /// the cross-schema race: two handles on one file, one terminal record wins.
    #[test]
    fn one_terminal_record_per_task_across_handles() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("project.sqlite");
        let (first, second) = (
            EventStore::open(&path).unwrap(),
            EventStore::open(&path).unwrap(),
        );
        let project = create_id(IdPrefix::Prj);
        let task = create_id(IdPrefix::Tsk);
        let record = |schema: &str| json!({ "$schema": format!("https://nosh.dev/schemas/{schema}/v1"), "marker": schema });
        let submit = |store: &EventStore, schema: &str| {
            let mut event = draft(&project, "record.submitted");
            event["correlationId"] = json!(format!("task:{task}"));
            event["payload"] = record(schema);
            store.append_terminal_submission(
                &task,
                "nosh_response_submit",
                &format!("https://nosh.dev/schemas/{schema}/v1"),
                &record(schema),
                &event,
            )
        };
        let won = submit(&first, "general-worker-completion").unwrap();
        let retry = submit(&second, "general-worker-completion").unwrap();
        assert!(retry.replayed && !won.replayed);
        assert_eq!(
            retry.receipt["event"]["eventId"],
            won.receipt["event"]["eventId"]
        );
        let error = submit(&second, "librarian-completion")
            .unwrap_err()
            .to_string();
        assert!(error.contains("already has terminal"), "{error}");
        assert_eq!(first.replay(&project, 0).unwrap().len(), 1);
    }

    #[test]
    fn javascript_trim() {
        assert!(js_trim_is_empty(" \t\n\u{a0}\u{feff}\u{2028}"));
        assert!(!js_trim_is_empty("\u{85}"));
        assert!(!js_trim_is_empty(" x "));
    }
}
