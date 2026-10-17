//! Skills and orchestration programs, as in `packages/skills`.

use crate::{Error, Result, rejected};
use nosh_wire::validate_record;
use serde_json::{Map, Value};
use std::collections::HashSet;

const SKILL_MANIFEST: &str = "https://nosh.dev/schemas/skill-manifest/v1";
const ORCHESTRATION_PROGRAM: &str = "https://nosh.dev/schemas/orchestration-program/v1";

/// Names every plain object has through `Object.prototype`, so `key in object` is true for them.
const OBJECT_PROTOTYPE: [&str; 12] = [
    "constructor",
    "hasOwnProperty",
    "isPrototypeOf",
    "propertyIsEnumerable",
    "toLocaleString",
    "toString",
    "valueOf",
    "__proto__",
    "__defineGetter__",
    "__defineSetter__",
    "__lookupGetter__",
    "__lookupSetter__",
];

/// JavaScript's `key in object` for a plain JSON object.
fn js_in(object: &Map<String, Value>, key: &str) -> bool {
    object.contains_key(key) || OBJECT_PROTOTYPE.contains(&key)
}

/// Validated skill manifests by `skillId`, in registration order.
#[derive(Default)]
pub struct SkillRegistry {
    skills: Vec<(String, Value)>,
}

impl SkillRegistry {
    pub fn add(&mut self, input: &Value) -> Result<Value> {
        let skill = validate_record(SKILL_MANIFEST, input)?;
        let id = skill["skillId"].as_str().unwrap_or_default().to_owned();
        match self.skills.iter_mut().find(|(existing, _)| *existing == id) {
            Some(entry) => entry.1 = skill.clone(),
            None => self.skills.push((id, skill.clone())),
        }
        Ok(skill)
    }

    pub fn get(&self, skill_id: &str) -> Result<Value> {
        self.skills
            .iter()
            .find(|(id, _)| id == skill_id)
            .map(|(_, skill)| skill.clone())
            .ok_or_else(|| Error::Rejected(format!("Unknown skill {skill_id}")))
    }

    pub fn list(&self) -> Vec<Value> {
        self.skills.iter().map(|(_, skill)| skill.clone()).collect()
    }

    /// The skill, if `thread`'s role may use it and the thread holds every required capability.
    pub fn applicable(&self, skill_id: &str, thread: &Value) -> Result<Value> {
        let skill = self.get(skill_id)?;
        let strings = |value: &Value| {
            value
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        let role = thread["role"].as_str().unwrap_or_default();
        if !strings(&skill["activation"]["roles"])
            .iter()
            .any(|r| r == role)
        {
            return rejected(format!(
                "Skill {skill_id} is not allowed for role {}",
                nosh_wire::js::string(thread.get("role"))
            ));
        }
        let capabilities = strings(&thread["capabilities"]);
        let missing: Vec<String> = strings(&skill["activation"]["requiredCapabilities"])
            .into_iter()
            .filter(|c| !capabilities.contains(c))
            .collect();
        if !missing.is_empty() {
            return rejected(format!(
                "Skill {skill_id} requires missing capabilities: {}",
                missing.join(", ")
            ));
        }
        Ok(skill)
    }
}

/// Validates an orchestration program: unique steps, a real start, declared state, and steps that
/// only target existing steps and declared state keys.
pub fn compile_program(input: &Value) -> Result<Value> {
    let program = validate_record(ORCHESTRATION_PROGRAM, input)?;
    let steps = program["steps"].as_array().cloned().unwrap_or_default();
    let ids: HashSet<&str> = steps
        .iter()
        .filter_map(|step| step["stepId"].as_str())
        .collect();
    if ids.len() != steps.len() {
        return rejected("Program step IDs must be unique");
    }
    if !program["startStepId"]
        .as_str()
        .is_some_and(|start| ids.contains(start))
    {
        return rejected("Program startStepId does not exist");
    }
    let schema = program["state"]["schema"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    validate_state(
        &schema,
        program["state"]["initial"]
            .as_object()
            .unwrap_or(&Map::new()),
    )?;
    for step in &steps {
        let step_id = nosh_wire::js::string(step.get("stepId"));
        for target in [&step["nextStepId"], &step["failureStepId"]] {
            if let Some(target) = target.as_str().filter(|t| !t.is_empty())
                && !ids.contains(target)
            {
                return rejected(format!(
                    "Program step {step_id} targets missing step {target}"
                ));
            }
        }
        // The step's instruction was already validated as a runtime-instruction template by the
        // program schema, as TypeScript's second parse re-checks.
        let output = step["outputStateKey"].as_str().filter(|k| !k.is_empty());
        if let Some(key) = output
            && !js_in(&schema, key)
        {
            return rejected(format!(
                "Program step {step_id} writes undeclared state key {key}"
            ));
        }
        if nosh_wire::js::truthy(step.get("background")) && output.is_some() {
            return rejected(format!(
                "Background step {step_id} cannot bind output before an await boundary"
            ));
        }
    }
    Ok(program)
}

/// Checks program state against its declared schema (`string`, `number`, `boolean`, `ref`, `json`).
pub fn validate_state(schema: &Map<String, Value>, values: &Map<String, Value>) -> Result<()> {
    // `Buffer.byteLength(JSON.stringify(values))`: canonical JSON has the same length.
    if nosh_wire::canonical_json(&Value::Object(values.clone())).len() > 1_000_000 {
        return rejected("Program state exceeds 1000000 bytes");
    }
    for key in values.keys() {
        if !js_in(schema, key) {
            return rejected(format!("State key {key} is not declared"));
        }
    }
    for (key, kind) in schema {
        let kind = nosh_wire::js::string(Some(kind));
        let value = values.get(key);
        // An undeclared value under an Object.prototype name reads as an inherited function.
        let inherited = value.is_none() && OBJECT_PROTOTYPE.contains(&key.as_str());
        if value.is_none() && !inherited {
            return rejected(format!("State key {key} is required"));
        }
        let valid = match (kind.as_str(), value) {
            ("json", _) => true,
            (_, None) => false,
            ("ref", Some(Value::String(text))) => is_ref(text),
            ("string", Some(Value::String(_)))
            | ("number", Some(Value::Number(_)))
            | ("boolean", Some(Value::Bool(_))) => true,
            _ => false,
        };
        if !valid {
            return rejected(format!("State key {key} must be {kind}"));
        }
    }
    Ok(())
}

/// `^[a-z][a-z0-9]*_[a-z0-9][a-z0-9.:-]*$`
fn is_ref(text: &str) -> bool {
    let Some((head, tail)) = text.split_once('_') else {
        return false;
    };
    let mut head_chars = head.chars();
    let head_ok = head_chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && head_chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let mut tail_chars = tail.chars();
    let tail_ok = tail_chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && tail_chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || ".:-".contains(c));
    head_ok && tail_ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs_match_the_wire_pattern() {
        for good in ["ref_1", "task_a.b:c-d", "x_0"] {
            assert!(is_ref(good), "{good}");
        }
        for bad in ["Ref_1", "ref", "_a", "ref_", "ref_A", "1ref_a", "a_b_c!"] {
            assert!(!is_ref(bad), "{bad}");
        }
        // The tail may contain further underscores? No: the pattern's tail excludes them.
        assert!(!is_ref("a_b_c"));
    }
}
