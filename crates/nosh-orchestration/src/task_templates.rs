//! Port of `apps/noshd/src/task-templates.ts`: schema-valid completion templates for worker Task
//! Packets. As with director briefs, the daemon fills identity, commits, branch, criteria, and
//! validator/Job references; the worker edits only judgment fields. Prose descriptions of strict
//! schemas were paraphrased by models in live runs.
//!
//! Divergence: embedded JSON has sorted keys (serde_json), not TypeScript's insertion order.

use nosh_wire::js::slice_to;
use nosh_wire::{episode_type_for_role, now_utc, schema_document_path, schema_uri};
use serde_json::{Value, json};
use std::path::Path;

/// The daemon's own run of a frozen evaluation, as a worker's packet reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct DaemonEvaluation {
    pub job_id: String,
    pub state: String,
    pub exit_code: Option<i64>,
    pub display_command: String,
    pub metrics: Value,
    pub metric_artifact_id: Option<String>,
    pub failure: Option<String>,
}

/// `JSON.stringify(value)`.
pub(crate) fn stringify(value: &Value) -> String {
    let mut value = value.clone();
    nosh_wire::normalize_numbers(&mut value);
    value.to_string()
}

/// The general-worker completion for `packet` (its `acceptanceCriteria` and `workspace`).
pub fn general_worker_completion_template(
    packet: &Value,
    evaluation: Option<&DaemonEvaluation>,
) -> Value {
    let evidence_artifacts: Vec<&str> = evaluation
        .and_then(|evaluation| evaluation.metric_artifact_id.as_deref())
        .into_iter()
        .collect();
    let workspace = &packet["workspace"];
    let criteria: Vec<Value> = packet["acceptanceCriteria"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|criterion| {
            let mut runs: Vec<Value> = criterion["validatorIds"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            if let Some(evaluation) = evaluation {
                runs.push(json!(evaluation.job_id));
            }
            json!({ "criterionId": criterion["criterionId"], "workerClaim": "satisfied", "validatorRunIds": runs, "artifactIds": evidence_artifacts, "notes": "EDIT: why the criterion is or is not met, citing the evidence above." })
        })
        .collect();
    let commands: Vec<Value> = evaluation
        .map(|evaluation| json!({ "commandId": "command_evaluation", "displayCommand": slice_to(&evaluation.display_command, 2_000), "exitCode": evaluation.exit_code, "resultArtifactId": evaluation.metric_artifact_id }))
        .into_iter()
        .collect();
    json!({
        "$schema": schema_uri("general-worker-completion"), "schemaVersion": 1, "taskOutcome": "completed",
        "workPerformed": [{ "action": "work_audit", "subject": "EDIT: what you inspected or changed, and what you found.", "artifactIds": evidence_artifacts }],
        "codeChanges": { "startingCommit": workspace["startingCommit"], "endingCommit": workspace["startingCommit"], "changedPaths": [], "diffArtifactId": null, "branch": workspace["branch"] },
        "commands": commands,
        "criteria": criteria,
        "scientificImpact": { "claimIds": [], "evidenceIds": [], "interpretation": "EDIT: what this result means for the research question." },
        "deviations": [], "newRisks": [], "unresolvedItems": [], "suggestedNextActions": [],
        "readyForDeterministicPostflight": true, "readyForReview": true,
    })
}

/// Episode draft for a runtime THREAD_STEP; field names follow episode-draft/v1 exactly.
fn episode_draft_template(role: &str, example_refs: &[Value]) -> Value {
    json!({ "$schema": schema_uri("episode-draft"), "schemaVersion": 1, "episodeType": episode_type_for_role(role), "summary": "EDIT: one-paragraph summary of what you did and found.", "facts": [{ "statement": "EDIT: a verified fact.", "evidenceRefs": example_refs, "confidence": "high" }], "decisions": [{ "statement": "EDIT: a decision you made.", "rationale": "EDIT: why.", "evidenceRefs": [] }], "artifactIds": [], "evidenceIds": [], "changedFiles": [], "unresolvedQuestions": [], "recommendedNextActions": [] })
}

/// A complete terminal envelope (record + episode) for any role; models copy structure instead of
/// writing it.
pub fn terminal_envelope(record: &Value, role: &str) -> String {
    stringify(
        &json!({ "$schema": schema_uri("terminal-output"), "schemaVersion": 1, "records": [record, episode_draft_template(role, &[])] }),
    )
}

/// The worker's whole final answer, prebuilt. Models hand-writing long nested JSON produced
/// unbalanced brackets and invented episode fields; copying a valid envelope avoids both.
pub fn completion_instructions(template: &Value, role: &str, schema_dir: &Path) -> String {
    let artifacts: Vec<Value> = template["criteria"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|criterion| {
            criterion["artifactIds"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .take(1)
        .collect();
    let envelope = json!({ "$schema": schema_uri("terminal-output"), "schemaVersion": 1, "records": [template, episode_draft_template(role, &artifacts)] });
    [
        format!(
            "Your final answer must be exactly this JSON envelope (completion schema: {}; episode schema: {}). Copy it, keep every key and bracket, and replace each \"EDIT: ...\" string.",
            schema_document_path(schema_dir, "general-worker-completion").display(),
            schema_document_path(schema_dir, "episode-draft").display()
        ),
        "Judgment you may change in the completion: taskOutcome (partial/blocked/failed when the work is not done), criteria[].workerClaim (unsatisfied/inconclusive when the evidence does not support the criterion; deferred_to_review with empty validatorRunIds only for a criterion that only the independent reviewer can establish, such as one requiring an independent Review), notes, and the plain-string arrays newRisks, unresolvedItems, and deviations. Keep suggestedNextActions empty: the director plans next actions. In the episode you may add facts/decisions objects of the same shape (their evidenceRefs are plain ID strings such as 'art_...', never objects) and plain-string unresolvedQuestions; leave artifactIds, evidenceIds, changedFiles, and recommendedNextActions as given.".to_owned(),
        "criteria[].validatorRunIds names what establishes each criterion: validator_task.postflight is the daemon's own Git/scope postflight (keep it for criteria it checks, such as no protected or out-of-scope changes), and job_... IDs are your passing nosh_run runs on your final content (a run made after your last edit; only the paper write-up, docs/paper.md and docs/paper.bib, may be committed after the runs it reports, so run a held-out evaluation once and then write it up). Never leave a satisfied criterion without one. Never hand-type a value that code computes (a fitted parameter, a metric, a threshold): produce it with code you run through nosh_run, or add a test that asserts the stored value equals the computation; if you cannot, report it in unresolvedItems and do not claim the criterion. A test counts only if it exercises the code under change (imports it or runs it); a test that re-implements the logic it checks proves nothing. Keep commands exactly as given: the daemon records every nosh_run itself, and you cite runs only through validatorRunIds.".to_owned(),
        "Do not commit and leave codeChanges as given: the daemon commits all your worktree edits after this answer and fills codeChanges and the episode's changedFiles. To cite a file from your worktree as evidence, put \"artifact:<relative/path>\" in an artifactIds list; the daemon snapshots it and substitutes its art_ ID. Leave every other ID you did not receive from the host unchanged.".to_owned(),
        stringify(&envelope),
    ]
    .join("\n")
}

/// Deliberately unresolvable until replaced with a real worktree path; the daemon snapshots each
/// "artifact:<path>" citation.
pub const REGISTER_ARTIFACT: &str = "artifact:EDIT/relative/path";

/// Librarian completion: every cited source, the report, and the bibliography are cited by worktree
/// path; the daemon snapshots them as Artifacts.
pub fn librarian_instructions(task_id: &str, network_allowed: bool, schema_dir: &Path) -> String {
    let folder = format!("research/{task_id}");
    let record = json!({
        "$schema": schema_uri("librarian-completion"), "schemaVersion": 1, "taskOutcome": "completed", "researchQuestion": "EDIT: the question you researched.",
        "searchCoverage": { "databases": [if network_allowed { "EDIT: database you searched" } else { "local_repository" }], "queries": ["EDIT: a query or file pattern you searched"], "dateRange": { "from": null, "to": null }, "language": ["en"], "inclusionCriteria": ["EDIT: an inclusion rule"], "exclusionCriteria": [] },
        "sources": [{ "sourceId": "source_1", "sourceType": "source_repository.file", "title": "EDIT: source title", "authors": [], "publicationDate": null, "canonicalUrl": "file:///EDIT/relative/path", "persistentId": null, "version": "EDIT: commit or version", "primarySource": true, "accessedAt": now_utc(), "artifactId": REGISTER_ARTIFACT, "relevance": "EDIT: why this source matters" }],
        "findings": [{ "findingId": "finding_1", "statement": "EDIT: one finding", "support": [{ "sourceId": "source_1", "locator": "EDIT: line, section, or key", "artifactId": REGISTER_ARTIFACT }], "confidence": "medium", "noveltyImplication": "" }],
        "contradictions": [], "evaluationDifferences": [], "knowledgeGaps": [], "candidateClaimEffects": [],
        "bibliographyArtifactId": format!("artifact:{folder}/bibliography.bib"), "reportArtifactId": format!("artifact:{folder}/report.md"), "readyForReview": true,
    });
    let reach = if network_allowed {
        "read allowlisted literature APIs with nosh_network_read and the local repository"
    } else {
        "network is disabled: research only the local repository"
    };
    [
        format!(
            "Steps: (1) {reach}; (2) write your report to {folder}/report.md and a BibTeX bibliography to {folder}/bibliography.bib with nosh_workspace_write; (3) answer with the envelope below. Cite each file you rely on as \"artifact:<relative/path>\" (no registration step: the daemon snapshots every cited file after your answer and substitutes its art_ ID)."
        ),
        format!(
            "Your final answer must be exactly this JSON envelope (schema: {}). Copy it, keep every key and bracket, replace each \"EDIT: ...\" and every \"{REGISTER_ARTIFACT}\" with real values (a source's artifactId and its findings' support artifactId are the same \"artifact:<path>\"), and add sources/findings objects of the same shape (sourceId/findingId are refs like source_2, finding_2). knowledgeGaps is a list of plain strings; leave contradictions, evaluationDifferences, and candidateClaimEffects empty unless you have them. Record 2 is your episode: keep exactly its keys, add facts/decisions objects of the same shape (evidenceRefs are plain ID strings such as 'art_...'), put your \"artifact:<path>\" citations in artifactIds, and leave recommendedNextActions empty (the director plans next actions).",
            schema_document_path(schema_dir, "librarian-completion").display()
        ),
        terminal_envelope(&record, "librarian_researcher"),
    ]
    .join("\n")
}

pub const UNDECIDED_VERDICT: &str = "DECIDE: PASS | REVISE | REDESIGN | BLOCKED";
pub const UNDECIDED_STATUS: &str = "DECIDE: PASS | FAIL | INCONCLUSIVE | NOT_APPLICABLE";

/// Independent review answer, prebuilt from the daemon's Review Request. The verdict and each
/// criterion status are deliberately undecided placeholders: a default would bias the reviewer.
pub fn reviewer_instructions(request: &Value, schema_dir: &Path) -> String {
    let criteria: Vec<Value> = request["criteria"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|criterion| json!({ "criterionId": criterion["criterionId"], "required": true, "status": UNDECIDED_STATUS, "finding": "EDIT: what the evidence shows for this criterion.", "evidenceRefs": [], "confidence": "medium", "defectIds": [] }))
        .collect();
    let verdict = json!({ "$schema": schema_uri("review-verdict"), "schemaVersion": 1, "templateVersion": "1.0.0", "reviewId": request["reviewId"], "reviewRequestId": request["reviewRequestId"], "reviewType": request["reviewType"], "target": request["target"], "reviewerAgentId": request["reviewerAgentId"], "independenceCheck": "pass", "verdict": UNDECIDED_VERDICT, "summary": "EDIT: your overall judgment and why.", "criteria": criteria, "defects": [], "missingRequiredInputs": [], "scientificIntegrityFlags": [], "recommendedGraphAction": "graph_none", "recommendedPromotion": "not_applicable", "reviewedArtifactIds": request["requiredArtifactIds"], "reviewedEvidenceIds": request["requiredEvidenceIds"], "submittedAt": now_utc() });
    let first: Vec<Value> = request["requiredArtifactIds"]
        .as_array()
        .into_iter()
        .flatten()
        .take(1)
        .cloned()
        .collect();
    let envelope = json!({ "$schema": schema_uri("terminal-output"), "schemaVersion": 1, "records": [verdict, episode_draft_template("reviewer", &first)] });
    [
        format!(
            "Submit exactly one {} with the exact Review Request identities, as this JSON envelope (schemas: {}, {}). Keep every key and bracket.",
            schema_uri("review-verdict"),
            schema_document_path(schema_dir, "review-verdict").display(),
            schema_document_path(schema_dir, "episode-draft").display()
        ),
        format!(
            "You must decide verdict ({}) and every criteria[].status ({}) from the evidence alone, replace each \"EDIT: ...\" string, and set confidence (low/medium/high). PASS requires every required criterion PASS and no blocking or major defect. A computed value (a fitted parameter, a metric, a selected threshold) counts as established only when a daemon run output or a test that pins that exact value shows it; a number merely present in a file or a report is unverified: compare it against the code and the run outputs, and if they disagree or nothing pins it, mark the criterion FAIL or INCONCLUSIVE with a defect. A test that re-implements the logic it claims to check, instead of importing or running the changed code, pins nothing. A criterion the worker marked deferred_to_review is yours to decide: PASS it only if the evidence you can verify (records, Job results, artifacts) establishes it; when it asks for an independent Review, this Review is that Review. Read every required Artifact with nosh_artifact_read before deciding. A declared Project-contract command (see each Job's description) is the approved way to run what its description names. For each problem add a defects entry {{defectId (ref, e.g. defect_metric-mismatch), severity (blocking/major/minor/observation), category (ref such as category_missing-evidence), title, description, criterionIds, evidenceRefs: [], requiredRemediation, acceptanceTest {{validatorId (ref), expected}}, suggestedOwnerRole (general_worker/librarian_researcher/reviewer/director), blocking}} and list its defectId in the criterion. Every ref (defectId, category, validatorId, refType) is a lowercase prefix, exactly ONE underscore, then hyphens or dots: defect_missing-guardrail-evidence, validator_experiment.postflight, source_evidence (never a second underscore). missingRequiredInputs lists only exact art_/evd_ IDs you were required to see but could not, never prose. In the verdict, evidenceRefs entries are objects {{refType, refId, locator}}. In the episode (record 2) evidenceRefs are plain ID strings such as \"art_...\" (never objects). Fill the episode with your verified facts and decisions; leave its recommendedNextActions empty.",
            &UNDECIDED_VERDICT[8..],
            &UNDECIDED_STATUS[8..]
        ),
        stringify(&envelope),
    ]
    .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn records(text: &str) -> Vec<Value> {
        let envelope: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        envelope["records"].as_array().unwrap().clone()
    }

    #[test]
    fn templates_are_schema_valid_once_their_placeholders_are_decided() {
        let packet = json!({ "acceptanceCriteria": [{ "criterionId": "criterion_a", "validatorIds": ["validator_task.postflight"], "required": true }], "workspace": { "startingCommit": "a".repeat(40), "branch": "nosh/task" } });
        let evaluation = DaemonEvaluation {
            job_id: "job_0123456789abcdef0123456789abcdef".into(),
            state: "completed".into(),
            exit_code: Some(0),
            display_command: "x".repeat(3_000),
            metrics: json!({}),
            metric_artifact_id: Some("art_0123456789abcdef0123456789abcdef".into()),
            failure: None,
        };
        let template = general_worker_completion_template(&packet, Some(&evaluation));
        assert_eq!(
            template["criteria"][0]["validatorRunIds"],
            json!(["validator_task.postflight", evaluation.job_id])
        );
        assert_eq!(
            nosh_wire::js::length(template["commands"][0]["displayCommand"].as_str().unwrap()),
            2_000
        );
        let dir = Path::new("schemas");
        let [completion, episode] =
            records(&completion_instructions(&template, "general_worker", dir))
                .try_into()
                .unwrap();
        assert!(
            nosh_wire::validate_record(&schema_uri("general-worker-completion"), &completion)
                .is_ok()
        );
        assert_eq!(episode["episodeType"], "episode_general-worker");
        assert_eq!(
            episode["facts"][0]["evidenceRefs"],
            json!([evaluation.metric_artifact_id])
        );
        assert!(nosh_wire::validate_record(&schema_uri("episode-draft"), &episode).is_ok());

        let request = json!({ "reviewId": "rev_0123456789abcdef0123456789abcdef", "reviewRequestId": "request_x", "reviewType": "task", "target": { "targetType": "target_task", "targetId": "tsk_0123456789abcdef0123456789abcdef", "targetVersion": 1 }, "reviewerAgentId": "agt_0123456789abcdef0123456789abcdef", "requiredArtifactIds": [], "requiredEvidenceIds": [], "criteria": [{ "criterionId": "criterion_a" }] });
        let text = reviewer_instructions(&request, dir);
        assert!(text.contains("You must decide verdict (PASS | REVISE | REDESIGN | BLOCKED) and every criteria[].status (PASS | FAIL | INCONCLUSIVE | NOT_APPLICABLE)"));
        let [mut verdict, _] = records(&text).try_into().unwrap();
        assert_eq!(verdict["verdict"], UNDECIDED_VERDICT);
        verdict["verdict"] = json!("PASS");
        verdict["criteria"][0]["status"] = json!("PASS");
        let checked = nosh_wire::validate_record(&schema_uri("review-verdict"), &verdict);
        assert!(checked.is_ok(), "{checked:?}");

        let text = librarian_instructions("tsk_0123456789abcdef0123456789abcdef", false, dir);
        assert!(text.contains("network is disabled"));
        assert!(text.contains(&format!(
                "schema: {}",
                Path::new("schemas")
                    .join("librarian-completion.v1.schema.json")
                    .display()
            )));
        let [librarian, episode] = records(&text).try_into().unwrap();
        assert_eq!(
            librarian["searchCoverage"]["databases"],
            json!(["local_repository"])
        );
        assert_eq!(episode["episodeType"], "episode_librarian-researcher");
    }
}
