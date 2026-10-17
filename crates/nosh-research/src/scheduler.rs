//! Node scheduling and focus alarms, as `Scheduler` and `FocusGovernor` in `packages/scheduler`.

use crate::graph::{GraphNode, VersionedDag, parse_ms};
use crate::{Error, Result, rejected};
use nosh_wire::js::{self, truthy};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};

/// What the scheduler may use right now (`SchedulingContext`; only the fields it reads).
pub struct SchedulingContext {
    pub paused: bool,
    pub workspace_conflicts: HashSet<String>,
    pub available_roles: HashSet<String>,
    pub available_agents: f64,
}

#[derive(Default)]
pub struct Scheduler {
    active_mission_by_project: HashMap<String, String>,
    allocation: HashMap<String, f64>,
}

impl Scheduler {
    pub fn activate_mission(&mut self, project_id: &str, mission_id: &str) -> Result<()> {
        if self
            .active_mission_by_project
            .get(project_id)
            .is_some_and(|active| active != mission_id)
        {
            return rejected("Version 1 allows one active Mission per Project");
        }
        self.active_mission_by_project
            .insert(project_id.to_owned(), mission_id.to_owned());
        Ok(())
    }

    pub fn release_mission(&mut self, project_id: &str, mission_id: &str) {
        if self
            .active_mission_by_project
            .get(project_id)
            .is_some_and(|active| active == mission_id)
        {
            self.active_mission_by_project.remove(project_id);
        }
    }

    /// Runnable nodes a worker could take now, best first.
    pub fn frontier(
        &self,
        graph: &VersionedDag,
        context: &SchedulingContext,
        now_ms: f64,
    ) -> Vec<GraphNode> {
        if context.paused || context.available_agents < 1.0 {
            return Vec::new();
        }
        let mut nodes: Vec<GraphNode> = graph
            .runnable()
            .into_iter()
            .filter(|node| {
                !context.workspace_conflicts.contains(&node.id)
                    && (context.available_roles.contains(&node.node_type)
                        || context.available_roles.contains("general_worker"))
            })
            .collect();
        let score = |node: &GraphNode| {
            score(
                node,
                self.allocation.get(&node.id).copied().unwrap_or(0.0),
                now_ms,
            )
        };
        // `(left, right) => score(right) - score(left)`, sorted the way V8 does (NaN scores included).
        nosh_wire::js::sort_by_number(&mut nodes, |left, right| score(right) - score(left));
        nodes
    }

    pub fn record_allocation(&mut self, node_id: &str) {
        *self.allocation.entry(node_id.to_owned()).or_insert(0.0) += 1.0;
    }

    pub fn restore_allocations(&mut self, distribution: &Map<String, Value>) {
        for (node_id, count) in distribution {
            if let Some(count) = count.as_f64().filter(|c| c.fract() == 0.0 && *c >= 0.0) {
                self.allocation.insert(node_id.clone(), count);
            }
        }
    }

    pub fn distribution(&self) -> BTreeMap<String, f64> {
        self.allocation
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect()
    }
}

fn score(node: &GraphNode, allocations: f64, now_ms: f64) -> f64 {
    let age_hours = js::max(0.0, now_ms - parse_ms(&node.created_at)) / 3_600_000.0;
    node.critical_weight * 100.0 + node.priority * 10.0 + js::min(age_hours, 72.0)
        - allocations * 30.0
}

struct Attempt {
    fingerprint: String,
    progress: bool,
    reason: Option<String>,
}

/// Notices a node going in circles: repeated identical attempts, or attempts without progress.
#[derive(Default)]
pub struct FocusGovernor {
    attempts: HashMap<String, Vec<Attempt>>,
}

impl FocusGovernor {
    /// `sha256` of the attempt with its tag, file, and artifact lists sorted.
    pub fn fingerprint(&self, input: &Value) -> Result<String> {
        let mut normalized = input
            .as_object()
            .cloned()
            .ok_or_else(|| Error::Rejected("Attempt input must be an object".into()))?;
        for key in [
            "hypothesisFamilyTags",
            "inScopeFiles",
            "inputArtifactHashes",
        ] {
            let mut values = normalized
                .get(key)
                .and_then(Value::as_array)
                .cloned()
                .ok_or_else(|| Error::Rejected(format!("input.{key} is not iterable")))?;
            // Array.prototype.sort compares String(value) by UTF-16 code units.
            values.sort_by(|a, b| {
                nosh_wire::js::string(Some(a))
                    .encode_utf16()
                    .cmp(nosh_wire::js::string(Some(b)).encode_utf16())
            });
            normalized.insert(key.into(), Value::Array(values));
        }
        Ok(nosh_wire::sha256(&Value::Object(normalized)))
    }

    /// Records an attempt and its progress, returning `{ fingerprint, progress, alarm }`.
    pub fn record(
        &mut self,
        input: &Value,
        delta: &Value,
        override_reason: Option<&str>,
    ) -> Result<Value> {
        let fingerprint = self.fingerprint(input)?;
        let progress = delta
            .as_object()
            .is_some_and(|d| d.values().any(|v| truthy(Some(v))));
        let node_id = input
            .get("nodeId")
            .map(|v| nosh_wire::js::string(Some(v)))
            .unwrap_or_else(|| "undefined".into());
        let history = self.attempts.entry(node_id.clone()).or_default();
        history.push(Attempt {
            fingerprint: fingerprint.clone(),
            progress,
            reason: override_reason.map(str::to_owned),
        });
        let recent = &history[history.len().saturating_sub(4)..];
        let identical = recent
            .iter()
            .filter(|a| a.fingerprint == fingerprint && a.reason.is_none())
            .count();
        let no_progress = recent.iter().filter(|a| !a.progress).count();
        let mut reasons = Vec::new();
        if identical >= 2 {
            reasons.push("repeated_attempt_fingerprint");
        }
        if no_progress >= 4 {
            reasons.push("four_actions_without_durable_progress");
        }
        if history.len() >= 3 && history[history.len() - 3..].iter().all(|a| !a.progress) {
            reasons.push("three_failed_or_nonprogressing_attempts");
        }
        if reasons.is_empty() {
            return Ok(json!({ "fingerprint": fingerprint, "progress": progress, "alarm": null }));
        }
        let level = if identical >= 2 {
            "differentiate"
        } else if no_progress >= 4 {
            "review"
        } else {
            "reanchor"
        };
        let alarm = json!({ "nodeId": node_id, "level": level, "reasons": reasons, "inputs": { "identical": identical, "noProgress": no_progress, "window": recent.len(), "fingerprint": fingerprint } });
        Ok(json!({ "fingerprint": fingerprint, "progress": progress, "alarm": alarm }))
    }

    /// Clears a node's alarm history, given proof that something material changed.
    pub fn clear(&mut self, node_id: &str, proof: &Value) -> Result<()> {
        let material = [
            "newFingerprint",
            "reviewId",
            "graphVersion",
            "redirectedNodeId",
        ]
        .iter()
        .any(|key| truthy(proof.get(*key)));
        if !material {
            return rejected("A Director cannot clear a focus alarm by reflection alone");
        }
        self.attempts.remove(node_id);
        Ok(())
    }
}
