//! Plan graphs and research state machines, as in `packages/graph` (`index.ts`, `research.ts`).

use crate::{Error, Result, rejected};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};

pub const MISSION_TRANSITIONS: &[(&str, &[&str])] = &[
    ("draft", &["planning"]),
    ("planning", &["awaiting_approval"]),
    ("awaiting_approval", &["running"]),
    (
        "running",
        &["pausing", "reviewing", "blocked", "stopping", "failed"],
    ),
    ("pausing", &["paused"]),
    ("paused", &["running", "stopping"]),
    ("reviewing", &["running", "completed"]),
    ("blocked", &["running", "stopping"]),
    ("stopping", &["stopped"]),
    ("completed", &[]),
    ("stopped", &[]),
    ("failed", &[]),
];

pub const DIRECTION_TRANSITIONS: &[(&str, &[&str])] = &[
    ("draft", &["proposed", "stopped"]),
    ("proposed", &["active", "stopped"]),
    ("active", &["paused", "blocked", "reviewing", "stopped"]),
    ("paused", &["active", "stopped"]),
    ("blocked", &["active", "stopped"]),
    ("reviewing", &["active", "closed", "rejected"]),
    ("closed", &[]),
    ("rejected", &[]),
    ("stopped", &[]),
];

const NODE_TRANSITIONS: &[(&str, &[&str])] = &[
    ("pending", &["ready", "cancelled", "superseded"]),
    ("ready", &["leased", "blocked", "cancelled", "superseded"]),
    ("leased", &["working", "ready", "blocked", "cancelled"]),
    (
        "working",
        &["postflight", "ready", "blocked", "failed", "cancelled"],
    ),
    ("postflight", &["reviewing", "working", "failed"]),
    ("reviewing", &["accepted", "working", "blocked", "failed"]),
    ("accepted", &[]),
    ("blocked", &["ready", "cancelled", "superseded"]),
    ("failed", &["ready", "superseded", "cancelled"]),
    ("waived", &[]),
    ("superseded", &[]),
    ("cancelled", &[]),
];

fn legal(table: &[(&str, &[&str])], current: &str, next: &str) -> bool {
    table
        .iter()
        .find(|(state, _)| *state == current)
        .is_some_and(|(_, targets)| targets.contains(&next))
}

pub fn is_legal_mission_transition(current: &str, next: &str) -> bool {
    legal(MISSION_TRANSITIONS, current, next)
}

pub fn is_legal_direction_transition(current: &str, next: &str) -> bool {
    legal(DIRECTION_TRANSITIONS, current, next)
}

pub fn is_legal_node_transition(current: &str, next: &str) -> bool {
    legal(NODE_TRANSITIONS, current, next)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Lease {
    pub lease_id: String,
    pub owner_id: String,
    pub version: f64,
    pub expires_at: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphNode {
    pub id: String,
    #[serde(rename = "type")]
    pub node_type: String,
    pub title: String,
    pub required: bool,
    pub criterion_ids: Vec<String>,
    pub hard_dependencies: Vec<String>,
    pub soft_dependencies: Vec<String>,
    pub state: String,
    pub attempt: f64,
    pub maximum_attempts: f64,
    pub priority: f64,
    pub critical_weight: f64,
    pub created_at: String,
    pub lease: Option<Lease>,
    /// Fields a newer NOSH wrote; TypeScript's `clone()` spreads and keeps them.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphVersion {
    pub version: i64,
    pub nodes: Vec<GraphNode>,
    pub rationale: String,
    pub evidence_ids: Vec<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GraphDiff {
    pub added: Vec<String>,
    pub superseded: Vec<String>,
    pub rewired: Vec<String>,
}

/// A Mission's or Direction's plan graph with its version history.
pub struct VersionedDag {
    pub scope_id: String,
    history: Vec<GraphVersion>,
}

impl VersionedDag {
    pub fn new(
        scope_id: &str,
        nodes: Vec<GraphNode>,
        rationale: &str,
        initial_version: i64,
    ) -> Result<Self> {
        validate(&nodes)?;
        if initial_version < 1 {
            return rejected("Initial graph version must be positive");
        }
        let mut dag = VersionedDag {
            scope_id: scope_id.to_owned(),
            history: vec![GraphVersion {
                version: initial_version,
                nodes,
                rationale: rationale.to_owned(),
                evidence_ids: Vec::new(),
                created_at: nosh_wire::now_utc(),
            }],
        };
        dag.refresh_ready()?;
        Ok(dag)
    }

    pub fn current(&self) -> GraphVersion {
        self.latest().clone()
    }

    pub fn versions(&self) -> Vec<GraphVersion> {
        self.history.clone()
    }

    /// Applies `operations` (`add_node`, `add_hard_edge`, `remove_hard_edge`, otherwise a
    /// supersede) on top of `base_version`, producing the next version.
    pub fn apply(
        &mut self,
        base_version: i64,
        operations: &[Value],
        rationale: &str,
        evidence_ids: &[String],
    ) -> Result<GraphVersion> {
        let current = self.current();
        if base_version != current.version {
            return rejected(format!(
                "Stale graph version {base_version}; current version is {}",
                current.version
            ));
        }
        let mut nodes = current.nodes;
        for operation in operations {
            let text = |key: &str| {
                operation
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            match operation.get("type").and_then(Value::as_str) {
                Some("add_node") => {
                    let node: GraphNode = serde_json::from_value(
                        operation.get("node").cloned().unwrap_or(Value::Null),
                    )?;
                    if nodes.iter().any(|existing| existing.id == node.id) {
                        return rejected(format!("Node {} already exists", node.id));
                    }
                    nodes.push(node);
                }
                Some(kind @ ("add_hard_edge" | "remove_hard_edge")) => {
                    let (from, to) = (text("from"), text("to"));
                    let source = required(&nodes, &from)?.clone();
                    let target = required(&nodes, &to)?;
                    assert_rewirable_dependency(&source, target)?;
                    let target = nodes
                        .iter_mut()
                        .find(|node| node.id == to)
                        .expect("checked above");
                    if kind == "add_hard_edge" {
                        if !target.hard_dependencies.contains(&from) {
                            target.hard_dependencies.push(from);
                        }
                    } else {
                        target.hard_dependencies.retain(|id| *id != from);
                    }
                }
                _ => supersede(
                    &mut nodes,
                    &text("nodeId"),
                    operation
                        .get("replacementId")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty()),
                )?,
            }
        }
        validate(&nodes)?;
        self.history.push(GraphVersion {
            version: current.version + 1,
            nodes,
            rationale: rationale.to_owned(),
            evidence_ids: evidence_ids.to_vec(),
            created_at: nosh_wire::now_utc(),
        });
        self.refresh_ready()?;
        Ok(self.current())
    }

    pub fn transition(&mut self, node_id: &str, next: &str) -> Result<GraphNode> {
        let nodes = &self.latest().nodes;
        let node = required(nodes, node_id)?;
        if !is_legal_node_transition(&node.state, next) {
            return rejected(format!("Illegal node transition {} -> {next}", node.state));
        }
        if next == "leased" {
            for dependency in &node.hard_dependencies {
                if !matches!(
                    required(nodes, dependency)?.state.as_str(),
                    "accepted" | "waived"
                ) {
                    return rejected("Hard dependencies are not satisfied");
                }
            }
        }
        if next == "ready" && node.attempt >= node.maximum_attempts {
            return rejected("Node attempt budget is exhausted");
        }
        let node = self
            .latest_mut()
            .nodes
            .iter_mut()
            .find(|node| node.id == node_id)
            .expect("checked above");
        if next == "working" {
            node.attempt += 1.0;
        }
        if matches!(
            next,
            "ready" | "accepted" | "blocked" | "failed" | "cancelled"
        ) {
            node.lease = None;
        }
        node.state = next.to_owned();
        let changed = node.clone();
        self.refresh_ready()?;
        Ok(changed)
    }

    pub fn lease(&mut self, node_id: &str, lease: Lease) -> Result<GraphNode> {
        let node = self
            .latest_mut()
            .nodes
            .iter_mut()
            .find(|node| node.id == node_id)
            .ok_or_else(|| Error::Rejected(format!("Unknown node {node_id}")))?;
        if node.state != "ready" || node.lease.is_some() {
            return rejected("Only an unleased ready node can be leased");
        }
        node.lease = Some(lease);
        node.state = "leased".into();
        Ok(node.clone())
    }

    /// Returns leases that expired by `now_ms` to `ready`; the released node IDs.
    pub fn release_expired(&mut self, now_ms: f64) -> Vec<String> {
        let mut released = Vec::new();
        for node in &mut self.latest_mut().nodes {
            if node
                .lease
                .as_ref()
                .is_some_and(|lease| parse_ms(&lease.expires_at) <= now_ms)
            {
                node.lease = None;
                node.state = "ready".into();
                released.push(node.id.clone());
            }
        }
        released
    }

    pub fn runnable(&self) -> Vec<GraphNode> {
        self.latest()
            .nodes
            .iter()
            .filter(|node| node.state == "ready" && node.lease.is_none())
            .cloned()
            .collect()
    }

    pub fn completion_ready(&self) -> bool {
        self.latest().nodes.iter().all(|node| {
            !node.required || matches!(node.state.as_str(), "accepted" | "waived" | "superseded")
        })
    }

    /// The heaviest chain of hard dependencies by `criticalWeight`.
    pub fn critical_path(&self) -> Result<Vec<String>> {
        let nodes = &self.latest().nodes;
        let mut scores: Vec<(String, f64, Vec<String>)> = Vec::new();
        for id in topological(nodes)? {
            let node = required(nodes, &id)?;
            // The first heaviest dependency, as a stable descending sort's first element.
            let mut parent: Option<&(String, f64, Vec<String>)> = None;
            for dependency in &node.hard_dependencies {
                let candidate = scores.iter().find(|(scored, _, _)| scored == dependency);
                if candidate.is_some_and(|c| parent.is_none_or(|p| c.1 > p.1)) {
                    parent = candidate;
                }
            }
            let score = parent.map_or(0.0, |p| p.1) + node.critical_weight;
            let mut path = parent.map_or_else(Vec::new, |p| p.2.clone());
            path.push(id.clone());
            scores.push((id, score, path));
        }
        let mut best: Option<&(String, f64, Vec<String>)> = None;
        for entry in &scores {
            if best.is_none_or(|b| entry.1 > b.1) {
                best = Some(entry);
            }
        }
        Ok(best.map(|b| b.2.clone()).unwrap_or_default())
    }

    pub fn diff(&self, from_version: i64, to_version: i64) -> Result<GraphDiff> {
        let find = |version: i64| {
            self.history
                .iter()
                .find(|candidate| candidate.version == version)
        };
        let (Some(from), Some(to)) = (find(from_version), find(to_version)) else {
            return rejected("Unknown graph version");
        };
        let prior = |id: &str| from.nodes.iter().find(|node| node.id == id);
        let sorted = |ids: &[String]| {
            let mut ids = ids.to_vec();
            ids.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            ids
        };
        let added = to
            .nodes
            .iter()
            .filter(|node| prior(&node.id).is_none())
            .map(|node| node.id.clone())
            .collect();
        let superseded = to
            .nodes
            .iter()
            .filter(|node| {
                node.state == "superseded"
                    && prior(&node.id).is_none_or(|p| p.state != "superseded")
            })
            .map(|node| node.id.clone())
            .collect();
        let rewired = to
            .nodes
            .iter()
            .filter(|node| {
                prior(&node.id).is_some_and(|p| {
                    sorted(&p.hard_dependencies) != sorted(&node.hard_dependencies)
                        || sorted(&p.soft_dependencies) != sorted(&node.soft_dependencies)
                })
            })
            .map(|node| node.id.clone())
            .collect();
        Ok(GraphDiff {
            added,
            superseded,
            rewired,
        })
    }

    fn latest(&self) -> &GraphVersion {
        self.history.last().expect("a graph always has a version")
    }

    fn latest_mut(&mut self) -> &mut GraphVersion {
        self.history
            .last_mut()
            .expect("a graph always has a version")
    }

    fn refresh_ready(&mut self) -> Result<()> {
        let states: HashMap<String, String> = self
            .latest()
            .nodes
            .iter()
            .map(|node| (node.id.clone(), node.state.clone()))
            .collect();
        for node in &mut self.latest_mut().nodes {
            if node.state != "pending" {
                continue;
            }
            let mut satisfied = true;
            for dependency in &node.hard_dependencies {
                let state = states
                    .get(dependency)
                    .ok_or_else(|| Error::Rejected(format!("Unknown node {dependency}")))?;
                satisfied &= matches!(state.as_str(), "accepted" | "waived");
            }
            if satisfied {
                node.state = "ready".into();
            }
        }
        Ok(())
    }
}

fn supersede(nodes: &mut [GraphNode], node_id: &str, replacement_id: Option<&str>) -> Result<()> {
    let node = required(nodes, node_id)?.clone();
    if node.lease.is_some()
        || !matches!(
            node.state.as_str(),
            "pending" | "ready" | "blocked" | "failed"
        )
    {
        return rejected("Only an unleased nonterminal node may be superseded");
    }
    let connected = nodes.iter().any(|candidate| {
        candidate.id != node.id
            && candidate
                .hard_dependencies
                .iter()
                .chain(&candidate.soft_dependencies)
                .any(|id| *id == node.id)
    });
    let Some(replacement_id) = replacement_id else {
        if !node.criterion_ids.is_empty() || connected {
            return rejected(
                "Cannot supersede a connected or criterion-owning node without a replacement",
            );
        }
        nodes
            .iter_mut()
            .find(|n| n.id == node_id)
            .expect("checked above")
            .state = "superseded".into();
        return Ok(());
    };
    let replacement = required(nodes, replacement_id)?;
    if replacement.lease.is_some() || !matches!(replacement.state.as_str(), "pending" | "ready") {
        return rejected("Replacement node must be unleased and pre-execution");
    }
    if replacement
        .criterion_ids
        .iter()
        .any(|criterion| node.criterion_ids.contains(criterion))
    {
        return rejected("Replacement node already owns a superseded criterion");
    }
    let merge = |own: &[String], inherited: &[String]| {
        unique(
            own.iter()
                .chain(inherited)
                .filter(|id| **id != replacement_id && **id != node.id)
                .cloned(),
        )
    };
    let replacement = nodes
        .iter_mut()
        .find(|n| n.id == replacement_id)
        .expect("checked above");
    replacement
        .criterion_ids
        .extend(node.criterion_ids.iter().cloned());
    replacement.hard_dependencies = merge(&replacement.hard_dependencies, &node.hard_dependencies);
    replacement.soft_dependencies = merge(&replacement.soft_dependencies, &node.soft_dependencies);
    for candidate in nodes
        .iter_mut()
        .filter(|c| c.id != node.id && c.id != replacement_id)
    {
        let rename = |ids: &[String]| {
            unique(ids.iter().map(|id| {
                if *id == node.id {
                    replacement_id.to_owned()
                } else {
                    id.clone()
                }
            }))
        };
        candidate.hard_dependencies = rename(&candidate.hard_dependencies);
        candidate.soft_dependencies = rename(&candidate.soft_dependencies);
    }
    let superseded = nodes
        .iter_mut()
        .find(|n| n.id == node_id)
        .expect("checked above");
    superseded.criterion_ids.clear();
    superseded.state = "superseded".into();
    Ok(())
}

/// `[...new Set(values)]`: first occurrences, in order.
fn unique(values: impl Iterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    values.filter(|value| seen.insert(value.clone())).collect()
}

fn validate(nodes: &[GraphNode]) -> Result<()> {
    let mut ids = HashSet::new();
    for node in nodes {
        if !ids.insert(node.id.as_str()) {
            return rejected(format!("Duplicate node {}", node.id));
        }
    }
    for node in nodes {
        let (mut hard, mut soft) = (HashSet::new(), HashSet::new());
        for dependency in &node.hard_dependencies {
            if !ids.contains(dependency.as_str()) {
                return rejected(format!("Unknown hard dependency {dependency}"));
            }
            if *dependency == node.id {
                return rejected(format!("Node {} cannot hard-depend on itself", node.id));
            }
            if !hard.insert(dependency) {
                return rejected(format!("Duplicate hard dependency {dependency}"));
            }
        }
        for dependency in &node.soft_dependencies {
            if !ids.contains(dependency.as_str()) {
                return rejected(format!("Unknown soft dependency {dependency}"));
            }
            if *dependency == node.id {
                return rejected(format!("Node {} cannot soft-depend on itself", node.id));
            }
            if hard.contains(dependency) || !soft.insert(dependency) {
                return rejected(format!("Ambiguous soft dependency {dependency}"));
            }
        }
    }
    topological(nodes).map(|_| ())
}

/// Node IDs in dependency order (depth-first over hard dependencies, in node order).
fn topological(nodes: &[GraphNode]) -> Result<Vec<String>> {
    fn visit(
        nodes: &[GraphNode],
        id: &str,
        visiting: &mut HashSet<String>,
        visited: &mut HashSet<String>,
        out: &mut Vec<String>,
    ) -> Result<()> {
        if visiting.contains(id) {
            return rejected("Scheduling graph contains a cycle");
        }
        if visited.contains(id) {
            return Ok(());
        }
        visiting.insert(id.to_owned());
        for dependency in &required(nodes, id)?.hard_dependencies {
            visit(nodes, dependency, visiting, visited, out)?;
        }
        visiting.remove(id);
        visited.insert(id.to_owned());
        out.push(id.to_owned());
        Ok(())
    }
    let (mut visiting, mut visited, mut out) = (HashSet::new(), HashSet::new(), Vec::new());
    for node in nodes {
        visit(nodes, &node.id, &mut visiting, &mut visited, &mut out)?;
    }
    Ok(out)
}

fn required<'a>(nodes: &'a [GraphNode], id: &str) -> Result<&'a GraphNode> {
    nodes
        .iter()
        .find(|node| node.id == id)
        .ok_or_else(|| Error::Rejected(format!("Unknown node {id}")))
}

fn assert_rewirable_dependency(source: &GraphNode, target: &GraphNode) -> Result<()> {
    if matches!(source.state.as_str(), "superseded" | "cancelled") {
        return rejected("Cannot use a superseded or cancelled dependency source");
    }
    if target.lease.is_some()
        || !matches!(
            target.state.as_str(),
            "pending" | "ready" | "blocked" | "failed"
        )
    {
        return rejected("Only an unleased pre-execution or resettable target may be rewired");
    }
    Ok(())
}

/// `Date.parse` of an ISO timestamp, in milliseconds (`NaN` when unparseable).
pub(crate) fn parse_ms(text: &str) -> f64 {
    time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
        .map_or(f64::NAN, |at| {
            (at.unix_timestamp_nanos() / 1_000_000) as f64
        })
}

// ---- Experiment trees (research.ts) ----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Experiment {
    pub experiment_id: String,
    pub parent_experiment_id: Option<String>,
    pub idea_fingerprint: String,
    pub attempt_fingerprint: String,
    pub evaluation_contract_hash: String,
    pub branch: String,
    pub evaluated_commit: Option<String>,
    pub depth: f64,
    pub state: String,
    pub review_id: Option<String>,
    pub reviewer_agent_id: Option<String>,
    pub guardrails_pass: Option<bool>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExperimentReview {
    pub review_id: String,
    pub reviewer_agent_id: String,
    pub verdict: String,
}

/// An Autoresearch experiment tree: one root, evaluated parents, bounded depth and fan-out, and a
/// fresh Reviewer for every evaluation.
pub struct ExperimentTree {
    pub evaluation_contract_hash: String,
    pub maximum_depth: f64,
    pub maximum_children_per_parent: f64,
    experiments: Vec<Experiment>,
    reviewers: HashSet<String>,
}

impl ExperimentTree {
    pub fn new(
        evaluation_contract_hash: &str,
        maximum_depth: f64,
        maximum_children_per_parent: f64,
    ) -> Self {
        ExperimentTree {
            evaluation_contract_hash: evaluation_contract_hash.to_owned(),
            maximum_depth,
            maximum_children_per_parent,
            experiments: Vec::new(),
            reviewers: HashSet::new(),
        }
    }

    pub fn add(&mut self, experiment: Experiment) -> Result<Experiment> {
        if experiment.evaluation_contract_hash != self.evaluation_contract_hash {
            return rejected("Evaluation contract is frozen");
        }
        if self
            .experiments
            .iter()
            .any(|e| e.experiment_id == experiment.experiment_id)
        {
            return rejected("Experiment already exists");
        }
        if self
            .experiments
            .iter()
            .any(|e| e.idea_fingerprint == experiment.idea_fingerprint)
        {
            return rejected("Duplicate experiment idea fingerprint");
        }
        match &experiment.parent_experiment_id {
            None => {
                if !self.experiments.is_empty() {
                    return rejected("Experiment tree has exactly one root");
                }
                if experiment.depth != 0.0 {
                    return rejected("Root depth must be zero");
                }
            }
            Some(parent_id) => {
                let parent = self.required(parent_id)?;
                if parent.evaluated_commit.as_deref().is_none_or(str::is_empty)
                    || !matches!(
                        parent.state.as_str(),
                        "accepted" | "rejected" | "inconclusive"
                    )
                {
                    return rejected("Parent must be evaluated before adding a child");
                }
                if experiment.depth != parent.depth + 1.0 || experiment.depth > self.maximum_depth {
                    return rejected("Experiment depth violates the tree contract");
                }
                let children = self
                    .experiments
                    .iter()
                    .filter(|child| child.parent_experiment_id.as_deref() == Some(parent_id))
                    .count();
                if children as f64 >= self.maximum_children_per_parent {
                    return rejected("Parent child limit exceeded");
                }
            }
        }
        self.experiments.push(experiment.clone());
        Ok(experiment)
    }

    pub fn evaluate(
        &mut self,
        experiment_id: &str,
        commit: &str,
        state: &str,
        review: Option<&ExperimentReview>,
        guardrails_pass: bool,
    ) -> Result<Experiment> {
        let experiment = self.required(experiment_id)?;
        if experiment
            .evaluated_commit
            .as_deref()
            .is_some_and(|c| !c.is_empty() && c != commit)
        {
            return rejected("Evaluated experiment commit is immutable");
        }
        if state == "accepted"
            && (review.map(|r| r.verdict.as_str()) != Some("PASS") || !guardrails_pass)
        {
            return rejected("Acceptance requires passing Review and guardrails");
        }
        if let Some(review) = review
            && self.reviewers.contains(&review.reviewer_agent_id)
        {
            return rejected("Each material round requires a fresh Reviewer session");
        }
        let experiment = self
            .experiments
            .iter_mut()
            .find(|e| e.experiment_id == experiment_id)
            .expect("checked above");
        experiment.evaluated_commit = Some(commit.to_owned());
        experiment.state = state.to_owned();
        experiment.review_id = review.map(|r| r.review_id.clone());
        experiment.reviewer_agent_id = review.map(|r| r.reviewer_agent_id.clone());
        experiment.guardrails_pass = Some(guardrails_pass);
        let evaluated = experiment.clone();
        if let Some(review) = review {
            self.reviewers.insert(review.reviewer_agent_id.clone());
        }
        Ok(evaluated)
    }

    pub fn get(&self, experiment_id: &str) -> Result<Experiment> {
        self.required(experiment_id).cloned()
    }

    pub fn all(&self) -> Vec<Experiment> {
        self.experiments.clone()
    }

    /// Accepted experiments without an accepted child.
    pub fn frontier(&self) -> Vec<Experiment> {
        self.experiments
            .iter()
            .filter(|e| {
                e.state == "accepted"
                    && !self.experiments.iter().any(|child| {
                        child.parent_experiment_id.as_deref() == Some(e.experiment_id.as_str())
                            && child.state == "accepted"
                    })
            })
            .cloned()
            .collect()
    }

    pub fn shape_warnings(&self) -> Vec<String> {
        let all = &self.experiments;
        let mut warnings = Vec::new();
        let children_of = |id: &str| {
            all.iter()
                .filter(|e| e.parent_experiment_id.as_deref() == Some(id))
                .count()
        };
        if let Some(root) = all.iter().find(|e| e.parent_experiment_id.is_none())
            && children_of(&root.experiment_id) as f64 > self.maximum_children_per_parent
        {
            warnings.push("flat_fan".into());
        }
        let longest = all.iter().map(|e| e.depth).fold(0.0, nosh_wire::js::max);
        let branching = all
            .iter()
            .filter(|e| {
                e.parent_experiment_id
                    .as_deref()
                    .is_some_and(|parent| !parent.is_empty() && children_of(parent) > 1)
            })
            .count();
        if longest >= 4.0 && branching == 0 {
            warnings.push("unsupported_noodle".into());
        }
        warnings
    }

    fn required(&self, id: &str) -> Result<&Experiment> {
        self.experiments
            .iter()
            .find(|e| e.experiment_id == id)
            .ok_or_else(|| Error::Rejected(format!("Unknown experiment {id}")))
    }
}
