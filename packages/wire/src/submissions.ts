import { id, observedVersionsSchema, schemaUri, schemaVersion, sha256Digest, templateVersion, timestamp } from "./common.js";
import { jsonValueSchema } from "./json.js";
import { z } from "zod";

const ref = z.string().regex(/^[a-z][a-z0-9]*_[a-z0-9][a-z0-9.:-]*$/).max(128);
const text = z.string().min(1).max(2000);
const list = z.array(z.string().min(1).max(1000)).max(100);
const outcome = z.enum(["completed", "partial", "blocked", "failed", "cancelled"]);
const scope = z.object({ projectId: id("prj"), missionId: id("mis").nullable(), directionId: id("dir").nullable(), autoresearchId: id("ar").nullable() }).strict();
const versionRange = z.object({ fromSequence: z.number().int().nonnegative(), toSequence: z.number().int().nonnegative() }).strict();

export const taskPacketSchema = z
  .object({
    $schema: z.literal(schemaUri("task-packet")), schemaVersion, templateVersion,
    taskId: id("tsk"), attempt: z.number().int().positive(), taskType: ref,
    assignedRole: z.enum(["librarian_researcher", "general_worker", "reviewer"]), assignedAgentId: id("agt"),
    scope: scope.extend({ experimentId: id("exp").nullable(), graphNodeId: ref }),
    goalStack: z.object({ projectGoalId: ref, projectGoalSummary: text, missionCriterionIds: z.array(ref), missionCriterionSummary: text, directionQuestionId: ref.nullable(), directionQuestionSummary: text.nullable(), nodeObjective: text }).strict(),
    whyNow: text, instructions: list, inScope: list, outOfScope: list,
    inputArtifactIds: z.array(id("art")), inputEvidenceIds: z.array(id("evd")),
    requiredOutputs: z.array(z.object({ outputId: ref, kind: ref, required: z.boolean() }).strict()),
    acceptanceCriteria: z.array(z.object({ criterionId: ref, statement: text, validatorIds: z.array(ref), reviewRubricIds: z.array(ref), required: z.boolean() }).strict()),
    workspace: z.object({ worktreeId: ref, branch: text, startingCommit: z.string().min(7).max(128), writeScopes: list, protectedScopes: list }).strict(),
    permissions: z.object({ network: z.enum(["disabled", "allowlisted", "enabled"]), subprocess: z.enum(["disabled", "allowlisted", "enabled"]), gitCommit: z.boolean(), gitPush: z.boolean(), delegation: z.literal("request_only"), allowedToolIds: z.array(ref) }).strict(),
    budget: z.object({ deadline: timestamp, maximumWallClockSeconds: z.number().int().positive(), maximumModelTokens: z.number().int().positive(), maximumToolCalls: z.number().int().positive(), maximumRepairAttempts: z.number().int().nonnegative() }).strict(),
    progressPolicy: z.object({ milestoneIntervalSeconds: z.number().int().positive(), emitOnFirstDurableDelta: z.boolean(), emitOnBlocker: z.boolean(), emitOnAnomaly: z.boolean(), maximumSilentSeconds: z.number().int().positive() }).strict(),
    responseSchema: z.string().url(), observedVersions: observedVersionsSchema,
    lease: z.object({ leaseId: ref, expiresAt: timestamp, heartbeatSeconds: z.number().int().positive() }).strict(),
    issuedBy: z.enum(["nosh", "mission_director", "research_director"]), issuedAt: timestamp,
  })
  .strict();

export const generalWorkerCompletionSchema = z.object({
  $schema: z.literal(schemaUri("general-worker-completion")), schemaVersion, taskOutcome: outcome,
  workPerformed: z.array(z.object({ action: ref, subject: text, artifactIds: z.array(id("art")) }).strict()),
  codeChanges: z.object({ startingCommit: z.string(), endingCommit: z.string(), changedPaths: list, diffArtifactId: id("art").nullable(), branch: z.string() }).strict(),
  commands: z.array(z.object({ commandId: ref, displayCommand: z.string().max(2000), exitCode: z.number().int().nullable(), resultArtifactId: id("art").nullable() }).strict()),
  criteria: z.array(z.object({ criterionId: ref, workerClaim: z.enum(["satisfied", "unsatisfied", "inconclusive", "not_applicable"]), validatorRunIds: z.array(ref), artifactIds: z.array(id("art")), notes: z.string().max(2000) }).strict()),
  scientificImpact: z.object({ claimIds: z.array(id("clm")), evidenceIds: z.array(id("evd")), interpretation: text }).strict(),
  deviations: list, newRisks: list, unresolvedItems: list,
  suggestedNextActions: z.array(z.object({ actionType: ref, targetId: ref, reason: text, priority: z.enum(["low", "normal", "high", "critical"]) }).strict()),
  readyForDeterministicPostflight: z.boolean(), readyForReview: z.boolean(),
}).strict();

export const librarianCompletionSchema = z.object({
  $schema: z.literal(schemaUri("librarian-completion")), schemaVersion, taskOutcome: outcome, researchQuestion: text,
  searchCoverage: z.object({ databases: list, queries: list, dateRange: z.object({ from: z.string().date().nullable(), to: z.string().date().nullable() }).strict(), language: list, inclusionCriteria: list, exclusionCriteria: list }).strict(),
  sources: z.array(z.object({ sourceId: ref, sourceType: ref, title: text, authors: list, publicationDate: z.string().date().nullable(), canonicalUrl: z.string().url(), persistentId: z.string().max(500).nullable(), version: z.string().max(200), primarySource: z.boolean(), accessedAt: timestamp, artifactId: id("art"), relevance: text }).strict()),
  findings: z.array(z.object({ findingId: ref, statement: text, support: z.array(z.object({ sourceId: ref, locator: text, artifactId: id("art") }).strict()).min(1), confidence: z.enum(["low", "medium", "high"]), noveltyImplication: z.string().max(2000) }).strict()),
  contradictions: z.array(jsonValueSchema), evaluationDifferences: z.array(jsonValueSchema), knowledgeGaps: list, candidateClaimEffects: z.array(jsonValueSchema),
  bibliographyArtifactId: id("art"), reportArtifactId: id("art"), readyForReview: z.boolean(),
}).strict();

export const blockerSchema = z.object({
  $schema: z.literal(schemaUri("blocker")), schemaVersion, blockerId: id("blk"), taskId: id("tsk"), scopeType: ref, scopeId: ref,
  category: z.enum(["missing_input", "missing_credential", "approval_required", "resource_unavailable", "external_service", "contract_conflict", "scientific_ambiguity", "safety_policy", "workspace_conflict", "unknown_after_diagnosis"]),
  summary: text, evidenceArtifactIds: z.array(id("art")), workCompletedBeforeBlock: list,
  safeState: z.object({ branch: z.string(), head: z.string(), activeJobIds: z.array(id("job")), uncommittedChanges: z.boolean() }).strict(),
  attemptedResolutions: z.array(z.object({ action: text, result: text, attemptFingerprint: sha256Digest }).strict()),
  requiredAuthority: z.enum(["user", "mission_director", "research_director", "daemon_policy"]), requestedAction: text,
  resumePredicate: z.object({ validatorId: ref, parameters: jsonValueSchema }).strict(), fallbackOptions: z.array(jsonValueSchema), reportedAt: timestamp,
}).strict();

export const taskFailureSchema = z.object({
  $schema: z.literal(schemaUri("task-failure")), schemaVersion, taskId: id("tsk"), attempt: z.number().int().positive(), failureCode: ref, phase: ref,
  summary: text, diagnosticArtifactIds: z.array(id("art")), failedValidatorIds: z.array(ref),
  workState: z.object({ branch: z.string(), startingCommit: z.string(), endingCommit: z.string(), uncommittedChanges: z.boolean(), activeJobIds: z.array(id("job")) }).strict(),
  attemptedResolutionFingerprints: z.array(sha256Digest), retriable: z.boolean(), sameApproachAllowed: z.boolean(), changedPremiseRequired: z.string().max(2000).nullable(),
  recommendedDisposition: z.enum(["retry_transient", "revise_same_node", "redesign", "reduce_scope", "record_negative_result", "stop"]), artifactIdsWorthRetaining: z.array(id("art")), reportedAt: timestamp,
}).strict().superRefine((value, context) => {
  if (!value.sameApproachAllowed && !value.changedPremiseRequired) context.addIssue({ code: "custom", path: ["changedPremiseRequired"], message: "a changed premise is required when the same approach is forbidden" });
});

export const delegationRequestSchema = z.object({
  $schema: z.literal(schemaUri("delegation-request")), schemaVersion, requestId: ref, requestingTaskId: id("tsk"), requestingAgentId: id("agt"),
  requestedRole: z.enum(["librarian_researcher", "general_worker", "reviewer"]), proposedObjective: text, reason: text, proposedInputs: z.array(ref), proposedOutputs: z.array(ref), scopeBoundaries: list,
  budgetEstimate: z.object({ modelTokens: z.number().int().positive(), wallClockSeconds: z.number().int().positive() }).strict(), dependencyEffect: text, submittedAt: timestamp,
}).strict();

const reviewTypeSchema = z.enum(["task", "experiment", "experiment_round", "autoresearch_closure", "direction_closure", "claim", "paper_section", "mission_completion"]);
const reviewTargetSchema = z.object({ targetType: ref, targetId: ref, targetVersion: z.number().int().positive() }).strict();

export const reviewRequestSchema = z.object({
  $schema: z.literal(schemaUri("review-request")), schemaVersion, reviewRequestId: ref, reviewId: id("rev"),
  reviewType: reviewTypeSchema,
  target: reviewTargetSchema, scope,
  producerAgentIds: z.array(id("agt")).min(1), reviewerAgentId: id("agt"), independenceCheck: z.literal("pass"),
  contractRefs: z.array(z.object({ kind: ref, id: ref, hash: sha256Digest }).strict()), requiredArtifactIds: z.array(id("art")), requiredEvidenceIds: z.array(id("evd")),
  deterministicPostflight: z.object({ status: z.enum(["pass", "fail"]), validatorRunIds: z.array(ref) }).strict(),
  criteria: z.array(z.object({ criterionId: ref, statement: text, required: z.boolean(), severityIfFailed: z.enum(["blocking", "major", "minor", "observation"]) }).strict()).min(1),
  allowedVerdicts: z.array(z.enum(["PASS", "REVISE", "REDESIGN", "BLOCKED"])).min(1), issuedAt: timestamp,
}).strict().superRefine((value, context) => {
  if (value.producerAgentIds.includes(value.reviewerAgentId)) context.addIssue({ code: "custom", path: ["reviewerAgentId"], message: "reviewer must be independent from every producer" });
});

const evidenceRef = z.object({ refType: ref, refId: ref, locator: z.string().min(1).max(1000) }).strict();
export const reviewVerdictSchema = z.object({
  $schema: z.literal(schemaUri("review-verdict")), schemaVersion, templateVersion, reviewId: id("rev"), reviewRequestId: ref,
  reviewType: reviewTypeSchema, target: reviewTargetSchema, reviewerAgentId: id("agt"), independenceCheck: z.enum(["pass", "fail"]), verdict: z.enum(["PASS", "REVISE", "REDESIGN", "BLOCKED"]), summary: text,
  criteria: z.array(z.object({ criterionId: ref, required: z.boolean().optional(), status: z.enum(["PASS", "FAIL", "INCONCLUSIVE", "NOT_APPLICABLE"]), finding: text, evidenceRefs: z.array(evidenceRef), confidence: z.enum(["low", "medium", "high"]), defectIds: z.array(ref) }).strict()).min(1),
  defects: z.array(z.object({ defectId: ref, severity: z.enum(["blocking", "major", "minor", "observation"]), category: ref, title: text, description: text, criterionIds: z.array(ref), evidenceRefs: z.array(evidenceRef), requiredRemediation: text, acceptanceTest: z.object({ validatorId: ref, expected: z.string().max(500) }).strict(), suggestedOwnerRole: z.enum(["librarian_researcher", "general_worker", "reviewer", "director"]), blocking: z.boolean() }).strict()),
  missingRequiredInputs: z.array(ref), scientificIntegrityFlags: list, recommendedGraphAction: ref, recommendedPromotion: z.enum(["promote", "hold", "reject", "not_applicable"]),
  reviewedArtifactIds: z.array(id("art")), reviewedEvidenceIds: z.array(id("evd")), submittedAt: timestamp,
}).strict().superRefine((value, context) => {
  if (value.verdict !== "PASS") return;
  if (value.independenceCheck !== "pass") context.addIssue({ code: "custom", path: ["verdict"], message: "PASS requires an independent reviewer" });
  if (value.missingRequiredInputs.length) context.addIssue({ code: "custom", path: ["missingRequiredInputs"], message: "PASS cannot omit required inputs" });
  if (value.criteria.some((criterion) => criterion.required !== false && (criterion.status === "FAIL" || criterion.status === "INCONCLUSIVE"))) context.addIssue({ code: "custom", path: ["criteria"], message: "PASS requires every required criterion to pass" });
  if (value.defects.some((defect) => defect.blocking || defect.severity === "blocking" || defect.severity === "major")) context.addIssue({ code: "custom", path: ["defects"], message: "PASS cannot contain an open blocking or major defect" });
});

export const handoffSchema = z.object({
  $schema: z.literal(schemaUri("handoff")), schemaVersion, templateVersion, handoffId: id("hnd"), logicalOwnerId: ref, fromAgentId: id("agt"), reason: ref, scope,
  goalStack: z.object({ projectGoalId: ref, missionCriterionIds: z.array(ref), directionQuestionId: ref.nullable(), currentGraphNodeId: ref.nullable() }).strict(), observedVersions: observedVersionsSchema,
  completedNodeIds: z.array(ref), acceptedArtifactIds: z.array(id("art")), acceptedEvidenceIds: z.array(id("evd")),
  branch: z.object({ name: z.string(), head: z.string(), uncommittedChanges: z.boolean() }).strict(), activeJobs: z.array(jsonValueSchema), openDefects: z.array(jsonValueSchema), openBlockerIds: z.array(id("blk")), failedHypothesisIds: z.array(ref), decisions: z.array(jsonValueSchema), readyNodeIds: z.array(ref),
  recommendedNextAction: z.object({ nodeId: ref, reason: text, requiredInputIds: z.array(ref) }).strict().nullable(), knownRisks: list, contextArtifactIds: z.array(id("art")), oldLeaseReleaseId: ref, createdAt: timestamp,
}).strict();

export const handoffTeachbackSchema = z.object({
  $schema: z.literal(schemaUri("handoff-teachback")), schemaVersion, handoffId: id("hnd"), toAgentId: id("agt"), logicalOwnerId: ref, decision: z.enum(["accepted", "rejected_conflict", "clarification_required"]),
  understoodGoalStack: handoffSchema.shape.goalStack, observedVersions: observedVersionsSchema, observedBranchHead: z.string(), acknowledgedDefectIds: z.array(ref), acknowledgedBlockerIds: z.array(id("blk")), selectedNextNodeId: ref.nullable(), plannedFirstAction: text, conflicts: list, submittedAt: timestamp,
}).strict();

const decision = z.object({ action: ref, targetId: ref, reason: text }).strict();
const nextWake = z.object({ type: z.enum(["event", "deadline", "event_or_deadline"]), deadline: timestamp.nullable() }).strict();
export const missionDirectorCycleSchema = z.object({
  $schema: z.literal(schemaUri("mission-director-cycle")), schemaVersion, cycleId: ref, missionId: id("mis"), directorAgentId: id("agt"), observedObjectiveVersion: z.number().int().positive(), observedGraphVersion: z.number().int().positive(), consumedEventRange: versionRange,
  northStarCheck: z.object({ missionObjective: text, unsatisfiedCriterionIds: z.array(ref), criticalPathNodeIds: z.array(ref), currentWorkContributes: z.boolean() }).strict(),
  portfolioCheck: z.object({ readyNodeIds: z.array(ref), starvedNodeIds: z.array(ref), overfocusedNodeId: ref.nullable(), fairnessAction: ref }).strict(),
  decisions: z.array(decision), createdTaskIds: z.array(id("tsk")), invokedDirectionIds: z.array(id("dir")), invokedAutoresearchIds: z.array(id("ar")), graphChangeProposalIds: z.array(ref), userInputRequestId: ref.nullable(), budgetWarnings: list, blockerIds: z.array(id("blk")), nextWake, submittedAt: timestamp,
}).strict();

export const researchDirectorCycleSchema = z.object({
  $schema: z.literal(schemaUri("research-director-cycle")), schemaVersion, cycleId: ref, directionId: id("dir"), directorAgentId: id("agt"), observedGraphVersion: z.number().int().positive(), evaluationContractHash: sha256Digest, consumedEventRange: versionRange,
  questionCheck: z.object({ questionId: ref, currentDisposition: z.enum(["unresolved", "supported", "refuted", "inconclusive"]), remainingUncertaintyIds: z.array(ref), currentWorkContributes: z.boolean() }).strict(),
  acceptedFrontierExperimentIds: z.array(ref), failedHypothesisIdsAdded: z.array(ref), decisions: z.array(decision), createdTaskIds: z.array(id("tsk")), invokedAutoresearchIds: z.array(id("ar")), graphChangeProposalIds: z.array(ref), directionSuggestionIds: z.array(ref), userInputRequestId: ref.nullable(), nextWake, submittedAt: timestamp,
}).strict();

export const graphChangeProposalSchema = z.object({
  $schema: z.literal(schemaUri("graph-change-proposal")), schemaVersion, proposalId: ref, scopeType: z.enum(["mission", "direction"]), scopeId: z.union([id("mis"), id("dir")]), baseGraphVersion: z.number().int().positive(), proposerRole: z.enum(["mission_director", "research_director", "user"]), reasonCode: ref, rationale: text, evidenceIds: z.array(id("evd")), operations: z.array(jsonValueSchema).min(1), expectedEffect: text, contractImpact: z.enum(["none", "non_material", "material"]), budgetImpact: z.object({ gpuSecondsDelta: z.number().int(), modelTokensDelta: z.number().int() }).strict(), approvalRequired: z.boolean(), submittedAt: timestamp,
}).strict();

export const submissionRegistry = {
  [schemaUri("task-packet")]: taskPacketSchema,
  [schemaUri("general-worker-completion")]: generalWorkerCompletionSchema,
  [schemaUri("librarian-completion")]: librarianCompletionSchema,
  [schemaUri("blocker")]: blockerSchema,
  [schemaUri("task-failure")]: taskFailureSchema,
  [schemaUri("delegation-request")]: delegationRequestSchema,
  [schemaUri("review-request")]: reviewRequestSchema,
  [schemaUri("review-verdict")]: reviewVerdictSchema,
  [schemaUri("handoff")]: handoffSchema,
  [schemaUri("handoff-teachback")]: handoffTeachbackSchema,
  [schemaUri("mission-director-cycle")]: missionDirectorCycleSchema,
  [schemaUri("research-director-cycle")]: researchDirectorCycleSchema,
  [schemaUri("graph-change-proposal")]: graphChangeProposalSchema,
} as const;
