//! Differential tests against the TypeScript store.
//!
//! `conformance/store-scenario.json` runs through the TypeScript `EventStore`/`HostRegistry`
//! (`scripts/store-ts.mjs`, which needs a built TypeScript tree) and through this crate. Every
//! result must match once generated IDs and timestamps are masked; hashes, sequences, cursors and
//! versions are compared exactly. Runs that switch implementation partway through the same
//! database prove each side reads, replays and continues what the other wrote, including
//! idempotency receipts, whose intent hashes must agree for the replay to succeed. Finally the
//! databases themselves must hold the same schema and rows.

use nosh_store::{Error, EventStore, GraphInput, HostRegistry, ProjectionInput};
use rusqlite::Connection;
use serde_json::{Map, Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const MASKED: &[&str] = &[
    "eventId",
    "timestamp",
    "createdAt",
    "updatedAt",
    "registeredAt",
    "intentId",
    "event_id",
    "created_at",
    "updated_at",
    "registered_at",
    "applied_at",
    "intent_id",
    "recorded_at",
];

fn rust_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    ops: Vec<Value>,
    /// Index of the first op of each scenario group, for split runs and messages.
    groups: Vec<(usize, String)>,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        for repo in ["repo-a", "repo-b", "repo-c", "repo-d"] {
            std::fs::create_dir(root.join(repo)).unwrap();
        }
        std::fs::write(root.join("repo-a/file.txt"), "").unwrap();
        // A junction, which Node's realpath follows like a symbolic link and needs no privilege.
        let status = Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(root.join("link-to-c"))
            .arg(root.join("repo-c"))
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "mklink /J failed");

        let tmp = root.to_string_lossy().into_owned();
        let scenario =
            std::fs::read_to_string(rust_root().join("conformance/store-scenario.json")).unwrap();
        let scenario = scenario
            .replace("{tmpLower}", &tmp.to_lowercase().replace('\\', "\\\\"))
            .replace("{tmp}", &tmp.replace('\\', "\\\\"));
        let scenario: Value = serde_json::from_str(&scenario).unwrap();
        let mut ops = Vec::new();
        let mut groups = Vec::new();
        for group in scenario["groups"].as_array().unwrap() {
            groups.push((ops.len(), group["name"].as_str().unwrap().to_owned()));
            ops.extend(group["ops"].as_array().unwrap().iter().map(expand));
        }
        Fixture {
            _dir: dir,
            root,
            ops,
            groups,
        }
    }

    fn databases(&self, name: &str) -> (PathBuf, PathBuf) {
        (
            self.root.join(format!("{name}.sqlite")),
            self.root.join(format!("{name}-host.sqlite")),
        )
    }

    fn describe(&self, index: usize) -> String {
        let group = self
            .groups
            .iter()
            .rev()
            .find(|(start, _)| *start <= index)
            .unwrap();
        format!(
            "op {index} ({}: {}) in \"{}\"",
            index - group.0,
            self.ops[index]["op"],
            group.1
        )
    }
}

/// Expands draft shorthand ({ project, type, ... }) into a full event draft.
fn expand(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(expand).collect()),
        Value::Object(map) if map.contains_key("project") => {
            let project = format!(
                "prj_{}",
                map["project"].as_str().unwrap().to_lowercase().repeat(32)
            );
            json!({
                "$schema": "https://nosh.dev/schemas/event/v1", "schemaVersion": 1,
                "retention": map.get("retention").cloned().unwrap_or(json!("persistent")),
                "type": map["type"], "source": "differential-test",
                "scope": { "projectId": project, "missionId": map.get("missionId").cloned().unwrap_or(Value::Null), "directionId": null, "autoresearchId": null, "experimentId": null, "runId": null, "jobId": null, "agentId": null },
                "correlationId": map.get("correlationId").cloned().unwrap_or(Value::Null), "causationId": null,
                "payload": map.get("payload").cloned().unwrap_or_else(|| json!({ "marker": map["type"] })),
            })
        }
        Value::Object(map) => {
            Value::Object(map.iter().map(|(k, v)| (k.clone(), expand(v))).collect())
        }
        other => other.clone(),
    }
}

fn run_typescript(store: &Path, host: &Path, prior: &[Value], ops: &[Value]) -> Vec<Value> {
    let request = json!({ "eventStore": store, "host": host, "prior": prior, "ops": ops });
    let mut child = Command::new("node")
        .arg(rust_root().join("scripts/store-ts.mjs"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("node is on PATH");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(request.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "store-ts.mjs failed (is the TypeScript tree built?):\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let results: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
    results.into_iter().map(normalize_typescript).collect()
}

/// TypeScript results as comparable values: zod failures and OS errors keep only their class.
fn normalize_typescript(result: Value) -> Value {
    if result.get("invalid") == Some(&json!(true)) {
        return json!({ "invalid": true });
    }
    if let Some(message) = result.get("error").and_then(Value::as_str) {
        let os = message.split_once(':').is_some_and(|(code, _)| {
            code.len() > 1 && code.starts_with('E') && code.bytes().all(|b| b.is_ascii_uppercase())
        });
        return if os {
            json!({ "os": true })
        } else {
            json!({ "error": message })
        };
    }
    result
}

fn run_rust(store_path: &Path, host_path: &Path, prior: &[Value], ops: &[Value]) -> Vec<Value> {
    let mut results: Vec<Value> = prior.to_vec();
    let mut output = Vec::new();
    let mut store = EventStore::open(store_path).unwrap();
    let mut host = HostRegistry::open(host_path).unwrap();
    for op in ops {
        let a = substitute(op, &results);
        let s = |key: &str| {
            a[key]
                .as_str()
                .unwrap_or_else(|| panic!("{key} in {a}"))
                .to_owned()
        };
        let i = |key: &str| a[key].as_i64().unwrap();
        let outcome: Result<Value, Error> = match a["op"].as_str().unwrap() {
            "append" => store.append(&a["draft"]),
            "appendIdempotent" => store
                .append_idempotent(&s("key"), &a["draft"])
                .map(Value::from),
            "appendTerminalSubmission" => store
                .append_terminal_submission(
                    &s("taskId"),
                    &s("toolName"),
                    &s("schemaUri"),
                    &a["record"],
                    &a["draft"],
                )
                .map(Value::from),
            "replay" => store.replay(&s("projectId"), i("after")).map(Value::from),
            "replayPage" => store
                .replay_page(&s("projectId"), i("after"), i("limit"), a["recent"] == true)
                .map(Value::from),
            "currentSequence" => store.current_sequence(&s("projectId")).map(Value::from),
            "commandReceipt" => store
                .command_receipt(&s("projectId"), &s("key"))
                .map(Option::unwrap_or_default),
            "terminalTurn" => store.terminal_turn(&s("projectId"), &s("turnId"), &a["context"]),
            "saveTerminalTurn" => store
                .save_terminal_turn(&s("projectId"), &s("turnId"), &a["receipt"])
                .map(|()| Value::Null),
            "beginOperation" => store.begin_operation(
                &s("projectId"),
                &s("operationType"),
                &s("key"),
                &a["request"],
            ),
            "completeOperation" => {
                store.complete_operation(&s("projectId"), &s("intentId"), &a["result"])
            }
            "failOperation" => store.fail_operation(&s("projectId"), &s("intentId"), &s("error")),
            "operationIntents" => store
                .operation_intents(&s("projectId"), a.get("state").and_then(Value::as_str))
                .map(Value::from),
            "projection" => store
                .projection(&s("projectId"), &s("entityType"), &s("entityId"))
                .map(Option::unwrap_or_default),
            "projections" => store
                .projections(&s("projectId"), &s("entityType"))
                .map(Value::from),
            "mutateProjection" => store
                .mutate_projection(
                    &s("key"),
                    a["expectedVersion"].as_i64(),
                    &a["draft"],
                    &projection_input(&a["input"]),
                )
                .map(Value::from),
            "graphVersions" => store
                .graph_versions(&s("projectId"), &s("scopeType"), &s("scopeId"))
                .map(Value::from),
            "writeSnapshot" => store.write_snapshot(&s("projectId"), i("sequence"), &a["state"]),
            "latestSnapshot" => store
                .latest_snapshot(&s("projectId"))
                .map(Option::unwrap_or_default),
            "register" => host.register(
                &s("projectId"),
                Path::new(&s("repositoryRoot")),
                Path::new(&s("databasePath")),
            ),
            "list" => host.list().map(Value::from),
            "exec" => {
                Connection::open(store_path)
                    .unwrap()
                    .execute_batch(&s("sql"))
                    .unwrap();
                Ok(Value::Null)
            }
            "reopen" => {
                drop((store, host));
                store = EventStore::open(store_path).unwrap();
                host = HostRegistry::open(host_path).unwrap();
                Ok(Value::Null)
            }
            other => panic!("unknown op {other}"),
        };
        let result = match outcome {
            Ok(value) => json!({ "ok": value }),
            Err(Error::Invalid(_)) => json!({ "invalid": true }),
            Err(Error::Io(_)) => json!({ "os": true }),
            Err(error) => json!({ "error": error.to_string() }),
        };
        results.push(result.clone());
        output.push(result);
    }
    output
}

fn projection_input(input: &Value) -> ProjectionInput {
    ProjectionInput {
        entity_type: input["entityType"].as_str().unwrap().to_owned(),
        entity_id: input["entityId"].as_str().unwrap().to_owned(),
        state: input["state"].as_str().unwrap().to_owned(),
        value: input["value"].clone(),
        graph: input.get("graph").map(|g| GraphInput {
            scope_type: g["scopeType"].as_str().unwrap().to_owned(),
            version: g["version"].as_i64().unwrap(),
            value: g["value"].clone(),
            rationale: g["rationale"].as_str().unwrap().to_owned(),
        }),
        records: input
            .get("records")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        intent: input.get("intent").cloned(),
    }
}

/// Resolves { "$ref": [index, ...path] } against earlier results, as store-ts.mjs does.
fn substitute(value: &Value, results: &[Value]) -> Value {
    match value {
        Value::Array(items) => {
            Value::Array(items.iter().map(|item| substitute(item, results)).collect())
        }
        Value::Object(map) => match map.get("$ref").and_then(Value::as_array) {
            Some(reference) => {
                let index = reference[0].as_i64().unwrap();
                let index = usize::try_from(if index < 0 {
                    results.len() as i64 + index
                } else {
                    index
                })
                .unwrap();
                reference[1..]
                    .iter()
                    .fold(results[index]["ok"].clone(), |node, key| {
                        node[key.as_str().unwrap()].clone()
                    })
            }
            None => Value::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), substitute(v, results)))
                    .collect(),
            ),
        },
        other => other.clone(),
    }
}

fn mask(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(mask).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        if MASKED.contains(&k.as_str()) && !v.is_null() {
                            json!("<masked>")
                        } else {
                            mask(v)
                        },
                    )
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Ops whose results are ordered by a millisecond timestamp: rows written in the same millisecond
/// tie, and their order then depends on timing, not on the implementation.
const ORDERED_BY_TIMESTAMP: &[&str] = &["list", "operationIntents", "projections"];

fn comparable(op: &Value, result: &Value) -> Value {
    let mut result = mask(result);
    if ORDERED_BY_TIMESTAMP.contains(&op["op"].as_str().unwrap())
        && let Some(Value::Array(rows)) = result.get_mut("ok")
    {
        rows.sort_by_key(Value::to_string);
    }
    result
}

fn compare(fixture: &Fixture, label: &str, expected: &[Value], actual: &[Value]) -> Vec<String> {
    assert_eq!(expected.len(), actual.len(), "{label}: result count");
    (0..expected.len())
        .filter(|&i| {
            comparable(&fixture.ops[i], &expected[i]) != comparable(&fixture.ops[i], &actual[i])
        })
        .map(|i| {
            format!(
                "{label}: {}\n    TypeScript: {}\n    Rust:       {}",
                fixture.describe(i),
                mask(&expected[i]),
                mask(&actual[i])
            )
        })
        .collect()
}

/// Every table's rows (in insertion order) with volatile columns masked and JSON columns parsed.
fn dump(path: &Path) -> Value {
    let connection = Connection::open(path).unwrap();
    let mut out = Map::new();
    let schema: Vec<(String, String, String, Option<String>)> = connection
        .prepare("SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY name")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    out.insert("sqlite_master".into(), json!(schema));
    for (kind, table, _, _) in &schema {
        if kind != "table" {
            continue;
        }
        let mut statement = connection
            .prepare(&format!("SELECT * FROM \"{table}\" ORDER BY rowid"))
            .unwrap();
        let columns: Vec<String> = statement
            .column_names()
            .into_iter()
            .map(str::to_owned)
            .collect();
        let rows: Vec<Value> = statement
            .query_map([], |row| {
                let mut record = Map::new();
                for (index, column) in columns.iter().enumerate() {
                    let value = match row.get_ref(index)? {
                        rusqlite::types::ValueRef::Null => Value::Null,
                        rusqlite::types::ValueRef::Integer(n) => json!(n),
                        rusqlite::types::ValueRef::Real(f) => json!(f),
                        rusqlite::types::ValueRef::Text(text) => {
                            let text = String::from_utf8_lossy(text).into_owned();
                            // Stored JSON must be canonical, as the TypeScript store writes it.
                            match serde_json::from_str::<Value>(&text) {
                                Ok(parsed)
                                    if column.ends_with("_json")
                                        && nosh_wire::canonical_json(&parsed) == text =>
                                {
                                    parsed
                                }
                                Ok(_) if column.ends_with("_json") => {
                                    json!({ "<non-canonical JSON>": text })
                                }
                                _ => Value::String(text),
                            }
                        }
                        rusqlite::types::ValueRef::Blob(_) => json!("<blob>"),
                    };
                    record.insert(column.clone(), value);
                }
                Ok(mask(&Value::Object(record)))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        out.insert(table.clone(), Value::Array(rows));
    }
    Value::Object(out)
}

#[test]
fn rust_store_matches_typescript_store() {
    let fixture = Fixture::new();
    let mut failures = Vec::new();

    let (ts_store, ts_host) = fixture.databases("typescript");
    let expected = run_typescript(&ts_store, &ts_host, &[], &fixture.ops);
    let (rust_store, rust_host) = fixture.databases("rust");
    let actual = run_rust(&rust_store, &rust_host, &[], &fixture.ops);
    failures.extend(compare(&fixture, "all-Rust run", &expected, &actual));
    for (label, ts, rust) in [
        ("event store", &ts_store, &rust_store),
        ("host registry", &ts_host, &rust_host),
    ] {
        let (theirs, ours) = (dump(ts), dump(rust));
        for (table, rows) in theirs.as_object().unwrap() {
            if ours.get(table) != Some(rows) {
                failures.push(format!(
                    "{label} table {table} differs:\n    TypeScript: {rows}\n    Rust:       {}",
                    ours.get(table).unwrap_or(&Value::Null)
                ));
            }
        }
        assert_eq!(
            theirs.as_object().unwrap().len(),
            ours.as_object().unwrap().len(),
            "{label}: same tables"
        );
    }

    // Hand the same database from one implementation to the other at several points.
    for &(split, _) in fixture.groups.iter().skip(1).step_by(2) {
        for typescript_first in [true, false] {
            let name = format!(
                "split-{split}-{}",
                if typescript_first {
                    "ts-rust"
                } else {
                    "rust-ts"
                }
            );
            let (store, host) = fixture.databases(&name);
            let (prefix_ops, suffix_ops) = fixture.ops.split_at(split);
            let prefix = if typescript_first {
                run_typescript(&store, &host, &[], prefix_ops)
            } else {
                run_rust(&store, &host, &[], prefix_ops)
            };
            let suffix = if typescript_first {
                run_rust(&store, &host, &prefix, suffix_ops)
            } else {
                run_typescript(&store, &host, &prefix, suffix_ops)
            };
            failures.extend(compare(
                &fixture,
                &name,
                &expected,
                &[prefix, suffix].concat(),
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} difference(s) from the TypeScript store:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
