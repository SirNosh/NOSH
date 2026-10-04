import { spawnSync } from "node:child_process";
import { EventEmitter, once } from "node:events";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createId } from "@nosh/core";
import { JobSupervisor } from "@nosh/jobs";
import { EventStore, type RegisteredProject } from "@nosh/persistence";
import type { AgentInspection, PiAdapter, PiSessionOptions } from "@nosh/pi-adapter";
import { isTaskTerminalRecord, schemaUri, sha256, validateRecord, type EventDraft, type JsonValue } from "@nosh/wire";
import { describe, expect, it, vi } from "vitest";
import { AutoresearchSupervisor } from "./autoresearch-supervisor.js";
import { ResearchControl } from "./research-control.js";

class ExperimentAgents {
  private readonly sessions = new Map<string, PiSessionOptions>();
  readonly reviewedEvidenceCandidates: Record<string, JsonValue>[] = [];
  constructor(private readonly append: (draft: EventDraft) => void, private readonly transformReview: (record: Record<string, JsonValue>) => Record<string, JsonValue> = (record) => record) {}
  inspect(): AgentInspection[] { return [...this.sessions.values()].map((session) => ({ ...session, piSessionId: `pi_${session.agentId}`, status: "running", currentTool: null, activeToolIds: [], startedAt: new Date().toISOString(), lastEventAt: new Date().toISOString(), modelProvider: session.model?.provider ?? null, modelId: session.model?.id ?? null, modelName: null, thinkingLevel: session.thinkingLevel ?? "medium", contextTokens: null, contextWindow: null, contextPercent: null })); }
  async start(options: PiSessionOptions): Promise<AgentInspection> { this.sessions.set(options.agentId, options); return this.inspect().find((agent) => agent.agentId === options.agentId)!; }
  async prompt(agentId: string, prompt: string): Promise<void> {
    const session = this.sessions.get(agentId)!; const now = new Date().toISOString();
    if (prompt.includes(schemaUri("experiment-proposal"))) { const experimentId = required(prompt, /(?:experimentId=|"experimentId":")(exp_[0-9a-f]{32})/); const autoresearchId = required(prompt, /(?:autoresearchId=|"autoresearchId":")(ar_[0-9a-f]{32})/); const parentExperimentId = required(prompt, /(?:parentExperimentId=|"parentExperimentId":")(exp_[0-9a-f]{32})/); const round = Number(required(prompt, /(?:round=|"round":)(\d+)/)); const evaluationContractHash = required(prompt, /(?:evaluationContractHash=|"evaluationContractHash":")(sha256:[0-9a-f]{64})/); this.submit(session, { $schema: schemaUri("experiment-proposal"), schemaVersion: 1, proposalId: `proposal_${experimentId.slice(4)}`, autoresearchId, experimentId, parentExperimentId, round, hypothesis: `Variant ${experimentId.slice(4, 10)} improves the primary metric`, rationale: "Exercise the frozen evaluation contract", primaryChange: { category: "change_variant", description: "Write the bounded variant input", expectedFiles: ["variant.json"] }, expectedEffect: { score: "measurable" }, guardrailRisks: ["Fixture only"], novelty: { ideaFingerprint: sha256({ experimentId }), nearestAttemptFingerprint: null, difference: "Distinct fixture ordinal", duplicateStatus: "distinct" }, evaluationContractHash, estimatedGpuSeconds: 0, proposedByAgentId: agentId, submittedAt: now }); return; }
    const packetText = prompt.split("before work: ")[1]?.split(". Then implement")[0]; if (packetText) { const packet = JSON.parse(packetText) as { taskId: string; attempt: number; assignedAgentId: string; requiredOutputs: Array<{ outputId: string }>; acceptanceCriteria: Array<{ criterionId: string }>; workspace: { startingCommit: string; branch: string }; lease: { leaseId: string } }; this.submit(session, { $schema: schemaUri("task-acknowledgement"), schemaVersion: 1, taskId: packet.taskId, attempt: packet.attempt, agentId, decision: "accepted", understoodObjective: "Implement one bounded variant", understoodOutputIds: packet.requiredOutputs.map((output) => output.outputId), understoodCriterionIds: packet.acceptanceCriteria.map((criterion) => criterion.criterionId), observedLeaseId: packet.lease.leaseId, observedStartingCommit: packet.workspace.startingCommit, conflicts: [], clarificationRequest: null, submittedAt: now }); const ordinal = Number(required(prompt, /variantOrdinal=(\d+)/)); const score = [2, 3, 5, 1][ordinal - 1]!; writeFileSync(join(session.cwd, "variant.json"), `${JSON.stringify({ score })}\n`); git(session.cwd, ["add", "--", "variant.json"]); git(session.cwd, ["commit", "-m", `experiment variant ${ordinal}`]); const endingCommit = git(session.cwd, ["rev-parse", "HEAD"]); const validator = `validator_${packet.taskId.slice(4)}`; this.submit(session, { $schema: schemaUri("general-worker-completion"), schemaVersion: 1, taskOutcome: "completed", workPerformed: [{ action: "action_implementation", subject: "Bounded variant", artifactIds: [] }], codeChanges: { startingCommit: packet.workspace.startingCommit, endingCommit, changedPaths: ["variant.json"], diffArtifactId: null, branch: packet.workspace.branch }, commands: [{ commandId: validator, displayCommand: "fixture validation", exitCode: 0, resultArtifactId: null }], criteria: [{ criterionId: packet.acceptanceCriteria[0]!.criterionId, workerClaim: "satisfied", validatorRunIds: [validator], artifactIds: [], notes: "Fixture passed" }], scientificImpact: { claimIds: [], evidenceIds: [], interpretation: "Fixture variant" }, deviations: [], newRisks: [], unresolvedItems: [], suggestedNextActions: [], readyForDeterministicPostflight: true, readyForReview: true }); return; }
    const requestText = prompt.split("Review Request: ")[1]?.split(". Submit")[0]; if (requestText) { const request = JSON.parse(requestText) as { reviewId: string; reviewRequestId: string; reviewType: string; target: JsonValue; criteria: Array<{ criterionId: string; required: boolean }>; requiredArtifactIds: string[]; requiredEvidenceIds: string[] };
      if (request.reviewType === "experiment") {
        const candidateText = prompt.split("Review this candidate Evidence for publication with your exact disposition: ")[1]?.split(" Review Request: ")[0];
        expect(candidateText).toBeDefined();
        const candidate = JSON.parse(candidateText!) as Record<string, JsonValue>;
        expect(request.requiredEvidenceIds).toEqual([candidate.evidenceId]);
        expect(validateRecord(schemaUri("evidence"), candidate).ok).toBe(true);
        this.reviewedEvidenceCandidates.push(candidate);
      }
      this.submit(session, this.transformReview({ $schema: schemaUri("review-verdict"), schemaVersion: 1, templateVersion: "1.0.0", reviewId: request.reviewId, reviewRequestId: request.reviewRequestId, reviewType: request.reviewType, target: request.target, reviewerAgentId: agentId, independenceCheck: "pass", verdict: "PASS", summary: "Independent fixture Review passed", criteria: request.criteria.map((criterion) => ({ criterionId: criterion.criterionId, required: criterion.required, status: "PASS", finding: "Frozen inputs and comparison passed", evidenceRefs: [], confidence: "high", defectIds: [] })), defects: [], missingRequiredInputs: [], scientificIntegrityFlags: [], recommendedGraphAction: "action_accept", recommendedPromotion: request.reviewType === "autoresearch_closure" ? "not_applicable" : "promote", reviewedArtifactIds: request.requiredArtifactIds, reviewedEvidenceIds: request.requiredEvidenceIds, submittedAt: now })); return; }
    throw new Error("Unexpected fixture prompt");
  }
  stop(agentId: string): void { this.sessions.delete(agentId); }
  private submit(session: PiSessionOptions, record: JsonValue): void { this.append({ $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "record.submitted", source: "fixture", scope: { projectId: session.projectId, missionId: session.missionId, directionId: session.directionId, autoresearchId: session.autoresearchId, experimentId: session.experimentId, runId: session.runId, jobId: session.jobId, agentId: session.agentId }, correlationId: `task:${session.taskId}`, causationId: null, payload: record }); }
}

describe("AutoresearchSupervisor", () => {
  it("recovers a two-round frozen Direction experiment lineage with Jobs, Evidence, and reviewed promotion", async () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-autoresearch-")); const repositoryRoot = join(directory, "repository"); mkdirSync(repositoryRoot); initializeRepository(repositoryRoot); const projectId = createId("prj"); const databasePath = join(directory, "project.sqlite"); const project: RegisteredProject = { projectId, repositoryRoot, databasePath, registeredAt: new Date().toISOString() }; let store = new EventStore(databasePath); const research = new ResearchControl(() => store, () => project, () => undefined); const agents = new ExperimentAgents((draft) => { appendFixtureEvent(store, draft); }); const jobs = new JobSupervisor(join(directory, "jobs"), (job) => { store.append({ $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "job.state_changed", source: "jobs", scope: { projectId, missionId: job.missionId, directionId: job.directionId, autoresearchId: job.autoresearchId, experimentId: job.experimentId, runId: job.runId, jobId: job.jobId, agentId: null }, correlationId: job.runId, causationId: null, payload: job as unknown as JsonValue }); });
    try {
      const evaluationContract: JsonValue = { primaryMetric: { name: "score", objective: "minimize", minimumEffect: 0.5 }, baselineMetrics: { score: 4 }, roundWidths: [3, 1], execution: { runner: "native", command: [process.execPath, "-e", "const fs=require('fs');const v=JSON.parse(fs.readFileSync('variant.json','utf8'));fs.writeFileSync('metrics.json',JSON.stringify({score:v.score}));"], resultPath: "metrics.json", timeoutSeconds: 30, usesGpu: false } }; let direction = research.createDirection(projectId, { question: "Which bounded variant improves score?", decisionUse: "Select the reviewed winner", evaluationContract, idempotencyKey: "direction-create" }); direction = research.transitionDirection(projectId, direction.entityId, direction.version, "proposed", "direction-proposed"); direction = research.transitionDirection(projectId, direction.entityId, direction.version, "active", "direction-active"); const reviewId = createId("rev"); const baselineTaskId = createId("tsk"); const reviewerId = createId("agt"); const producerId = createId("agt"); const reviewRequestId = `request_${reviewId.slice(4)}`; const baselineNodeId = direction.value.nodes[0]!.id;
      direction = research.transitionDirectionNode(projectId, direction.entityId, direction.version, baselineNodeId, "leased", "baseline-leased", { leaseId: baselineTaskId, ownerId: producerId, version: direction.version, expiresAt: new Date(Date.now() + 60_000).toISOString() });
      for (const state of ["working", "postflight", "reviewing"] as const) direction = research.transitionDirectionNode(projectId, direction.entityId, direction.version, baselineNodeId, state, `baseline-${state}`);
      const target = { targetType: "graph_node", targetId: baselineNodeId, targetVersion: direction.version }; const scope = { projectId, missionId: null, directionId: direction.entityId, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: reviewerId }; research.submitDaemonRecord(projectId, scope, `task:${baselineTaskId}`, { $schema: schemaUri("review-request"), schemaVersion: 1, reviewRequestId, reviewId, reviewType: "task", target, scope: { projectId, missionId: null, directionId: direction.entityId, autoresearchId: null }, producerAgentIds: [producerId], reviewerAgentId: reviewerId, independenceCheck: "pass", contractRefs: [{ kind: "contract_evaluation", id: `contract_${direction.entityId.slice(4)}`, hash: direction.value.evaluationContractHash }], requiredArtifactIds: [], requiredEvidenceIds: [], deterministicPostflight: { status: "pass", validatorRunIds: ["validator_direction.baseline"] }, criteria: [{ criterionId: baselineNodeId, statement: "Validate the immutable baseline", required: true, severityIfFailed: "blocking" }], allowedVerdicts: ["PASS", "REVISE", "REDESIGN", "BLOCKED"], issuedAt: new Date().toISOString() }, `baseline-review-request:${reviewId}`); research.submitDaemonRecord(projectId, scope, `task:${baselineTaskId}`, { $schema: schemaUri("review-verdict"), schemaVersion: 1, templateVersion: "1.0.0", reviewId, reviewRequestId, reviewType: "task", target, reviewerAgentId: reviewerId, independenceCheck: "pass", verdict: "PASS", summary: "Frozen baseline passed", criteria: [{ criterionId: baselineNodeId, required: true, status: "PASS", finding: "The baseline commit and contract resolve", evidenceRefs: [], confidence: "high", defectIds: [] }], defects: [], missingRequiredInputs: [], scientificIntegrityFlags: [], recommendedGraphAction: "action_accept", recommendedPromotion: "not_applicable", reviewedArtifactIds: [], reviewedEvidenceIds: [], submittedAt: new Date().toISOString() }, `baseline-review-verdict:${reviewId}`);
      direction = research.transitionDirectionNode(projectId, direction.entityId, direction.version, baselineNodeId, "accepted", "baseline-node-accepted");
      direction = research.acceptDirectionBaseline(projectId, direction.entityId, direction.version, { commit: git(repositoryRoot, ["rev-parse", "HEAD"]), reviewId, evaluationContractHash: direction.value.evaluationContractHash, idempotencyKey: "baseline-accepted" }); let execution = research.createAutoresearch(projectId, { decisionQuestion: direction.value.question, directionId: direction.entityId, familyTags: ["fixture"], scope: ["variant.json"], maximumExperiments: 4, maximumRounds: 2, idempotencyKey: "autoresearch-create" }); execution = research.transitionAutoresearch(projectId, execution.entityId, execution.version, "running", "autoresearch-running"); let restarted = false;
      for (let cycle = 0; cycle < 80 && research.autoresearchExecution(projectId, execution.entityId).state !== "completed"; cycle += 1) { const supervisor = new AutoresearchSupervisor(research, agents as unknown as PiAdapter, jobs, (spec) => { const intent = store.beginOperation(projectId, "job.launch", `job-launch:${spec.jobId}`, spec as unknown as JsonValue); const job = jobs.start(spec); store.completeOperation(projectId, intent.intentId, job as unknown as JsonValue); return job; }, () => [project], join(directory, "pi-package"), (draft) => { appendFixtureEvent(store, draft); }); await supervisor.tick(); await new Promise((resolve) => setTimeout(resolve, 25)); const current = research.autoresearchExecution(projectId, execution.entityId); if (!restarted && current.value.currentRound >= 1 && !jobs.list().some((job) => ["starting", "running", "checkpointing", "finishing"].includes(job.state))) { store.close(); store = new EventStore(databasePath); restarted = true; } }
      const completed = research.autoresearchExecution(projectId, execution.entityId); const records = research.submitted(projectId).filter((entry) => entry.event.scope.autoresearchId === execution.entityId).map((entry) => entry.record); const errors = store.replay(projectId).filter((entry) => entry.scope.autoresearchId === execution.entityId && entry.type === "autoresearch.supervisor_error").map((entry) => entry.payload); expect({ state: completed.state, errors }).toEqual({ state: "completed", errors: [] }); expect(completed.value.currentRound).toBe(2); expect(completed.value.acceptedFrontierExperimentIds).toHaveLength(1); expect(new Set(records.filter((record) => (record as { $schema?: string }).$schema === schemaUri("experiment-result")).map((record) => (record as { experimentId: string }).experimentId))).toHaveLength(4); const firstRound = records.find((record) => (record as { $schema?: string; round?: number }).$schema === schemaUri("autoresearch-round") && (record as { round?: number }).round === 1) as { promotedExperimentIds: string[] }; const firstWinner = records.find((record) => (record as { $schema?: string; experimentId?: string }).$schema === schemaUri("experiment-result") && (record as { experimentId?: string }).experimentId === firstRound.promotedExperimentIds[0]) as { comparison: { candidateScore: number } }; expect(firstWinner.comparison.candidateScore).toBe(2); expect(records.filter((record) => (record as { $schema?: string }).$schema === schemaUri("autoresearch-round"))).toHaveLength(2); expect(records.filter((record) => (record as { $schema?: string }).$schema === schemaUri("job-result"))).toHaveLength(4); expect(records.some((record) => (record as { $schema?: string; polarity?: string }).$schema === schemaUri("evidence") && (record as { polarity?: string }).polarity === "contradicts")).toBe(true); expect(records.filter((record) => (record as { $schema?: string }).$schema === schemaUri("experiment-manifest"))).toHaveLength(4); expect(store.operationIntents(projectId, "pending")).toEqual([]); expect(restarted).toBe(true);
      expectCompletionAuthority(records, completed);
      expectReviewedEvidenceAuthority(records);
      expect(records.find((record) => (record as { $schema?: string }).$schema === schemaUri("autoresearch-completion-packet"))).toMatchObject({ bestExperimentId: completed.value.acceptedFrontierExperimentIds[0] });
    } finally { jobs.close(); store.close(); rmSync(directory, { recursive: true, force: true }); }
  }, 90_000);

  it("rebuilds model-token use and blocks before issuing work when its budget is exhausted", async () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-autoresearch-budget-")); const repositoryRoot = join(directory, "repository"); mkdirSync(repositoryRoot); const projectId = createId("prj"); const project: RegisteredProject = { projectId, repositoryRoot, databasePath: join(directory, "project.sqlite"), registeredAt: new Date().toISOString() }; const store = new EventStore(project.databasePath); const research = new ResearchControl(() => store, () => project, () => undefined); const agents = new ExperimentAgents((draft) => { appendFixtureEvent(store, draft); }); const jobs = new JobSupervisor(join(directory, "jobs"), () => undefined);
    try { const contract: JsonValue = { primaryMetric: { name: "score", objective: "maximize", minimumEffect: 1 }, baselineMetrics: { score: 0 }, execution: { runner: "native", command: [process.execPath, "-e", "process.exit(0)"], resultPath: "metrics.json", timeoutSeconds: 30 } }; let execution = research.createAutoresearch(projectId, { decisionQuestion: "Honor the recovered token ceiling", evaluationContract: contract, idempotencyKey: "budget-create" }); execution = research.transitionAutoresearch(projectId, execution.entityId, execution.version, "running", "budget-running"); store.append({ $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "agent.completed", source: "fixture", scope: { projectId, missionId: null, directionId: null, autoresearchId: execution.entityId, experimentId: null, runId: null, jobId: null, agentId: createId("agt") }, correlationId: execution.entityId, causationId: null, payload: { modelTokens: execution.value.maximumModelTokens } }); const supervisor = new AutoresearchSupervisor(research, agents as unknown as PiAdapter, jobs, (spec) => jobs.start(spec), () => [project], join(directory, "pi-package"), (draft) => { appendFixtureEvent(store, draft); }); await supervisor.tick(); expect(research.autoresearchExecution(projectId, execution.entityId).state).toBe("blocked"); expect(research.autoresearchBudgetUse(projectId, execution.entityId).modelTokens).toBe(execution.value.maximumModelTokens); expect(research.records(projectId, "experiment-proposal")).toEqual([]); }
    finally { jobs.close(); store.close(); rmSync(directory, { recursive: true, force: true }); }
  });


  it.each([
    ["a failed job", "process.exit(1)"],
    ["an absent result file", "process.exit(0)"],
    ["malformed result JSON", "require('fs').writeFileSync('metrics.json','{')"],
    ["a result without the primary metric", "require('fs').writeFileSync('metrics.json','{}')"],
    ["a non-finite primary metric", "require('fs').writeFileSync('metrics.json','{\"score\":\"NaN\"}')"],
  ])("persists durable negative evidence for %s", async (_label, command) => {
    const run = await runAutoresearchFixture({ primaryMetric: { name: "score", objective: "maximize", minimumEffect: 1 }, baselineMetrics: { score: 0 }, roundWidths: [1], execution: { runner: "native", command: [process.execPath, "-e", command], resultPath: "metrics.json", timeoutSeconds: 30, usesGpu: false } });
    try {
      expect(run.execution.state).toBe("completed");
      expect(run.errors).toEqual([]);
      expect(run.records.find((record) => record.$schema === schemaUri("job-result"))).toMatchObject({ $schema: schemaUri("job-result") });
      expect(run.records.find((record) => record.$schema === schemaUri("experiment-result"))).toMatchObject({ runSetStatus: "invalid", deterministicValidation: "fail", promotionDecision: "rejected" });
      expect(run.records.find((record) => record.$schema === schemaUri("evidence"))).toMatchObject({ evidenceType: "evidence_negative.result", polarity: "contradicts" });
      expect(run.records.find((record) => record.$schema === schemaUri("autoresearch-completion-packet"))).toMatchObject({ terminalReason: "all_hypotheses_rejected", bestExperimentId: null });
      expectCompletionAuthority(run.records, run.execution);
    } finally {
      run.cleanup();
    }
  }, 90_000);

  it("rejects an improving experiment that violates a contract guardrail", async () => {
    const run = await runAutoresearchFixture({ primaryMetric: { name: "score", objective: "minimize", minimumEffect: 1 }, baselineMetrics: { score: 4 }, guardrails: [{ name: "guardrail_pass", direction: "maximize", threshold: 1 }], roundWidths: [1], execution: { runner: "native", command: [process.execPath, "-e", "require('fs').writeFileSync('metrics.json','{\"score\":2,\"guardrail_pass\":false}')"], resultPath: "metrics.json", timeoutSeconds: 30, usesGpu: false } });
    try {
      expect(run.errors).toEqual([]);
      expect(run.records.find((record) => record.$schema === schemaUri("experiment-result"))).toMatchObject({ deterministicValidation: "pass", guardrails: [{ metric: "guardrail_pass", value: false, passed: false }], promotionDecision: "rejected" });
      expect(run.records.find((record) => record.$schema === schemaUri("evidence"))).toMatchObject({ evidenceType: "evidence_negative.result", statement: expect.stringContaining("violated contract guardrails: guardrail_pass=false violates >= 1") });
    } finally {
      run.cleanup();
    }
  }, 90_000);

  it("stops after a reviewed winner reaches the frozen target", async () => {
    const run = await runAutoresearchFixture({ primaryMetric: { name: "score", objective: "minimize", minimumEffect: 1 }, baselineMetrics: { score: 4 }, stopConditions: { target: 2 }, roundWidths: [1], execution: { runner: "native", command: [process.execPath, "-e", "require('fs').writeFileSync('metrics.json','{\"score\":2}')"], resultPath: "metrics.json", timeoutSeconds: 30, usesGpu: false } });
    try {
      expect(run.execution.state).toBe("completed");
      expect(run.errors).toEqual([]);
      expect(run.records.filter((record) => record.$schema === schemaUri("experiment-result"))).toHaveLength(2);
      expect(run.records.find((record) => record.$schema === schemaUri("autoresearch-completion-packet"))).toMatchObject({ terminalReason: "target_reached" });
      expectCompletionAuthority(run.records, run.execution);
      expectReviewedEvidenceAuthority(run.records);
      expect(run.reviewedEvidenceCandidates).toHaveLength(1);
      const candidate = run.reviewedEvidenceCandidates[0]!;
      const evidence = run.records.find((record) => record.$schema === schemaUri("evidence") && record.evidenceId === candidate.evidenceId)!;
      expect(evidence).toEqual({ ...candidate, quality: { ...(candidate.quality as Record<string, JsonValue>), status: "reviewed", reviewId: (evidence.quality as { reviewId: string }).reviewId } });
    } finally {
      run.cleanup();
    }
  }, 90_000);

  it("recovers one canonical completion packet after publication but before the state transition", async () => {
    const run = await runAutoresearchFixture(targetContract(), { deferCompletion: true });
    try {
      expect(run.execution.state).toBe("completed");
      expect(run.errors).toEqual([]);
      expect(run.completionAttempts).toBe(2);
      expect(run.records.filter((record) => record.$schema === schemaUri("review-request") && record.reviewType === "autoresearch_closure")).toHaveLength(1);
      expectCompletionAuthority(run.records, run.execution);
    } finally { run.cleanup(); }
  }, 90_000);

  it.each(["missing Evidence coverage", "a stale target version", "a non-PASS verdict"])("does not mark Evidence reviewed or promote with %s", async (failure) => {
    const run = await runAutoresearchFixture(targetContract(), { transformReview: (record) => {
      if (record.reviewType !== "experiment") return record;
      if (failure === "missing Evidence coverage") return { ...record, reviewedEvidenceIds: [] };
      if (failure === "a stale target version") return { ...record, target: { ...(record.target as Record<string, JsonValue>), targetVersion: 2 } };
      return { ...record, verdict: "REVISE", recommendedPromotion: "hold" };
    } });
    try {
      expect(run.execution.state).toBe("completed");
      expect(run.errors).toEqual([]);
      expect(run.execution.value.acceptedFrontierExperimentIds).toEqual([]);
      expect(run.records.find((record) => record.$schema === schemaUri("evidence"))).toMatchObject({ quality: { status: "unreviewed", reviewId: null } });
      expect(run.records.find((record) => record.$schema === schemaUri("experiment-result"))).toMatchObject({ promotionRecommendation: "reject", promotionDecision: "rejected" });
      expect(run.records.find((record) => record.$schema === schemaUri("autoresearch-completion-packet"))).toMatchObject({ terminalReason: "all_hypotheses_rejected", bestExperimentId: null });
      expectCompletionAuthority(run.records, run.execution);
    } finally { run.cleanup(); }
  }, 90_000);

  it("does not report target_reached when the round Review holds the winner", async () => {
    const run = await runAutoresearchFixture(targetContract({ target: 2, maximumNoProgressRounds: 2 }), { transformReview: (record) => record.reviewType === "experiment_round" ? { ...record, verdict: "REVISE", recommendedPromotion: "hold" } : record });
    try {
      expect(run.execution.state).toBe("completed");
      expect(run.errors).toEqual([]);
      expect(run.execution.value.acceptedFrontierExperimentIds).toEqual([]);
      expect(run.records.find((record) => record.$schema === schemaUri("autoresearch-completion-packet"))).toMatchObject({ terminalReason: "budget_exhausted", bestExperimentId: null });
      expectCompletionAuthority(run.records, run.execution);
    } finally { run.cleanup(); }
  }, 90_000);

  it.each(["a stale target version", "a non-PASS verdict"])("blocks closure with %s", async (failure) => {
    const run = await runAutoresearchFixture(targetContract(), { transformReview: (record) => {
      if (record.reviewType !== "autoresearch_closure") return record;
      if (failure === "a stale target version") return { ...record, target: { ...(record.target as Record<string, JsonValue>), targetVersion: 1 } };
      return { ...record, verdict: "REVISE" };
    } });
    try {
      expect(run.execution.state).toBe("blocked");
      expect(run.errors).toEqual([{ message: "Autoresearch closure Review did not pass" }]);
      expect(run.records.filter((record) => record.$schema === schemaUri("autoresearch-completion-packet"))).toEqual([]);
    } finally { run.cleanup(); }
  }, 90_000);
});

function targetContract(stopConditions: JsonValue = { target: 2 }): JsonValue {
  return { primaryMetric: { name: "score", objective: "minimize", minimumEffect: 1 }, baselineMetrics: { score: 4 }, stopConditions, roundWidths: [1], execution: { runner: "native", command: [process.execPath, "-e", "require('fs').writeFileSync('metrics.json','{\"score\":2}')"], resultPath: "metrics.json", timeoutSeconds: 30, usesGpu: false } };
}

function expectCompletionAuthority(records: unknown[], execution: { entityId: string; version: number }): void {
  const values = records as Record<string, unknown>[];
  const packets = values.filter((record) => record.$schema === schemaUri("autoresearch-completion-packet"));
  expect(packets).toHaveLength(1);
  const packet = packets[0]!;
  expect(validateRecord(schemaUri("autoresearch-completion-packet"), packet as JsonValue).ok).toBe(true);
  const requests = values.filter((record) => record.$schema === schemaUri("review-request") && record.reviewId === packet.closureReviewId);
  const verdicts = values.filter((record) => record.$schema === schemaUri("review-verdict") && record.reviewId === packet.closureReviewId);
  expect(requests).toHaveLength(1);
  expect(verdicts).toHaveLength(1);
  const request = requests[0]!;
  const verdict = verdicts[0]!;
  expect(validateRecord(schemaUri("review-request"), request as JsonValue).ok).toBe(true);
  expect(validateRecord(schemaUri("review-verdict"), verdict as JsonValue).ok).toBe(true);
  expect(request).toMatchObject({ reviewType: "autoresearch_closure", target: { targetType: "target_autoresearch", targetId: execution.entityId, targetVersion: execution.version - 1 }, requiredArtifactIds: [packet.synthesisArtifactId], requiredEvidenceIds: packet.evidenceIds });
  expect(verdict).toMatchObject({ reviewRequestId: request.reviewRequestId, reviewType: request.reviewType, target: request.target, reviewerAgentId: request.reviewerAgentId, independenceCheck: "pass", verdict: "PASS", reviewedArtifactIds: request.requiredArtifactIds, reviewedEvidenceIds: request.requiredEvidenceIds });
  expect(request.producerAgentIds).not.toContain(request.reviewerAgentId);
}

function expectReviewedEvidenceAuthority(records: unknown[]): void {
  const values = records as Record<string, unknown>[];
  const reviewed = values.filter((record) => record.$schema === schemaUri("evidence") && (record.quality as { status: string }).status === "reviewed");
  expect(reviewed.length).toBeGreaterThan(0);
  for (const evidence of reviewed) {
    const reviewId = (evidence.quality as { reviewId: string }).reviewId;
    const requests = values.filter((record) => record.$schema === schemaUri("review-request") && record.reviewId === reviewId);
    const verdicts = values.filter((record) => record.$schema === schemaUri("review-verdict") && record.reviewId === reviewId);
    expect(requests).toHaveLength(1);
    expect(verdicts).toHaveLength(1);
    expect(requests[0]!.requiredEvidenceIds).toContain(evidence.evidenceId);
    expect(verdicts[0]).toMatchObject({ reviewRequestId: requests[0]!.reviewRequestId, target: requests[0]!.target, reviewerAgentId: requests[0]!.reviewerAgentId, verdict: "PASS" });
    expect(verdicts[0]!.reviewedEvidenceIds).toContain(evidence.evidenceId);
  }
}

async function runAutoresearchFixture(evaluationContract: JsonValue, options: { transformReview?: (record: Record<string, JsonValue>) => Record<string, JsonValue>; deferCompletion?: boolean } = {}) {
  const directory = mkdtempSync(join(tmpdir(), "nosh-autoresearch-terminal-"));
  const repositoryRoot = join(directory, "repository");
  mkdirSync(repositoryRoot);
  initializeRepository(repositoryRoot);
  const projectId = createId("prj");
  const project: RegisteredProject = { projectId, repositoryRoot, databasePath: join(directory, "project.sqlite"), registeredAt: new Date().toISOString() };
  const store = new EventStore(project.databasePath);
  const research = new ResearchControl(() => store, () => project, () => undefined);
  const agents = new ExperimentAgents((draft) => { appendFixtureEvent(store, draft); }, options.transformReview);
  const jobEvents = new EventEmitter();
  const jobs = new JobSupervisor(join(directory, "jobs"), (job) => {
    store.append({ $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "job.state_changed", source: "fixture", scope: { projectId, missionId: null, directionId: null, autoresearchId: null, experimentId: null, runId: job.runId, jobId: job.jobId, agentId: null }, correlationId: job.jobId, causationId: null, payload: job as unknown as JsonValue });
    if (["completed", "failed", "cancelled", "lost"].includes(job.state)) jobEvents.emit("terminal");
  });
  let execution = research.createAutoresearch(projectId, { decisionQuestion: "Does the bounded variant meet the frozen stop condition?", evaluationContract, maximumExperiments: 1, maximumRounds: 1, idempotencyKey: `terminal-${createId("cmd")}` });
  execution = research.transitionAutoresearch(projectId, execution.entityId, execution.version, "running", `terminal-running-${execution.entityId}`);
  const transition = research.transitionAutoresearch.bind(research);
  let completionAttempts = 0;
  const transitionSpy = vi.spyOn(research, "transitionAutoresearch").mockImplementation((...args) => {
    if (args[3] === "completed" && ++completionAttempts === 1 && options.deferCompletion) return research.autoresearchExecution(args[0], args[1]);
    return transition(...args);
  });
  for (let cycle = 0; cycle < 80 && research.autoresearchExecution(projectId, execution.entityId).state === "running"; cycle += 1) {
    // Reconstruct the supervisor to exercise its durable recovery path on every cycle.
    const supervisor = new AutoresearchSupervisor(research, agents as unknown as PiAdapter, jobs, (spec) => { const intent = store.beginOperation(projectId, "job.launch", `job-launch:${spec.jobId}`, spec as unknown as JsonValue); const job = jobs.start(spec); store.completeOperation(projectId, intent.intentId, job as unknown as JsonValue); return job; }, () => [project], join(directory, "pi-package"), (draft) => { appendFixtureEvent(store, draft); });
    await supervisor.tick();
    if (jobs.list().some((job) => ["starting", "running", "checkpointing", "finishing"].includes(job.state))) {
      await once(jobEvents, "terminal");
    }
  }
  const completed = research.autoresearchExecution(projectId, execution.entityId);
  const records = research.submitted(projectId).filter((entry) => entry.event.scope.autoresearchId === execution.entityId).map((entry) => entry.record as Record<string, unknown>);
  const errors = store.replay(projectId).filter((entry) => entry.scope.autoresearchId === execution.entityId && entry.type === "autoresearch.supervisor_error").map((entry) => entry.payload);
  transitionSpy.mockRestore();
  return { execution: completed, records, errors, completionAttempts, reviewedEvidenceCandidates: agents.reviewedEvidenceCandidates, cleanup: () => { jobs.close(); store.close(); rmSync(directory, { recursive: true, force: true }); } };
}

function initializeRepository(path: string): void { git(path, ["init", "-b", "main"]); git(path, ["config", "user.email", "fixture@nosh.test"]); git(path, ["config", "user.name", "NOSH Fixture"]); writeFileSync(join(path, ".gitignore"), ".nosh/\n"); writeFileSync(join(path, "README.md"), "# Fixture\n"); git(path, ["add", "--", ".gitignore", "README.md"]); git(path, ["commit", "-m", "fixture baseline"]); }
function git(path: string, args: string[]): string { const result = spawnSync("git", ["-C", path, ...args], { encoding: "utf8", windowsHide: true }); if (result.status !== 0) throw new Error(result.stderr.trim() || `git ${args[0]} failed`); return result.stdout.trim(); }
function required(value: string, pattern: RegExp): string { const match = pattern.exec(value)?.[1]; if (!match) throw new Error(`Missing fixture prompt value for ${pattern}`); return match; }

function appendFixtureEvent(store: EventStore, draft: EventDraft): void {
  if (draft.type === "record.submitted" && isTaskTerminalRecord(draft.payload)) {
    const taskId = /^task:(tsk_[0-9a-f]{32})$/.exec(draft.correlationId ?? "")?.[1];
    if (!taskId) throw new Error("Fixture terminal submission requires an exact Task correlation");
    store.appendTerminalSubmission(taskId, "fixture", (draft.payload as { $schema: string }).$schema, draft.payload, draft);
  } else store.append(draft);
}
