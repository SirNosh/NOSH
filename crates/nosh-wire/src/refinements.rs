//! zod refinements and defaults that the generated JSON Schemas cannot express.
//!
//! `conformance/schemas/refinements.json` (written by `scripts/sync-schemas.mjs` from the live zod
//! schemas) says *where* each refinement and default applies; this module supplies *what* each
//! refinement checks, keyed by the SHA-256 of its TypeScript source. A changed or new refinement
//! therefore has an unknown hash, and `every_refinement_has_a_rust_rule` fails until it is ported.

use crate::registry::{Issue, escape_pointer};
use serde_json::Value;
use std::collections::BTreeMap;

/// A relative path inside the refined value and the zod issue message.
type Findings = Vec<(&'static [&'static str], String)>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Input {
    Object,
    String,
}

struct Rule {
    source_sha256: &'static str,
    input: Input,
    check: fn(&Value) -> Findings,
}

/// Each rule mirrors one TypeScript refinement; the comment names where it lives.
static RULES: &[Rule] = &[
    // submissions.ts `gitBranch` (.refine; zod's wrapper hides the check, see sync-schemas.mjs).
    Rule {
        source_sha256: "eb1c928035abb0b1fb7df9d13bbab32c6a48d97924a6ce217ae9d04511f23ac4",
        input: Input::String,
        check: git_branch,
    },
    Rule {
        source_sha256: "d3fb3eb16c521dcf9a2ab480be3456043ebdb56f851ab13a8682fd489de59adc",
        input: Input::Object,
        check: claim,
    },
    Rule {
        source_sha256: "03e1a8ae8b62b8d9399f527a4c6cb9677b0ae524b6bf879483cafbd77bb91b19",
        input: Input::Object,
        check: direction_closure,
    },
    Rule {
        source_sha256: "65d30f4c82ddfdc1ad5ae50a470111857058c2b296b3ebb7203a8d4c8a796ab6",
        input: Input::Object,
        check: episode_trace,
    },
    Rule {
        source_sha256: "583ae6d460285b94e12e802cf4cfadb88f771804b82b53d6cd46e83951b10456",
        input: Input::Object,
        check: event_sequence,
    },
    Rule {
        source_sha256: "380112c50795da3c938dabcb0e8837bb742c6e30034315667218eca8e06d85a8",
        input: Input::Object,
        check: evidence,
    },
    Rule {
        source_sha256: "2c520951f3efbb0786974fce2e0637ee348db0ed5ca2c601ebdd9d08e4800997",
        input: Input::Object,
        check: mission_completion,
    },
    Rule {
        source_sha256: "e67c1bb51ada975754dabc95a7c05092fd58ba784bcb9b155d4fa25548264831",
        input: Input::Object,
        check: mission_spawn,
    },
    Rule {
        source_sha256: "07a27a1a92c9563084ec0ced3a8f64c1b2f05b9d6918e36c03e2406ea065cc91",
        input: Input::Object,
        check: runtime_instruction,
    },
    Rule {
        source_sha256: "d5e32e8564054a2f9b9ab4764c3bee174da53c9d975e3d6612c1efa101072290",
        input: Input::Object,
        check: review_request,
    },
    Rule {
        source_sha256: "505a498e00f42d40ac4d308001084e2168873bcb02e674cb6e5c7fb33c9933af",
        input: Input::Object,
        check: review_verdict,
    },
    Rule {
        source_sha256: "c7f11582d3b5f4a980ca4f4403741563ab2bdd9466bdd577d420249986e8ca5d",
        input: Input::Object,
        check: skill_manifest,
    },
    Rule {
        source_sha256: "4a5816c1d7a464e7fb777e6557dfe64125e964922d1c720a41045f6ffc7f454d",
        input: Input::Object,
        check: task_failure,
    },
];

struct When {
    depth: usize,
    key: String,
    equals: Value,
}

struct Located<T> {
    path: Vec<String>,
    when: Vec<When>,
    item: T,
}

pub(crate) struct Extras {
    pub(crate) formats: BTreeMap<String, String>,
    refinements: BTreeMap<String, Vec<Located<&'static Rule>>>,
    defaults: BTreeMap<String, Vec<Located<Value>>>,
}

impl Extras {
    pub(crate) fn parse(text: &str) -> Self {
        let document: Value = serde_json::from_str(text).expect("refinements.json");
        let strings = |value: &Value| -> Vec<String> {
            value
                .as_array()
                .expect("path array")
                .iter()
                .map(|s| s.as_str().expect("path segment").to_owned())
                .collect()
        };
        let located = |entry: &Value| -> (String, Vec<String>, Vec<When>) {
            let when = entry["when"]
                .as_array()
                .expect("when array")
                .iter()
                .map(|w| When {
                    depth: strings(&w["path"]).len(),
                    key: w["key"].as_str().expect("when.key").to_owned(),
                    equals: w["equals"].clone(),
                })
                .collect();
            (
                entry["schema"].as_str().expect("schema").to_owned(),
                strings(&entry["path"]),
                when,
            )
        };
        let mut refinements: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for entry in document["refinements"].as_array().expect("refinements") {
            let (schema, path, when) = located(entry);
            let hash = entry["rule"].as_str().expect("rule");
            let rule = RULES
                .iter()
                .find(|rule| rule.source_sha256 == hash)
                .unwrap_or_else(|| {
                    panic!(
                        "no Rust rule for the refinement at {schema} /{}",
                        path.join("/")
                    )
                });
            refinements.entry(schema).or_default().push(Located {
                path,
                when,
                item: rule,
            });
        }
        let mut defaults: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for entry in document["defaults"].as_array().expect("defaults") {
            let (schema, path, when) = located(entry);
            defaults.entry(schema).or_default().push(Located {
                path,
                when,
                item: entry["value"].clone(),
            });
        }
        let formats = document["formats"]
            .as_object()
            .expect("formats")
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().expect("format regex").to_owned()))
            .collect();
        Extras {
            formats,
            refinements,
            defaults,
        }
    }

    /// Inserts zod defaults for absent keys.
    pub(crate) fn apply_defaults(&self, schema: &str, value: &mut Value) {
        for default in self.defaults.get(schema).into_iter().flatten() {
            let (key, parent) = default.path.split_last().expect("a default names a key");
            let targets: Vec<Vec<String>> = select(value, parent)
                .into_iter()
                .filter(|(at, _)| holds(value, at, &default.when))
                .map(|(at, _)| at)
                .collect();
            for at in targets {
                if let Some(Value::Object(map)) = value.pointer_mut(&pointer(&at, &[])) {
                    map.entry(key.clone())
                        .or_insert_with(|| default.item.clone());
                }
            }
        }
    }

    /// Runs the refinements for `schema` over a schema-valid value.
    pub(crate) fn check(&self, schema: &str, value: &Value) -> Vec<Issue> {
        let mut issues = Vec::new();
        for refinement in self.refinements.get(schema).into_iter().flatten() {
            for (at, node) in select(value, &refinement.path) {
                // Absent or null: zod never runs a refinement inside an optional or nullable it skipped.
                let applies = match refinement.item.input {
                    Input::Object => node.is_object(),
                    Input::String => node.is_string(),
                };
                if !applies || !holds(value, &at, &refinement.when) {
                    continue;
                }
                for (relative, message) in (refinement.item.check)(node) {
                    issues.push(Issue {
                        pointer: pointer(&at, relative),
                        code: "custom".into(),
                        message,
                    });
                }
            }
        }
        issues
    }
}

/// Every concrete location matching `pattern`, where `*` is any array index or object key.
fn select<'v>(value: &'v Value, pattern: &[String]) -> Vec<(Vec<String>, &'v Value)> {
    fn walk<'v>(
        value: &'v Value,
        pattern: &[String],
        at: &mut Vec<String>,
        out: &mut Vec<(Vec<String>, &'v Value)>,
    ) {
        let Some((head, rest)) = pattern.split_first() else {
            out.push((at.clone(), value));
            return;
        };
        let children: Vec<(String, &Value)> = match (head.as_str(), value) {
            ("*", Value::Array(items)) => items
                .iter()
                .enumerate()
                .map(|(i, item)| (i.to_string(), item))
                .collect(),
            ("*", Value::Object(map)) => map.iter().map(|(k, item)| (k.clone(), item)).collect(),
            (key, Value::Object(map)) => map
                .get(key)
                .map(|item| (key.to_owned(), item))
                .into_iter()
                .collect(),
            _ => Vec::new(),
        };
        for (segment, child) in children {
            at.push(segment);
            walk(child, rest, at, out);
            at.pop();
        }
    }
    let mut out = Vec::new();
    walk(value, pattern, &mut Vec::new(), &mut out);
    out
}

/// Whether each discriminated-union condition holds; a condition's path is a prefix of `at`.
fn holds(root: &Value, at: &[String], when: &[When]) -> bool {
    when.iter().all(|w| {
        root.pointer(&pointer(&at[..w.depth], &[]))
            .and_then(|union| union.get(&w.key))
            == Some(&w.equals)
    })
}

fn pointer(at: &[String], relative: &[&str]) -> String {
    at.iter()
        .map(String::as_str)
        .chain(relative.iter().copied())
        .map(|segment| format!("/{}", escape_pointer(segment)))
        .collect()
}

// JavaScript semantics the TypeScript refinements rely on.

/// `value === null` (an absent key is `undefined`, which is not `null`).
fn is_null(value: Option<&Value>) -> bool {
    matches!(value, Some(Value::Null))
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

/// `.length` of an array field (zod has already checked the type).
fn len(value: &Value, key: &str) -> usize {
    value.get(key).and_then(Value::as_array).map_or(0, Vec::len)
}

fn text<'v>(value: &'v Value, key: &str) -> Option<&'v str> {
    value.get(key).and_then(Value::as_str)
}

fn number(value: &Value, key: &str) -> Option<f64> {
    value.get(key).and_then(Value::as_f64)
}

fn git_branch(value: &Value) -> Findings {
    let branch = value.as_str().unwrap_or_default();
    let safe = !branch.contains("..")
        && !branch.contains("//")
        && !branch.ends_with('/')
        && !branch.ends_with('.')
        && !branch
            .split('/')
            .any(|part| part == "." || part.starts_with('.') || part.ends_with(".lock"));
    if safe {
        vec![]
    } else {
        vec![(&[], "must be a safe Git branch name".into())]
    }
}

fn claim(v: &Value) -> Findings {
    let empty = [
        "supportingEvidenceIds",
        "contradictingEvidenceIds",
        "qualifyingEvidenceIds",
        "limitations",
    ]
    .iter()
    .all(|key| len(v, key) == 0);
    if empty {
        vec![(
            &["limitations"],
            "Claim must have evidence or an explicit limitation".into(),
        )]
    } else {
        vec![]
    }
}

fn direction_closure(v: &Value) -> Findings {
    if len(v, "openDefectIds") > 0 {
        vec![(
            &["openDefectIds"],
            "Direction closure cannot contain open defects".into(),
        )]
    } else {
        vec![]
    }
}

fn episode_trace(v: &Value) -> Findings {
    let trace = &v["trace"];
    match (
        number(trace, "lastSequence"),
        number(trace, "firstSequence"),
    ) {
        (Some(last), Some(first)) if last < first => vec![(
            &["trace", "lastSequence"],
            "lastSequence must not precede firstSequence".into(),
        )],
        _ => vec![],
    }
}

fn event_sequence(v: &Value) -> Findings {
    let sequence = v.get("sequence");
    match text(v, "retention") {
        Some("persistent") if is_null(sequence) => {
            vec![(&["sequence"], "persistent events require a sequence".into())]
        }
        Some("ephemeral") if !is_null(sequence) => vec![(
            &["sequence"],
            "ephemeral events cannot have a sequence".into(),
        )],
        _ => vec![],
    }
}

fn evidence(v: &Value) -> Findings {
    let mut out: Findings = Vec::new();
    if (text(v, "createdBy") == Some("agent")) != !is_null(v.get("createdByAgentId")) {
        out.push((
            &["createdByAgentId"],
            "createdByAgentId must be present exactly for agent-created evidence".into(),
        ));
    }
    let quality = &v["quality"];
    let unreviewed = text(quality, "status") == Some("unreviewed");
    let review_named = !is_null(quality.get("reviewId"));
    if unreviewed && review_named {
        out.push((
            &["quality", "reviewId"],
            "unreviewed Evidence must not name a Review".into(),
        ));
    }
    if !unreviewed && !review_named {
        out.push((
            &["quality", "reviewId"],
            "reviewed or rejected Evidence must name a Review".into(),
        ));
    }
    out
}

fn mission_completion(v: &Value) -> Findings {
    if len(&v["claimAudit"], "unresolvedClaimIds") > 0 || len(v, "openDefectIds") > 0 {
        vec![(
            &["openDefectIds"],
            "Mission completion cannot contain unresolved claims or defects".into(),
        )]
    } else {
        vec![]
    }
}

fn mission_spawn(v: &Value) -> Findings {
    if text(v, "kind") == Some("research_direction") && !truthy(v.get("decisionUse")) {
        vec![(
            &["decisionUse"],
            "A research_direction spawn needs decisionUse: what the answer will decide".into(),
        )]
    } else {
        vec![]
    }
}

fn runtime_instruction(v: &Value) -> Findings {
    let mut out: Findings = Vec::new();
    match text(v, "operation") {
        Some("THREAD_AWAIT") => {
            // `value.threadIds ?? []`
            let thread_ids: &[Value] = v
                .get("threadIds")
                .and_then(Value::as_array)
                .map_or(&[], Vec::as_slice);
            let unique = thread_ids
                .iter()
                .enumerate()
                .all(|(i, id)| !thread_ids[..i].contains(id));
            if !unique {
                out.push((&["threadIds"], "threadIds must be unique".into()));
            }
            let policy = text(v, "policy");
            let minimum = number(v, "minimumCount");
            if policy == Some("minimum_count")
                && minimum.is_none_or(|m| m > thread_ids.len() as f64)
            {
                out.push((
                    &["minimumCount"],
                    "minimum_count requires a value no greater than threadIds.length".into(),
                ));
            }
            if policy == Some("deadline") && is_null(v.get("deadline")) {
                out.push((&["deadline"], "deadline policy requires deadline".into()));
            }
        }
        Some("STOP") if is_null(v.get("threadId")) && is_null(v.get("programId")) => {
            out.push((&["threadId"], "STOP requires threadId or programId".into()));
        }
        _ => {}
    }
    out
}

fn review_request(v: &Value) -> Findings {
    let producers = v.get("producerAgentIds").and_then(Value::as_array);
    let reviewer = v.get("reviewerAgentId");
    if producers.is_some_and(|ids| reviewer.is_some_and(|r| ids.contains(r))) {
        vec![(
            &["reviewerAgentId"],
            "reviewer must be independent from every producer".into(),
        )]
    } else {
        vec![]
    }
}

fn review_verdict(v: &Value) -> Findings {
    let mut out: Findings = Vec::new();
    let verdict = text(v, "verdict").unwrap_or_default();
    let criteria: &[Value] = v
        .get("criteria")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice);
    let defects: &[Value] = v
        .get("defects")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice);
    let status = |criterion: &Value| text(criterion, "status").unwrap_or_default().to_owned();
    if matches!(verdict, "REVISE" | "REDESIGN")
        && defects.is_empty()
        && criteria
            .iter()
            .all(|c| matches!(status(c).as_str(), "PASS" | "NOT_APPLICABLE"))
    {
        out.push((&["verdict"], format!("{verdict} needs a failing or inconclusive criterion or a recorded defect; when every criterion passes and no defect is open, the verdict is PASS")));
    }
    if verdict != "PASS" {
        return out;
    }
    if text(v, "independenceCheck") != Some("pass") {
        out.push((&["verdict"], "PASS requires an independent reviewer".into()));
    }
    if len(v, "missingRequiredInputs") > 0 {
        out.push((
            &["missingRequiredInputs"],
            "PASS cannot omit required inputs".into(),
        ));
    }
    // `criterion.required !== false`: an omitted flag counts as required.
    if criteria.iter().any(|c| {
        c.get("required") != Some(&Value::Bool(false))
            && matches!(status(c).as_str(), "FAIL" | "INCONCLUSIVE")
    }) {
        out.push((
            &["criteria"],
            "PASS requires every required criterion to pass".into(),
        ));
    }
    if defects.iter().any(|d| {
        truthy(d.get("blocking")) || matches!(text(d, "severity"), Some("blocking" | "major"))
    }) {
        out.push((
            &["defects"],
            "PASS cannot contain an open blocking or major defect".into(),
        ));
    }
    out
}

fn skill_manifest(v: &Value) -> Findings {
    if (text(v, "executionMode") == Some("program")) != !is_null(v.get("programId")) {
        vec![(
            &["programId"],
            "program mode requires programId and prompt mode forbids it".into(),
        )]
    } else {
        vec![]
    }
}

fn task_failure(v: &Value) -> Findings {
    if !truthy(v.get("sameApproachAllowed")) && !truthy(v.get("changedPremiseRequired")) {
        vec![(
            &["changedPremiseRequired"],
            "a changed premise is required when the same approach is forbidden".into(),
        )]
    } else {
        vec![]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn extras() -> Extras {
        Extras::parse(include_str!(
            "../../../conformance/schemas/refinements.json"
        ))
    }

    #[test]
    fn every_refinement_has_a_rust_rule_and_every_rule_is_used() {
        let extras = extras(); // panics on a refinement without a rule
        for rule in RULES {
            let used = extras
                .refinements
                .values()
                .flatten()
                .any(|r| std::ptr::eq(r.item, rule));
            assert!(
                used,
                "rule {} matches no TypeScript refinement; it changed or was removed",
                rule.source_sha256
            );
        }
    }

    // Rules that no schema-valid record can trigger still need direct coverage.
    #[test]
    fn mission_spawn_requires_decision_use_for_directions() {
        let direction = json!({ "kind": "research_direction", "decisionUse": "" });
        assert_eq!(mission_spawn(&direction).len(), 1);
        assert!(
            mission_spawn(&json!({ "kind": "research_direction", "decisionUse": "choose" }))
                .is_empty()
        );
        assert!(mission_spawn(&json!({ "kind": "mission" })).is_empty());
    }

    #[test]
    fn select_expands_wildcards_and_skips_absent_keys() {
        let value =
            json!({ "steps": [{ "x": 1 }, { "y": 2 }, { "x": 3 }], "map": { "a": { "x": 4 } } });
        let at = |pattern: &[&str]| -> Vec<String> {
            let pattern: Vec<String> = pattern.iter().map(|s| (*s).to_owned()).collect();
            select(&value, &pattern)
                .into_iter()
                .map(|(at, _)| pointer(&at, &[]))
                .collect()
        };
        assert_eq!(at(&["steps", "*", "x"]), ["/steps/0/x", "/steps/2/x"]);
        assert_eq!(at(&["map", "*", "x"]), ["/map/a/x"]);
        assert_eq!(at(&[]), [""]);
        assert!(at(&["missing", "*"]).is_empty());
    }

    #[test]
    fn javascript_truthiness() {
        for falsy in [
            None,
            Some(json!(null)),
            Some(json!(false)),
            Some(json!(0)),
            Some(json!(0.0)),
            Some(json!("")),
        ] {
            assert!(!truthy(falsy.as_ref()), "{falsy:?}");
        }
        for truthy_value in [
            json!(true),
            json!(1),
            json!(-0.5),
            json!("x"),
            json!([]),
            json!({}),
        ] {
            assert!(truthy(Some(&truthy_value)), "{truthy_value}");
        }
        assert!(is_null(Some(&json!(null))) && !is_null(None));
    }
}
