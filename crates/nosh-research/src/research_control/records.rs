//! Submitted records, Review authority, daemon records, user Evidence and Claims, and the durable
//! operation intents behind them.

use super::*;
use nosh_wire::{is_task_terminal_record, now_utc, sha256};

/// A Review's request and verdict as the daemon judges them (`reviewAuthority`).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewAuthority {
    pub review_id: String,
    pub request: Option<Value>,
    pub verdict: Option<Value>,
    /// `pending`, `resolved`, `invalid`, or `ambiguous`.
    pub state: &'static str,
    pub issues: Vec<&'static str>,
}

/// A resolved Review: its request and its verdict (`exactReviewJoin`).
pub(super) struct Joined {
    pub request: Value,
    pub verdict: Value,
}

impl<H: Host> ResearchControl<H> {
    /// `record.submitted` events, optionally for one correlation (`submitted`); each event's
    /// payload is its record.
    pub fn submitted(&self, project_id: &str, correlation_id: Option<&str>) -> Result<Vec<Value>> {
        Ok(self
            .store(project_id)?
            .replay(project_id, 0)?
            .into_iter()
            .filter(|event| {
                event["type"] == "record.submitted"
                    && correlation_id
                        .filter(|id| !id.is_empty())
                        .is_none_or(|id| is(event.get("correlationId"), id))
            })
            .collect())
    }

    pub(super) fn submitted_records(
        &self,
        project_id: &str,
        correlation_id: Option<&str>,
    ) -> Result<Vec<Value>> {
        Ok(self
            .submitted(project_id, correlation_id)?
            .into_iter()
            .map(|mut event| event["payload"].take())
            .collect())
    }

    pub fn terminal_record(
        &self,
        project_id: &str,
        task_id: &str,
        expected_schema: Option<&str>,
    ) -> Result<Option<Value>> {
        let mut entries: Vec<Value> = self
            .submitted_records(project_id, Some(&format!("task:{task_id}")))?
            .into_iter()
            .filter(is_task_terminal_record)
            .collect();
        if entries.len() > 1 {
            return rejected(format!("Task {task_id} has multiple terminal records"));
        }
        let record = entries.pop();
        if let (Some(record), Some(schema)) =
            (&record, expected_schema.filter(|schema| !schema.is_empty()))
            && !is(record.get("$schema"), &schema_uri(schema))
        {
            return rejected(format!(
                "Task {task_id} terminal schema does not match {}",
                schema_uri(schema)
            ));
        }
        Ok(record)
    }

    /// Submitted records, optionally only those of one schema.
    pub fn records(&self, project_id: &str, schema_name: Option<&str>) -> Result<Vec<Value>> {
        Ok(self
            .submitted_records(project_id, None)?
            .into_iter()
            .filter(|record| {
                schema_name
                    .filter(|name| !name.is_empty())
                    .is_none_or(|name| is_schema(record, name))
            })
            .collect())
    }

    pub fn reviews(&self, project_id: &str) -> Result<Vec<ReviewAuthority>> {
        let records = self.records(project_id, None)?;
        let mut review_ids = unique(
            records
                .iter()
                .filter(|record| {
                    (is_schema(record, "review-request") || is_schema(record, "review-verdict"))
                        && is_str(record.get("reviewId"))
                })
                .map(|record| string(record.get("reviewId"))),
        );
        js_sort(&mut review_ids);
        Ok(review_ids
            .iter()
            .map(|review_id| review_authority(&records, review_id))
            .collect())
    }

    pub fn is_domain_effect_record(&self, record: &Value) -> bool {
        record.is_object() && domain::domain_effect_key(record).is_some()
    }

    pub fn submit_daemon_record(
        &self,
        project_id: &str,
        scope: &Value,
        correlation_id: &str,
        record: &Value,
        idempotency_key: &str,
    ) -> Result<Value> {
        self.host.assert_writable(project_id)?;
        if !is(scope.get("projectId"), project_id) {
            return rejected("Daemon record scope Project does not match the selected Project");
        }
        let Some(uri) = record.get("$schema").and_then(Value::as_str) else {
            return rejected("Daemon record requires $schema");
        };
        let value = validate_record(uri, record).map_err(|issues| {
            Error::Rejected(format!(
                "Invalid daemon record: {}",
                pointer_messages(&issues)
            ))
        })?;
        let store = self.store(project_id)?;
        if self.is_domain_effect_record(&value) {
            return rejected(
                "Daemon record path cannot submit domain-effect records without Task authority",
            );
        }
        let draft = record_submitted(scope, correlation_id, value.clone());
        if is_task_terminal_record(&value) {
            let Some(task) = crate::regex!(r"^task:(tsk_[0-9a-f]{32})$")
                .captures(correlation_id)
                .map(|captures| captures[1].to_owned())
            else {
                return rejected(
                    "Daemon terminal records require an exact task:<tsk_...> correlation",
                );
            };
            let stored = store.append_terminal_submission(
                &task,
                "nosh_daemon_record",
                uri,
                &value,
                &draft,
            )?;
            if !stored.replayed {
                self.host.publish(&stored.receipt["event"]);
            }
            return Ok(stored.receipt["event"].clone());
        }
        if let Some(replay) = store.command_receipt(project_id, idempotency_key)? {
            let event = &replay["event"];
            if event["type"] != "record.submitted"
                || !is(event["scope"].get("projectId"), project_id)
                || !is(event.get("correlationId"), correlation_id)
                || sha256(&event["payload"]) != sha256(&value)
            {
                return rejected("Idempotency key was already used for a different daemon record");
            }
            return Ok(event.clone());
        }
        if uri == schema_uri("evidence") {
            self.assert_evidence_record(project_id, &value)?;
            if self
                .records(project_id, Some("evidence"))?
                .iter()
                .any(|entry| same(entry.get("evidenceId"), value.get("evidenceId")))
            {
                return rejected(format!(
                    "Evidence {} already exists",
                    string(value.get("evidenceId"))
                ));
            }
        } else if uri == schema_uri("claim") {
            self.assert_claim_references(project_id, &value)?;
            if self
                .records(project_id, Some("claim"))?
                .iter()
                .any(|entry| {
                    same(entry.get("claimId"), value.get("claimId"))
                        && same(entry.get("claimVersion"), value.get("claimVersion"))
                })
            {
                return rejected(format!(
                    "Claim {} version {} already exists",
                    string(value.get("claimId")),
                    string(value.get("claimVersion"))
                ));
            }
        }
        let stored = store.append_idempotent(idempotency_key, &draft)?;
        if !stored.replayed {
            self.host.publish(&stored.receipt["event"]);
        }
        Ok(stored.receipt["event"].clone())
    }

    pub fn submit_user_evidence(
        &self,
        project_id: &str,
        input: &Value,
        idempotency_key: &str,
    ) -> Result<Value> {
        let mut sanitized = sanitize_user_record(
            project_id,
            input,
            &[
                "evidenceType",
                "statement",
                "polarity",
                "sourceRefs",
                "evaluationContractHash",
                "scopeLimitations",
                "quality",
            ],
        )?;
        if sanitized.get("quality").is_none_or(Value::is_null) {
            sanitized.insert(
                "quality".into(),
                json!({ "status": "unreviewed", "reviewId": null, "confidence": "low" }),
            );
        }
        let input_hash = sha256(
            &json!({ "projectId": project_id, "kind": "user.evidence.create", "input": sanitized }),
        );
        let mut record = json!({ "$schema": schema_uri("evidence"), "schemaVersion": 1, "evidenceId": deterministic_id("evd", project_id, idempotency_key), "projectId": project_id });
        record.as_object_mut().expect("object").extend(sanitized);
        record["createdBy"] = json!("user");
        record["createdByAgentId"] = Value::Null;
        record["createdAt"] = json!(now_utc());
        self.user_record_operation(
            project_id,
            "user.evidence.create",
            idempotency_key,
            &input_hash,
            Some(json!({ "inputHash": input_hash, "record": record })),
        )
    }

    pub fn latest_claims(&self, project_id: &str) -> Result<Vec<Value>> {
        let mut latest: Vec<(String, f64, String, Value)> = Vec::new();
        for record in self.records(project_id, Some("claim"))? {
            let (Some(claim_id), true) = (
                record
                    .get("claimId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                is_integer(record.get("claimVersion")),
            ) else {
                continue;
            };
            let version = number(record.get("claimVersion")).expect("checked");
            let hash = sha256(&record);
            match latest.iter_mut().find(|entry| entry.0 == claim_id) {
                Some(prior) => {
                    if prior.1 == version && prior.2 != hash {
                        return rejected(format!(
                            "Ambiguous Claim version {claim_id}@{}",
                            string(record.get("claimVersion"))
                        ));
                    }
                    if prior.1 < version {
                        *prior = (claim_id, version, hash, record);
                    }
                }
                None => latest.push((claim_id, version, hash, record)),
            }
        }
        // `localeCompare`; Claim IDs are `clm_` and hex, where it agrees with code-unit order.
        latest.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
        Ok(latest.into_iter().map(|entry| entry.3).collect())
    }

    pub fn submit_user_claim(
        &self,
        project_id: &str,
        input: &Value,
        idempotency_key: &str,
    ) -> Result<Value> {
        let sanitized = sanitize_user_record(project_id, input, CLAIM_FIELDS)?;
        let input_hash = sha256(
            &json!({ "projectId": project_id, "kind": "user.claim.create", "input": sanitized }),
        );
        let mut record = json!({ "$schema": schema_uri("claim"), "schemaVersion": 1, "claimId": deterministic_id("clm", project_id, idempotency_key), "projectId": project_id });
        record.as_object_mut().expect("object").extend(sanitized);
        record["claimVersion"] = json!(1);
        record["supersedesClaimVersion"] = Value::Null;
        record["updatedAt"] = json!(now_utc());
        self.user_record_operation(
            project_id,
            "user.claim.create",
            idempotency_key,
            &input_hash,
            Some(json!({ "inputHash": input_hash, "record": record })),
        )
    }

    pub fn edit_user_claim(
        &self,
        project_id: &str,
        claim_id: &str,
        expected_claim_version: f64,
        changes: &Value,
        idempotency_key: &str,
    ) -> Result<Value> {
        let sanitized = sanitize_user_record(project_id, changes, CLAIM_FIELDS)?;
        let mut hashed = json!({ "projectId": project_id, "kind": "user.claim.edit", "claimId": claim_id, "expectedClaimVersion": expected_claim_version, "changes": sanitized });
        normalize_numbers(&mut hashed);
        let input_hash = sha256(&hashed);
        let prior = self.external_operation(project_id, idempotency_key)?;
        let request = if prior.is_some() {
            None
        } else {
            if expected_claim_version.trunc() != expected_claim_version
                || expected_claim_version < 1.0
            {
                return rejected("Claim edit requires a positive expected claim version");
            }
            let Some(current) = self
                .latest_claims(project_id)?
                .into_iter()
                .find(|claim| is(claim.get("claimId"), claim_id))
            else {
                return rejected("Unknown Claim");
            };
            if number(current.get("claimVersion")) != Some(expected_claim_version) {
                return rejected(format!(
                    "Claim version conflict: expected {}, current {}",
                    string(Some(&to_json(&expected_claim_version)?)),
                    string(current.get("claimVersion"))
                ));
            }
            let mut record = current;
            record.as_object_mut().expect("object").extend(sanitized);
            record["$schema"] = json!(schema_uri("claim"));
            record["schemaVersion"] = json!(1);
            record["claimId"] = json!(claim_id);
            record["projectId"] = json!(project_id);
            record["claimVersion"] = to_json(&(expected_claim_version + 1.0))?;
            record["supersedesClaimVersion"] = to_json(&expected_claim_version)?;
            record["updatedAt"] = json!(now_utc());
            Some(json!({ "inputHash": input_hash, "record": record }))
        };
        self.user_record_operation(
            project_id,
            "user.claim.edit",
            idempotency_key,
            &input_hash,
            request,
        )
    }

    /// The shared tail of the user-record commands: replay a finished intent, otherwise check the
    /// record, record the intent, and submit it. `request` is `None` only when an intent exists.
    fn user_record_operation(
        &self,
        project_id: &str,
        kind: &str,
        idempotency_key: &str,
        input_hash: &str,
        request: Option<Value>,
    ) -> Result<Value> {
        let prior = self.external_operation(project_id, idempotency_key)?;
        let request = match &prior {
            Some(prior) => {
                let request = asserted_user_record_intent(prior, kind, input_hash)?;
                if let Some(replay) = self.terminal_operation_replay(project_id, prior)? {
                    return Ok(replay);
                }
                request
            }
            None => request.expect("a new user-record operation carries its request"),
        };
        let record = user_record_request(&request)?;
        if kind == "user.evidence.create" {
            self.assert_evidence_record(project_id, record)?;
        } else {
            self.assert_claim_references(project_id, record)?;
        }
        let intent =
            self.store(project_id)?
                .begin_operation(project_id, kind, idempotency_key, &request)?;
        self.finish_user_record_operation(project_id, &intent)
    }

    pub(super) fn assert_claim_references(&self, project_id: &str, record: &Value) -> Result<()> {
        let claim = validate_record(&schema_uri("claim"), record).map_err(|issues| {
            Error::Rejected(format!("Invalid Claim: {}", pointer_messages(&issues)))
        })?;
        let ids: Vec<String> = [
            "supportingEvidenceIds",
            "contradictingEvidenceIds",
            "qualifyingEvidenceIds",
        ]
        .iter()
        .flat_map(|name| {
            claim[*name]
                .as_array()
                .into_iter()
                .flatten()
                .map(|id| string(Some(id)))
        })
        .collect();
        if ids.iter().collect::<HashSet<_>>().len() != ids.len() {
            return rejected("Claim Evidence IDs cannot be duplicated across polarity arrays");
        }
        let evidence: HashSet<String> = self
            .records(project_id, Some("evidence"))?
            .iter()
            .filter(|entry| entry.is_object() && is(entry.get("projectId"), project_id))
            .map(|entry| string(entry.get("evidenceId")))
            .collect();
        if ids.iter().any(|id| !evidence.contains(id)) {
            return rejected("Claim references Evidence that does not exist in this Project");
        }
        let status = string(claim.get("status"));
        let requires_review = [
            "supported",
            "partially_supported",
            "qualified",
            "not_supported",
            "contradicted",
        ]
        .contains(&status.as_str());
        let required_review = claim.get("requiredReviewId");
        if requires_review && required_review == Some(&Value::Null) {
            return rejected(format!(
                "Claim status {status} requires an exact Claim Review"
            ));
        }
        if required_review != Some(&Value::Null) {
            let failure = || {
                rejected(
                    "Claim required Review must exactly target this Claim version and cover every referenced Evidence",
                )
            };
            let Some(review) =
                exact_review_join(&self.records(project_id, None)?, &string(required_review))
            else {
                return failure();
            };
            let target = &review.request["target"];
            if !is(review.request.get("reviewType"), "claim")
                || !target.is_object()
                || !same(target.get("targetId"), claim.get("claimId"))
                || !same(target.get("targetVersion"), claim.get("claimVersion"))
                || !is(review.verdict.get("verdict"), "PASS")
            {
                return failure();
            }
            // `ids.every((id) => stringValues(...).includes(id))`: never read when no Evidence is cited.
            if !ids.is_empty() {
                let reviewed = string_values(review.verdict.get("reviewedEvidenceIds"))?;
                if !ids.iter().all(|id| reviewed.contains(id)) {
                    return failure();
                }
            }
        }
        Ok(())
    }

    pub(super) fn assert_evidence_record(&self, project_id: &str, record: &Value) -> Result<()> {
        let evidence = validate_record(&schema_uri("evidence"), record).map_err(|issues| {
            Error::Rejected(format!("Invalid Evidence: {}", pointer_messages(&issues)))
        })?;
        if !is(evidence.get("projectId"), project_id) {
            return rejected("Evidence Project does not match submission Project");
        }
        let quality = &evidence["quality"];
        let status = string(quality.get("status"));
        if status != "unreviewed" {
            let review = match quality.get("reviewId").and_then(Value::as_str) {
                Some(review_id) => exact_review_join(&self.records(project_id, None)?, review_id),
                None => None,
            };
            let failure = || {
                rejected(format!(
                    "{status} Evidence requires an exact Review covering this Evidence with matching disposition"
                ))
            };
            let Some(review) = review else {
                return failure();
            };
            let passed = is(review.verdict.get("verdict"), "PASS");
            if (status == "reviewed") != passed
                || !string_values(review.verdict.get("reviewedEvidenceIds"))?
                    .contains(&string(evidence.get("evidenceId")))
            {
                return failure();
            }
        }
        Ok(())
    }

    pub(super) fn terminal_operation_replay(
        &self,
        project_id: &str,
        intent: &Value,
    ) -> Result<Option<Value>> {
        match intent["state"].as_str() {
            Some("pending") => Ok(None),
            Some("failed") => rejected(
                intent
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("Operation previously failed"),
            ),
            _ => match self
                .store(project_id)?
                .command_receipt(project_id, &string(intent.get("idempotencyKey")))?
            {
                Some(receipt) => Ok(Some(receipt["event"].clone())),
                None => rejected("Completed operation is missing its command receipt"),
            },
        }
    }

    pub(super) fn finish_user_record_operation(
        &self,
        project_id: &str,
        intent: &Value,
    ) -> Result<Value> {
        let record = user_record_request(&intent["request"])?;
        let id = match intent["operationType"].as_str() {
            Some("user.evidence.create") => {
                self.assert_evidence_record(project_id, record)?;
                string(record.get("evidenceId"))
            }
            Some("user.claim.create" | "user.claim.edit") => {
                self.assert_claim_references(project_id, record)?;
                string(record.get("claimId"))
            }
            _ => return rejected("Unsupported user-record operation"),
        };
        let event = self.submit_daemon_record(
            project_id,
            &project_scope(project_id),
            &id,
            record,
            &string(intent.get("idempotencyKey")),
        )?;
        self.store(project_id)?.complete_operation(
            project_id,
            &string(intent.get("intentId")),
            &json!({ "eventId": event["eventId"] }),
        )?;
        Ok(event)
    }

    pub fn begin_external_operation(
        &self,
        project_id: &str,
        operation_type: &str,
        idempotency_key: &str,
        request: &Value,
    ) -> Result<Value> {
        Ok(self.store(project_id)?.begin_operation(
            project_id,
            operation_type,
            idempotency_key,
            request,
        )?)
    }

    pub fn complete_external_operation(
        &self,
        project_id: &str,
        intent_id: &str,
        result: &Value,
    ) -> Result<Value> {
        Ok(self
            .store(project_id)?
            .complete_operation(project_id, intent_id, result)?)
    }

    /// The durable intent recorded under an idempotency key, if any.
    pub fn external_operation(
        &self,
        project_id: &str,
        idempotency_key: &str,
    ) -> Result<Option<Value>> {
        Ok(self
            .store(project_id)?
            .operation_intents(project_id, None)?
            .into_iter()
            .find(|intent| is(intent.get("idempotencyKey"), idempotency_key)))
    }

    pub fn pending_external_operations(
        &self,
        project_id: &str,
        operation_type: Option<&str>,
    ) -> Result<Vec<Value>> {
        Ok(self
            .store(project_id)?
            .operation_intents(project_id, Some("pending"))?
            .into_iter()
            .filter(|intent| {
                operation_type
                    .filter(|kind| !kind.is_empty())
                    .is_none_or(|kind| is(intent.get("operationType"), kind))
            })
            .collect())
    }

    pub fn scoped_events(
        &self,
        project_id: &str,
        mission_id: &str,
        event_type: Option<&str>,
    ) -> Result<Vec<Value>> {
        Ok(self
            .store(project_id)?
            .replay(project_id, 0)?
            .into_iter()
            .filter(|event| {
                is(event["scope"].get("missionId"), mission_id)
                    && event_type
                        .filter(|kind| !kind.is_empty())
                        .is_none_or(|kind| is(event.get("type"), kind))
            })
            .collect())
    }

    /// Whether exactly one Review of `review_type` targets `target_id` (at `target_version`, and
    /// with `review_id` when given), and its verdict passes every required criterion, Artifact,
    /// and Evidence.
    pub(super) fn passing_review(
        &self,
        project_id: &str,
        target_id: &str,
        review_type: &str,
        target_version: Option<f64>,
        review_id: Option<&str>,
    ) -> Result<bool> {
        // One snapshot of the records, as the TypeScript keeps one hydrated read.
        let records = self.records(project_id, None)?;
        let requests: Vec<&Value> = records
            .iter()
            .filter(|record| is_schema(record, "review-request"))
            .filter(|request| {
                is(request.get("reviewType"), review_type)
                    && request["target"].is_object()
                    && is(request["target"].get("targetId"), target_id)
                    && target_version.is_none_or(|version| {
                        number(request["target"].get("targetVersion")) == Some(version)
                    })
                    && review_id
                        .filter(|id| !id.is_empty())
                        .is_none_or(|id| is(request.get("reviewId"), id))
            })
            .collect();
        let [request] = requests.as_slice() else {
            return Ok(false);
        };
        let Some(request_review_id) = request.get("reviewId").and_then(Value::as_str) else {
            return Ok(false);
        };
        // The joined request is necessarily `request`: it is the only review-request with its ID.
        let Some(joined) = exact_review_join(&records, request_review_id) else {
            return Ok(false);
        };
        if !is(joined.verdict.get("verdict"), "PASS") {
            return Ok(false);
        }
        let required_criteria: Vec<String> = objects(joined.request.get("criteria"))
            .filter(|criterion| criterion.get("required") != Some(&Value::Bool(false)))
            .filter_map(|criterion| {
                criterion
                    .get("criterionId")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect();
        let verdict_criteria: Vec<&Value> = objects(joined.verdict.get("criteria")).collect();
        if !required_criteria.iter().all(|criterion_id| {
            verdict_criteria.iter().any(|criterion| {
                is(criterion.get("criterionId"), criterion_id)
                    && is(criterion.get("status"), "PASS")
            })
        }) {
            return Ok(false);
        }
        let covered = |required: &str, reviewed: &str| {
            joined
                .request
                .get(required)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .all(|id| {
                    id.as_str()
                        .is_some_and(|id| includes(joined.verdict.get(reviewed), id))
                })
        };
        Ok(covered("requiredArtifactIds", "reviewedArtifactIds")
            && covered("requiredEvidenceIds", "reviewedEvidenceIds"))
    }
}

const CLAIM_FIELDS: &[&str] = &[
    "text",
    "claimType",
    "status",
    "supportingEvidenceIds",
    "contradictingEvidenceIds",
    "qualifyingEvidenceIds",
    "limitations",
    "paperLocations",
    "requiredReviewId",
];

fn sanitize_user_record(
    project_id: &str,
    input: &Value,
    allowed: &[&str],
) -> Result<Map<String, Value>> {
    let Some(input) = input.as_object() else {
        return rejected("User record must be an object");
    };
    if input
        .get("projectId")
        .is_some_and(|id| !is(Some(id), project_id))
    {
        return rejected("Record Project must match the route Project");
    }
    if let Some(key) = input
        .keys()
        .find(|key| *key != "projectId" && !allowed.contains(&key.as_str()))
    {
        return rejected(format!("User record field is not allowed: {key}"));
    }
    Ok(input
        .iter()
        .filter(|(key, _)| *key != "projectId")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect())
}

fn deterministic_id(prefix: &str, project_id: &str, idempotency_key: &str) -> String {
    let hash = sha256(
        &json!({ "prefix": prefix, "projectId": project_id, "idempotencyKey": idempotency_key }),
    );
    format!("{prefix}_{}", &hash[7..39])
}

fn asserted_user_record_intent(intent: &Value, kind: &str, input_hash: &str) -> Result<Value> {
    if !is(intent.get("operationType"), kind)
        || !intent["request"].is_object()
        || !is(intent["request"].get("inputHash"), input_hash)
    {
        return rejected("Idempotency key was already used for different user input");
    }
    Ok(intent["request"].clone())
}

fn user_record_request(request: &Value) -> Result<&Value> {
    match request.get("record") {
        Some(record) if request.is_object() && record.is_object() => Ok(record),
        _ => rejected("User-record operation intent is corrupt"),
    }
}

pub(super) fn exact_review_join(records: &[Value], review_id: &str) -> Option<Joined> {
    let authority = review_authority(records, review_id);
    match (authority.state, authority.request, authority.verdict) {
        ("resolved", Some(request), Some(verdict)) => Some(Joined { request, verdict }),
        _ => None,
    }
}

pub(super) fn review_authority(records: &[Value], review_id: &str) -> ReviewAuthority {
    let requests: Vec<&Value> = records
        .iter()
        .filter(|record| {
            is_schema(record, "review-request") && is(record.get("reviewId"), review_id)
        })
        .collect();
    let verdicts: Vec<&Value> = records
        .iter()
        .filter(|record| {
            is_schema(record, "review-verdict") && is(record.get("reviewId"), review_id)
        })
        .collect();
    let result =
        |request: Option<&Value>, verdict: Option<&Value>, state, issues| ReviewAuthority {
            review_id: review_id.to_owned(),
            request: request.cloned(),
            verdict: verdict.cloned(),
            state,
            issues,
        };
    if requests.len() > 1 {
        let mut issues = vec!["multiple_review_requests"];
        if verdicts.len() > 1 {
            issues.push("multiple_review_verdicts");
        }
        return result(None, None, "ambiguous", issues);
    }
    let [request] = requests.as_slice() else {
        return result(
            None,
            None,
            "invalid",
            vec!["missing_or_invalid_review_request"],
        );
    };
    if !valid("review-request", request) {
        return result(
            None,
            None,
            "invalid",
            vec!["missing_or_invalid_review_request"],
        );
    }
    let producers = request.get("producerAgentIds").and_then(Value::as_array);
    let independent = producers.is_some_and(|producers| {
        producers.iter().all(Value::is_string)
            && !producers
                .iter()
                .any(|id| same(Some(id), request.get("reviewerAgentId")))
    });
    if !is_str(request.get("reviewRequestId"))
        || !is_str(request.get("reviewType"))
        || !is_str(request.get("reviewerAgentId"))
        || !request["target"].is_object()
        || !independent
    {
        return result(
            None,
            None,
            "invalid",
            vec!["invalid_review_request_identity_or_independence"],
        );
    }
    match verdicts.as_slice() {
        [] => result(Some(request), None, "pending", Vec::new()),
        [verdict] => {
            let (target, wanted) = (&verdict["target"], &request["target"]);
            let matches = valid("review-verdict", verdict)
                && same(
                    verdict.get("reviewRequestId"),
                    request.get("reviewRequestId"),
                )
                && same(verdict.get("reviewType"), request.get("reviewType"))
                && same(
                    verdict.get("reviewerAgentId"),
                    request.get("reviewerAgentId"),
                )
                && is(verdict.get("independenceCheck"), "pass")
                && target.is_object()
                && same(target.get("targetType"), wanted.get("targetType"))
                && same(target.get("targetId"), wanted.get("targetId"))
                && same(target.get("targetVersion"), wanted.get("targetVersion"));
            if matches {
                result(Some(request), Some(verdict), "resolved", Vec::new())
            } else {
                result(
                    Some(request),
                    None,
                    "invalid",
                    vec!["invalid_or_mismatched_review_verdict"],
                )
            }
        }
        _ => result(
            Some(request),
            None,
            "ambiguous",
            vec!["multiple_review_verdicts"],
        ),
    }
}
