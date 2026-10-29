//! Project discovery: the intake prompt and the successor template of the draft contract, as
//! `intakePrompt` and `contractSuccessorTemplate` in `apps/noshd/src/daemon.ts`.
//!
//! The JSON embedded in the prompt has sorted keys, where TypeScript keeps the file's order.

use nosh_research::project::detect_runnable_commands;
use nosh_wire::js::string;
use serde_json::{Value, json};
use std::path::Path;

/// The discovery conversation's instructions for the Project's Nosh session. `schema_dir` holds
/// the shipped record schemas.
pub fn intake_prompt(contract: &Value, repository_root: &Path, schema_dir: &Path) -> String {
    let detected = detect_runnable_commands(repository_root);
    // Commands approved here let workers run tests/evaluations as daemon Jobs from the first
    // Direction, with no later amendment.
    let commands = if detected.is_empty() {
        "\n\nNo runnable test or evaluation commands were detected. If the user names some, add them as the optional execution field: { \"runner\": \"native\", \"commands\": [{ \"commandId\": \"command_test\", \"description\": \"…\", \"argv\": [\"node\", \"--test\"], \"timeoutSeconds\": 120 }] } (argv lists only, never shell strings; npm is not allowed, call node directly).".to_owned()
    } else {
        format!(
            "\n\nRunnable commands detected in this repository (already in the template's execution field): {}. Confirm with the user which are the right validators; remove the others, or remove the whole execution field if none. Add any other command the user names in the same shape (a new command_<name> commandId, an argv list, timeoutSeconds), and set each timeoutSeconds to the user's runtime with headroom. Keep each argv exactly as detected: Jobs launch executables directly with no shell, so never use npm or npx (shell shims on Windows); call node (or the tool) directly. A command must already work in the repository today; a flag that a later Mission adds belongs in a later contract amendment.",
            Value::Array(detected.clone())
        )
    };
    let schema = schema_dir.join("project-contract.v1.schema.json");
    // The draft and a successor template are daemon facts: the model never has to find, read, or
    // reconstruct them.
    format!(
        "This Project has a draft, unapproved Project contract for \"{}\". Conduct a facilitated research-discovery conversation: ask exactly one focused question per turn; do not invent requirements; challenge vague, contradictory, unfalsifiable, or overly broad claims; and periodically reflect the shared understanding. Establish the research problem, motivation, decision use, contribution, scope, datasets and licenses, baselines, evaluation and falsification criteria, reproducibility, risks, intended paper outcome, and realistic GPU and time budgets before proposing an approved contract. Current north-star question: {}\n\nCurrent draft contract (version {}; do not search for the file): {}\n\nWhen the user explicitly approves the concrete proposal, submit it with nosh_project_contract_submit using exactly this successor template: keep every value that is not an \"EDIT: …\" string (identity, version, provenance, branch, paper claim policy), replace each \"EDIT: …\" string with the agreed content, and fill the string lists (domainTags, scope.included/excluded, licensingConstraints, paper.requiredSections) with plain strings. datasets is a list of objects such as {{\"name\": \"…\", \"license\": \"…\", \"source\": \"…\"}}. Refs (goalId, contributionType, policy values) are a lowercase prefix, one underscore, then lowercase letters, digits, '.', ':' or '-'. For literature reads by the librarian set policies.network to \"network_research.allowlisted\"; otherwise keep it. Successor template: {}\n\nOnly if a field is unclear, the canonical JSON Schema is {}.{commands}",
        string(contract.get("workingTitle")),
        string(contract.pointer("/northStar/question")),
        string(contract.get("contractVersion")),
        contract,
        contract_successor_template(contract, &detected),
        schema.display(),
    )
}

/// The next contract version with daemon facts kept and research content marked for the agent to
/// fill.
pub fn contract_successor_template(contract: &Value, detected: &[Value]) -> Value {
    let pending = |value: &Value, prompt: &str| -> Value {
        let text = string(Some(value));
        if text.starts_with("Pending collaborative") || text.ends_with("_pending") {
            json!(format!("EDIT: {prompt}"))
        } else {
            value.clone()
        }
    };
    let mut template = contract.clone();
    let north_star = &contract["northStar"];
    let mut star = north_star.clone();
    star["question"] = pending(&north_star["question"], "the falsifiable research question");
    star["decisionUse"] = pending(
        &north_star["decisionUse"],
        "the decision this research informs, and for whom",
    );
    star["contributionType"] = pending(
        &north_star["contributionType"],
        "a ref such as contribution_empirical.evaluation",
    );
    let filled = |key: &str, default: &str| -> Value {
        match contract.pointer(key).and_then(Value::as_array) {
            Some(items) if !items.is_empty() => Value::Array(items.clone()),
            _ => json!([default]),
        }
    };
    template["contractVersion"] =
        json!(contract["contractVersion"].as_f64().unwrap_or(f64::NAN) + 1.0);
    template["approvedAt"] = Value::Null;
    template["northStar"] = star;
    template["domainTags"] = filled("/domainTags", "EDIT: a domain tag");
    template["scope"] = json!({ "included": filled("/scope/included", "EDIT: what the study covers"), "excluded": contract.pointer("/scope/excluded").cloned().unwrap_or(Value::Null) });
    if !detected.is_empty() && !nosh_wire::js::truthy(contract.get("execution")) {
        template["execution"] = json!({ "runner": "native", "commands": detected });
    }
    nosh_wire::normalize_numbers(&mut template);
    template
}
