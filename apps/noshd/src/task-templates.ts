/**
 * Schema-valid completion templates for worker Task Packets. As with director briefs, the daemon
 * fills identity, commits, branch, criteria, and validator/Job references; the worker edits only
 * judgment fields. Prose descriptions of strict schemas were paraphrased by models in live runs.
 */
import { episodeTypeForRole, schemaDocumentPath, schemaUri, type JsonValue } from "@nosh/wire";

export type DaemonEvaluation = { jobId: string; state: string; exitCode: number | null; displayCommand: string; metrics: JsonValue; metricArtifactId: string | null; failure: string | null };
type PacketFacts = { acceptanceCriteria: Array<{ criterionId: string; validatorIds: string[]; required: boolean }>; workspace: { startingCommit: string; branch: string } };

export function generalWorkerCompletionTemplate(packet: PacketFacts, evaluation: DaemonEvaluation | null): JsonValue {
  const evidenceArtifacts = evaluation?.metricArtifactId ? [evaluation.metricArtifactId] : [];
  return {
    $schema: schemaUri("general-worker-completion"), schemaVersion: 1, taskOutcome: "completed",
    workPerformed: [{ action: "work_audit", subject: "EDIT: what you inspected or changed, and what you found.", artifactIds: evidenceArtifacts }],
    codeChanges: { startingCommit: packet.workspace.startingCommit, endingCommit: packet.workspace.startingCommit, changedPaths: [], diffArtifactId: null, branch: packet.workspace.branch },
    commands: evaluation ? [{ commandId: "command_evaluation", displayCommand: evaluation.displayCommand.slice(0, 2000), exitCode: evaluation.exitCode, resultArtifactId: evaluation.metricArtifactId }] : [],
    criteria: packet.acceptanceCriteria.map((criterion) => ({ criterionId: criterion.criterionId, workerClaim: "satisfied", validatorRunIds: [...criterion.validatorIds, ...(evaluation ? [evaluation.jobId] : [])], artifactIds: evidenceArtifacts, notes: "EDIT: why the criterion is or is not met, citing the evidence above." })),
    scientificImpact: { claimIds: [], evidenceIds: [], interpretation: "EDIT: what this result means for the research question." },
    deviations: [], newRisks: [], unresolvedItems: [], suggestedNextActions: [],
    readyForDeterministicPostflight: true, readyForReview: true,
  };
}

/** Episode draft for a runtime THREAD_STEP; field names follow episode-draft/v1 exactly. */
function episodeDraftTemplate(role: string, exampleRefs: string[] = []): JsonValue {
  return { $schema: schemaUri("episode-draft"), schemaVersion: 1, episodeType: episodeTypeForRole(role), summary: "EDIT: one-paragraph summary of what you did and found.", facts: [{ statement: "EDIT: a verified fact.", evidenceRefs: exampleRefs, confidence: "high" }], decisions: [{ statement: "EDIT: a decision you made.", rationale: "EDIT: why.", evidenceRefs: [] }], artifactIds: [], evidenceIds: [], changedFiles: [], unresolvedQuestions: [], recommendedNextActions: [] };
}

/** A complete terminal envelope (record + episode) for any role; models copy structure instead of writing it. */
export function terminalEnvelope(record: JsonValue, role: string): string {
  return JSON.stringify({ $schema: schemaUri("terminal-output"), schemaVersion: 1, records: [record, episodeDraftTemplate(role)] });
}

/**
 * The worker's whole final answer, prebuilt. Models hand-writing long nested JSON produced
 * unbalanced brackets and invented episode fields; copying a valid envelope avoids both.
 */
export function completionInstructions(template: JsonValue, role = "general_worker"): string {
  const artifacts = ((template as { criteria?: Array<{ artifactIds?: string[] }> }).criteria ?? []).flatMap((criterion) => criterion.artifactIds ?? []);
  const envelope = { $schema: schemaUri("terminal-output"), schemaVersion: 1, records: [template, episodeDraftTemplate(role, artifacts.slice(0, 1))] };
  return [
    `Your final answer must be exactly this JSON envelope (completion schema: ${schemaDocumentPath("general-worker-completion")}; episode schema: ${schemaDocumentPath("episode-draft")}). Copy it, keep every key and bracket, and replace each "EDIT: ..." string.`,
    "Judgment you may change in the completion: taskOutcome (partial/blocked/failed when the work is not done), criteria[].workerClaim (unsatisfied/inconclusive when the evidence does not support the criterion; deferred_to_review with empty validatorRunIds only for a criterion that only the independent reviewer can establish, such as one requiring an independent Review), notes, and the plain-string arrays newRisks, unresolvedItems, and deviations. Keep suggestedNextActions empty: the director plans next actions. In the episode you may add facts/decisions objects of the same shape (their evidenceRefs are plain ID strings such as 'art_...', never objects) and plain-string unresolvedQuestions; leave artifactIds, evidenceIds, changedFiles, and recommendedNextActions as given.",
    "criteria[].validatorRunIds names what establishes each criterion: validator_task.postflight is the daemon's own Git/scope postflight (keep it for criteria it checks, such as no protected or out-of-scope changes), and job_... IDs are your passing nosh_run runs on your final content (a run made after your last edit; only the paper write-up, docs/paper.md and docs/paper.bib, may be committed after the runs it reports, so run a held-out evaluation once and then write it up). Never leave a satisfied criterion without one. Never hand-type a value that code computes (a fitted parameter, a metric, a threshold): produce it with code you run through nosh_run, or add a test that asserts the stored value equals the computation; if you cannot, report it in unresolvedItems and do not claim the criterion. A test counts only if it exercises the code under change (imports it or runs it); a test that re-implements the logic it checks proves nothing. Keep commands exactly as given: the daemon records every nosh_run itself, and you cite runs only through validatorRunIds.",
    "Do not commit and leave codeChanges as given: the daemon commits all your worktree edits after this answer and fills codeChanges and the episode's changedFiles. To cite a file from your worktree as evidence, put \"artifact:<relative/path>\" in an artifactIds list; the daemon snapshots it and substitutes its art_ ID. Leave every other ID you did not receive from the host unchanged.",
    JSON.stringify(envelope),
  ].join("\n");
}

/** Deliberately unresolvable until replaced with a real worktree path; the daemon snapshots each "artifact:<path>" citation. */
export const REGISTER_ARTIFACT = "artifact:EDIT/relative/path";

/** Librarian completion: every cited source, the report, and the bibliography are cited by worktree path; the daemon snapshots them as Artifacts. */
export function librarianInstructions(taskId: string, networkAllowed: boolean): string {
  const folder = `research/${taskId}`;
  const record = {
    $schema: schemaUri("librarian-completion"), schemaVersion: 1, taskOutcome: "completed", researchQuestion: "EDIT: the question you researched.",
    searchCoverage: { databases: [networkAllowed ? "EDIT: database you searched" : "local_repository"], queries: ["EDIT: a query or file pattern you searched"], dateRange: { from: null, to: null }, language: ["en"], inclusionCriteria: ["EDIT: an inclusion rule"], exclusionCriteria: [] },
    sources: [{ sourceId: "source_1", sourceType: "source_repository.file", title: "EDIT: source title", authors: [], publicationDate: null, canonicalUrl: "file:///EDIT/relative/path", persistentId: null, version: "EDIT: commit or version", primarySource: true, accessedAt: new Date().toISOString(), artifactId: REGISTER_ARTIFACT, relevance: "EDIT: why this source matters" }],
    findings: [{ findingId: "finding_1", statement: "EDIT: one finding", support: [{ sourceId: "source_1", locator: "EDIT: line, section, or key", artifactId: REGISTER_ARTIFACT }], confidence: "medium", noveltyImplication: "" }],
    contradictions: [], evaluationDifferences: [], knowledgeGaps: [], candidateClaimEffects: [],
    bibliographyArtifactId: `artifact:${folder}/bibliography.bib`, reportArtifactId: `artifact:${folder}/report.md`, readyForReview: true,
  };
  return [
    `Steps: (1) ${networkAllowed ? "read allowlisted literature APIs with nosh_network_read and the local repository" : "network is disabled: research only the local repository"}; (2) write your report to ${folder}/report.md and a BibTeX bibliography to ${folder}/bibliography.bib with nosh_workspace_write; (3) answer with the envelope below. Cite each file you rely on as "artifact:<relative/path>" (no registration step: the daemon snapshots every cited file after your answer and substitutes its art_ ID).`,
    `Your final answer must be exactly this JSON envelope (schema: ${schemaDocumentPath("librarian-completion")}). Copy it, keep every key and bracket, replace each "EDIT: ..." and every "${REGISTER_ARTIFACT}" with real values (a source's artifactId and its findings' support artifactId are the same "artifact:<path>"), and add sources/findings objects of the same shape (sourceId/findingId are refs like source_2, finding_2). knowledgeGaps is a list of plain strings; leave contradictions, evaluationDifferences, and candidateClaimEffects empty unless you have them. Record 2 is your episode: keep exactly its keys, add facts/decisions objects of the same shape (evidenceRefs are plain ID strings such as 'art_...'), put your "artifact:<path>" citations in artifactIds, and leave recommendedNextActions empty (the director plans next actions).`,
    terminalEnvelope(record, "librarian_researcher"),
  ].join("\n");
}

type ReviewFacts = { reviewId: string; reviewRequestId: string; reviewType: string; target: { targetType: string; targetId: string; targetVersion: number }; reviewerAgentId: string; requiredArtifactIds: string[]; requiredEvidenceIds: string[]; criteria: Array<{ criterionId: string }> };
export const UNDECIDED_VERDICT = "DECIDE: PASS | REVISE | REDESIGN | BLOCKED";
export const UNDECIDED_STATUS = "DECIDE: PASS | FAIL | INCONCLUSIVE | NOT_APPLICABLE";

/**
 * Independent review answer, prebuilt from the daemon's Review Request. The verdict and each
 * criterion status are deliberately undecided placeholders: a default would bias the reviewer.
 */
export function reviewerInstructions(request: ReviewFacts): string {
  const verdict = { $schema: schemaUri("review-verdict"), schemaVersion: 1, templateVersion: "1.0.0", reviewId: request.reviewId, reviewRequestId: request.reviewRequestId, reviewType: request.reviewType, target: request.target, reviewerAgentId: request.reviewerAgentId, independenceCheck: "pass", verdict: UNDECIDED_VERDICT, summary: "EDIT: your overall judgment and why.", criteria: request.criteria.map((criterion) => ({ criterionId: criterion.criterionId, required: true, status: UNDECIDED_STATUS, finding: "EDIT: what the evidence shows for this criterion.", evidenceRefs: [], confidence: "medium", defectIds: [] })), defects: [], missingRequiredInputs: [], scientificIntegrityFlags: [], recommendedGraphAction: "graph_none", recommendedPromotion: "not_applicable", reviewedArtifactIds: request.requiredArtifactIds, reviewedEvidenceIds: request.requiredEvidenceIds, submittedAt: new Date().toISOString() };
  const envelope = { $schema: schemaUri("terminal-output"), schemaVersion: 1, records: [verdict, episodeDraftTemplate("reviewer", request.requiredArtifactIds.slice(0, 1))] };
  return [
    `Submit exactly one ${schemaUri("review-verdict")} with the exact Review Request identities, as this JSON envelope (schemas: ${schemaDocumentPath("review-verdict")}, ${schemaDocumentPath("episode-draft")}). Keep every key and bracket.`,
    `You must decide verdict (${UNDECIDED_VERDICT.slice(8)}) and every criteria[].status (${UNDECIDED_STATUS.slice(8)}) from the evidence alone, replace each "EDIT: ..." string, and set confidence (low/medium/high). PASS requires every required criterion PASS and no blocking or major defect. A computed value (a fitted parameter, a metric, a selected threshold) counts as established only when a daemon run output or a test that pins that exact value shows it; a number merely present in a file or a report is unverified: compare it against the code and the run outputs, and if they disagree or nothing pins it, mark the criterion FAIL or INCONCLUSIVE with a defect. A test that re-implements the logic it claims to check, instead of importing or running the changed code, pins nothing. A criterion the worker marked deferred_to_review is yours to decide: PASS it only if the evidence you can verify (records, Job results, artifacts) establishes it; when it asks for an independent Review, this Review is that Review. Read every required Artifact with nosh_artifact_read before deciding. A declared Project-contract command (see each Job's description) is the approved way to run what its description names. For each problem add a defects entry {defectId (ref, e.g. defect_metric-mismatch), severity (blocking/major/minor/observation), category (ref such as category_missing-evidence), title, description, criterionIds, evidenceRefs: [], requiredRemediation, acceptanceTest {validatorId (ref), expected}, suggestedOwnerRole (general_worker/librarian_researcher/reviewer/director), blocking} and list its defectId in the criterion. Every ref (defectId, category, validatorId, refType) is a lowercase prefix, exactly ONE underscore, then hyphens or dots: defect_missing-guardrail-evidence, validator_experiment.postflight, source_evidence (never a second underscore). missingRequiredInputs lists only exact art_/evd_ IDs you were required to see but could not, never prose. In the verdict, evidenceRefs entries are objects {refType, refId, locator}. In the episode (record 2) evidenceRefs are plain ID strings such as "art_..." (never objects). Fill the episode with your verified facts and decisions; leave its recommendedNextActions empty.`,
    JSON.stringify(envelope),
  ].join("\n");
}
