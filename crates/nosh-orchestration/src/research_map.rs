//! A compact, daemon-derived picture of what a Project is doing, as
//! `apps/noshd/src/research-map.ts`: Missions and Directions with their graph nodes, Autoresearch
//! executions with their experiment tree, live workers, and running Jobs. Clients render it as-is.
//!
//! Inputs are the JSON shapes the TypeScript reads: projections `{ entityId, version, state,
//! value }`, submitted `(event, record)` pairs, events, agent inspections, and Job records.

use nosh_wire::js::{number_string, parse_number, string, to_fixed, truthy};
use nosh_wire::now_utc;
use regex::Regex;
use serde_json::{Map, Value, json};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

pub struct ResearchMapInput<'a> {
    pub missions: &'a [Value],
    pub directions: &'a [Value],
    pub autoresearch: &'a [Value],
    /// `record.submitted` events with their records.
    pub submitted: &'a [(Value, Value)],
    pub events: &'a [Value],
    pub agents: &'a [Value],
    pub jobs: &'a [Value],
}

const TERMINAL: [&str; 5] = ["completed", "stopped", "failed", "closed", "cancelled"];
const MAX_ROOTS: usize = 12;
const MAX_CHILDREN: usize = 30;
static SCORE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"score (-?[\d.e-]+)").expect("regex"));

fn leaf(kind: &str, id: &str, title: &str, state: &str, detail: &str) -> Value {
    json!({ "kind": kind, "id": id, "title": title, "state": state, "detail": detail, "progress": null, "children": [] })
}

fn text(value: &Value, key: &str) -> String {
    string(value.get(key))
}

/// `[a, b].filter(Boolean).join(" · ")`.
fn joined(parts: &[String]) -> String {
    parts
        .iter()
        .filter(|part| !part.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join(" · ")
}

/// `{ scopeType, scopeId, nodeId }` of the graph node that spawned an entry, when recorded.
fn origin_of(entry: &Value) -> Option<(String, String, String)> {
    let origin = entry.pointer("/value/origin")?;
    let scope_type = origin.get("scopeType")?.as_str()?;
    if scope_type != "mission" && scope_type != "direction" {
        return None;
    }
    Some((
        scope_type.to_owned(),
        origin.get("scopeId")?.as_str()?.to_owned(),
        origin.get("nodeId")?.as_str()?.to_owned(),
    ))
}

fn nodes_of(entry: &Value) -> Vec<Value> {
    entry
        .pointer("/value/nodes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn worker_node(agent: &Value) -> Value {
    let model = if truthy(agent.get("modelId")) {
        let thinking = if truthy(agent.get("thinkingLevel")) {
            format!(":{}", text(agent, "thinkingLevel"))
        } else {
            String::new()
        };
        format!("{}{thinking}", text(agent, "modelId"))
    } else {
        String::new()
    };
    let context = match agent.get("contextPercent") {
        Some(Value::Null) => String::new(),
        // Math.round: halves round up.
        Some(Value::Number(percent)) => format!(
            "ctx {}%",
            number_string((percent.as_f64().unwrap_or(f64::NAN) + 0.5).floor())
        ),
        _ => "ctx NaN%".to_owned(),
    };
    let role = text(agent, "role");
    let title = if role == "nosh" {
        "project agent (chat)".to_owned()
    } else {
        role.replace('_', " ")
    };
    let state = if truthy(agent.get("currentTool")) {
        format!("running {}", text(agent, "currentTool"))
    } else {
        text(agent, "status")
    };
    leaf(
        "worker",
        &text(agent, "agentId"),
        &title,
        &state,
        &joined(&[model, context]),
    )
}

struct Experiments {
    nodes: HashMap<String, Vec<Value>>,
    parents: HashMap<String, String>,
}

/// Experiment states from durable records: proposed → implementing → evaluating → promoted /
/// held / rejected, or failed.
fn experiment_tree(input: &ResearchMapInput<'_>) -> Experiments {
    let failed: HashMap<String, String> = input
        .events
        .iter()
        .filter(|event| event["type"] == "autoresearch.experiment_failed")
        .map(|event| {
            let phase = match event.pointer("/payload/phase") {
                None | Some(Value::Null) => "failed".to_owned(),
                phase => string(phase),
            };
            (string(event.pointer("/payload/experimentId")), phase)
        })
        .collect();
    let of = |schema: &str| -> Vec<&Value> {
        let uri = format!("https://nosh.dev/schemas/{schema}/v1");
        input
            .submitted
            .iter()
            .map(|(_, record)| record)
            .filter(|record| record.get("$schema").and_then(Value::as_str) == Some(uri.as_str()))
            .collect()
    };
    let results: HashMap<String, &Value> = of("experiment-result")
        .into_iter()
        .map(|record| (text(record, "experimentId"), record))
        .collect();
    let runs: HashSet<String> = of("run-manifest")
        .into_iter()
        .map(|record| text(record, "experimentId"))
        .collect();
    let working: HashSet<String> = input
        .agents
        .iter()
        .filter(|agent| truthy(agent.get("experimentId")))
        .map(|agent| text(agent, "experimentId"))
        .collect();
    let mut nodes: HashMap<String, Vec<Value>> = HashMap::new();
    let mut parents = HashMap::new();
    for proposal in of("experiment-proposal") {
        let experiment_id = text(proposal, "experimentId");
        let autoresearch_id = text(proposal, "autoresearchId");
        let result = results.get(&experiment_id);
        let state = if let Some(result) = result {
            text(result, "promotionDecision")
        } else if failed.contains_key(&experiment_id) {
            "failed".to_owned()
        } else if runs.contains(&experiment_id) {
            "evaluating".to_owned()
        } else if working.contains(&experiment_id) {
            "implementing".to_owned()
        } else {
            "proposed".to_owned()
        };
        let score = result.and_then(|result| result.pointer("/comparison/candidateScore"));
        let delta = result.and_then(|result| result.pointer("/comparison/improvement"));
        let score_text = match score.and_then(Value::as_f64) {
            Some(score) => {
                let change = match delta.and_then(Value::as_f64) {
                    Some(delta) => format!(
                        " ({}{})",
                        if delta >= 0.0 { "+" } else { "" },
                        number_string(parse_number(&to_fixed(delta, 4)))
                    ),
                    None => String::new(),
                };
                format!("score {}{change}", number_string(score))
            }
            None => String::new(),
        };
        let guardrail = result
            .and_then(|result| result.get("guardrails"))
            .and_then(Value::as_array)
            .is_some_and(|guards| {
                guards
                    .iter()
                    .any(|guard| guard.get("passed") == Some(&Value::Bool(false)))
            });
        let detail = joined(&[
            format!("round {}", string(proposal.get("round"))),
            score_text,
            if guardrail {
                "guardrail failed".to_owned()
            } else {
                String::new()
            },
            failed
                .get(&experiment_id)
                .map(|phase| format!("failed in {phase}"))
                .unwrap_or_default(),
        ]);
        let list = nodes.entry(autoresearch_id).or_default();
        if !list
            .iter()
            .any(|entry| entry["id"] == experiment_id.as_str())
        {
            let title = match proposal.get("hypothesis") {
                None | Some(Value::Null) => experiment_id.clone(),
                hypothesis => string(hypothesis),
            };
            list.push(leaf("experiment", &experiment_id, &title, &state, &detail));
        }
        let parent = match proposal.get("parentExperimentId") {
            None | Some(Value::Null) => String::new(),
            parent => string(parent),
        };
        parents.insert(experiment_id, parent);
    }
    Experiments { nodes, parents }
}

struct Builder<'a> {
    input: &'a ResearchMapInput<'a>,
    workers: HashMap<String, &'a Value>,
    running_jobs: Vec<&'a Value>,
    /// Spawned Directions (true) and Autoresearch (false) under `scopeId/nodeId`.
    spawned: HashMap<String, Vec<(bool, &'a Value)>>,
    /// Every entity placed inside another, so it is never repeated nor emitted as a root.
    nested: RefCell<HashSet<String>>,
    experiments: Experiments,
}

impl<'a> Builder<'a> {
    fn job_node(job: &Value) -> Value {
        let command: Vec<String> = job
            .get("command")
            .and_then(Value::as_array)
            .map(|parts| parts.iter().map(|part| string(Some(part))).collect())
            .unwrap_or_default();
        leaf(
            "job",
            &text(job, "jobId"),
            &nosh_wire::js::slice_to(&command.join(" "), 120),
            &text(job, "state"),
            &text(job, "runner"),
        )
    }

    fn jobs_where(&self, keep: impl Fn(&Value) -> bool) -> Vec<Value> {
        self.running_jobs
            .iter()
            .filter(|job| keep(job))
            .map(|job| Self::job_node(job))
            .collect()
    }

    fn place(&self, entries: &[(bool, &Value)]) -> Vec<Value> {
        let mut placed = Vec::new();
        for (is_direction, entry) in entries {
            let id = text(entry, "entityId");
            if !self.nested.borrow_mut().insert(id) {
                continue;
            }
            placed.push(if *is_direction {
                self.direction_node(entry)
            } else {
                self.autoresearch_node(entry)
            });
        }
        placed
    }

    fn node_tree(&self, scope_id: &str, nodes: &[Value]) -> Vec<Value> {
        nodes
            .iter()
            .take(MAX_CHILDREN)
            .map(|node| {
                let owner = node
                    .pointer("/lease/ownerId")
                    .filter(|owner| truthy(Some(owner)))
                    .and_then(|owner| self.workers.get(&string(Some(owner))));
                let attempt = node
                    .get("attempt")
                    .and_then(Value::as_f64)
                    .unwrap_or(f64::NAN);
                let state = text(node, "state");
                let attempts = if attempt > 1.0 || state == "failed" || state == "blocked" {
                    format!(
                        "attempt {}/{}",
                        string(node.get("attempt")),
                        string(node.get("maximumAttempts"))
                    )
                } else {
                    String::new()
                };
                let detail = joined(&[text(node, "type").replace('_', " "), attempts]);
                let mut tree = leaf(
                    "node",
                    &text(node, "id"),
                    &text(node, "title"),
                    &state,
                    &detail,
                );
                let mut children: Vec<Value> =
                    owner.map(|agent| worker_node(agent)).into_iter().collect();
                let key = format!("{scope_id}/{}", text(node, "id"));
                if let Some(spawned) = self.spawned.get(&key) {
                    children.extend(self.place(spawned));
                }
                tree["children"] = Value::Array(children);
                tree
            })
            .collect()
    }

    fn autoresearch_node(&self, execution: &Value) -> Value {
        let id = text(execution, "entityId");
        let value = &execution["value"];
        let all = self.experiments.nodes.get(&id).cloned().unwrap_or_default();
        // Each experiment hangs under the experiment it started from; those started from the
        // baseline are the run's children.
        let index: HashMap<String, usize> = all
            .iter()
            .enumerate()
            .map(|(position, node)| (text(node, "id"), position))
            .collect();
        let mut children_of: Vec<Vec<usize>> = vec![Vec::new(); all.len()];
        let mut top: Vec<usize> = Vec::new();
        for (position, node) in all.iter().enumerate() {
            let parent = self
                .experiments
                .parents
                .get(&text(node, "id"))
                .and_then(|parent| index.get(parent));
            let siblings = match parent {
                Some(parent) => &mut children_of[*parent],
                None => &mut top,
            };
            if siblings.len() < MAX_CHILDREN {
                siblings.push(position);
            }
        }
        fn render(
            position: usize,
            all: &[Value],
            children_of: &[Vec<usize>],
            path: &mut Vec<usize>,
        ) -> Value {
            let mut node = all[position].clone();
            // An experiment that is its own ancestor would make the TypeScript tree circular.
            path.push(position);
            let unvisited: Vec<usize> = children_of[position]
                .iter()
                .copied()
                .filter(|child| !path.contains(child))
                .collect();
            let children: Vec<Value> = unvisited
                .into_iter()
                .map(|child| render(child, all, children_of, path))
                .collect();
            path.pop();
            node["children"] = Value::Array(children);
            node
        }
        let mut children: Vec<Value> = top
            .iter()
            .map(|position| render(*position, &all, &children_of, &mut Vec::new()))
            .collect();
        // The current best is the accepted frontier (the metric may be minimized), not the largest
        // promoted score.
        let frontier: HashSet<String> = value
            .get("acceptedFrontierExperimentIds")
            .and_then(Value::as_array)
            .map(|ids| ids.iter().map(|id| string(Some(id))).collect())
            .unwrap_or_default();
        let best: Vec<f64> = all
            .iter()
            .filter(|child| frontier.contains(&text(child, "id")))
            .map(|child| {
                SCORE
                    .captures(&text(child, "detail"))
                    .map_or(f64::NAN, |found| parse_number(&found[1]))
            })
            .filter(|score| score.is_finite())
            .collect();
        let metric = value.pointer("/evaluationContract/primaryMetric/name");
        let baseline = value
            .pointer("/evaluationContract/baselineMetrics")
            .and_then(|metrics| metrics.get(string(metric)))
            .and_then(Value::as_f64);
        let or = |key: &str, default: &str| match value.get(key) {
            None | Some(Value::Null) => default.to_owned(),
            found => string(found),
        };
        let detail = joined(&[
            format!(
                "round {}/{}",
                or("currentRound", "1"),
                or("maximumRounds", "?")
            ),
            format!(
                "{}/{} experiments",
                all.len(),
                or("maximumExperiments", "?")
            ),
            best.first()
                .map(|best| format!("best {}", number_string(*best)))
                .unwrap_or_default(),
            baseline
                .map(|baseline| format!("baseline {}", number_string(baseline)))
                .unwrap_or_default(),
        ]);
        children.extend(self.jobs_where(|job| {
            job.get("autoresearchId").and_then(Value::as_str) == Some(id.as_str())
        }));
        children.truncate(MAX_CHILDREN);
        let title = match value.get("decisionQuestion") {
            None | Some(Value::Null) => id.clone(),
            question => string(question),
        };
        let mut node = leaf(
            "autoresearch",
            &id,
            &title,
            &text(execution, "state"),
            &detail,
        );
        node["children"] = Value::Array(children);
        // updatedAt lets a client pick the most recent run (the TUI sidebar's climb).
        node["updatedAt"] = json!(match value.get("updatedAt") {
            None | Some(Value::Null) => String::new(),
            updated => string(updated),
        });
        node
    }

    fn direction_node(&self, direction: &Value) -> Value {
        let id = text(direction, "entityId");
        let nodes = nodes_of(direction);
        let mut children = self.node_tree(&id, &nodes);
        // Autoresearch with no (resolvable) spawning node keeps the flat placement under its
        // Direction.
        let executions: Vec<(bool, &Value)> = self
            .input
            .autoresearch
            .iter()
            .filter(|execution| {
                execution
                    .pointer("/value/directionId")
                    .and_then(Value::as_str)
                    == Some(id.as_str())
            })
            .map(|execution| (false, execution))
            .collect();
        children.extend(self.place(&executions));
        children.extend(self.jobs_where(|job| {
            job.get("directionId").and_then(Value::as_str) == Some(id.as_str())
                && !truthy(job.get("autoresearchId"))
        }));
        graph_root("direction", direction, "question", &nodes, children)
    }
}

/// A Mission or Direction with its node progress and projection version.
fn graph_root(
    kind: &str,
    entry: &Value,
    title_key: &str,
    nodes: &[Value],
    children: Vec<Value>,
) -> Value {
    let id = text(entry, "entityId");
    let title = match entry.pointer(&format!("/value/{title_key}")) {
        None | Some(Value::Null) => id.clone(),
        title => string(title),
    };
    let version = entry.get("version").cloned().unwrap_or(Value::Null);
    let mut node = leaf(
        kind,
        &id,
        &title,
        &text(entry, "state"),
        &format!("v{}", string(Some(&version))),
    );
    node["version"] = version;
    node["progress"] = json!({ "done": nodes.iter().filter(|node| node["state"] == "accepted").count(), "total": nodes.len() });
    node["children"] = Value::Array(children);
    node
}

pub fn build_research_map(input: &ResearchMapInput<'_>) -> Value {
    let workers = input
        .agents
        .iter()
        .map(|agent| (text(agent, "agentId"), agent))
        .collect();
    let running_jobs = input
        .jobs
        .iter()
        .filter(|job| {
            !["completed", "failed", "cancelled", "lost"].contains(&text(job, "state").as_str())
        })
        .collect();
    // A child Direction / Autoresearch hangs under the graph node that spawned it: its recorded
    // origin, else (records written before origins existed) the Mission node whose lease it
    // holds. Keys are scope-qualified: node ids repeat.
    let spawnable: Vec<(bool, &Value)> = input
        .directions
        .iter()
        .map(|entry| (true, entry))
        .chain(input.autoresearch.iter().map(|entry| (false, entry)))
        .collect();
    let mut spawned: HashMap<String, Vec<(bool, &Value)>> = HashMap::new();
    for (is_direction, entry) in &spawnable {
        if let Some((_, scope_id, node_id)) = origin_of(entry) {
            spawned
                .entry(format!("{scope_id}/{node_id}"))
                .or_default()
                .push((*is_direction, entry));
        }
    }
    for mission in input.missions {
        for node in nodes_of(mission) {
            let owner = node.pointer("/lease/ownerId");
            let child = spawnable
                .iter()
                .find(|(_, entry)| strict(entry.get("entityId"), owner));
            if let Some((is_direction, child)) = child
                && origin_of(child).is_none()
            {
                spawned
                    .entry(format!(
                        "{}/{}",
                        text(mission, "entityId"),
                        text(&node, "id")
                    ))
                    .or_default()
                    .push((*is_direction, child));
            }
        }
    }
    let builder = Builder {
        input,
        workers,
        running_jobs,
        spawned,
        nested: RefCell::new(HashSet::new()),
        experiments: experiment_tree(input),
    };
    let mut roots: Vec<Value> = Vec::new();
    for mission in input.missions {
        let id = text(mission, "entityId");
        let nodes = nodes_of(mission);
        let mut children = builder.node_tree(&id, &nodes);
        // Directions opened under the Mission without a spawning node, and Mission-scoped
        // children whose node is gone.
        let owned: Vec<(bool, &Value)> = spawnable
            .iter()
            .filter(|(is_direction, entry)| match origin_of(entry) {
                Some((scope_type, scope_id, _)) => scope_type == "mission" && scope_id == id,
                None => {
                    *is_direction
                        && entry.pointer("/value/missionId").and_then(Value::as_str)
                            == Some(id.as_str())
                }
            })
            .copied()
            .collect();
        children.extend(builder.place(&owned));
        children.extend(builder.jobs_where(|job| {
            job.get("missionId").and_then(Value::as_str) == Some(id.as_str())
                && !truthy(job.get("directionId"))
                && !truthy(job.get("autoresearchId"))
        }));
        roots.push(graph_root("mission", mission, "title", &nodes, children));
    }
    for direction in input.directions {
        if !builder
            .nested
            .borrow()
            .contains(&text(direction, "entityId"))
        {
            roots.push(builder.direction_node(direction));
        }
    }
    for execution in input.autoresearch {
        if !builder
            .nested
            .borrow()
            .contains(&text(execution, "entityId"))
        {
            roots.push(builder.autoresearch_node(execution));
        }
    }
    // Live work first, then the most recently updated finished work.
    let updated = |node: &Value| -> String {
        input
            .missions
            .iter()
            .chain(input.directions)
            .chain(input.autoresearch)
            .find(|entry| entry.get("entityId") == node.get("id"))
            .and_then(|entry| match entry.pointer("/value/updatedAt") {
                None | Some(Value::Null) => None,
                updated => Some(string(updated)),
            })
            .unwrap_or_default()
    };
    let terminal = |node: &Value| TERMINAL.contains(&text(node, "state").as_str());
    roots.sort_by(|left, right| {
        terminal(left)
            .cmp(&terminal(right))
            .then_with(|| updated(right).cmp(&updated(left)))
    });
    roots.truncate(MAX_ROOTS);
    let mut map = Map::new();
    map.insert("generatedAt".into(), json!(now_utc()));
    map.insert("roots".into(), Value::Array(roots));
    map.insert(
        "workers".into(),
        Value::Array(input.agents.iter().map(worker_node).collect()),
    );
    Value::Object(map)
}

/// `a === b` where either side may be absent.
fn strict(a: Option<&Value>, b: Option<&Value>) -> bool {
    nosh_wire::js::strict_equal(a, b)
}
