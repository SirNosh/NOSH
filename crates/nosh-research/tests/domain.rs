//! Differential tests of the domain packages: `conformance/domain-scenario.json` runs through the
//! TypeScript graph, scheduler, skills, episodes, threads, and agent-runtime packages
//! (`scripts/domain-ts.mjs`, needs a built TypeScript tree) and through this crate. Results must
//! be equal as canonical JSON once timestamps are masked; error messages must match exactly.

use nosh_research::agent_runtime::{StructuredSubmissionGate, validate_teachback};
use nosh_research::episodes::{
    create_episode, episode_integrity, fallback_episode_draft, render_episode,
};
use nosh_research::graph::{
    ExperimentReview, ExperimentTree, VersionedDag, is_legal_direction_transition,
    is_legal_mission_transition,
};
use nosh_research::scheduler::{FocusGovernor, Scheduler, SchedulingContext};
use nosh_research::skills::{SkillRegistry, compile_program, validate_state};
use nosh_research::threads::{create_thread, ensure_can_step, render_thread_context};
use nosh_research::{Error, Result};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const MASKED: &[&str] = &["createdAt", "updatedAt"];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Expands `$node`, `$fixture` (with `set`/`delete`), and `$repeat` shorthand.
fn expand(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(expand).collect()),
        Value::Object(map) if map.contains_key("$node") => {
            let mut node = json!({ "id": "", "type": "general_worker", "title": "node", "required": true, "criterionIds": [], "hardDependencies": [], "softDependencies": [], "state": "pending", "attempt": 0, "maximumAttempts": 2, "priority": 0, "criticalWeight": 1, "createdAt": "2026-01-01T00:00:00.000Z", "lease": null });
            for (key, item) in map["$node"].as_object().unwrap() {
                node[key] = expand(item);
            }
            node.as_object_mut()
                .unwrap()
                .entry("title")
                .and_modify(|t| {
                    *t = json!(format!("node {}", map["$node"]["id"].as_str().unwrap()))
                });
            node
        }
        Value::Object(map) if map.contains_key("$fixture") => {
            let path = repo_root()
                .join("conformance/schemas/fixtures")
                .join(format!("{}.json", map["$fixture"].as_str().unwrap()));
            let mut fixture: Value = serde_json::from_str(
                &std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("{}: {e}", path.display())),
            )
            .unwrap();
            for pointer in map
                .get("delete")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let (parent, key) = pointer.as_str().unwrap().rsplit_once('/').unwrap();
                let target = if parent.is_empty() {
                    &mut fixture
                } else {
                    fixture.pointer_mut(parent).unwrap()
                };
                match target {
                    Value::Object(object) => {
                        object.remove(&key.replace("~1", "/").replace("~0", "~"));
                    }
                    Value::Array(items) => {
                        items.remove(key.parse().unwrap());
                    }
                    _ => panic!("cannot delete {pointer}"),
                }
            }
            for (pointer, item) in map
                .get("set")
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
            {
                let (parent, key) = pointer.rsplit_once('/').unwrap();
                let target = if parent.is_empty() {
                    &mut fixture
                } else {
                    fixture
                        .pointer_mut(parent)
                        .unwrap_or_else(|| panic!("no {parent}"))
                };
                match target {
                    Value::Object(object) => {
                        object.insert(key.replace("~1", "/").replace("~0", "~"), expand(item));
                    }
                    Value::Array(items) => items[key.parse::<usize>().unwrap()] = expand(item),
                    _ => panic!("cannot set {pointer}"),
                }
            }
            fixture
        }
        Value::Object(map) if map.contains_key("$repeat") => {
            let [text, count] = map["$repeat"].as_array().unwrap().as_slice() else {
                panic!("$repeat")
            };
            json!(
                text.as_str()
                    .unwrap()
                    .repeat(usize::try_from(count.as_u64().unwrap()).unwrap())
            )
        }
        Value::Object(map) => {
            Value::Object(map.iter().map(|(k, v)| (k.clone(), expand(v))).collect())
        }
        other => other.clone(),
    }
}

fn typescript(ops: &[Value]) -> Vec<Value> {
    let mut child = Command::new("node")
        .arg(repo_root().join("scripts/domain-ts.mjs"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("node is on PATH");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(json!({ "ops": ops }).to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "domain-ts.mjs failed (is the TypeScript tree built?):\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let results: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
    results
        .into_iter()
        .map(|r| {
            if r["invalid"] == true {
                json!({ "invalid": true })
            } else if let Some(e) = r.get("error") {
                json!({ "error": e })
            } else {
                r
            }
        })
        .collect()
}

enum Object {
    Dag(VersionedDag),
    Tree(ExperimentTree),
    Scheduler(Scheduler),
    Focus(FocusGovernor),
    Skills(SkillRegistry),
    Gate(StructuredSubmissionGate),
}

fn rust(ops: &[Value]) -> Vec<Value> {
    let mut objects: HashMap<String, Object> = HashMap::new();
    let mut results: Vec<Value> = Vec::new();
    for op in ops {
        let a = substitute(op, &results);
        let outcome = run_op(&a, &mut objects);
        results.push(match outcome {
            Ok(value) => json!({ "ok": value }),
            Err(Error::Invalid(_)) => json!({ "invalid": true }),
            Err(error) => json!({ "error": error.to_string() }),
        });
    }
    results
}

fn run_op(a: &Value, objects: &mut HashMap<String, Object>) -> Result<Value> {
    let s = |key: &str| {
        a[key]
            .as_str()
            .unwrap_or_else(|| panic!("{key} in {a}"))
            .to_owned()
    };
    let strings = |value: &Value| {
        value
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|i| i.as_str().unwrap().to_owned())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    let name = a
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let to_value = |value: &dyn erased::Serialize| -> Result<Value> { Ok(value.to_json()?) };
    macro_rules! get {
        ($variant:ident) => {
            match objects.get_mut(&name) {
                Some(Object::$variant(object)) => object,
                _ => panic!("no {} {name}", stringify!($variant)),
            }
        };
    }
    Ok(match a["op"].as_str().unwrap() {
        "dag.new" => {
            let nodes = serde_json::from_value(a["nodes"].clone())?;
            objects.insert(
                name.clone(),
                Object::Dag(VersionedDag::new(
                    &s("scopeId"),
                    nodes,
                    &s("rationale"),
                    a["initialVersion"].as_i64().unwrap(),
                )?),
            );
            Value::Null
        }
        "dag.apply" => to_value(&get!(Dag).apply(
            a["baseVersion"].as_i64().unwrap(),
            a["operations"].as_array().unwrap(),
            &s("rationale"),
            &strings(&a["evidenceIds"]),
        )?)?,
        "dag.transition" => to_value(&get!(Dag).transition(&s("nodeId"), &s("next"))?)?,
        "dag.lease" => {
            to_value(&get!(Dag).lease(&s("nodeId"), serde_json::from_value(a["lease"].clone())?)?)?
        }
        "dag.releaseExpired" => json!(get!(Dag).release_expired(a["now"].as_f64().unwrap())),
        "dag.current" => to_value(&get!(Dag).current())?,
        "dag.versions" => to_value(&get!(Dag).versions())?,
        "dag.runnable" => to_value(&get!(Dag).runnable())?,
        "dag.completionReady" => json!(get!(Dag).completion_ready()),
        "dag.criticalPath" => json!(get!(Dag).critical_path()?),
        "dag.diff" => {
            to_value(&get!(Dag).diff(a["from"].as_i64().unwrap(), a["to"].as_i64().unwrap())?)?
        }
        "legal.mission" => json!(is_legal_mission_transition(&s("current"), &s("next"))),
        "legal.direction" => json!(is_legal_direction_transition(&s("current"), &s("next"))),
        "tree.new" => {
            objects.insert(
                name.clone(),
                Object::Tree(ExperimentTree::new(
                    &s("hash"),
                    a["maximumDepth"].as_f64().unwrap(),
                    a["maximumChildren"].as_f64().unwrap(),
                )),
            );
            Value::Null
        }
        "tree.add" => to_value(&get!(Tree).add(serde_json::from_value(a["experiment"].clone())?)?)?,
        "tree.evaluate" => {
            let review: Option<ExperimentReview> = serde_json::from_value(a["review"].clone())?;
            to_value(&get!(Tree).evaluate(
                &s("id"),
                &s("commit"),
                &s("state"),
                review.as_ref(),
                a["guardrailsPass"].as_bool().unwrap(),
            )?)?
        }
        "tree.get" => to_value(&get!(Tree).get(&s("id"))?)?,
        "tree.all" => to_value(&get!(Tree).all())?,
        "tree.frontier" => to_value(&get!(Tree).frontier())?,
        "tree.shapeWarnings" => json!(get!(Tree).shape_warnings()),
        "scheduler.new" => {
            objects.insert(name.clone(), Object::Scheduler(Scheduler::default()));
            Value::Null
        }
        "scheduler.activate" => get!(Scheduler)
            .activate_mission(&s("projectId"), &s("missionId"))
            .map(|()| Value::Null)?,
        "scheduler.release" => {
            get!(Scheduler).release_mission(&s("projectId"), &s("missionId"));
            Value::Null
        }
        "scheduler.frontier" => {
            let c = &a["context"];
            let context = SchedulingContext {
                paused: c["paused"].as_bool().unwrap(),
                workspace_conflicts: strings(&c["workspaceConflicts"]).into_iter().collect(),
                available_roles: strings(&c["availableRoles"]).into_iter().collect(),
                available_agents: c["available"]["agents"].as_f64().unwrap(),
            };
            let Some(Object::Dag(dag)) = objects.get(&s("dag")) else {
                panic!("no dag")
            };
            let Some(Object::Scheduler(scheduler)) = objects.get(&name) else {
                panic!("no scheduler")
            };
            to_value(&scheduler.frontier(dag, &context, a["now"].as_f64().unwrap()))?
        }
        "scheduler.record" => {
            get!(Scheduler).record_allocation(&s("nodeId"));
            Value::Null
        }
        "scheduler.restore" => {
            get!(Scheduler).restore_allocations(a["distribution"].as_object().unwrap());
            Value::Null
        }
        "scheduler.distribution" => json!(get!(Scheduler).distribution()),
        "focus.new" => {
            objects.insert(name.clone(), Object::Focus(FocusGovernor::default()));
            Value::Null
        }
        "focus.fingerprint" => json!(get!(Focus).fingerprint(&a["input"])?),
        "focus.record" => get!(Focus).record(
            &a["input"],
            &a["delta"],
            a.get("override").and_then(Value::as_str),
        )?,
        "focus.clear" => get!(Focus)
            .clear(&s("nodeId"), &a["proof"])
            .map(|()| Value::Null)?,
        "skills.new" => {
            objects.insert(name.clone(), Object::Skills(SkillRegistry::default()));
            Value::Null
        }
        "skills.add" => get!(Skills).add(&a["input"])?,
        "skills.get" => get!(Skills).get(&s("skillId"))?,
        "skills.list" => json!(get!(Skills).list()),
        "skills.applicable" => get!(Skills).applicable(&s("skillId"), &a["thread"])?,
        "compileProgram" => compile_program(&a["input"])?,
        "validateState" => validate_state(
            a["schema"].as_object().unwrap(),
            a["values"].as_object().unwrap(),
        )
        .map(|()| Value::Null)?,
        "createEpisode" => create_episode(a["input"].as_object().unwrap())?,
        "fallbackEpisodeDraft" => fallback_episode_draft(&s("episodeType"), &s("summary")),
        "renderEpisode" => json!(render_episode(&a["episode"])),
        "episodeIntegrity" => json!(episode_integrity(&a["episode"])),
        "createThread" => create_thread(&a["instruction"])?,
        "ensureCanStep" => ensure_can_step(&a["thread"]).map(|()| Value::Null)?,
        "renderThreadContext" => json!(render_thread_context(
            &a["thread"],
            a["episodes"].as_array().unwrap(),
            &strings(&a["extraRefs"]),
            &strings(&a["skillPrompts"])
        )),
        "gate.new" => {
            objects.insert(
                name.clone(),
                Object::Gate(StructuredSubmissionGate::default()),
            );
            Value::Null
        }
        "gate.submit" => get!(Gate).submit(&s("tool"), &s("key"), &a["record"]),
        "gate.supported" => json!(get!(Gate).supported_schemas()),
        "validateTeachback" => validate_teachback(&a["state"], &a["teachback"]),
        other => panic!("unknown op {other}"),
    })
}

/// Lets `run_op` serialize any result type through one closure.
mod erased {
    pub trait Serialize {
        fn to_json(&self) -> serde_json::Result<serde_json::Value>;
    }
    impl<T: serde::Serialize> Serialize for T {
        fn to_json(&self) -> serde_json::Result<serde_json::Value> {
            serde_json::to_value(self)
        }
    }
}

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
                    .collect::<Map<_, _>>(),
            ),
        },
        other => other.clone(),
    }
}

/// Timestamps masked, and validation `errors` lists compared as sets: zod reports issues in
/// schema-definition order, JSON Schema alphabetically (the issues themselves must match).
fn mask(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(mask).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    let masked = if MASKED.contains(&k.as_str()) && v.is_string() {
                        json!("<masked>")
                    } else {
                        mask(v)
                    };
                    let masked = match (k.as_str(), masked) {
                        ("errors", Value::Array(mut issues)) => {
                            issues.sort_by_key(|issue| issue.to_string());
                            Value::Array(issues)
                        }
                        (_, other) => other,
                    };
                    (k.clone(), masked)
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

#[test]
fn rust_domain_matches_typescript_domain() {
    let scenario: Value = serde_json::from_str(
        &std::fs::read_to_string(repo_root().join("conformance/domain-scenario.json")).unwrap(),
    )
    .unwrap();
    let mut ops = Vec::new();
    let mut labels = Vec::new();
    for group in scenario["groups"].as_array().unwrap() {
        for (index, op) in group["ops"].as_array().unwrap().iter().enumerate() {
            ops.push(expand(op));
            labels.push(format!(
                "\"{}\" op {index} ({})",
                group["name"].as_str().unwrap(),
                op["op"]
            ));
        }
    }
    let (theirs, ours) = (typescript(&ops), rust(&ops));
    let failures: Vec<String> = (0..ops.len())
        .filter(|&i| {
            nosh_wire::canonical_json(&mask(&theirs[i]))
                != nosh_wire::canonical_json(&mask(&ours[i]))
        })
        .map(|i| {
            format!(
                "{}\n    TypeScript: {}\n    Rust:       {}",
                labels[i],
                mask(&theirs[i]),
                mask(&ours[i])
            )
        })
        .collect();
    assert!(
        failures.is_empty(),
        "{} difference(s) from TypeScript:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
