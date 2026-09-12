import { existsSync, mkdirSync, readFileSync, readdirSync, renameSync, rmSync, statSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { spawnSync } from "node:child_process";
import { createId } from "@nosh/core";
import { ArtifactStore, exportPaper } from "@nosh/evidence";
import { validateTeachback } from "@nosh/agent-runtime";
import { VersionedDag, isLegalDirectionTransition, isLegalMissionTransition, type GraphNode, type GraphOperation, type MissionState, type DirectionState } from "@nosh/graph";
import type { EntityProjection, EventStore, OperationIntent, RegisteredProject } from "@nosh/persistence";
import { canonicalJson, isTaskTerminalRecord, schemaUri, sha256, validateRecord, type EventDraft, type EventEnvelope, type JsonValue } from "@nosh/wire";

export type MissionProjection = { missionId: string; projectId: string; title: string; objective: string; nonObjectives: string[]; deliverables: string[]; successCriteria: string[]; startingEvidence: string[]; budgets: { maximumModelTokens: number; maximumWallClockSeconds: number; maximumGpuSeconds: number; maximumDiskBytes: number; maximumConcurrentAgents: number }; approvalBoundaries: string[]; externalActionRestrictions: string[]; pausePolicy: "safe"; finalReviewRubric: string[]; approvedGraphVersion: number | null; approvedAt: string | null; canonicalContract: JsonValue | null; state: MissionState; graphVersion: number; nodes: GraphNode[]; createdAt: string; updatedAt: string };
export type DirectionProjection = { directionId: string; projectId: string; missionId: string | null; questionId: string; question: string; decisionUse: string; falsifiability: { supportingOutcome: string; refutingOutcome: string; inconclusiveOutcome: string }; projectClaimIds: string[]; scope: JsonValue; evaluationContractId: string; evaluationContract: JsonValue; evaluationContractHash: string; plannedBaselineExperimentId: string; integrationBranch: string; stoppingRules: Array<{ ruleId: string; statement: string }>; budget: JsonValue; closureRubric: Array<{ rubricId: string; statement: string }>; createdBy: "user"; approvedBy: "user" | null; activatedAt: string | null; canonicalContract: JsonValue | null; acceptedBaseline: { commit: string; reviewId: string } | null; state: DirectionState; graphVersion: number; nodes: GraphNode[]; createdAt: string; updatedAt: string };
export type AutoresearchProjection = { autoresearchId: string; projectId: string; directionId: string | null; missionId: string | null; invokedByRole: "user"; decisionQuestion: string; familyTags: string[]; scope: string[]; fingerprint: string; state: "draft" | "running" | "paused" | "blocked" | "completed" | "stopped" | "failed"; evaluationContractId: string; evaluationContract: JsonValue; evaluationContractHash: string; rootExperimentId: string; acceptedFrontierExperimentIds: string[]; forbiddenChangeScopes: string[]; variantPolicy: JsonValue; stopConditions: Array<{ type: string; value: number }>; promotionRubricIds: string[]; currentRound: number; maximumExperiments: number; maximumRounds: number; maximumDepth: number; maximumChildrenPerParent: number; maximumWallClockSeconds: number; maximumModelTokens: number; maximumGpuSeconds: number; maximumDiskBytes: number; canonicalContract: JsonValue | null; createdAt: string; updatedAt: string };
export type Stored<T> = Omit<EntityProjection, "value"> & { value: T };
export type MissionBudgetUse = { modelTokens: number; wallClockSeconds: number; gpuSeconds: number; diskBytes: number };
export type AutoresearchBudgetUse = MissionBudgetUse & { experiments: number; rounds: number };
export type DomainSession = {
  agentId: string;
  role: "nosh" | "user" | "mission_director" | "research_director" | "librarian_researcher" | "general_worker" | "reviewer";
  taskId: string | null;
  missionId: string | null;
  directionId: string | null;
  autoresearchId: string | null;
  experimentId: string | null;
  runId: string | null;
  jobId: string | null;
};
type TaskAuthority = { graphNodeId: string; missionId: string | null; directionId: string | null; leaseId: string; leaseProjectionVersion: number; agentId: string; maximumModelTokens: number; maximumWallClockSeconds: number; delegation: "disabled" | "request_only" };
type PendingHandoff = {
  acceptedHandoffHash: string;
  handoffId: string;
  logicalOwnerId: string;
  recipientAgentId: string;
  record: Record<string, JsonValue>;
  scope: EventDraft["scope"];
  source: TaskAuthority & { taskId: string; role: DomainSession["role"]; projectionVersion: number };
};

const autoresearchTransitions: Record<AutoresearchProjection["state"], AutoresearchProjection["state"][]> = { draft: ["running", "stopped"], running: ["paused", "blocked", "completed", "stopped", "failed"], paused: ["running", "stopped"], blocked: ["running", "stopped", "failed"], completed: [], stopped: [], failed: [] };

export const librarianResearchHosts = ["api.crossref.org", "api.openalex.org", "api.semanticscholar.org", "arxiv.org", "export.arxiv.org", "eutils.ncbi.nlm.nih.gov", "pubmed.ncbi.nlm.nih.gov"] as const;

export class ResearchControl {
  constructor(private readonly store: (projectId: string) => EventStore, private readonly project: (projectId: string) => RegisteredProject, private readonly publish: (event: EventEnvelope) => void, private readonly assertWritable: (projectId: string) => void = () => {}) {}

  missions(projectId: string): Array<Stored<MissionProjection>> { return this.store(projectId).projections(projectId, "mission") as Array<Stored<MissionProjection>>; }
  mission(projectId: string, missionId: string): Stored<MissionProjection> { const value = this.store(projectId).projection(projectId, "mission", missionId); if (!value) throw new Error("Unknown Mission"); return value as Stored<MissionProjection>; }
  graphProposals(projectId: string): Array<Stored<JsonValue>> { return this.store(projectId).projections(projectId, "graph_proposal") as Array<Stored<JsonValue>>; }
  createMission(projectId: string, input: { title: string; objective: string; deliverables: string[]; successCriteria: string[]; nonObjectives?: string[]; startingEvidence?: string[]; idempotencyKey: string }): Stored<MissionProjection> {
    const deliverables = input.deliverables.map((value) => value.trim()).filter(Boolean);
    const successCriteria = input.successCriteria.map((value) => value.trim()).filter(Boolean);
    const startingEvidence = (input.startingEvidence ?? []).map((value) => value.trim()).filter(Boolean);
    const replay = createdProjectionReplay<MissionProjection>(this.store(projectId), projectId, input.idempotencyKey, "mission", "mission.created");
    if (replay) {
      const value = replay.value;
      if (value.title !== input.title.trim() || value.objective !== input.objective.trim() || !sameStringSet(value.deliverables, deliverables) || !sameStringSet(value.successCriteria, successCriteria) || !sameStringSet(value.nonObjectives, (input.nonObjectives ?? []).map((item) => item.trim()).filter(Boolean)) || !sameStringSet(value.startingEvidence, startingEvidence)) throw new Error("Mission creation idempotency key was reused for a different request");
      return replay;
    }
    if (!input.title.trim() || !input.objective.trim() || !deliverables.length || !successCriteria.length) throw new Error("Mission title, objective, deliverables, and success criteria are required");
    assertExistingEvidenceIds(this.submitted(projectId).filter((entry) => isSchema(entry.record, "evidence")), startingEvidence);
    const missionId = createId("mis");
    const now = new Date().toISOString();
    const nodes = defaultMissionNodes(now, successCriteria.map((_, index) => `criterion_${index + 1}`));
    const graph = new VersionedDag(missionId, nodes).current();
    const value: MissionProjection = {
      missionId, projectId, title: input.title.trim(), objective: input.objective.trim(),
      nonObjectives: (input.nonObjectives ?? []).map((value) => value.trim()).filter(Boolean),
      deliverables, successCriteria, startingEvidence,
      budgets: { maximumModelTokens: 1_000_000, maximumWallClockSeconds: 86_400, maximumGpuSeconds: 0, maximumDiskBytes: 10_000_000_000, maximumConcurrentAgents: 2 },
      approvalBoundaries: ["Protected-branch integration and budget increases require user approval"],
      externalActionRestrictions: ["No publication, push, purchase, or third-party message without explicit user approval"],
      pausePolicy: "safe",
      finalReviewRubric: ["Every required node is accepted", "Claims resolve to reviewed evidence or explicit limitations", "Artifacts and evaluated commits resolve to stored hashes", "No unresolved blocking defect remains"],
      approvedGraphVersion: null, approvedAt: null, canonicalContract: null,
      state: "draft", graphVersion: graph.version, nodes: graph.nodes, createdAt: now, updatedAt: now,
    };
    return this.mutate(projectId, "mission", missionId, 0, "mission.created", input.idempotencyKey, value, { scopeType: "mission", version: 1, value: graph as unknown as JsonValue, rationale: "initial plan" });
  }
  transitionMission(projectId: string, missionId: string, expectedVersion: number, next: MissionState, idempotencyKey: string): Stored<MissionProjection> {
    const intent = { command: "mission.transition", missionId, expectedVersion, next };
    const replay = projectionIntentReplay<MissionProjection>(this.store(projectId), projectId, idempotencyKey, "mission", missionId, intent);
    if (replay) return replay;
    const current = this.mission(projectId, missionId);
    if (!isLegalMissionTransition(current.value.state, next)) throw new Error(`Illegal Mission transition ${current.value.state} -> ${next}`);
    if (next === "running" && this.missions(projectId).some((mission) => mission.entityId !== missionId && ["running", "pausing", "paused", "reviewing", "blocked", "stopping"].includes(mission.state))) throw new Error("Version 1 allows one active Mission per Project");
    if (next === "completed") {
      const packet = currentMissionCompletionPacket(this.records(projectId), missionId, current.version, current.value.graphVersion);
      const basis = this.missionCompletionBasis(projectId, missionId);
      if (!packet || this.missionCompletionIssues(projectId, missionId).length || basis.issues.length || !sameMissionCriterionMappings(packet.criteria, basis.criteria) || !sameStringSet(packet.deliverableArtifactIds, basis.artifactIds) || !sameStringSet(packet.deterministicPreReview && isObject(packet.deterministicPreReview) ? packet.deterministicPreReview.validatorRunIds : null, basis.validatorRunIds) || !sameStringSet(packet.openDefectIds, basis.openDefectIds)) throw new Error("Mission completion requires an exact current completion packet, each contract criterion's task, validator, independent Review, artifact, and defect basis");
    }
    const now = new Date().toISOString(); const approving = current.value.state === "awaiting_approval" && next === "running"; const approvedAt = approving ? now : current.value.approvedAt;
    return this.mutate(projectId, "mission", missionId, expectedVersion, `mission.${next}`, idempotencyKey, { ...current.value, approvedGraphVersion: approving ? current.value.graphVersion : current.value.approvedGraphVersion, approvedAt, state: next, updatedAt: now }, undefined, undefined, approving, intent);
  }
  mutateMissionGraph(projectId: string, missionId: string, expectedVersion: number, baseGraphVersion: number, operations: GraphOperation[], rationale: string, evidenceIds: string[], idempotencyKey: string, approved = false): Stored<MissionProjection> {
    const intent = { command: "mission.graph_mutation", missionId, expectedVersion, baseGraphVersion, operations, rationale, evidenceIds: [...evidenceIds].sort(), approved };
    const replay = projectionIntentReplay<MissionProjection>(this.store(projectId), projectId, idempotencyKey, "mission", missionId, intent);
    if (replay) return replay;
    const current = this.mission(projectId, missionId);
    if (!approved && !["draft", "planning", "awaiting_approval"].includes(current.value.state)) throw new Error("Direct Mission graph mutation is allowed only before Mission approval");
    if (approved && !current.value.canonicalContract) throw new Error("Mission graph approval requires an existing canonical Mission contract");
    if (current.value.graphVersion !== baseGraphVersion) throw new Error(`Stale graph version ${baseGraphVersion}; current version is ${current.value.graphVersion}`);
    assertExistingEvidenceIds(this.submitted(projectId).filter((entry) => isSchema(entry.record, "evidence")), current.value.startingEvidence);
    const graph = new VersionedDag(missionId, current.value.nodes, "hydrated", current.value.graphVersion);
    const version = graph.apply(baseGraphVersion, graphOperations(operations, current.value.nodes), rationale, evidenceIds);
    assertMissionCriterionOwnership(current.value.successCriteria, version.nodes);
    return this.mutate(projectId, "mission", missionId, expectedVersion, "mission.graph_changed", idempotencyKey, { ...current.value, graphVersion: version.version, nodes: version.nodes, approvedGraphVersion: approved ? version.version : current.value.approvedGraphVersion, updatedAt: new Date().toISOString() }, { scopeType: "mission", version: version.version, value: version as unknown as JsonValue, rationale }, undefined, false, intent);
  }
  transitionMissionNode(projectId: string, missionId: string, expectedVersion: number, nodeId: string, next: GraphNode["state"], idempotencyKey: string, lease?: NonNullable<GraphNode["lease"]>): Stored<MissionProjection> {
    const intent = { command: "mission.node_transition", missionId, expectedVersion, nodeId, next, lease: lease ?? null };
    const replay = projectionIntentReplay<MissionProjection>(this.store(projectId), projectId, idempotencyKey, "mission", missionId, intent);
    if (replay) return replay;
    const current = this.mission(projectId, missionId);
    if (!["running", "pausing", "reviewing"].includes(current.state)) throw new Error("Mission nodes can change only while the Mission is active");
    this.assertLeaseTransition(next, lease, expectedVersion);
    const graph = new VersionedDag(missionId, current.value.nodes, "hydrated", current.value.graphVersion);
    const node = lease ? graph.lease(nodeId, lease) : graph.transition(nodeId, next);
    const value = { ...current.value, nodes: graph.current().nodes, updatedAt: new Date().toISOString() };
    return this.mutate(projectId, "mission", missionId, expectedVersion, "mission.node_state_changed", idempotencyKey, value, undefined, { nodeId, nodeState: node.state, attempt: node.attempt, leaseId: node.lease?.leaseId ?? null }, false, intent);
  }

  directions(projectId: string): Array<Stored<DirectionProjection>> { return this.store(projectId).projections(projectId, "direction") as Array<Stored<DirectionProjection>>; }
  direction(projectId: string, directionId: string): Stored<DirectionProjection> { const value = this.store(projectId).projection(projectId, "direction", directionId); if (!value) throw new Error("Unknown Direction"); return value as Stored<DirectionProjection>; }
  createDirection(projectId: string, input: { question: string; decisionUse: string; missionId?: string | null; evaluationContract?: JsonValue; idempotencyKey: string }): Stored<DirectionProjection> {
    const question = input.question.trim(); const decisionUse = input.decisionUse.trim();
    const evaluationContract = input.evaluationContract ?? { metrics: [], datasets: [], seeds: [0] };
    const replay = createdProjectionReplay<DirectionProjection>(this.store(projectId), projectId, input.idempotencyKey, "direction", "direction.created");
    if (replay) {
      const value = replay.value;
      if (value.question !== question || value.decisionUse !== decisionUse || value.missionId !== (input.missionId ?? null) || canonicalJson(value.evaluationContract) !== canonicalJson(evaluationContract)) throw new Error("Direction creation idempotency key was reused for a different request");
      return replay;
    }
    if (!isObject(evaluationContract)) throw new Error("Direction evaluation contract must be an object");
    if (!question || !decisionUse) throw new Error("Direction question and decision use are required");
    if (input.missionId) this.mission(projectId, input.missionId);
    const directionId = createId("dir"); const now = new Date().toISOString();
    const graph = new VersionedDag(directionId, defaultDirectionNodes(now)).current();
    const evaluationContractHash = sha256(evaluationContract);
    const questionId = `question_${crypto.randomUUID().replaceAll("-", "")}`;
    const evaluationContractId = `evaluation_${crypto.randomUUID().replaceAll("-", "")}`;
    const plannedBaselineExperimentId = createId("exp");
    const falsifiability = {
      supportingOutcome: `Evidence supports: ${question}`,
      refutingOutcome: `Evidence refutes: ${question}`,
      inconclusiveOutcome: `Evidence is inconclusive for: ${question}`,
    };
    const stoppingRules = [{ ruleId: `stop_${crypto.randomUUID().replaceAll("-", "")}`, statement: "Stop when the evaluation contract is exhausted." }];
    const closureRubric = [{ rubricId: `rubric_${crypto.randomUUID().replaceAll("-", "")}`, statement: decisionUse }];
    const value: DirectionProjection = {
      directionId, projectId, missionId: input.missionId ?? null, questionId, question, decisionUse,
      falsifiability, projectClaimIds: [], scope: { missionId: input.missionId ?? null, evaluationContractHash }, evaluationContractId, evaluationContract,
      evaluationContractHash, plannedBaselineExperimentId, integrationBranch: `direction/${directionId}`,
      stoppingRules, budget: { evaluationContractHash }, closureRubric, createdBy: "user", approvedBy: null, activatedAt: null, canonicalContract: null,
      acceptedBaseline: null, state: "draft", graphVersion: graph.version, nodes: graph.nodes, createdAt: now, updatedAt: now,
    };
    return this.mutate(projectId, "direction", directionId, 0, "direction.created", input.idempotencyKey, value, { scopeType: "direction", version: 1, value: graph as unknown as JsonValue, rationale: "initial direction plan" });
  }
  transitionDirection(projectId: string, directionId: string, expectedVersion: number, next: DirectionState, idempotencyKey: string): Stored<DirectionProjection> {
    const intent = { command: "direction.transition", directionId, expectedVersion, next };
    const replay = projectionIntentReplay<DirectionProjection>(this.store(projectId), projectId, idempotencyKey, "direction", directionId, intent);
    if (replay) return replay;
    const current = this.direction(projectId, directionId);
    if (!isLegalDirectionTransition(current.value.state, next)) throw new Error(`Illegal Direction transition ${current.value.state} -> ${next}`);
    if (next === "closed") {
      const graphComplete = current.value.nodes.every((node) => !node.required || ["accepted", "waived", "superseded"].includes(node.state));
      const executionsComplete = this.autoresearch(projectId).filter((execution) => execution.value.directionId === directionId).every((execution) => ["completed", "stopped", "failed"].includes(execution.state));
      const evidence = this.submitted(projectId).filter((entry) => entry.event.scope.directionId === directionId && isSchema(entry.record, "evidence"));
      if (!graphComplete || !executionsComplete || !evidence.length || !this.passingReview(projectId, directionId, "direction_closure", current.version)) throw new Error("Direction closure requires complete graph, terminal Autoresearch, evidence, and independent Review");
    }
    const now = new Date().toISOString(); const activating = current.value.state === "proposed" && next === "active"; const activatedAt = activating ? now : current.value.activatedAt;
    return this.mutate(projectId, "direction", directionId, expectedVersion, `direction.${next}`, idempotencyKey, { ...current.value, state: next, approvedBy: activating ? "user" : current.value.approvedBy, activatedAt, updatedAt: now }, undefined, undefined, activating, intent);
  }
  transitionDirectionNode(projectId: string, directionId: string, expectedVersion: number, nodeId: string, next: GraphNode["state"], idempotencyKey: string, lease?: NonNullable<GraphNode["lease"]>): Stored<DirectionProjection> {
    const intent = { command: "direction.node_transition", directionId, expectedVersion, nodeId, next, lease: lease ?? null };
    const replay = projectionIntentReplay<DirectionProjection>(this.store(projectId), projectId, idempotencyKey, "direction", directionId, intent);
    if (replay) return replay;
    const current = this.direction(projectId, directionId);
    if (!["active", "reviewing"].includes(current.state)) throw new Error("Direction nodes can change only while the Direction is active");
    this.assertLeaseTransition(next, lease, expectedVersion);
    const graph = new VersionedDag(directionId, current.value.nodes, "hydrated", current.value.graphVersion);
    const node = lease ? graph.lease(nodeId, lease) : graph.transition(nodeId, next);
    return this.mutate(projectId, "direction", directionId, expectedVersion, "direction.node_state_changed", idempotencyKey, { ...current.value, nodes: graph.current().nodes, updatedAt: new Date().toISOString() }, undefined, { nodeId, nodeState: node.state, attempt: node.attempt, leaseId: node.lease?.leaseId ?? null }, false, intent);
  }
  mutateDirectionGraph(projectId: string, directionId: string, expectedVersion: number, baseGraphVersion: number, operations: GraphOperation[], rationale: string, evidenceIds: string[], idempotencyKey: string): Stored<DirectionProjection> {
    const intent = { command: "direction.graph_mutation", directionId, expectedVersion, baseGraphVersion, operations, rationale, evidenceIds: [...evidenceIds].sort() };
    const replay = projectionIntentReplay<DirectionProjection>(this.store(projectId), projectId, idempotencyKey, "direction", directionId, intent);
    if (replay) return replay;
    const current = this.direction(projectId, directionId);
    if (current.value.graphVersion !== baseGraphVersion) throw new Error(`Stale graph version ${baseGraphVersion}; current version is ${current.value.graphVersion}`);
    const graph = new VersionedDag(directionId, current.value.nodes, "hydrated", current.value.graphVersion);
    const version = graph.apply(baseGraphVersion, graphOperations(operations, current.value.nodes), rationale, evidenceIds);
    return this.mutate(projectId, "direction", directionId, expectedVersion, "direction.graph_changed", idempotencyKey, { ...current.value, graphVersion: version.version, nodes: version.nodes, updatedAt: new Date().toISOString() }, { scopeType: "direction", version: version.version, value: version as unknown as JsonValue, rationale }, undefined, false, intent);
  }
  approveGraphProposal(projectId: string, proposalId: string, expectedProposalVersion: number, idempotencyKey: string): Stored<JsonValue> {
    this.assertWritable(projectId);
    const store = this.store(projectId);
    const intent = { command: "graph.proposal_approval", proposalId, expectedProposalVersion, approval: { proposalId, approverRole: "user" } };
    const replay = store.commandReceipt(projectId, idempotencyKey);
    if (replay) {
      const approved = store.projection(projectId, "graph_proposal", proposalId);
      if (replay.intentHash !== sha256(intent) || replay.event.type !== "graph.proposal_approved" || replay.event.correlationId !== proposalId || !approved || approved.state !== "approved") throw new Error("Idempotency key was already used for a different graph-proposal approval");
      return approved as Stored<JsonValue>;
    }
    const pending = store.projection(projectId, "graph_proposal", proposalId);
    if (!pending || pending.state !== "pending" || pending.version !== expectedProposalVersion || !isObject(pending.value) || !isObject(pending.value.record) || !isObject(pending.value.scope)) throw new Error("Graph proposal is not an approvable pending proposal at the requested version");
    const record = pending.value.record;
    if (!validateRecord(schemaUri("graph-change-proposal"), record).ok || record.approvalRequired !== true || typeof record.scopeType !== "string" || typeof record.scopeId !== "string" || typeof record.baseGraphVersion !== "number") throw new Error("Pending graph proposal is corrupt");
    if (record.scopeType === "mission" && record.contractImpact === "material") throw new Error("Material Mission contract changes are rejected; create a new Mission contract instead of mutating the approved canonical contract");
    const applyKey = `graph-proposal-apply:${proposalId}`;
    if (!store.commandReceipt(projectId, applyKey)) {
      if (record.scopeType === "mission") {
        const current = this.mission(projectId, record.scopeId);
        if (current.value.graphVersion !== record.baseGraphVersion) throw new Error("Graph proposal approval uses a stale Mission graph version");
        this.mutateMissionGraph(projectId, record.scopeId, current.version, record.baseGraphVersion, graphOperations(record.operations, current.value.nodes), String(record.rationale), stringValues(record.evidenceIds), applyKey, true);
      } else if (record.scopeType === "direction") {
        const current = this.direction(projectId, record.scopeId);
        if (current.value.graphVersion !== record.baseGraphVersion) throw new Error("Graph proposal approval uses a stale Direction graph version");
        this.mutateDirectionGraph(projectId, record.scopeId, current.version, record.baseGraphVersion, graphOperations(record.operations, current.value.nodes), String(record.rationale), stringValues(record.evidenceIds), applyKey);
      } else throw new Error("Pending graph proposal has an unknown scope");
    }
    const approved = store.mutateProjection(idempotencyKey, expectedProposalVersion, {
      $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "graph.proposal_approved", source: "noshd",
      scope: pending.value.scope as EventDraft["scope"], correlationId: proposalId, causationId: null,
      payload: { proposalId, approverRole: "user", contractImpact: record.contractImpact },
    }, { entityType: "graph_proposal", entityId: proposalId, state: "approved", value: { ...pending.value, approvedAt: new Date().toISOString(), approvedBy: "user" }, intent });
    if (!approved.replayed) this.publish(approved.event);
    return approved.projection as Stored<JsonValue>;
  }
  acceptDirectionBaseline(projectId: string, directionId: string, expectedVersion: number, input: { commit: string; reviewId: string; evaluationContractHash: string; idempotencyKey: string }): Stored<DirectionProjection> {
    const intent = { command: "direction.baseline_accept", directionId, expectedVersion, commit: input.commit, reviewId: input.reviewId, evaluationContractHash: input.evaluationContractHash };
    const replay = projectionIntentReplay<DirectionProjection>(this.store(projectId), projectId, input.idempotencyKey, "direction", directionId, intent);
    if (replay) return replay;
    const current = this.direction(projectId, directionId);
    if (current.state !== "active") throw new Error("Direction must be active before baseline acceptance");
    if (input.evaluationContractHash !== current.value.evaluationContractHash) throw new Error("Baseline evaluation contract does not match the frozen Direction contract");
    if (!/^[0-9a-f]{7,64}$/i.test(input.commit)) throw new Error("Baseline commit must be an immutable Git commit hash");
    const baselineNode = canonicalDirectionBaselineNode(current.value);
    if (baselineNode.state !== "accepted" || current.version < 2) throw new Error("Baseline acceptance requires the canonical baseline node to be accepted");
    const reviewedDirectionVersion = current.version - 1;
    if (!this.passingReview(projectId, baselineNode.id, "task", reviewedDirectionVersion, input.reviewId)) throw new Error("Baseline acceptance requires an exact canonical PASS Review for the baseline node at the pre-acceptance Direction projection version");
    const commit = gitCommit(this.project(projectId).repositoryRoot, input.commit);
    return this.mutate(projectId, "direction", directionId, expectedVersion, "direction.baseline_accepted", input.idempotencyKey, { ...current.value, acceptedBaseline: { commit, reviewId: input.reviewId }, updatedAt: new Date().toISOString() }, undefined, undefined, false, intent);
  }

  autoresearch(projectId: string): Array<Stored<AutoresearchProjection>> { return this.store(projectId).projections(projectId, "autoresearch") as Array<Stored<AutoresearchProjection>>; }
  autoresearchExecution(projectId: string, autoresearchId: string): Stored<AutoresearchProjection> { const execution = this.store(projectId).projection(projectId, "autoresearch", autoresearchId) as Stored<AutoresearchProjection> | undefined; if (!execution) throw new Error("Unknown Autoresearch execution"); return execution; }
  createAutoresearch(projectId: string, input: { decisionQuestion: string; directionId?: string | null; missionId?: string | null; familyTags?: string[]; scope?: string[]; evaluationContract?: JsonValue; maximumExperiments?: number; maximumRounds?: number; maximumWallClockSeconds?: number; maximumModelTokens?: number; maximumGpuSeconds?: number; maximumDiskBytes?: number; idempotencyKey: string }): Stored<AutoresearchProjection> {
    const decisionQuestion = input.decisionQuestion.trim();
    const maximumExperiments = input.maximumExperiments ?? 12; const maximumRounds = input.maximumRounds ?? 8; const maximumWallClockSeconds = input.maximumWallClockSeconds ?? 86_400; const maximumModelTokens = input.maximumModelTokens ?? 500_000; const maximumGpuSeconds = input.maximumGpuSeconds ?? 86_400; const maximumDiskBytes = input.maximumDiskBytes ?? 10_000_000_000;
    const familyTags = [...new Set(input.familyTags ?? [])].sort(); const scope = [...new Set(input.scope ?? [])].sort();
    const replay = createdProjectionReplay<AutoresearchProjection>(this.store(projectId), projectId, input.idempotencyKey, "autoresearch", "autoresearch.created");
    if (replay) {
      const value = replay.value;
      if (value.decisionQuestion !== decisionQuestion || value.directionId !== (input.directionId ?? null) || (typeof input.missionId === "string" && value.missionId !== input.missionId) || !sameStringSet(value.familyTags, familyTags) || !sameStringSet(value.scope, scope) || (input.evaluationContract !== undefined && (!isObject(input.evaluationContract) || canonicalJson(value.evaluationContract) !== canonicalJson(input.evaluationContract))) || value.maximumExperiments !== maximumExperiments || value.maximumRounds !== maximumRounds || value.maximumWallClockSeconds !== maximumWallClockSeconds || value.maximumModelTokens !== maximumModelTokens || value.maximumGpuSeconds !== maximumGpuSeconds || value.maximumDiskBytes !== maximumDiskBytes) throw new Error("Autoresearch idempotency key was reused with different input");
      return replay;
    }
    if (!decisionQuestion) throw new Error("Autoresearch decision question is required");
    if (!Number.isInteger(maximumExperiments) || maximumExperiments < 1 || maximumExperiments > 32 || !Number.isInteger(maximumRounds) || maximumRounds < 1 || maximumRounds > 16 || !Number.isInteger(maximumWallClockSeconds) || maximumWallClockSeconds < 1 || !Number.isInteger(maximumModelTokens) || maximumModelTokens < 1 || !Number.isInteger(maximumGpuSeconds) || maximumGpuSeconds < 0 || !Number.isInteger(maximumDiskBytes) || maximumDiskBytes < 1) throw new Error("Autoresearch budgets are invalid");
    const direction = input.directionId ? this.direction(projectId, input.directionId) : null;
    if (direction && (direction.state !== "active" || !direction.value.acceptedBaseline)) throw new Error("Direction Autoresearch requires an active Direction with a reviewed immutable baseline");
    if (input.missionId) this.mission(projectId, input.missionId);
    const evaluationContract = input.evaluationContract ?? direction?.value.evaluationContract ?? { metrics: [], datasets: [], seeds: [0] };
    if (!isObject(evaluationContract)) throw new Error("Autoresearch evaluation contract must be an object");
    const evaluationContractHash = sha256(evaluationContract);
    if (direction && evaluationContractHash !== direction.value.evaluationContractHash) throw new Error("Autoresearch cannot change the frozen Direction evaluation contract");
    const fingerprint = sha256({ decisionQuestion: decisionQuestion.toLowerCase(), familyTags, scope, evaluationContractHash });
    const siblings = this.autoresearch(projectId).filter((execution) => execution.value.directionId === input.directionId);
    if (siblings.length >= 12) throw new Error("Direction Autoresearch execution budget exhausted");
    if (siblings.some((execution) => execution.value.fingerprint === fingerprint)) throw new Error("Autoresearch execution is not meaningfully separate");
    const autoresearchId = createId("ar"); const now = new Date().toISOString();
    const value: AutoresearchProjection = {
      autoresearchId, projectId, directionId: input.directionId ?? null, missionId: input.missionId ?? direction?.value.missionId ?? null,
      invokedByRole: "user", decisionQuestion, familyTags, scope, fingerprint, state: "draft",
      evaluationContractId: direction?.value.evaluationContractId ?? `evaluation_${crypto.randomUUID().replaceAll("-", "")}`,
      evaluationContract, evaluationContractHash, rootExperimentId: createId("exp"), acceptedFrontierExperimentIds: [],
      forbiddenChangeScopes: [".nosh/**"], variantPolicy: { maximumDepth: 8, maximumChildrenPerParent: 4 },
      stopConditions: [{ type: "maximum_rounds", value: maximumRounds }], promotionRubricIds: [`rubric_${crypto.randomUUID().replaceAll("-", "")}`],
      currentRound: 0, maximumExperiments, maximumRounds, maximumDepth: 8, maximumChildrenPerParent: 4, maximumWallClockSeconds, maximumModelTokens,
      maximumGpuSeconds, maximumDiskBytes, canonicalContract: null, createdAt: now, updatedAt: now,
    };
    return this.mutate(projectId, "autoresearch", autoresearchId, 0, "autoresearch.created", input.idempotencyKey, value, undefined, undefined, true);
  }
  transitionAutoresearch(projectId: string, autoresearchId: string, expectedVersion: number, next: AutoresearchProjection["state"], idempotencyKey: string): Stored<AutoresearchProjection> {
    const intent = { command: "autoresearch.transition", autoresearchId, expectedVersion, next };
    const replay = projectionIntentReplay<AutoresearchProjection>(this.store(projectId), projectId, idempotencyKey, "autoresearch", autoresearchId, intent);
    if (replay) return replay;
    const current = this.autoresearchExecution(projectId, autoresearchId);
    if (!autoresearchTransitions[current.value.state].includes(next)) throw new Error(`Illegal Autoresearch transition ${current.value.state} -> ${next}`);
    if (next === "completed") {
      const packets = this.submitted(projectId)
        .filter((entry) => entry.event.scope.autoresearchId === autoresearchId && isSchema(entry.record, "autoresearch-completion-packet") && isObject(entry.record))
        .map((entry) => entry.record)
        .filter((packet) => packet.autoresearchId === autoresearchId && packet.directionId === current.value.directionId && packet.evaluationContractHash === current.value.evaluationContractHash && validateRecord(schemaUri("autoresearch-completion-packet"), packet).ok);
      if (packets.length !== 1 || typeof packets[0]!.closureReviewId !== "string" || !this.passingReview(projectId, autoresearchId, "autoresearch_closure", current.version, packets[0]!.closureReviewId)) throw new Error("Autoresearch completion requires exactly one current canonical completion packet and its exact PASS closure Review at the current execution version");
    }
    return this.mutate(projectId, "autoresearch", autoresearchId, expectedVersion, `autoresearch.${next}`, idempotencyKey, { ...current.value, state: next, updatedAt: new Date().toISOString() }, undefined, undefined, false, intent);
  }
  advanceAutoresearch(projectId: string, autoresearchId: string, expectedVersion: number, round: number, frontier: string[], idempotencyKey: string): Stored<AutoresearchProjection> {
    const canonicalFrontier = [...new Set(frontier)].sort();
    const intent = { command: "autoresearch.advance", autoresearchId, expectedVersion, round, frontier: canonicalFrontier };
    const replay = projectionIntentReplay<AutoresearchProjection>(this.store(projectId), projectId, idempotencyKey, "autoresearch", autoresearchId, intent);
    if (replay) return replay;
    const current = this.autoresearchExecution(projectId, autoresearchId);
    if (current.state !== "running" || round < current.value.currentRound || round > current.value.maximumRounds) throw new Error("Autoresearch round update violates its contract");
    return this.mutate(projectId, "autoresearch", autoresearchId, expectedVersion, "autoresearch.round_completed", idempotencyKey, { ...current.value, currentRound: round, acceptedFrontierExperimentIds: canonicalFrontier, updatedAt: new Date().toISOString() }, undefined, undefined, false, intent);
  }

  validateDomainRecord(projectId: string, session: DomainSession, record: JsonValue): void {
    if (!isObject(record)) return;
    if (record.$schema === schemaUri("progress-update")) {
      const packet = this.taskAuthority(projectId, session, String(record.taskId));
      if (record.agentId !== session.agentId) throw new Error("Progress record actor does not own the task");
      this.assertWorkingLease(projectId, packet, session);
      return;
    }
    if (record.$schema === schemaUri("blocker")) {
      const packet = this.taskAuthority(projectId, session, String(record.taskId));
      if (record.scopeType !== "graph_node" || record.scopeId !== packet.graphNodeId) throw new Error("Blocker must be scoped to its assigned graph node");
      this.assertLiveLease(projectId, packet, session);
      return;
    }
    if (record.$schema === schemaUri("delegation-request")) {
      const authority = this.taskAuthority(projectId, session, String(record.requestingTaskId));
      this.assertLiveLease(projectId, authority, session);
      if (authority.delegation !== "request_only" || record.requestingAgentId !== session.agentId || !isObject(record.budgetEstimate) || typeof record.budgetEstimate.modelTokens !== "number" || typeof record.budgetEstimate.wallClockSeconds !== "number" || record.budgetEstimate.modelTokens > authority.maximumModelTokens || record.budgetEstimate.wallClockSeconds > authority.maximumWallClockSeconds) throw new Error("Delegation request exceeds the parent task budget or actor authority");
      return;
    }
    if (record.$schema === schemaUri("graph-change-proposal")) {
      if (record.proposerRole !== session.role || !["mission_director", "research_director", "user"].includes(session.role)) throw new Error("Graph proposal role does not match the submitting authority");
      if (record.contractImpact === "material" && record.approvalRequired !== true) throw new Error("Material graph proposals require explicit user approval");
      let nodes: GraphNode[];
      if (record.scopeType === "mission" && session.missionId && record.scopeId === session.missionId) {
        const mission = this.mission(projectId, session.missionId);
        if (mission.value.graphVersion !== record.baseGraphVersion) throw new Error("Graph proposal uses a stale Mission graph version");
        if (mission.value.approvedGraphVersion !== null && record.approvalRequired !== true) throw new Error("Mission graph changes after approval require explicit user approval");
        nodes = mission.value.nodes;
      } else if (record.scopeType === "direction" && session.directionId && record.scopeId === session.directionId) {
        const direction = this.direction(projectId, session.directionId);
        if (direction.value.graphVersion !== record.baseGraphVersion) throw new Error("Graph proposal uses a stale Direction graph version");
        nodes = direction.value.nodes;
      } else throw new Error("Graph proposal is outside the submitting session scope");
      graphOperations(record.operations, nodes);
      return;
    }
    if (record.$schema === schemaUri("handoff")) {
      const packet = this.taskAuthority(projectId, session, session.taskId ?? "");
      this.assertLiveLease(projectId, packet, session);
      if (record.fromAgentId !== session.agentId || typeof record.toAgentId !== "string" || record.toAgentId === session.agentId || record.oldLeaseReleaseId !== packet.leaseId || !isObject(record.scope) || !sameScope(record.scope, session) || !isObject(record.goalStack) || record.goalStack.currentGraphNodeId !== packet.graphNodeId) throw new Error("Handoff must name a distinct recipient and preserve its exact live source Task Packet scope, lease, and graph node");
      return;
    }
    if (record.$schema === schemaUri("handoff-teachback")) {
      const handoff = this.pendingHandoff(projectId, String(record.handoffId), eventScope(projectId, session), String(record.toAgentId));
      if (record.toAgentId !== session.agentId || record.logicalOwnerId !== handoff.logicalOwnerId) throw new Error("Teach-back is not addressed to the durable handoff recipient");
      const checked = validateTeachback(handoffState(handoff.record), {
        handoffId: String(record.handoffId), logicalOwnerId: String(record.logicalOwnerId), understoodGoalStack: handoffGoalStack(record.understoodGoalStack), decision: String(record.decision),
        observedVersions: objectVersions(record.observedVersions), observedBranchHead: String(record.observedBranchHead),
        acknowledgedDefectIds: stringValues(record.acknowledgedDefectIds), acknowledgedBlockerIds: stringValues(record.acknowledgedBlockerIds),
        selectedNextNodeId: typeof record.selectedNextNodeId === "string" ? record.selectedNextNodeId : null, conflicts: stringValues(record.conflicts),
      });
      if (!checked.ok) throw new Error(`Teach-back does not exactly match the handoff: ${checked.conflicts.join(", ")}`);
      return;
    }
  }

  applyDomainEffect(projectId: string, session: DomainSession, record: JsonValue): void {
    if (isObject(record)) this.assertDomainRecordPayloadBinding(projectId, record);
    if (isObject(record) && record.$schema === schemaUri("handoff") && typeof record.handoffId === "string") {
      const existing = this.store(projectId).projection(projectId, "handoff", record.handoffId);
      if (existing) { this.assertPendingHandoffRecord(existing.value, record); return; }
    }
    if (isObject(record) && record.$schema === schemaUri("handoff-teachback") && typeof record.handoffId === "string") {
      const terminal = this.store(projectId).projection(projectId, "handoff_decision", record.handoffId);
      if (terminal) { this.assertHandoffDecisionRecord(terminal.value, record); return; }
    }
    if (isObject(record)) {
      const effectKey = domainEffectKey(record);
      if (effectKey && this.store(projectId).commandReceipt(projectId, effectKey)) return;
    }
    this.validateDomainRecord(projectId, session, record);
    this.applyValidatedDomainEffect(projectId, session, record);
  }

  canonicalDomainRecord(projectId: string, record: JsonValue): { event: EventEnvelope } | null {
    if (!isObject(record)) return null;
    const key = domainEffectKey(record);
    if (!key) return null;
    const payloadHash = sha256(record);
    let canonical: { event: EventEnvelope } | null = null;
    for (const accepted of this.submitted(projectId)) {
      if (domainEffectKey(accepted.record) !== key) continue;
      if (sha256(accepted.record) !== payloadHash) throw new Error(`Domain record ${key} conflicts with its accepted canonical payload`);
      canonical ??= { event: accepted.event };
    }
    return canonical;
  }

  private assertDomainRecordPayloadBinding(projectId: string, record: Record<string, JsonValue>): void {
    this.canonicalDomainRecord(projectId, record);
  }

  reconcileAcceptedDomainEffects(projectId: string): { recovered: number; failed: number } {
    let recovered = 0;
    let failed = 0;
    for (const accepted of this.submitted(projectId)) {
      const record = accepted.record;
      if (!isObject(record)) continue;
      const effectKey = domainEffectKey(record);
      if (!effectKey || this.store(projectId).commandReceipt(projectId, effectKey)) continue;
      try {
        const authority = this.recoverDomainAuthority(projectId, accepted.event, record);
        this.applyValidatedDomainEffect(projectId, authority.session, record, authority.task);
        recovered += 1;
      } catch (error) {
        failed += 1;
        const message = error instanceof Error ? error.message : "Domain-effect recovery failed";
        const stored = this.store(projectId).appendIdempotent(`recovery:domain-effect:${accepted.event.eventId}`, {
          $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "recovery.domain_effect_failed", source: "noshd",
          scope: accepted.event.scope, correlationId: accepted.event.eventId, causationId: accepted.event.eventId,
          payload: { recordEventId: accepted.event.eventId, schema: typeof record.$schema === "string" ? record.$schema : null, effectKey, message },
        });
        if (!stored.replayed) this.publish(stored.receipt.event);
      }
    }
    return { recovered, failed };
  }

  submitted(projectId: string, correlationId?: string): Array<{ event: EventEnvelope; record: UnknownRecord }> { return this.store(projectId).replay(projectId).filter((event) => event.type === "record.submitted" && (!correlationId || event.correlationId === correlationId)).map((event) => ({ event, record: event.payload as UnknownRecord })); }
  terminalRecord(projectId: string, taskId: string, expectedSchema?: string): JsonValue | undefined {
    const entries = this.submitted(projectId, `task:${taskId}`).filter((entry) => isTaskTerminalRecord(entry.record));
    if (entries.length > 1) throw new Error(`Task ${taskId} has multiple terminal records`);
    const record = entries[0]?.record;
    if (record && expectedSchema && record.$schema !== schemaUri(expectedSchema)) throw new Error(`Task ${taskId} terminal schema does not match ${schemaUri(expectedSchema)}`);
    return record;
  }
  records(projectId: string, schemaName?: string): UnknownRecord[] { return this.submitted(projectId).map((entry) => entry.record).filter((record) => !schemaName || record.$schema === schemaUri(schemaName)); }
  reviews(projectId: string): JsonValue[] {
    const records = this.records(projectId);
    const reviewIds = [...new Set(records.filter((record) => (isSchema(record, "review-request") || isSchema(record, "review-verdict")) && typeof record.reviewId === "string").map((record) => record.reviewId as string))].sort();
    return reviewIds.map((reviewId) => reviewAuthority(records, reviewId) as unknown as JsonValue);
  }
  isDomainEffectRecord(record: JsonValue): boolean {
    return isObject(record) && domainEffectKey(record) !== null;
  }
  submitDaemonRecord(projectId: string, scope: EventDraft["scope"], correlationId: string, record: JsonValue, idempotencyKey: string): EventEnvelope {
    this.assertWritable(projectId);
    if (scope.projectId !== projectId) throw new Error("Daemon record scope Project does not match the selected Project");
    const uri = (record as { $schema?: unknown }).$schema;
    if (typeof uri !== "string") throw new Error("Daemon record requires $schema");
    const parsed = validateRecord(uri, record);
    if (!parsed.ok) throw new Error(`Invalid daemon record: ${parsed.errors.map((error) => `${error.pointer} ${error.message}`).join("; ")}`);
    const value = parsed.value as Record<string, JsonValue>; const store = this.store(projectId);
    if (this.isDomainEffectRecord(value)) throw new Error("Daemon record path cannot submit domain-effect records without Task authority");
    const draft: EventDraft = { $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "record.submitted", source: "noshd", scope, correlationId, causationId: null, payload: value };
    if (isTaskTerminalRecord(value)) {
      const task = /^task:(tsk_[0-9a-f]{32})$/.exec(correlationId)?.[1];
      if (!task) throw new Error("Daemon terminal records require an exact task:<tsk_...> correlation");
      const stored = store.appendTerminalSubmission(task, "nosh_daemon_record", uri, value, draft);
      if (!stored.replayed) this.publish(stored.receipt.event);
      return stored.receipt.event;
    }
    const replay = store.commandReceipt(projectId, idempotencyKey);
    if (replay) {
      if (replay.event.type !== "record.submitted" || replay.event.scope.projectId !== projectId || replay.event.correlationId !== correlationId || sha256(replay.event.payload) !== sha256(value)) throw new Error("Idempotency key was already used for a different daemon record");
      return replay.event;
    }
    if (uri === schemaUri("evidence")) {
      this.assertEvidenceRecord(projectId, value);
      if (this.records(projectId, "evidence").some((entry) => isObject(entry) && entry.evidenceId === value.evidenceId)) throw new Error(`Evidence ${String(value.evidenceId)} already exists`);
    } else if (uri === schemaUri("claim")) {
      this.assertClaimReferences(projectId, value);
      if (this.records(projectId, "claim").some((entry) => isObject(entry) && entry.claimId === value.claimId && entry.claimVersion === value.claimVersion)) throw new Error(`Claim ${String(value.claimId)} version ${String(value.claimVersion)} already exists`);
    }
    const stored = store.appendIdempotent(idempotencyKey, { $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "record.submitted", source: "noshd", scope, correlationId, causationId: null, payload: value });
    if (!stored.replayed) this.publish(stored.receipt.event);
    return stored.receipt.event;
  }
  submitUserEvidence(projectId: string, input: JsonValue, idempotencyKey: string): EventEnvelope {
    const sanitized = sanitizeEvidenceInput(projectId, input);
    const store = this.store(projectId);
    const inputHash = sha256({ projectId, kind: "user.evidence.create", input: sanitized });
    const prior = store.operationIntents(projectId).find((intent) => intent.idempotencyKey === idempotencyKey);
    const request = prior ? assertedUserRecordIntent(prior, "user.evidence.create", inputHash) : {
      inputHash, record: { $schema: schemaUri("evidence"), schemaVersion: 1, evidenceId: deterministicId("evd", projectId, idempotencyKey), projectId, ...sanitized, createdBy: "user", createdByAgentId: null, createdAt: new Date().toISOString() },
    };
    if (prior) { const replay = this.terminalOperationReplay(projectId, prior); if (replay) return replay; }
    const record = userRecordRequest(request as JsonValue);
    this.assertEvidenceRecord(projectId, record);
    const intent = store.beginOperation(projectId, "user.evidence.create", idempotencyKey, request as JsonValue);
    return this.finishUserRecordOperation(projectId, intent);
  }
  latestClaims(projectId: string): JsonValue[] {
    const latest = new Map<string, { version: number; hash: string; record: Record<string, JsonValue> }>();
    for (const record of this.records(projectId, "claim")) if (isObject(record) && typeof record.claimId === "string" && typeof record.claimVersion === "number" && Number.isInteger(record.claimVersion)) {
      const hash = sha256(record);
      const prior = latest.get(record.claimId);
      if (prior && prior.version === record.claimVersion && prior.hash !== hash) throw new Error(`Ambiguous Claim version ${record.claimId}@${record.claimVersion}`);
      if (!prior || prior.version < record.claimVersion) latest.set(record.claimId, { version: record.claimVersion, hash, record });
    }
    return [...latest.values()].map((entry) => entry.record).sort((left, right) => String(left.claimId).localeCompare(String(right.claimId))) as JsonValue[];
  }
  submitUserClaim(projectId: string, input: JsonValue, idempotencyKey: string): EventEnvelope {
    const sanitized = sanitizeClaimInput(projectId, input);
    const store = this.store(projectId);
    const inputHash = sha256({ projectId, kind: "user.claim.create", input: sanitized });
    const prior = store.operationIntents(projectId).find((intent) => intent.idempotencyKey === idempotencyKey);
    const request = prior ? assertedUserRecordIntent(prior, "user.claim.create", inputHash) : {
      inputHash, record: { $schema: schemaUri("claim"), schemaVersion: 1, claimId: deterministicId("clm", projectId, idempotencyKey), projectId, ...sanitized, claimVersion: 1, supersedesClaimVersion: null, updatedAt: new Date().toISOString() },
    };
    if (prior) { const replay = this.terminalOperationReplay(projectId, prior); if (replay) return replay; }
    const record = userRecordRequest(request as JsonValue);
    this.assertClaimReferences(projectId, record);
    const intent = store.beginOperation(projectId, "user.claim.create", idempotencyKey, request as JsonValue);
    return this.finishUserRecordOperation(projectId, intent);
  }
  editUserClaim(projectId: string, claimId: string, expectedClaimVersion: number, changes: JsonValue, idempotencyKey: string): EventEnvelope {
    const sanitized = sanitizeClaimInput(projectId, changes);
    const store = this.store(projectId);
    const inputHash = sha256({ projectId, kind: "user.claim.edit", claimId, expectedClaimVersion, changes: sanitized });
    const prior = store.operationIntents(projectId).find((intent) => intent.idempotencyKey === idempotencyKey);
    let request: Record<string, JsonValue>;
    if (prior) {
      request = assertedUserRecordIntent(prior, "user.claim.edit", inputHash);
      const replay = this.terminalOperationReplay(projectId, prior);
      if (replay) return replay;
    } else {
      if (!Number.isInteger(expectedClaimVersion) || expectedClaimVersion < 1) throw new Error("Claim edit requires a positive expected claim version");
      const current = this.latestClaims(projectId).find((claim) => isObject(claim) && claim.claimId === claimId);
      if (!current || !isObject(current)) throw new Error("Unknown Claim");
      if (current.claimVersion !== expectedClaimVersion) throw new Error(`Claim version conflict: expected ${expectedClaimVersion}, current ${String(current.claimVersion)}`);
      request = { inputHash, record: { ...current, ...sanitized, $schema: schemaUri("claim"), schemaVersion: 1, claimId, projectId, claimVersion: expectedClaimVersion + 1, supersedesClaimVersion: expectedClaimVersion, updatedAt: new Date().toISOString() } };
    }
    const record = userRecordRequest(request);
    this.assertClaimReferences(projectId, record);
    const intent = store.beginOperation(projectId, "user.claim.edit", idempotencyKey, request);
    return this.finishUserRecordOperation(projectId, intent);
  }
  private assertClaimReferences(projectId: string, record: Record<string, JsonValue>): void {
    const parsed = validateRecord(schemaUri("claim"), record);
    if (!parsed.ok) throw new Error(`Invalid Claim: ${parsed.errors.map((error) => `${error.pointer} ${error.message}`).join("; ")}`);
    const claim = parsed.value as Record<string, JsonValue>;
    const groups = ["supportingEvidenceIds", "contradictingEvidenceIds", "qualifyingEvidenceIds"].map((name) => claim[name] as string[]);
    const ids = groups.flat();
    if (new Set(ids).size !== ids.length) throw new Error("Claim Evidence IDs cannot be duplicated across polarity arrays");
    const evidence = new Set(this.records(projectId, "evidence").filter(isObject).filter((entry) => entry.projectId === projectId).map((entry) => String(entry.evidenceId)));
    if (ids.some((id) => !evidence.has(id))) throw new Error("Claim references Evidence that does not exist in this Project");
    const requiresReview = ["supported", "partially_supported", "qualified", "not_supported", "contradicted"].includes(String(claim.status));
    if (requiresReview && claim.requiredReviewId === null) throw new Error(`Claim status ${String(claim.status)} requires an exact Claim Review`);
    if (claim.requiredReviewId !== null) {
      const review = exactReviewJoin(this.records(projectId), String(claim.requiredReviewId));
      if (!review || review.request.reviewType !== "claim" || !isObject(review.request.target) || review.request.target.targetId !== claim.claimId || review.request.target.targetVersion !== claim.claimVersion || review.verdict.verdict !== "PASS" || !ids.every((id) => stringValues(review.verdict.reviewedEvidenceIds).includes(id))) throw new Error("Claim required Review must exactly target this Claim version and cover every referenced Evidence");
    }
  }
  private assertEvidenceRecord(projectId: string, record: Record<string, JsonValue>): void {
    const parsed = validateRecord(schemaUri("evidence"), record);
    if (!parsed.ok) throw new Error(`Invalid Evidence: ${parsed.errors.map((error) => `${error.pointer} ${error.message}`).join("; ")}`);
    const evidence = parsed.value as Record<string, JsonValue>;
    if (evidence.projectId !== projectId) throw new Error("Evidence Project does not match submission Project");
    const quality = evidence.quality as Record<string, JsonValue>;
    const reviewId = quality.reviewId;
    if (quality.status !== "unreviewed") {
      const review = typeof reviewId === "string" ? exactReviewJoin(this.records(projectId), reviewId) : null;
      const correctDisposition = quality.status === "reviewed" ? review?.verdict.verdict === "PASS" : Boolean(review && review.verdict.verdict !== "PASS");
      if (!review || !correctDisposition || !stringValues(review.verdict.reviewedEvidenceIds).includes(String(evidence.evidenceId))) throw new Error(`${String(quality.status)} Evidence requires an exact Review covering this Evidence with matching disposition`);
    }
  }
  private terminalOperationReplay(projectId: string, intent: OperationIntent): EventEnvelope | null {
    if (intent.state === "pending") return null;
    if (intent.state === "failed") throw new Error(intent.error ?? "Operation previously failed");
    const receipt = this.store(projectId).commandReceipt(projectId, intent.idempotencyKey);
    if (!receipt) throw new Error("Completed operation is missing its command receipt");
    return receipt.event;
  }
  private finishUserRecordOperation(projectId: string, intent: OperationIntent): EventEnvelope {
    const record = userRecord(intent);
    if (intent.operationType === "user.evidence.create") this.assertEvidenceRecord(projectId, record);
    else if (intent.operationType === "user.claim.create" || intent.operationType === "user.claim.edit") this.assertClaimReferences(projectId, record);
    else throw new Error("Unsupported user-record operation");
    const id = intent.operationType === "user.evidence.create" ? String(record.evidenceId) : String(record.claimId);
    const event = this.submitDaemonRecord(projectId, projectScope(projectId), id, record as JsonValue, intent.idempotencyKey);
    this.store(projectId).completeOperation(projectId, intent.intentId, { eventId: event.eventId });
    return event;
  }
  beginExternalOperation(projectId: string, operationType: string, idempotencyKey: string, request: JsonValue): OperationIntent { return this.store(projectId).beginOperation(projectId, operationType, idempotencyKey, request); }
  completeExternalOperation(projectId: string, intentId: string, result: JsonValue): OperationIntent { return this.store(projectId).completeOperation(projectId, intentId, result); }
  pendingExternalOperations(projectId: string, operationType?: string): OperationIntent[] { return this.store(projectId).operationIntents(projectId, "pending").filter((intent) => !operationType || intent.operationType === operationType); }
  scopedEvents(projectId: string, missionId: string, type?: string): EventEnvelope[] { return this.store(projectId).replay(projectId).filter((event) => event.scope.missionId === missionId && (!type || event.type === type)); }
  missionBudgetUse(projectId: string, missionId: string): MissionBudgetUse { const mission = this.mission(projectId, missionId); const events = this.scopedEvents(projectId, missionId); const startedAt = events.find((event) => event.type === "mission.running")?.timestamp ?? mission.value.createdAt; return { ...eventResourceUse(events), wallClockSeconds: Math.max(0, Math.floor((Date.now() - Date.parse(startedAt)) / 1000)), diskBytes: directoryBytes(join(this.project(projectId).repositoryRoot, ".nosh")) }; }
  missionCompletionBasis(projectId: string, missionId: string): MissionCompletionBasis {
    const mission = this.mission(projectId, missionId);
    const records = this.submitted(projectId).filter((entry) => entry.event.scope.missionId === missionId);
    const packets = records.filter((entry) => isSchema(entry.record, "task-packet")).map((entry) => entry.record as Record<string, JsonValue>).filter((packet) => packet.assignedRole !== "reviewer" && isObject(packet.scope) && packet.scope.missionId === missionId);
    const completions: MissionCompletionBasis["completions"] = [];
    const criteria: MissionCompletionBasis["criteria"] = [];
    const issues: string[] = [];
    const taskResults = new Map<string, { packet: Record<string, JsonValue>; completion: Record<string, JsonValue>; request: Record<string, JsonValue>; verdict: Record<string, JsonValue> }>();
    for (const node of mission.value.nodes.filter((node) => node.required && node.state === "accepted" && !["reviewer", "claim_review", "final_review", "approval"].includes(node.type))) {
      const candidates: Array<{ packet: Record<string, JsonValue>; completion: Record<string, JsonValue>; request: Record<string, JsonValue>; verdict: Record<string, JsonValue> }> = [];
      for (const packet of packets.filter((entry) => isObject(entry.scope) && entry.scope.graphNodeId === node.id)) {
        if (!validateRecord(schemaUri("task-packet"), packet).ok || packet.attempt !== node.attempt || typeof packet.taskId !== "string" || typeof packet.assignedAgentId !== "string" || typeof packet.responseSchema !== "string") continue;
        const completionRecords = records.filter((entry) => entry.event.correlationId === `task:${packet.taskId}` && isObject(entry.record) && entry.record.$schema === packet.responseSchema).map((entry) => entry.record as Record<string, JsonValue>);
        if (completionRecords.length !== 1 || !validateRecord(packet.responseSchema, completionRecords[0]!).ok || completionRecords[0]!.taskOutcome !== "completed" || completionRecords[0]!.readyForReview !== true) continue;
        // The reviewer has its own Task ID. Bind its request to the worker packet by
        // the daemon-issued contract reference, not by the review session's correlation.
        const requests = records.map((entry) => entry.record).filter((entry) => isObject(entry)
          && entry.$schema === schemaUri("review-request") && entry.reviewType === "task"
          && isObject(entry.target) && entry.target.targetType === "graph_node" && entry.target.targetId === node.id
          && isArray(entry.producerAgentIds) && stringValues(entry.producerAgentIds).includes(String(packet.assignedAgentId))
          && isArray(entry.contractRefs) && entry.contractRefs.filter(isObject).some((reference) => reference.kind === "contract_task.packet" && reference.id === packet.taskId && reference.hash === sha256(packet)));
        if (requests.length !== 1 || typeof requests[0]!.reviewId !== "string") continue;
        const joined = exactReviewJoin(records.map((entry) => entry.record), requests[0]!.reviewId);
        if (!joined || joined.request !== requests[0] || !this.passingReview(projectId, node.id, "task", Number((joined.request.target as Record<string, JsonValue>).targetVersion), String(joined.request.reviewId))) continue;
        candidates.push({ packet, completion: completionRecords[0]!, request: joined.request, verdict: joined.verdict });
      }
      if (candidates.length !== 1) {
        issues.push(candidates.length ? `accepted node ${node.id} has ambiguous independently reviewed Task Packet mappings` : `accepted node ${node.id} lacks one completed independently reviewed authoritative Task Packet`);
        continue;
      }
      const task = candidates[0]!;
      taskResults.set(task.packet.taskId as string, task);
      const references = completionReferences(task.completion);
      completions.push({ nodeId: node.id, taskId: task.packet.taskId as string, agentId: task.packet.assignedAgentId as string, completion: task.completion, review: task.verdict, artifactIds: references.artifactIds, evidenceIds: references.evidenceIds, validatorRunIds: [] });
    }
    for (const contractCriterion of missionCriteria(mission.value)) {
      const candidates = [...taskResults.values()].filter((task) => isArray(task.packet.acceptanceCriteria) && task.packet.acceptanceCriteria.filter(isObject).some((criterion) => criterion.criterionId === contractCriterion.criterionId));
      if (candidates.length !== 1) {
        issues.push(candidates.length ? `Mission contract criterion ${contractCriterion.criterionId} has ambiguous accepted-node Task Packet mappings` : `Mission contract criterion ${contractCriterion.criterionId} is absent from an independently reviewed accepted-node Task Packet`);
        continue;
      }
      const task = candidates[0]!;
      const completionCriteria = isArray(task.completion.criteria) ? task.completion.criteria.filter(isObject).filter((criterion) => criterion.criterionId === contractCriterion.criterionId) : [];
      const reviewCriteria = isArray(task.verdict.criteria) ? task.verdict.criteria.filter(isObject).filter((criterion) => criterion.criterionId === contractCriterion.criterionId) : [];
      if (completionCriteria.length !== 1 || reviewCriteria.length !== 1 || completionCriteria[0]!.workerClaim !== "satisfied" || !isArray(completionCriteria[0]!.validatorRunIds) || !completionCriteria[0]!.validatorRunIds.length || !completionCriteria[0]!.validatorRunIds.every((id) => typeof id === "string") || reviewCriteria[0]!.status !== "PASS") {
        issues.push(`Mission contract criterion ${contractCriterion.criterionId} lacks one satisfied worker claim, validator set, and exact independent PASS Review`);
        continue;
      }
      const validatorRunIds = completionCriteria[0]!.validatorRunIds as string[];
      const references = completionReferences(task.completion);
      criteria.push({ criterionId: contractCriterion.criterionId, statement: contractCriterion.statement, taskId: task.packet.taskId as string, reviewId: task.verdict.reviewId as string, validatorRunIds, artifactIds: references.artifactIds, evidenceIds: references.evidenceIds });
      const completion = completions.find((entry) => entry.taskId === task.packet.taskId);
      if (completion) completion.validatorRunIds.push(...validatorRunIds);
    }
    const requestedArtifactIds = [...new Set(completions.flatMap((completion) => completion.artifactIds))];
    const artifactIds = requestedArtifactIds.filter((id) => artifactExists(this.project(projectId), id));
    for (const artifactId of requestedArtifactIds) if (!artifactIds.includes(artifactId)) issues.push(`Mission completion references missing Artifact ${artifactId}`);
    const requestedEvidenceIds = [...new Set(completions.flatMap((completion) => completion.evidenceIds))];
    const evidenceIds = requestedEvidenceIds.filter((id) => this.records(projectId, "evidence").some((record) => isObject(record) && record.projectId === projectId && record.evidenceId === id && validateRecord(schemaUri("evidence"), record).ok));
    for (const evidenceId of requestedEvidenceIds) if (!evidenceIds.includes(evidenceId)) issues.push(`Mission completion references missing Project Evidence ${evidenceId}`);
    const scopedClaimIds = new Set(records.filter((entry) => isSchema(entry.record, "claim") && typeof entry.record.claimId === "string").map((entry) => entry.record.claimId as string));
    const claims = latestScopedClaims(this.submitted(projectId), issues).filter((claim) => scopedClaimIds.has(String(claim.claimId)) || [...stringValues(claim.supportingEvidenceIds), ...stringValues(claim.contradictingEvidenceIds), ...stringValues(claim.qualifyingEvidenceIds)].some((evidenceId) => evidenceIds.includes(evidenceId)));
    const evidenceById = new Map(this.records(projectId, "evidence").filter((entry) => isObject(entry) && entry.projectId === projectId && typeof entry.evidenceId === "string" && validateRecord(schemaUri("evidence"), entry).ok).map((entry) => [entry.evidenceId as string, entry]));
    const unresolvedClaimIds: string[] = [];
    for (const claim of claims) {
      const claimIssues = claimResolutionIssues(claim, evidenceById, this.submitted(projectId).map((entry) => entry.record));
      if (claimIssues.length) { unresolvedClaimIds.push(String(claim.claimId)); issues.push(...claimIssues); }
    }
    const terminalNodes = new Set(mission.value.nodes.filter((node) => ["accepted", "waived", "superseded"].includes(node.state)).map((node) => node.id));
    const blockers = records.filter((entry) => isSchema(entry.record, "blocker")).map((entry) => entry.record as Record<string, JsonValue>).filter((blocker) => blocker.scopeType !== "graph_node" || typeof blocker.scopeId !== "string" || !terminalNodes.has(blocker.scopeId)).map((blocker) => String(blocker.blockerId));
    const scopedRecordValues = records.map((entry) => entry.record);
    const reviewIds = [...new Set(scopedRecordValues.filter((record) => (isSchema(record, "review-request") || isSchema(record, "review-verdict")) && typeof record.reviewId === "string").map((record) => record.reviewId as string))];
    const exactReviews = reviewIds.map((reviewId) => reviewAuthority(scopedRecordValues, reviewId)).filter((review): review is { reviewId: string; request: Record<string, JsonValue>; verdict: Record<string, JsonValue>; state: "resolved"; issues: string[] } => review.state === "resolved" && review.request !== null && review.verdict !== null);
    const currentReviews = new Map<string, Array<{ request: Record<string, JsonValue>; verdict: Record<string, JsonValue> }>>();
    for (const review of exactReviews) {
      const target = review.request.target as Record<string, JsonValue>; const key = `${String(review.request.reviewType)}:${String(target.targetType)}:${String(target.targetId)}`; const version = Number(target.targetVersion); const current = currentReviews.get(key) ?? []; const currentVersion = current.length ? Number((current[0]!.request.target as Record<string, JsonValue>).targetVersion) : -1;
      if (version > currentVersion) currentReviews.set(key, [{ request: review.request, verdict: review.verdict }]); else if (version === currentVersion) current.push({ request: review.request, verdict: review.verdict });
    }
    const ambiguousReviewIds = [...currentReviews.entries()].filter(([, reviews]) => reviews.length !== 1).map(([key, reviews]) => `ambiguous_review:${key}@${String((reviews[0]!.request.target as Record<string, JsonValue>).targetVersion)}`);
    for (const issue of ambiguousReviewIds) issues.push(`Mission completion has ${issue}`);
    const reviewDefects = [...currentReviews.values()].filter((reviews) => reviews.length === 1).flatMap((reviews) => isArray(reviews[0]!.verdict.defects) ? reviews[0]!.verdict.defects.filter(isObject).filter((defect) => defect.blocking === true || defect.severity === "blocking" || defect.severity === "major").map((defect) => String(defect.defectId)) : []);
    const openDefectIds = [...new Set([...blockers, ...ambiguousReviewIds, ...reviewDefects])];
    const validatorRunIds = [...new Set(criteria.flatMap((criterion) => criterion.validatorRunIds))];
    if (criteria.length !== mission.value.successCriteria.length) issues.push("every Mission contract criterion requires an exact task, validator, and independent Review mapping");
    if (!artifactIds.length) issues.push("accepted task completions provide no stored deliverable artifacts");
    if (unresolvedClaimIds.length) issues.push(`unresolved claims: ${unresolvedClaimIds.join(", ")}`);
    if (openDefectIds.length) issues.push(`open blocking or major defects: ${openDefectIds.join(", ")}`);
    return { completions, criteria, artifactIds, evidenceIds, validatorRunIds, claims, unresolvedClaimIds, openDefectIds, issues: [...new Set(issues)] };
  }
  autoresearchBudgetUse(projectId: string, autoresearchId: string): AutoresearchBudgetUse { const execution = this.autoresearchExecution(projectId, autoresearchId); const events = this.store(projectId).replay(projectId).filter((event) => event.scope.autoresearchId === autoresearchId); const records = this.submitted(projectId).filter((entry) => entry.event.scope.autoresearchId === autoresearchId).map((entry) => entry.record as { $schema?: string; experimentId?: string; sizeBytes?: number }); const startedAt = events.find((event) => event.type === "autoresearch.running")?.timestamp ?? execution.value.createdAt; const experimentIds = [...new Set(records.filter((record) => record.$schema === schemaUri("experiment-proposal") && record.experimentId).map((record) => record.experimentId!))]; const root = this.project(projectId).repositoryRoot; const diskBytes = records.filter((record) => record.$schema === schemaUri("artifact")).reduce((sum, record) => sum + Math.max(0, Number(record.sizeBytes ?? 0)), 0) + directoryBytes(join(root, ".nosh", "autoresearch", autoresearchId)) + experimentIds.reduce((sum, id) => sum + directoryBytes(join(root, ".nosh", "worktrees", `wt_${id.slice(4)}`)), 0) + directoryBytes(join(root, ".nosh", "worktrees", `integration_${autoresearchId.slice(3)}`)); return { ...eventResourceUse(events), wallClockSeconds: Math.max(0, Math.floor((Date.now() - Date.parse(startedAt)) / 1000)), diskBytes, experiments: experimentIds.length, rounds: execution.value.currentRound }; }
  missionCompletionIssues(projectId: string, missionId: string): string[] { const mission = this.mission(projectId, missionId); const issues: string[] = []; for (const node of mission.value.nodes) if (node.required && !["accepted", "waived", "superseded"].includes(node.state)) issues.push(`required node ${node.id} is ${node.state}`); const use = this.missionBudgetUse(projectId, missionId); if (use.modelTokens > mission.value.budgets.maximumModelTokens) issues.push("Mission model-token budget is exceeded"); if (use.wallClockSeconds > mission.value.budgets.maximumWallClockSeconds) issues.push("Mission wall-clock budget is exceeded"); if (use.gpuSeconds > mission.value.budgets.maximumGpuSeconds) issues.push("Mission GPU budget is exceeded"); if (use.diskBytes > mission.value.budgets.maximumDiskBytes) issues.push("Mission disk budget is exceeded"); const evidenceIds = new Set(this.records(projectId, "evidence").map((record) => (record as { evidenceId?: string }).evidenceId).filter((id): id is string => Boolean(id))); for (const record of this.records(projectId, "claim")) { const claim = record as { claimId?: string; supportingEvidenceIds?: string[]; contradictingEvidenceIds?: string[]; qualifyingEvidenceIds?: string[]; limitations?: string[] }; const references = [...(claim.supportingEvidenceIds ?? []), ...(claim.contradictingEvidenceIds ?? []), ...(claim.qualifyingEvidenceIds ?? [])]; if (!references.length && !(claim.limitations ?? []).length) issues.push(`claim ${claim.claimId ?? "unknown"} is unsupported and unqualified`); for (const id of references) if (!evidenceIds.has(id)) issues.push(`claim ${claim.claimId ?? "unknown"} references missing evidence ${id}`); } const artifacts = new ArtifactStore(join(this.project(projectId).repositoryRoot, ".nosh", "artifacts")); for (const record of this.records(projectId, "artifact")) { const id = (record as { artifactId?: string }).artifactId; if (id) try { artifacts.resolve(id, (record as { version?: number }).version); } catch { issues.push(`artifact ${id} does not resolve to its stored hash`); } } return [...new Set(issues)]; }
  readPaper(projectId: string): { markdown: string; bibliography: string; markdownHash: string; bibliographyHash: string; version: string } {
    const root = this.project(projectId).repositoryRoot;
    const markdown = readText(join(root, "docs", "paper.md"));
    const bibliography = readText(join(root, "docs", "paper.bib"));
    const markdownHash = sha256(markdown); const bibliographyHash = sha256(bibliography);
    return { markdown, bibliography, markdownHash, bibliographyHash, version: sha256({ markdownHash, bibliographyHash }) };
  }
  savePaper(projectId: string, markdown: string, bibliography: string, expected: { markdownHash: string; bibliographyHash: string; version: string }, idempotencyKey: string): EventEnvelope {
    this.assertWritable(projectId);
    if (Buffer.byteLength(markdown) > 2_000_000 || Buffer.byteLength(bibliography) > 2_000_000) throw new Error("Paper Markdown and bibliography must each be at most 2 MB");
    const store = this.store(projectId);
    const candidate = { markdown, bibliography, markdownHash: sha256(markdown), bibliographyHash: sha256(bibliography), expected, bytes: { markdown: Buffer.byteLength(markdown), bibliography: Buffer.byteLength(bibliography) } } as JsonValue;
    const prior = store.operationIntents(projectId).find((intent) => intent.idempotencyKey === idempotencyKey);
    if (!prior && store.operationIntents(projectId, "pending").some((intent) => intent.operationType === "paper.replace")) throw new Error("A paper replacement is already pending");
    if (prior && (prior.operationType !== "paper.replace" || sha256(prior.request) !== sha256(candidate))) throw new Error("Idempotency key was already used for a different paper replacement");
    if (prior) { const replay = this.terminalOperationReplay(projectId, prior); if (replay) return replay; }
    else {
      const current = this.readPaper(projectId);
      if (current.markdownHash !== expected.markdownHash || current.bibliographyHash !== expected.bibliographyHash || current.version !== expected.version) throw new Error("Paper version conflict");
    }
    return this.finishPaperOperation(projectId, store.beginOperation(projectId, "paper.replace", idempotencyKey, prior?.request ?? candidate));
  }
  reconcilePendingOperations(projectId: string): number {
    let completed = 0;
    for (const intent of this.store(projectId).operationIntents(projectId, "pending")) {
      if (intent.operationType === "paper.replace") this.finishPaperOperation(projectId, intent);
      else if (["user.evidence.create", "user.claim.create", "user.claim.edit"].includes(intent.operationType)) this.finishUserRecordOperation(projectId, intent);
      else continue;
      completed += 1;
    }
    return completed;
  }
  exportPaper(projectId: string): JsonValue { this.assertWritable(projectId); const root = this.project(projectId).repositoryRoot; return exportPaper(join(root, "docs", "paper.md"), join(root, "docs", "paper.bib"), join(root, ".nosh", "paper-export")) as unknown as JsonValue; }


  private assertLeaseTransition(next: GraphNode["state"], lease: NonNullable<GraphNode["lease"]> | undefined, expectedVersion: number): void {
    if ((next === "leased") !== Boolean(lease)) throw new Error("Only a leased transition may carry a lease, and leased transitions require one");
    if (lease && (!lease.leaseId || !lease.ownerId || lease.version !== expectedVersion || !Number.isFinite(Date.parse(lease.expiresAt)) || Date.parse(lease.expiresAt) <= Date.now())) throw new Error("Lease must name an owner, match the projection version, and expire in the future");
  }

  private recoverDomainAuthority(projectId: string, event: EventEnvelope, record: Record<string, JsonValue>): { session: DomainSession; task?: TaskAuthority } {
    const scope = event.scope;
    if (!scope.agentId) throw new Error("Accepted domain record has no durable submitting agent");
    if (record.$schema === schemaUri("handoff-teachback")) {
      if (typeof record.toAgentId !== "string" || record.toAgentId !== scope.agentId || typeof record.handoffId !== "string") throw new Error("Teach-back recipient does not match the durable event scope");
      const handoff = this.pendingHandoff(projectId, record.handoffId, scope, record.toAgentId);
      return {
        session: { agentId: handoff.recipientAgentId, role: handoff.source.role, taskId: null, missionId: handoff.scope.missionId, directionId: handoff.scope.directionId, autoresearchId: handoff.scope.autoresearchId, experimentId: handoff.scope.experimentId, runId: handoff.scope.runId, jobId: handoff.scope.jobId },
      };
    }
    if (record.$schema === schemaUri("graph-change-proposal")) {
      const role = record.proposerRole;
      if ((record.scopeType === "mission" && record.scopeId !== scope.missionId) || (record.scopeType === "direction" && record.scopeId !== scope.directionId) || (role !== "mission_director" && role !== "research_director" && role !== "user")) throw new Error("Graph proposal does not match its durable accepted scope");
      return { session: { agentId: scope.agentId, role: role as DomainSession["role"], taskId: null, missionId: scope.missionId, directionId: scope.directionId, autoresearchId: scope.autoresearchId, experimentId: scope.experimentId, runId: scope.runId, jobId: scope.jobId } };
    }
    const recordAgentId = record.$schema === schemaUri("progress-update") ? record.agentId : record.$schema === schemaUri("handoff") ? record.fromAgentId : record.$schema === schemaUri("handoff-teachback") ? record.toAgentId : scope.agentId;
    if (recordAgentId !== scope.agentId) throw new Error("Accepted domain record actor does not match durable event scope");
    const taskId = typeof record.taskId === "string" ? record.taskId : null;
    const graphNodeId = isObject(record.goalStack) && typeof record.goalStack.currentGraphNodeId === "string"
      ? record.goalStack.currentGraphNodeId
      : isObject(record.understoodGoalStack) && typeof record.understoodGoalStack.currentGraphNodeId === "string"
        ? record.understoodGoalStack.currentGraphNodeId
        : null;
    const packets = this.submitted(projectId)
      .map((entry) => entry.record)
      .filter((entry): entry is Record<string, JsonValue> => isSchema(entry, "task-packet") && isObject(entry))
      .filter((packet) => packet.assignedAgentId === scope.agentId && isObject(packet.scope) && packet.scope.projectId === projectId && packet.scope.missionId === scope.missionId && packet.scope.directionId === scope.directionId && packet.scope.autoresearchId === scope.autoresearchId && packet.scope.experimentId === scope.experimentId)
      .filter((packet) => taskId ? packet.taskId === taskId : graphNodeId ? isObject(packet.scope) && packet.scope.graphNodeId === graphNodeId : false);
    if (packets.length !== 1) throw new Error("Accepted domain record cannot be bound to exactly one durable Task Packet");
    const packet = packets[0]!;
    if (typeof packet.taskId !== "string" || typeof packet.assignedRole !== "string" || !isObject(packet.scope) || !isObject(packet.lease) || typeof packet.lease.leaseId !== "string" || !isObject(packet.observedVersions) || !isObject(packet.budget) || typeof packet.budget.maximumModelTokens !== "number" || typeof packet.budget.maximumWallClockSeconds !== "number" || !isObject(packet.permissions) || typeof packet.permissions.delegation !== "string" || typeof packet.scope.graphNodeId !== "string") throw new Error("Durable Task Packet lacks recoverable authority");
    const leaseProjectionVersion = scope.missionId ? packet.observedVersions.leaseVersion : scope.directionId ? packet.observedVersions.directionProjectionVersion : undefined;
    if (!Number.isInteger(leaseProjectionVersion) || Number(leaseProjectionVersion) < 1) throw new Error("Durable Task Packet lacks its exact lease projection version");
    const session: DomainSession = {
      agentId: scope.agentId,
      role: packet.assignedRole as DomainSession["role"],
      taskId: packet.taskId,
      missionId: scope.missionId,
      directionId: scope.directionId,
      autoresearchId: scope.autoresearchId,
      experimentId: scope.experimentId,
      runId: scope.runId,
      jobId: scope.jobId,
    };
    return {
      session,
      task: {
        graphNodeId: packet.scope.graphNodeId,
        missionId: scope.missionId,
        directionId: scope.directionId,
        leaseId: packet.lease.leaseId,
        leaseProjectionVersion: Number(leaseProjectionVersion),
        agentId: scope.agentId,
        maximumModelTokens: packet.budget.maximumModelTokens,
        maximumWallClockSeconds: packet.budget.maximumWallClockSeconds,
        delegation: packet.permissions.delegation === "request_only" ? "request_only" : "disabled",
      },
    };
  }

  private applyValidatedDomainEffect(projectId: string, session: DomainSession, record: JsonValue, recoveredTask?: TaskAuthority): void {
    this.assertWritable(projectId);
    if (!isObject(record)) return;
    const key = domainEffectKey(record);
    if (key && this.store(projectId).commandReceipt(projectId, key)) return;
    if (record.$schema === schemaUri("progress-update")) {
      this.domainEvent(projectId, session, "task.progress_recorded", `domain-progress:${String(record.progressId)}`, record);
      return;
    }
    if (record.$schema === schemaUri("blocker")) {
      const packet = recoveredTask ?? this.taskAuthority(projectId, session, String(record.taskId));
      if (packet.missionId) {
        this.transitionMissionNode(projectId, packet.missionId, this.mission(projectId, packet.missionId).version, packet.graphNodeId, "blocked", `domain-blocker:${String(record.blockerId)}`);
      } else if (packet.directionId) {
        this.transitionDirectionNode(projectId, packet.directionId, this.direction(projectId, packet.directionId).version, packet.graphNodeId, "blocked", `domain-blocker:${String(record.blockerId)}`);
      } else {
        this.domainEvent(projectId, session, "task.blocked", `domain-blocker:${String(record.blockerId)}`, record);
      }
      return;
    }
    if (record.$schema === schemaUri("graph-change-proposal")) {
      const operations = record.scopeType === "mission" ? graphOperations(record.operations, this.mission(projectId, String(record.scopeId)).value.nodes) : graphOperations(record.operations, this.direction(projectId, String(record.scopeId)).value.nodes);
      if (record.approvalRequired !== true) {
        if (record.scopeType === "mission") this.mutateMissionGraph(projectId, String(record.scopeId), this.mission(projectId, String(record.scopeId)).version, Number(record.baseGraphVersion), operations, String(record.rationale), stringValues(record.evidenceIds), `domain-graph:${String(record.proposalId)}`);
        else this.mutateDirectionGraph(projectId, String(record.scopeId), this.direction(projectId, String(record.scopeId)).version, Number(record.baseGraphVersion), operations, String(record.rationale), stringValues(record.evidenceIds), `domain-graph:${String(record.proposalId)}`);
      } else {
        const pending = this.store(projectId).mutateProjection(`domain-graph:${String(record.proposalId)}`, 0, { $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "graph.proposal_pending", source: "noshd", scope: eventScope(projectId, session), correlationId: String(record.proposalId), causationId: null, payload: { proposalId: String(record.proposalId), contractImpact: record.contractImpact } }, { entityType: "graph_proposal", entityId: String(record.proposalId), state: "pending", value: { record, scope: eventScope(projectId, session), submittedAt: new Date().toISOString() } });
        if (!pending.replayed) this.publish(pending.event);
      }
      return;
    }
    if (record.$schema === schemaUri("delegation-request")) {
      this.domainEvent(projectId, session, "delegation.requested", `domain-delegation:${String(record.requestId)}`, record);
      return;
    }
    if (record.$schema === schemaUri("handoff")) {
      const source = recoveredTask ?? this.taskAuthority(projectId, session, session.taskId ?? "");
      this.persistPendingHandoff(projectId, session, record, source);
      return;
    }
    if (record.$schema === schemaUri("handoff-teachback")) {
      const handoff = this.pendingHandoff(projectId, String(record.handoffId), eventScope(projectId, session), String(record.toAgentId));
      this.applyHandoffDecision(projectId, handoff, record);
      return;
    }
  }

  private taskAuthority(projectId: string, session: DomainSession, taskId: string): TaskAuthority {
    if (!session.taskId || taskId !== session.taskId) throw new Error("Domain record task does not match the active Pi session");
    const packets = this.submitted(projectId).map((entry) => entry.record).filter((entry): entry is Record<string, JsonValue> => isSchema(entry, "task-packet") && isObject(entry) && entry.taskId === taskId);
    if (packets.length !== 1) throw new Error("Task authority requires exactly one matching durable Task Packet");
    const packet = packets[0]!;
    if (packet.assignedAgentId !== session.agentId || packet.assignedRole !== session.role || !isObject(packet.scope) || packet.scope.projectId !== projectId || !sameTaskScope(packet.scope, session) || !isObject(packet.lease) || typeof packet.lease.leaseId !== "string" || typeof packet.lease.expiresAt !== "string" || !isObject(packet.observedVersions) || typeof packet.scope.graphNodeId !== "string" || !isObject(packet.budget) || typeof packet.budget.maximumModelTokens !== "number" || typeof packet.budget.maximumWallClockSeconds !== "number" || !isObject(packet.permissions) || (packet.permissions.delegation !== "disabled" && packet.permissions.delegation !== "request_only")) throw new Error("No authoritative Task Packet matches the active Pi session");
    const leaseProjectionVersion = session.missionId ? packet.observedVersions.leaseVersion : session.directionId ? packet.observedVersions.directionProjectionVersion : undefined;
    if (!Number.isInteger(leaseProjectionVersion) || Number(leaseProjectionVersion) < 1) throw new Error("Task Packet lacks the exact lease projection version");
    if (Date.parse(packet.lease.expiresAt) <= Date.now()) throw new Error("Task lease has expired");
    if (session.missionId && packet.observedVersions.missionGraphVersion !== this.mission(projectId, session.missionId).value.graphVersion) throw new Error("Task packet observes a stale Mission graph version");
    if (session.directionId && packet.observedVersions.directionGraphVersion !== this.direction(projectId, session.directionId).value.graphVersion) throw new Error("Task packet observes a stale Direction graph version");
    return { graphNodeId: packet.scope.graphNodeId, missionId: session.missionId, directionId: session.directionId, leaseId: packet.lease.leaseId, leaseProjectionVersion: Number(leaseProjectionVersion), agentId: session.agentId, maximumModelTokens: packet.budget.maximumModelTokens, maximumWallClockSeconds: packet.budget.maximumWallClockSeconds, delegation: packet.permissions.delegation };
  }

  private assertLiveLease(projectId: string, packet: TaskAuthority, session: DomainSession): void {
    const nodes = packet.missionId ? this.mission(projectId, packet.missionId).value.nodes : packet.directionId ? this.direction(projectId, packet.directionId).value.nodes : [];
    const node = nodes.find((entry) => entry.id === packet.graphNodeId);
    if (!node || !node.lease || node.lease.leaseId !== packet.leaseId || node.lease.ownerId !== session.agentId || !["leased", "working"].includes(node.state)) throw new Error("Blocker requires the caller's live graph lease");
  }
  private assertWorkingLease(projectId: string, packet: TaskAuthority, session: DomainSession): void {
    const nodes = packet.missionId ? this.mission(projectId, packet.missionId).value.nodes : packet.directionId ? this.direction(projectId, packet.directionId).value.nodes : [];
    const node = nodes.find((entry) => entry.id === packet.graphNodeId);
    if (!node || !node.lease || node.lease.leaseId !== packet.leaseId || node.lease.ownerId !== session.agentId || node.state !== "working") throw new Error("Progress requires the caller's current working graph lease");
  }
  private persistPendingHandoff(projectId: string, session: DomainSession, record: Record<string, JsonValue>, source: TaskAuthority): PendingHandoff {
    const handoffId = String(record.handoffId); const store = this.store(projectId);
    const existing = store.projection(projectId, "handoff", handoffId);
    if (existing) { this.assertPendingHandoffRecord(existing.value, record); return pendingHandoffValue(existing.value); }
    const current = source.missionId ? this.mission(projectId, source.missionId) : source.directionId ? this.direction(projectId, source.directionId) : null;
    const node = current?.value.nodes.find((entry) => entry.id === source.graphNodeId);
    if (!current || current.version !== source.leaseProjectionVersion || !node || !node.lease || node.lease.leaseId !== source.leaseId || node.lease.ownerId !== source.agentId || !["leased", "working"].includes(node.state) || !session.taskId) throw new Error("Handoff source Task Packet is no longer the exact live lease authority");
    const value: PendingHandoff = {
      acceptedHandoffHash: sha256(record), handoffId, logicalOwnerId: String(record.logicalOwnerId), recipientAgentId: String(record.toAgentId), record,
      scope: eventScope(projectId, session),
      source: { ...source, taskId: session.taskId, role: session.role, projectionVersion: current.version },
    };
    const stored = store.mutateProjection(`domain-handoff:${handoffId}`, 0, {
      $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "handoff.pending", source: "noshd", scope: value.scope, correlationId: handoffId, causationId: null,
      payload: { handoffId, acceptedHandoffHash: value.acceptedHandoffHash, sourceTaskId: value.source.taskId, oldLeaseReleaseId: value.source.leaseId, graphNodeId: value.source.graphNodeId, recipientAgentId: value.recipientAgentId },
    }, { entityType: "handoff", entityId: handoffId, state: "pending", value: value as unknown as JsonValue, intent: { command: "handoff.pending", handoffId, acceptedHandoffHash: value.acceptedHandoffHash } });
    if (!stored.replayed) this.publish(stored.event);
    return pendingHandoffValue(stored.projection.value);
  }
  private pendingHandoff(projectId: string, handoffId: string, scope: EventDraft["scope"], recipientAgentId: string): PendingHandoff {
    const stored = this.store(projectId).projection(projectId, "handoff", handoffId);
    if (!stored || stored.state !== "pending") throw new Error("No pending accepted Handoff exists");
    const handoff = pendingHandoffValue(stored.value);
    if (handoff.handoffId !== handoffId || handoff.recipientAgentId !== recipientAgentId || !sameEventScope(handoff.scope, scope, false)) throw new Error("Handoff lookup is missing, duplicate, or outside the durable recipient scope");
    return handoff;
  }
  private assertPendingHandoffRecord(value: JsonValue, record: Record<string, JsonValue>): void {
    const pending = pendingHandoffValue(value);
    if (pending.acceptedHandoffHash !== sha256(record)) throw new Error("Handoff ID was reused for a different authoritative Handoff");
  }
  private assertHandoffDecisionRecord(value: JsonValue, record: Record<string, JsonValue>): void {
    if (!isObject(value) || typeof value.decisionHash !== "string" || value.decisionHash !== sha256(record)) throw new Error("Handoff already has a different terminal decision");
  }
  private applyHandoffDecision(projectId: string, handoff: PendingHandoff, record: Record<string, JsonValue>): void {
    const decision = String(record.decision);
    const checked = validateTeachback(handoffState(handoff.record), {
      handoffId: String(record.handoffId), logicalOwnerId: String(record.logicalOwnerId), understoodGoalStack: handoffGoalStack(record.understoodGoalStack), decision,
      observedVersions: objectVersions(record.observedVersions), observedBranchHead: String(record.observedBranchHead),
      acknowledgedDefectIds: stringValues(record.acknowledgedDefectIds), acknowledgedBlockerIds: stringValues(record.acknowledgedBlockerIds),
      selectedNextNodeId: typeof record.selectedNextNodeId === "string" ? record.selectedNextNodeId : null, conflicts: stringValues(record.conflicts),
    });
    if (!checked.ok) throw new Error(`Teach-back does not exactly match the Handoff: ${checked.conflicts.join(", ")}`);
    const store = this.store(projectId); const terminal = store.projection(projectId, "handoff_decision", handoff.handoffId);
    if (terminal) { this.assertHandoffDecisionRecord(terminal.value, record); return; }
    if (decision === "accepted") {
      this.assertHandoffOwnershipTransfer(projectId, handoff);
      this.releaseHandoffSourceLease(projectId, handoff);
      this.transferHandoffOwnership(projectId, handoff);
    }
    const decisionHash = sha256(record);
    const stored = store.mutateProjection(`domain-handoff-decision:${handoff.handoffId}`, 0, {
      $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: decision === "accepted" ? "handoff.accepted" : "handoff.decision_recorded", source: "noshd", scope: handoff.scope, correlationId: handoff.handoffId, causationId: null,
      payload: { handoffId: handoff.handoffId, logicalOwnerId: handoff.logicalOwnerId, recipientAgentId: handoff.recipientAgentId, decision, decisionHash },
    }, { entityType: "handoff_decision", entityId: handoff.handoffId, state: decision, value: { handoffId: handoff.handoffId, decisionHash, decision, recipientAgentId: handoff.recipientAgentId, decidedAt: new Date().toISOString() }, intent: { command: "handoff.decision", handoffId: handoff.handoffId, decisionHash } });
    if (!stored.replayed) this.publish(stored.event);
  }
  private assertHandoffOwnershipTransfer(projectId: string, handoff: PendingHandoff): void {
    const ownership = this.store(projectId).projection(projectId, "logical_ownership", handoff.logicalOwnerId);
    if (ownership && (!isObject(ownership.value) || ownership.value.handoffId !== handoff.handoffId || ownership.value.agentId !== handoff.recipientAgentId)) throw new Error("Logical ownership was already transferred by a different Handoff");
  }
  private releaseHandoffSourceLease(projectId: string, handoff: PendingHandoff): void {
    const source = handoff.source;
    if (source.missionId) {
      const current = this.mission(projectId, source.missionId); const node = current.value.nodes.find((entry) => entry.id === source.graphNodeId);
      if (current.version === source.projectionVersion) {
        if (!node || !node.lease || node.lease.leaseId !== source.leaseId || node.lease.ownerId !== source.agentId || !["leased", "working"].includes(node.state)) throw new Error("Handoff source Mission lease changed before release");
        this.transitionMissionNode(projectId, source.missionId, current.version, source.graphNodeId, "ready", `handoff-release:${handoff.handoffId}`);
      } else if (!(current.version === source.projectionVersion + 1 && node?.state === "ready" && node.lease === null)) throw new Error("Handoff source Mission projection changed before release");
      return;
    }
    if (source.directionId) {
      const current = this.direction(projectId, source.directionId); const node = current.value.nodes.find((entry) => entry.id === source.graphNodeId);
      if (current.version === source.projectionVersion) {
        if (!node || !node.lease || node.lease.leaseId !== source.leaseId || node.lease.ownerId !== source.agentId || !["leased", "working"].includes(node.state)) throw new Error("Handoff source Direction lease changed before release");
        this.transitionDirectionNode(projectId, source.directionId, current.version, source.graphNodeId, "ready", `handoff-release:${handoff.handoffId}`);
      } else if (!(current.version === source.projectionVersion + 1 && node?.state === "ready" && node.lease === null)) throw new Error("Handoff source Direction projection changed before release");
      return;
    }
    throw new Error("Handoff source has no graph authority");
  }
  private transferHandoffOwnership(projectId: string, handoff: PendingHandoff): void {
    const store = this.store(projectId); const existing = store.projection(projectId, "logical_ownership", handoff.logicalOwnerId);
    if (existing) { this.assertHandoffOwnershipTransfer(projectId, handoff); return; }
    const stored = store.mutateProjection(`handoff-ownership:${handoff.handoffId}`, 0, {
      $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "handoff.ownership_transferred", source: "noshd", scope: handoff.scope, correlationId: handoff.handoffId, causationId: null,
      payload: { handoffId: handoff.handoffId, logicalOwnerId: handoff.logicalOwnerId, recipientAgentId: handoff.recipientAgentId },
    }, { entityType: "logical_ownership", entityId: handoff.logicalOwnerId, state: "accepted", value: { handoffId: handoff.handoffId, logicalOwnerId: handoff.logicalOwnerId, agentId: handoff.recipientAgentId, acceptedHandoffHash: handoff.acceptedHandoffHash }, intent: { command: "handoff.ownership_transfer", handoffId: handoff.handoffId, acceptedHandoffHash: handoff.acceptedHandoffHash } });
    if (!stored.replayed) this.publish(stored.event);
  }

  private domainEvent(projectId: string, session: DomainSession, type: string, idempotencyKey: string, payload: JsonValue): void {
    this.assertWritable(projectId);
    const stored = this.store(projectId).appendIdempotent(idempotencyKey, { $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type, source: "noshd", scope: eventScope(projectId, session), correlationId: session.taskId, causationId: null, payload });
    if (!stored.replayed) this.publish(stored.receipt.event);
  }

  private finishPaperOperation(projectId: string, intent: OperationIntent): EventEnvelope {
    const request = paperReplaceRequest(intent.request);
    const root = this.project(projectId).repositoryRoot;
    const paths: [string, string] = [join(root, "docs", "paper.md"), join(root, "docs", "paper.bib")];
    const contents: [string, string] = [request.markdown, request.bibliography];
    const desired: [string, string] = [request.markdownHash, request.bibliographyHash];
    const expected: [string, string] = [request.expected.markdownHash, request.expected.bibliographyHash];
    recoverPaperStaging(paths, expected, desired, intent.intentId);
    const current = paths.map((path) => sha256(readText(path))) as [string, string];
    if (current.some((hash, index) => hash !== expected[index] && hash !== desired[index])) throw new Error("Paper targets changed outside the stored replacement intent");
    if (!current.every((hash, index) => hash === desired[index])) replacePaperPair(paths, contents, desired, intent.intentId);
    if (!paths.every((path, index) => sha256(readText(path)) === desired[index])) throw new Error("Paper pair replacement verification failed");
    cleanupPaperStaging(paths, intent.intentId);
    const version = sha256({ markdownHash: request.markdownHash, bibliographyHash: request.bibliographyHash });
    const store = this.store(projectId);
    const stored = store.appendIdempotent(intent.idempotencyKey, draft(projectId, "paper.saved", { markdownHash: request.markdownHash, bibliographyHash: request.bibliographyHash, version, bytes: request.bytes }));
    store.completeOperation(projectId, intent.intentId, { eventId: stored.receipt.event.eventId, markdownHash: request.markdownHash, bibliographyHash: request.bibliographyHash, version });
    if (!stored.replayed) this.publish(stored.receipt.event);
    return stored.receipt.event;
  }

  private passingReview(projectId: string, targetId: string, reviewType: string, targetVersion?: number, reviewId?: string): boolean {
    // Keep one hydrated snapshot: durable records from separate reads are not object-identical.
    const records = this.records(projectId);
    const reviews = records.filter((record) => isSchema(record, "review-request")).filter(isObject);
    const requests = reviews.filter((request) => request.reviewType === reviewType && isObject(request.target) && request.target.targetId === targetId && (targetVersion === undefined || request.target.targetVersion === targetVersion) && (!reviewId || request.reviewId === reviewId));
    if (requests.length !== 1 || typeof requests[0]!.reviewId !== "string") return false;
    const joined = exactReviewJoin(records, requests[0]!.reviewId);
    if (!joined || joined.request !== requests[0] || joined.verdict.verdict !== "PASS") return false;
    const requiredCriteria = isArray(joined.request.criteria) ? joined.request.criteria.filter(isObject).filter((criterion) => criterion.required !== false).map((criterion) => criterion.criterionId).filter((id): id is string => typeof id === "string") : [];
    const verdictCriteria = isArray(joined.verdict.criteria) ? joined.verdict.criteria.filter(isObject) : [];
    if (!requiredCriteria.every((criterionId) => verdictCriteria.some((criterion) => criterion.criterionId === criterionId && criterion.status === "PASS"))) return false;
    const requiredArtifacts = isArray(joined.request.requiredArtifactIds) ? joined.request.requiredArtifactIds : [];
    const requiredEvidence = isArray(joined.request.requiredEvidenceIds) ? joined.request.requiredEvidenceIds : [];
    return requiredArtifacts.every((id) => typeof id === "string" && isArray(joined.verdict.reviewedArtifactIds) && joined.verdict.reviewedArtifactIds.includes(id))
      && requiredEvidence.every((id) => typeof id === "string" && isArray(joined.verdict.reviewedEvidenceIds) && joined.verdict.reviewedEvidenceIds.includes(id));
  }

  private mutate<T extends MissionProjection | DirectionProjection | AutoresearchProjection>(
    projectId: string,
    entityType: "mission" | "direction" | "autoresearch",
    entityId: string,
    expectedVersion: number,
    eventType: string,
    idempotencyKey: string,
    value: T,
    graph?: { scopeType: string; version: number; value: JsonValue; rationale: string },
    details?: Record<string, JsonValue>,
    persistCanonicalContract = false,
    intent?: JsonValue,
  ): Stored<T> {
    this.assertWritable(projectId);
    const direction = value as DirectionProjection;
    const execution = value as AutoresearchProjection;
    const scope = {
      projectId,
      missionId: entityType === "mission" ? entityId : direction.missionId ?? null,
      directionId: entityType === "direction" ? entityId : execution.directionId ?? null,
      autoresearchId: entityType === "autoresearch" ? entityId : null,
      experimentId: null,
      runId: null,
      jobId: null,
      agentId: null,
    };
    const graphVersion = "graphVersion" in value ? value.graphVersion : null;
    const canonicalValue = persistCanonicalContract ? { ...value, canonicalContract: canonicalContract(entityType, value) } : value;
    const store = this.store(projectId);
    const companionRecords: EventDraft[] = [];
    if (persistCanonicalContract) {
      const contract = (canonicalValue as T & { canonicalContract: JsonValue }).canonicalContract;
      const parsed = validateRecord((contract as { $schema: string }).$schema, contract);
      if (!parsed.ok) throw new Error(`Invalid canonical contract: ${parsed.errors.map((error) => error.message).join("; ")}`);
      companionRecords.push(recordSubmitted(scope, entityId, parsed.value as JsonValue));
    }
    if (entityType === "mission" && (graph || typeof details?.nodeId === "string")) {
      const mission = canonicalValue as unknown as MissionProjection;
      const nodes = graph ? mission.nodes : mission.nodes.filter((node) => node.id === details!.nodeId);
      for (const graphNode of nodes) {
        const parsed = validateRecord(schemaUri("mission-node"), missionNodeRecord(mission, graphNode, graph?.rationale ?? "node state transition"));
        if (!parsed.ok) throw new Error(`Invalid mission node: ${parsed.errors.map((error) => error.message).join("; ")}`);
        companionRecords.push(recordSubmitted(scope, entityId, parsed.value as JsonValue));
      }
    }

    const result = store.mutateProjection(idempotencyKey, expectedVersion, {
      $schema: schemaUri("event"),
      schemaVersion: 1,
      retention: "persistent",
      type: eventType,
      source: "noshd",
      scope,
      correlationId: entityId,
      causationId: null,
      payload: { entityId, state: value.state, graphVersion, ...details },
    }, {
      entityType,
      entityId,
      state: value.state,
      value: canonicalValue as unknown as JsonValue,
      ...(graph ? { graph } : {}),
      records: companionRecords,
      ...(intent === undefined ? {} : { intent }),
    });
    if (!result.replayed) {
      this.publish(result.event);
      for (const record of result.records) this.publish(record);
    }
    return result.projection as Stored<T>;
  }
}

function node(id: string, type: string, title: string, criterionIds: string[], dependencies: string[], createdAt: string): GraphNode { return { id, type, title, required: true, criterionIds, hardDependencies: dependencies, softDependencies: [], state: "pending", attempt: 0, maximumAttempts: 3, priority: 5, criticalWeight: 1, createdAt, lease: null }; }
function missionCriteria(mission: MissionProjection): Array<{ criterionId: string; statement: string }> { return mission.successCriteria.map((statement, index) => ({ criterionId: `criterion_${index + 1}`, statement })); }
function defaultMissionNodes(createdAt: string, criterionIds: string[]): GraphNode[] { const plan = `mnode_${crypto.randomUUID().replaceAll("-", "")}`; const evidence = `mnode_${crypto.randomUUID().replaceAll("-", "")}`; const review = `mnode_${crypto.randomUUID().replaceAll("-", "")}`; return [node(plan, "implementation", "Produce the bounded deliverable", criterionIds, [], createdAt), node(evidence, "literature_review", "Resolve evidence and citations", [], [], createdAt), node(review, "final_review", "Independent final review", [], [plan, evidence], createdAt)]; }
function missionNodeRecord(mission: MissionProjection, graphNode: GraphNode, rationale: string): JsonValue {
  return { $schema: schemaUri("mission-node"), schemaVersion: 1, missionId: mission.missionId, graphVersion: mission.graphVersion, nodeId: graphNode.id, nodeType: graphNode.type, title: graphNode.title, rationale, criterionIds: graphNode.criterionIds, hardDependencyNodeIds: graphNode.hardDependencies, softDependencyNodeIds: graphNode.softDependencies, inputArtifactIds: [], expectedOutputKinds: [], assignedRole: graphNode.type === "literature_review" ? "librarian_researcher" : ["claim_review", "final_review", "approval"].includes(graphNode.type) ? "reviewer" : "general_worker", preconditionValidatorIds: [], postconditionValidatorIds: ["validator_task.postflight"], reviewRubricIds: ["rubric_scientific.correctness"], budget: { maximumAttempts: graphNode.maximumAttempts, priority: graphNode.priority, criticalWeight: graphNode.criticalWeight }, workspacePolicy: {}, state: graphNode.state, attempt: graphNode.attempt, lease: graphNode.lease, createdAt: graphNode.createdAt };
}
function canonicalContract(entityType: "mission" | "direction" | "autoresearch", value: MissionProjection | DirectionProjection | AutoresearchProjection): JsonValue {
  if (entityType === "mission") {
    const mission = value as MissionProjection;
    if (!mission.approvedAt) throw new Error("Mission canonical contract requires an approval timestamp");
    const contract = {
      $schema: schemaUri("mission-contract"),
      schemaVersion: 1,
      templateVersion: "1.0.0",
      missionId: mission.missionId,
      projectId: mission.projectId,
      predecessorMissionId: null,
      objectiveVersion: 1,
      title: mission.title,
      objective: mission.objective,
      nonObjectives: mission.nonObjectives,
      projectAlignment: { projectGoalId: mission.projectId, contribution: mission.objective },
      deliverables: mission.deliverables.map((statement, index) => ({ deliverableId: `deliverable_${index + 1}`, statement })),
      successCriteria: mission.successCriteria.map((statement, index) => ({ criterionId: `criterion_${index + 1}`, statement })),
      exitProofs: mission.successCriteria.map((statement, index) => ({ proofId: `proof_${index + 1}`, statement })),
      startingEvidenceIds: mission.startingEvidence,
      assumptions: [],
      initialGraphVersion: 1,
      budgets: {
        deadline: deadline(mission.approvedAt, mission.budgets.maximumWallClockSeconds),
        maximumWallClockSeconds: mission.budgets.maximumWallClockSeconds,
        maximumModelTokens: mission.budgets.maximumModelTokens,
        maximumGpuSeconds: mission.budgets.maximumGpuSeconds,
        maximumDiskBytes: mission.budgets.maximumDiskBytes,
        maximumExperiments: 0,
        maximumConcurrentAgents: mission.budgets.maximumConcurrentAgents,
        maximumConcurrentGpuJobs: 0,
      },
      approvalBoundaries: { values: mission.approvalBoundaries },
      pausePolicy: { mode: mission.pausePolicy },
      finalReviewRubric: mission.finalReviewRubric.map((statement, index) => ({ rubricId: `rubric_${index + 1}`, statement })),
      approvedBy: "user" as const,
      approvedAt: mission.approvedAt,
    };
    return { ...contract, contractHash: sha256(contract) };
  }
  if (entityType === "direction") {
    const direction = value as DirectionProjection;
    if (!direction.activatedAt || direction.approvedBy !== "user") throw new Error("Direction canonical contract requires a user activation timestamp");
    const contract = {
      $schema: schemaUri("direction-contract"),
      schemaVersion: 1,
      templateVersion: "1.0.0",
      directionId: direction.directionId,
      projectId: direction.projectId,
      missionId: direction.missionId,
      predecessorDirectionId: null,
      questionId: direction.questionId,
      primaryQuestion: direction.question,
      decisionUse: direction.decisionUse,
      falsifiability: direction.falsifiability,
      projectClaimIds: direction.projectClaimIds,
      scope: direction.scope,
      evaluationContractId: direction.evaluationContractId,
      evaluationContract: direction.evaluationContract,
      evaluationContractHash: direction.evaluationContractHash,
      baselineExperimentId: direction.plannedBaselineExperimentId,
      integrationBranch: direction.integrationBranch,
      initialGraphVersion: 1,
      stoppingRules: direction.stoppingRules,
      budget: direction.budget,
      closureRubric: direction.closureRubric,
      createdBy: direction.createdBy,
      approvedBy: direction.approvedBy,
      activatedAt: direction.activatedAt,
    };
    return { ...contract, contractHash: sha256(contract) };
  }
  const execution = value as AutoresearchProjection;
  const contract = {
    $schema: schemaUri("autoresearch-contract"),
    schemaVersion: 1,
    templateVersion: "1.0.0",
    autoresearchId: execution.autoresearchId,
    projectId: execution.projectId,
    missionId: execution.missionId,
    directionId: execution.directionId,
    invokedByRole: execution.invokedByRole,
    decisionQuestion: execution.decisionQuestion,
    distinctPurpose: execution.decisionQuestion,
    hypothesisFamilyTags: execution.familyTags.length ? execution.familyTags : [execution.decisionQuestion],
    duplicateCheck: { fingerprint: execution.fingerprint },
    evaluationContractId: execution.evaluationContractId,

    evaluationContractHash: execution.evaluationContractHash,
    rootExperimentId: execution.rootExperimentId,
    acceptedFrontierExperimentIds: execution.acceptedFrontierExperimentIds,
    allowedChangeScopes: execution.scope,
    forbiddenChangeScopes: execution.forbiddenChangeScopes,
    variantPolicy: execution.variantPolicy,
    budgets: {
      maximumExperiments: execution.maximumExperiments,
      maximumRounds: execution.maximumRounds,
      maximumWallClockSeconds: execution.maximumWallClockSeconds,
      maximumModelTokens: execution.maximumModelTokens,
      maximumGpuSeconds: execution.maximumGpuSeconds,
      maximumDiskBytes: execution.maximumDiskBytes,
    },
    stopConditions: execution.stopConditions,
    promotionRubricIds: execution.promotionRubricIds,
    createdAt: execution.createdAt,
  };
  return { ...contract, contractHash: sha256(contract) };
}
function sanitizeEvidenceInput(projectId: string, input: JsonValue): Record<string, JsonValue> {
  const value = sanitizeUserRecord(projectId, input, ["evidenceType", "statement", "polarity", "sourceRefs", "evaluationContractHash", "scopeLimitations", "quality"]);
  return { ...value, quality: value.quality ?? { status: "unreviewed", reviewId: null, confidence: "low" } };
}
function sanitizeClaimInput(projectId: string, input: JsonValue): Record<string, JsonValue> {
  return sanitizeUserRecord(projectId, input, ["text", "claimType", "status", "supportingEvidenceIds", "contradictingEvidenceIds", "qualifyingEvidenceIds", "limitations", "paperLocations", "requiredReviewId"]);
}
function sanitizeUserRecord(projectId: string, input: JsonValue, allowed: string[]): Record<string, JsonValue> {
  if (!isObject(input)) throw new Error("User record must be an object");
  if (input.projectId !== undefined && input.projectId !== projectId) throw new Error("Record Project must match the route Project");
  for (const key of Object.keys(input)) if (key !== "projectId" && !allowed.includes(key)) throw new Error(`User record field is not allowed: ${key}`);
  return Object.fromEntries(Object.entries(input).filter(([key]) => key !== "projectId")) as Record<string, JsonValue>;
}
function deterministicId(prefix: "evd" | "clm", projectId: string, idempotencyKey: string): string {
  return `${prefix}_${sha256({ prefix, projectId, idempotencyKey }).slice(7, 39)}`;
}
function assertedUserRecordIntent(intent: OperationIntent, type: string, inputHash: string): Record<string, JsonValue> {
  if (intent.operationType !== type || !isObject(intent.request) || intent.request.inputHash !== inputHash) throw new Error("Idempotency key was already used for different user input");
  return intent.request;
}
function userRecordRequest(request: JsonValue): Record<string, JsonValue> {
  if (!isObject(request) || !isObject(request.record)) throw new Error("User-record operation intent is corrupt");
  return request.record;
}
function userRecord(intent: OperationIntent): Record<string, JsonValue> { return userRecordRequest(intent.request); }
function currentMissionCompletionPacket(records: JsonValue[], missionId: string, targetVersion: number, graphVersion: number): Record<string, JsonValue> | null {
  const requests = records.filter((record) => isSchema(record, "review-request") && record.reviewType === "mission_completion" && isObject(record.target) && record.target.targetType === "mission_record" && record.target.targetId === missionId && record.target.targetVersion === targetVersion).map((record) => record as UnknownRecord);
  if (requests.length !== 1 || typeof requests[0]!.reviewId !== "string") return null;
  const review = exactReviewJoin(records, requests[0]!.reviewId);
  if (!review || review.verdict.verdict !== "PASS" || !isArray(review.request.contractRefs)) return null;
  const hashes = review.request.contractRefs.filter(isObject).filter((reference) => reference.kind === "contract_mission.completion" && typeof reference.hash === "string").map((reference) => reference.hash as string);
  if (hashes.length !== 1) return null;
  const packets = records.filter((record) => isSchema(record, "mission-completion-packet") && record.missionId === missionId && record.objectiveVersion === 1 && record.graphVersion === graphVersion && sha256(record) === hashes[0]);
  return packets.length === 1 ? packets[0]! as Record<string, JsonValue> : null;
}
function exactReviewJoin(records: JsonValue[], reviewId: string): { request: Record<string, JsonValue>; verdict: Record<string, JsonValue> } | null {
  const authority = reviewAuthority(records, reviewId);
  return authority.state === "resolved" && authority.request && authority.verdict ? { request: authority.request, verdict: authority.verdict } : null;
}
function reviewAuthority(records: JsonValue[], reviewId: string): { reviewId: string; request: Record<string, JsonValue> | null; verdict: Record<string, JsonValue> | null; state: "pending" | "resolved" | "invalid" | "ambiguous"; issues: string[] } {
  const requests = records.filter((record) => isSchema(record, "review-request") && record.reviewId === reviewId).map((record) => record as UnknownRecord);
  const verdicts = records.filter((record) => isSchema(record, "review-verdict") && record.reviewId === reviewId).map((record) => record as UnknownRecord);
  if (requests.length > 1) return { reviewId, request: null, verdict: null, state: "ambiguous", issues: ["multiple_review_requests", ...(verdicts.length > 1 ? ["multiple_review_verdicts"] : [])] };
  if (requests.length !== 1 || !validateRecord(schemaUri("review-request"), requests[0]!).ok) return { reviewId, request: null, verdict: null, state: "invalid", issues: ["missing_or_invalid_review_request"] };
  const request = requests[0]!;
  if (typeof request.reviewRequestId !== "string" || typeof request.reviewType !== "string" || typeof request.reviewerAgentId !== "string" || !isObject(request.target) || !isArray(request.producerAgentIds) || request.producerAgentIds.some((id) => typeof id !== "string") || request.producerAgentIds.includes(request.reviewerAgentId)) return { reviewId, request: null, verdict: null, state: "invalid", issues: ["invalid_review_request_identity_or_independence"] };
  if (verdicts.length > 1) return { reviewId, request, verdict: null, state: "ambiguous", issues: ["multiple_review_verdicts"] };
  if (!verdicts.length) return { reviewId, request, verdict: null, state: "pending", issues: [] };
  const verdict = verdicts[0]!;
  if (!validateRecord(schemaUri("review-verdict"), verdict).ok || verdict.reviewRequestId !== request.reviewRequestId || verdict.reviewType !== request.reviewType || verdict.reviewerAgentId !== request.reviewerAgentId || verdict.independenceCheck !== "pass" || !isObject(verdict.target) || verdict.target.targetType !== request.target.targetType || verdict.target.targetId !== request.target.targetId || verdict.target.targetVersion !== request.target.targetVersion) return { reviewId, request, verdict: null, state: "invalid", issues: ["invalid_or_mismatched_review_verdict"] };
  return { reviewId, request, verdict, state: "resolved", issues: [] };
}
function paperReplaceRequest(value: JsonValue): { markdown: string; bibliography: string; markdownHash: string; bibliographyHash: string; expected: { markdownHash: string; bibliographyHash: string; version: string }; bytes: { markdown: number; bibliography: number } } {
  if (!isObject(value) || typeof value.markdown !== "string" || typeof value.bibliography !== "string" || typeof value.markdownHash !== "string" || typeof value.bibliographyHash !== "string" || !isObject(value.expected) || typeof value.expected.markdownHash !== "string" || typeof value.expected.bibliographyHash !== "string" || typeof value.expected.version !== "string" || !isObject(value.bytes) || typeof value.bytes.markdown !== "number" || typeof value.bytes.bibliography !== "number" || sha256(value.markdown) !== value.markdownHash || sha256(value.bibliography) !== value.bibliographyHash || Buffer.byteLength(value.markdown) !== value.bytes.markdown || Buffer.byteLength(value.bibliography) !== value.bytes.bibliography || value.expected.version !== sha256({ markdownHash: value.expected.markdownHash, bibliographyHash: value.expected.bibliographyHash })) throw new Error("Paper operation intent is corrupt");
  return { markdown: value.markdown, bibliography: value.bibliography, markdownHash: value.markdownHash, bibliographyHash: value.bibliographyHash, expected: { markdownHash: value.expected.markdownHash, bibliographyHash: value.expected.bibliographyHash, version: value.expected.version }, bytes: { markdown: value.bytes.markdown, bibliography: value.bytes.bibliography } };
}
function readText(path: string): string { return existsSync(path) ? readFileSync(path, "utf8") : ""; }
function paperPaths(paths: [string, string], operationId: string): { staged: [string, string]; previous: [string, string] } {
  return { staged: [`${paths[0]}.${operationId}.0.tmp`, `${paths[1]}.${operationId}.1.tmp`], previous: [`${paths[0]}.${operationId}.0.rollback`, `${paths[1]}.${operationId}.1.rollback`] };
}
function recoverPaperStaging(paths: [string, string], expected: [string, string], desired: [string, string], operationId: string): void {
  const { staged, previous } = paperPaths(paths, operationId);
  for (const path of staged) rmSync(path, { force: true });
  for (let index = 0; index < paths.length; index += 1) if (existsSync(previous[index]!)) {
    if (!existsSync(paths[index]!)) renameSync(previous[index]!, paths[index]!);
    else {
      const targetHash = sha256(readText(paths[index]!));
      if (targetHash !== expected[index] && targetHash !== desired[index]) throw new Error("Paper replacement recovery found an inconsistent rollback pair");
      rmSync(previous[index]!, { force: true });
    }
  }
}
function cleanupPaperStaging(paths: [string, string], operationId: string): void {
  const { staged, previous } = paperPaths(paths, operationId);
  for (const path of [...staged, ...previous]) try { rmSync(path, { force: true }); } catch (error) { process.stderr.write(`nosh paper cleanup retained ${path}: ${error instanceof Error ? error.message : "unknown error"}\n`); }
}
function replacePaperPair(paths: [string, string], contents: [string, string], desired: [string, string], operationId: string): void {
  const { staged, previous } = paperPaths(paths, operationId);
  const moved = [false, false];
  const installed = [false, false];
  let committed = false;
  try {
    for (let index = 0; index < paths.length; index += 1) { mkdirSync(dirname(paths[index]!), { recursive: true }); writeFileSync(staged[index]!, contents[index]!, "utf8"); }
    for (let index = 0; index < paths.length; index += 1) if (existsSync(paths[index]!)) { renameSync(paths[index]!, previous[index]!); moved[index] = true; }
    for (let index = 0; index < paths.length; index += 1) { renameSync(staged[index]!, paths[index]!); installed[index] = true; }
    if (!paths.every((path, index) => sha256(readText(path)) === desired[index])) throw new Error("Paper pair staging verification failed");
    committed = true;
  } catch (error) {
    if (!committed) for (let index = 0; index < paths.length; index += 1) {
      rmSync(staged[index]!, { force: true });
      if (installed[index]) rmSync(paths[index]!, { force: true });
      if (moved[index] && existsSync(previous[index]!)) renameSync(previous[index]!, paths[index]!);
    }
    throw error;
  }
  cleanupPaperStaging(paths, operationId);
}


function deadline(createdAt: string, seconds: number): string { return new Date(Date.parse(createdAt) + seconds * 1000).toISOString(); }
function defaultDirectionNodes(createdAt: string): GraphNode[] { const baseline = `dnode_${crypto.randomUUID().replaceAll("-", "")}`; const research = `dnode_${crypto.randomUUID().replaceAll("-", "")}`; const synthesis = `dnode_${crypto.randomUUID().replaceAll("-", "")}`; return [node(baseline, "general_worker", "Reproduce accepted baseline", [], [], createdAt), node(research, "general_worker", "Evaluate bounded hypothesis families", [], [baseline], createdAt), node(synthesis, "reviewer", "Synthesize and review results", [], [research], createdAt)]; }
function canonicalDirectionBaselineNode(direction: DirectionProjection): GraphNode {
  const nodes = direction.nodes.filter((node) => node.type === "general_worker" && node.title === "Reproduce accepted baseline" && !node.hardDependencies.length && !node.softDependencies.length);
  if (nodes.length !== 1) throw new Error("Direction has no unique canonical baseline node");
  return nodes[0]!;
}
export type MissionCompletionBasis = {
  criteria: Array<{ criterionId: string; statement: string; taskId: string; reviewId: string; validatorRunIds: string[]; artifactIds: string[]; evidenceIds: string[] }>;
  completions: Array<{ nodeId: string; taskId: string; agentId: string; completion: Record<string, JsonValue>; review: Record<string, JsonValue>; artifactIds: string[]; evidenceIds: string[]; validatorRunIds: string[] }>;
  artifactIds: string[];
  evidenceIds: string[];
  validatorRunIds: string[];
  unresolvedClaimIds: string[];
  claims: Array<Record<string, JsonValue>>;
  openDefectIds: string[];
  issues: string[];
};
type UnknownRecord = Record<string, JsonValue>;
function isObject(value: unknown): value is UnknownRecord { return typeof value === "object" && value !== null && !Array.isArray(value); }
function isArray(value: unknown): value is JsonValue[] { return Array.isArray(value); }
function isSchema(value: unknown, name: string): value is UnknownRecord { return isObject(value) && value.$schema === schemaUri(name); }
function sameStringSet(value: unknown, expected: string[]): boolean {
  return isArray(value) && value.every((item) => typeof item === "string") && new Set(value as string[]).size === value.length && new Set(expected).size === expected.length && (value as string[]).length === expected.length && (value as string[]).every((item) => expected.includes(item));
}
function sameMissionCriterionMappings(value: unknown, expected: MissionCompletionBasis["criteria"]): boolean {
  if (!isArray(value) || value.length !== expected.length) return false;
  return expected.every((criterion) => {
    const candidate = value.find((entry) => isObject(entry) && entry.criterionId === criterion.criterionId);
    const actual = candidate && isObject(candidate) ? candidate : null;
    return !!actual && actual.taskId === criterion.taskId && actual.reviewId === criterion.reviewId && sameStringSet(actual.validatorRunIds, criterion.validatorRunIds) && sameStringSet(actual.artifactIds, criterion.artifactIds) && sameStringSet(actual.evidenceIds, criterion.evidenceIds);
  });
}
function projectScope(projectId: string): EventDraft["scope"] {
  return { projectId, missionId: null, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: null };
}

function recordSubmitted(scope: EventDraft["scope"], correlationId: string, record: JsonValue): EventDraft {
  return { $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "record.submitted", source: "noshd", scope, correlationId, causationId: null, payload: record };
}
function completionReferences(record: Record<string, JsonValue>): { artifactIds: string[]; evidenceIds: string[] } {
  const work = isArray(record.workPerformed) ? record.workPerformed.filter(isObject) : [];
  const criteria = isArray(record.criteria) ? record.criteria.filter(isObject) : [];
  const code = isObject(record.codeChanges) ? record.codeChanges : {};
  const sources = isArray(record.sources) ? record.sources.filter(isObject) : [];
  const findings = isArray(record.findings) ? record.findings.filter(isObject) : [];
  const artifactIds = [code.diffArtifactId, record.bibliographyArtifactId, record.reportArtifactId, ...work.flatMap((entry) => isArray(entry.artifactIds) ? entry.artifactIds : []), ...criteria.flatMap((entry) => isArray(entry.artifactIds) ? entry.artifactIds : []), ...sources.map((entry) => entry.artifactId), ...findings.flatMap((entry) => isArray(entry.support) ? entry.support.filter(isObject).map((support) => support.artifactId) : [])].filter((id): id is string => typeof id === "string");
  const impact = isObject(record.scientificImpact) ? record.scientificImpact : {};
  const evidenceIds = (isArray(impact.evidenceIds) ? impact.evidenceIds : []).filter((id): id is string => typeof id === "string");
  return { artifactIds: [...new Set(artifactIds)], evidenceIds: [...new Set(evidenceIds)] };
}
function artifactExists(project: RegisteredProject, artifactId: string): boolean {
  try { new ArtifactStore(join(project.repositoryRoot, ".nosh", "artifacts")).resolve(artifactId); return true; } catch { return false; }
}
function claimResolutionIssues(claim: Record<string, JsonValue>, evidence: Map<string, Record<string, JsonValue>>, records: JsonValue[]): string[] {
  const claimId = String(claim.claimId);
  const references = [...stringValues(claim.supportingEvidenceIds), ...stringValues(claim.contradictingEvidenceIds), ...stringValues(claim.qualifyingEvidenceIds)];
  const issues = references.filter((id) => !evidence.has(id)).map((id) => `Claim ${claimId} references missing Project Evidence ${id}`);
  const reviewed = (ids: string[]): boolean => ids.some((evidenceId) => {
    const source = evidence.get(evidenceId); const quality = source && isObject(source.quality) ? source.quality : null;
    if (!quality || typeof quality.reviewId !== "string") return false;
    const review = exactReviewJoin(records, quality.reviewId);
    return Boolean(review && review.verdict.verdict === "PASS" && isArray(review.request.requiredEvidenceIds) && review.request.requiredEvidenceIds.includes(evidenceId) && isArray(review.verdict.reviewedEvidenceIds) && review.verdict.reviewedEvidenceIds.includes(evidenceId));
  });
  const limitations = stringValues(claim.limitations);
  if (claim.status === "hypothesis" || claim.status === "under_test") issues.push(`Claim ${claimId} is ${String(claim.status)}`);
  else if (claim.status === "supported" || claim.status === "partially_supported") { if (!reviewed(stringValues(claim.supportingEvidenceIds))) issues.push(`Claim ${claimId} lacks independently PASS-reviewed supporting Evidence`); }
  else if (claim.status === "contradicted" || claim.status === "not_supported") { if (!reviewed(stringValues(claim.contradictingEvidenceIds))) issues.push(`Claim ${claimId} lacks independently PASS-reviewed contradicting Evidence`); }
  else if (claim.status === "qualified") { if (!reviewed(stringValues(claim.qualifyingEvidenceIds)) && !limitations.length) issues.push(`Claim ${claimId} lacks independently PASS-reviewed qualifying Evidence or explicit limitations`); }
  else if (claim.status === "limited" && !limitations.length) issues.push(`Claim ${claimId} is limited without explicit limitations`);
  return issues;
}
function latestScopedClaims(records: Array<{ event: EventEnvelope; record: JsonValue }>, issues: string[]): Array<Record<string, JsonValue>> {
  const claims = new Map<string, { version: number; hash: string; record: Record<string, JsonValue> }>();
  for (const { record } of records) {
    if (!isSchema(record, "claim")) continue;
    const parsed = validateRecord(schemaUri("claim"), record);
    const version = record.claimVersion;
    if (!parsed.ok || typeof record.claimId !== "string" || typeof version !== "number" || !Number.isInteger(version)) { issues.push(`invalid Claim record ${typeof record.claimId === "string" ? record.claimId : "unknown"}`); continue; }
    const current = claims.get(record.claimId); const hash = sha256(record);
    if (current && current.version === version && current.hash !== hash) { issues.push(`ambiguous Claim version ${record.claimId}@${version}`); continue; }
    if (!current || version > current.version) claims.set(record.claimId, { version, hash, record });
  }
  return [...claims.values()].map((claim) => claim.record);
}
function draft(projectId: string, type: string, payload: JsonValue): EventDraft { return { $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type, source: "noshd", scope: { projectId, missionId: null, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: null }, correlationId: null, causationId: null, payload }; }
function directoryBytes(path: string): number { if (!existsSync(path)) return 0; let bytes = 0; for (const entry of readdirSync(path, { withFileTypes: true })) { const child = join(path, entry.name); if (entry.isSymbolicLink()) continue; if (entry.isDirectory()) bytes += directoryBytes(child); else if (entry.isFile()) bytes += statSync(child).size; } return bytes; }
function eventResourceUse(events: EventEnvelope[]): Pick<MissionBudgetUse, "modelTokens" | "gpuSeconds"> { const modelTokens = events.filter((event) => ["agent.completed", "agent.retrying", "agent.failed"].includes(event.type)).reduce((sum, event) => sum + Math.max(0, Number((event.payload as { modelTokens?: number }).modelTokens ?? 0)), 0); const jobs = new Map<string, { usesGpu?: boolean; startedAt?: string | null; finishedAt?: string | null }>(); for (const event of events) if (event.type === "job.state_changed" && event.scope.jobId) jobs.set(event.scope.jobId, event.payload as { usesGpu?: boolean; startedAt?: string | null; finishedAt?: string | null }); const gpuSeconds = [...jobs.values()].filter((job) => job.usesGpu && job.startedAt).reduce((sum, job) => sum + Math.max(0, Math.floor((Date.parse(job.finishedAt ?? new Date().toISOString()) - Date.parse(job.startedAt!)) / 1000)), 0); return { modelTokens, gpuSeconds }; }
function gitCommit(repositoryRoot: string, commit: string): string { const result = spawnSync("git", ["-C", repositoryRoot, "rev-parse", `${commit}^{commit}`], { encoding: "utf8", windowsHide: true }); if (result.status !== 0) throw new Error("Baseline commit does not resolve in the Project repository"); return result.stdout.trim(); }
function eventScope(projectId: string, session: DomainSession): EventDraft["scope"] {
  return { projectId, missionId: session.missionId, directionId: session.directionId, autoresearchId: session.autoresearchId, experimentId: session.experimentId, runId: session.runId, jobId: session.jobId, agentId: session.agentId };
}
function sameScope(value: Record<string, JsonValue>, session: DomainSession, requireAgent = true): boolean {
  return value.missionId === session.missionId && value.directionId === session.directionId && value.autoresearchId === session.autoresearchId && value.experimentId === session.experimentId && value.runId === session.runId && value.jobId === session.jobId && (!requireAgent || value.agentId === session.agentId);
}
function sameTaskScope(value: Record<string, JsonValue>, session: DomainSession): boolean {
  return value.missionId === session.missionId && value.directionId === session.directionId && value.autoresearchId === session.autoresearchId && value.experimentId === session.experimentId && (value.runId ?? null) === session.runId && (value.jobId ?? null) === session.jobId;
}
function stringValues(value: unknown): string[] {
  if (!isArray(value) || !value.every((entry) => typeof entry === "string")) throw new Error("Expected an array of string references");
  return value as string[];
}
function objectVersions(value: unknown): Record<string, number | string> {
  if (!isObject(value) || !Object.values(value).every((entry) => typeof entry === "string" || typeof entry === "number" && Number.isInteger(entry) && entry >= 0)) throw new Error("Expected observed versions");
  return value as Record<string, number | string>;
}
function graphOperations(value: unknown, existing: GraphNode[] = []): GraphOperation[] {
  if (!isArray(value) || !value.length) throw new Error("Graph mutation requires operations");
  const known = new Set<string>(existing.map((node) => node.id));
  const operations: GraphOperation[] = [];
  for (const operation of value) {
    if (!isObject(operation) || typeof operation.type !== "string") throw new Error("Invalid graph operation");
    if (operation.type === "add_node" && isGraphNode(operation.node)) {
      if (known.has(operation.node.id) || [...operation.node.hardDependencies, ...operation.node.softDependencies].some((dependency) => !known.has(dependency))) throw new Error("Added graph node has duplicate or unresolved dependencies");
      known.add(operation.node.id); operations.push({ type: "add_node", node: operation.node }); continue;
    }
    if ((operation.type === "add_hard_edge" || operation.type === "remove_hard_edge") && typeof operation.from === "string" && typeof operation.to === "string" && operation.from !== operation.to && known.has(operation.from) && known.has(operation.to)) { operations.push({ type: operation.type, from: operation.from, to: operation.to }); continue; }
    if (operation.type === "supersede_node" && typeof operation.nodeId === "string" && known.has(operation.nodeId) && (operation.replacementId === null || typeof operation.replacementId === "string" && operation.replacementId !== operation.nodeId && known.has(operation.replacementId))) { operations.push({ type: "supersede_node", nodeId: operation.nodeId, replacementId: operation.replacementId }); continue; }
    throw new Error("Invalid graph operation");
  }
  return operations;
}
function isGraphNode(value: unknown): value is GraphNode {
  if (!isObject(value) || !validGraphText(value.id) || !validGraphText(value.type) || !validGraphText(value.title) || typeof value.required !== "boolean" || !isArray(value.criterionIds) || !value.criterionIds.every((item) => typeof item === "string" && /^criterion_[1-9]\d*$/.test(item)) || new Set(value.criterionIds).size !== value.criterionIds.length || !isArray(value.hardDependencies) || !isArray(value.softDependencies) || !value.hardDependencies.every((item) => typeof item === "string" && validGraphText(item)) || !value.softDependencies.every((item) => typeof item === "string" && validGraphText(item))) return false;
  const dependencies = [...value.hardDependencies, ...value.softDependencies] as string[];
  if (new Set(dependencies).size !== dependencies.length || dependencies.includes(value.id)) return false;
  return value.state === "pending" && value.attempt === 0 && Number.isInteger(value.maximumAttempts) && Number(value.maximumAttempts) >= 1 && typeof value.priority === "number" && Number.isFinite(value.priority) && typeof value.criticalWeight === "number" && Number.isFinite(value.criticalWeight) && typeof value.createdAt === "string" && Number.isFinite(Date.parse(value.createdAt)) && value.lease === null;
}
function validMissionNodeType(value: unknown): value is string { return typeof value === "string" && ["literature_review", "reproduction", "research_direction", "autoresearch", "implementation", "dataset", "analysis", "claim_review", "paper_section", "integration", "approval", "synthesis", "final_review"].includes(value); }
function assertMissionCriterionOwnership(criteria: string[], nodes: GraphNode[]): void {
  if (nodes.some((node) => !validMissionNodeType(node.type))) throw new Error("Mission graph nodes must use canonical mission node types");
  const expected = criteria.map((_, index) => `criterion_${index + 1}`);
  const assigned = nodes.filter((node) => node.state !== "superseded").flatMap((node) => node.criterionIds);
  if (new Set(assigned).size !== assigned.length || expected.some((criterionId) => assigned.filter((id) => id === criterionId).length !== 1) || assigned.some((criterionId) => !expected.includes(criterionId))) throw new Error("Each Mission contract criterion must have exactly one graph-node owner");
}
function validGraphText(value: unknown): value is string { return typeof value === "string" && value.trim().length > 0 && value.length <= 256; }
function createdProjectionReplay<T extends JsonValue>(store: EventStore, projectId: string, idempotencyKey: string, entityType: string, eventType: string): Stored<T> | null {
  const receipt = store.commandReceipt(projectId, idempotencyKey) as unknown as { event: EventEnvelope; projection?: EntityProjection<T> } | undefined;
  if (!receipt) return null;
  const projection = receipt.projection;
  if (receipt.event.type !== eventType || receipt.event.scope.projectId !== projectId || !projection || projection.projectId !== projectId || projection.entityType !== entityType) throw new Error("Creation idempotency key was previously used for a different command");
  return projection as Stored<T>;
}
function projectionIntentReplay<T extends JsonValue>(store: EventStore, projectId: string, idempotencyKey: string, entityType: string, entityId: string, intent: JsonValue): Stored<T> | null {
  const receipt = store.commandReceipt(projectId, idempotencyKey) as unknown as { projection?: EntityProjection<T>; intentHash?: string } | undefined;
  if (!receipt) return null;
  if (!receipt.projection || receipt.projection.projectId !== projectId || receipt.projection.entityType !== entityType || receipt.projection.entityId !== entityId || receipt.intentHash !== sha256(intent)) throw new Error("Idempotency key was previously used for a different projection intent");
  return receipt.projection as Stored<T>;
}
function assertExistingEvidenceIds(records: Array<{ event: EventEnvelope; record: JsonValue }>, evidenceIds: string[]): void {
  const existing = new Set(records.filter((entry) => isObject(entry.record) && typeof entry.record.evidenceId === "string").map((entry) => String((entry.record as Record<string, JsonValue>).evidenceId)));
  if (evidenceIds.some((evidenceId) => !existing.has(evidenceId))) throw new Error("Mission references evidence that does not exist in the Project");
}
function handoffGoalStack(value: unknown): { projectGoalId: string; missionCriterionIds: string[]; directionQuestionId: string | null; currentGraphNodeId: string | null } {
  if (!isObject(value) || typeof value.projectGoalId !== "string" || !isArray(value.missionCriterionIds) || !value.missionCriterionIds.every((item) => typeof item === "string") || (value.directionQuestionId !== null && typeof value.directionQuestionId !== "string") || (value.currentGraphNodeId !== null && typeof value.currentGraphNodeId !== "string")) throw new Error("Handoff goal stack is invalid");
  return { projectGoalId: value.projectGoalId, missionCriterionIds: value.missionCriterionIds as string[], directionQuestionId: value.directionQuestionId as string | null, currentGraphNodeId: value.currentGraphNodeId as string | null };
}
function pendingHandoffValue(value: JsonValue): PendingHandoff {
  if (!isObject(value) || typeof value.acceptedHandoffHash !== "string" || typeof value.handoffId !== "string" || typeof value.logicalOwnerId !== "string" || typeof value.recipientAgentId !== "string" || !isObject(value.record) || !isObject(value.scope) || !isObject(value.source)) throw new Error("Pending Handoff projection is malformed");
  const scope = value.scope; const source = value.source;
  if (typeof scope.projectId !== "string" || ["missionId", "directionId", "autoresearchId", "experimentId", "runId", "jobId", "agentId"].some((field) => scope[field] !== null && typeof scope[field] !== "string") || typeof source.taskId !== "string" || typeof source.role !== "string" || !["nosh", "user", "mission_director", "research_director", "librarian_researcher", "general_worker", "reviewer"].includes(source.role) || (source.missionId !== null && typeof source.missionId !== "string") || (source.directionId !== null && typeof source.directionId !== "string") || typeof source.graphNodeId !== "string" || typeof source.leaseId !== "string" || typeof source.agentId !== "string" || !Number.isInteger(source.leaseProjectionVersion) || !Number.isInteger(source.projectionVersion) || typeof source.maximumModelTokens !== "number" || typeof source.maximumWallClockSeconds !== "number" || (source.delegation !== "disabled" && source.delegation !== "request_only")) throw new Error("Pending Handoff authority is malformed");
  return { acceptedHandoffHash: value.acceptedHandoffHash, handoffId: value.handoffId, logicalOwnerId: value.logicalOwnerId, recipientAgentId: value.recipientAgentId, record: value.record, scope: scope as unknown as EventDraft["scope"], source: source as unknown as PendingHandoff["source"] };
}
function sameEventScope(left: EventDraft["scope"], right: EventDraft["scope"], requireAgent: boolean): boolean {
  return left.projectId === right.projectId && left.missionId === right.missionId && left.directionId === right.directionId && left.autoresearchId === right.autoresearchId && left.experimentId === right.experimentId && left.runId === right.runId && left.jobId === right.jobId && (!requireAgent || left.agentId === right.agentId);
}
function handoffState(handoff: Record<string, JsonValue>): { handoffId: string; logicalOwnerId: string; goalStack: ReturnType<typeof handoffGoalStack>; observedVersions: Record<string, number | string>; branchHead: string; defectIds: string[]; blockerIds: string[]; readyNodeIds: string[] } {
  const defects = isArray(handoff.openDefects) ? handoff.openDefects.flatMap((defect) => typeof defect === "string" ? [defect] : isObject(defect) && typeof defect.defectId === "string" ? [defect.defectId] : []) : [];
  if (!isObject(handoff.branch) || typeof handoff.branch.head !== "string") throw new Error("Handoff branch state is invalid");
  return { handoffId: String(handoff.handoffId), logicalOwnerId: String(handoff.logicalOwnerId), goalStack: handoffGoalStack(handoff.goalStack), observedVersions: objectVersions(handoff.observedVersions), branchHead: handoff.branch.head, defectIds: defects, blockerIds: stringValues(handoff.openBlockerIds), readyNodeIds: stringValues(handoff.readyNodeIds) };
}
function domainEffectKey(record: Record<string, JsonValue>): string | null {
  if (record.$schema === schemaUri("progress-update") && typeof record.progressId === "string") return `domain-progress:${record.progressId}`;
  if (record.$schema === schemaUri("blocker") && typeof record.blockerId === "string") return `domain-blocker:${record.blockerId}`;
  if (record.$schema === schemaUri("graph-change-proposal") && typeof record.proposalId === "string") return `domain-graph:${record.proposalId}`;
  if (record.$schema === schemaUri("delegation-request") && typeof record.requestId === "string") return `domain-delegation:${record.requestId}`;
  if (record.$schema === schemaUri("handoff") && typeof record.handoffId === "string") return `domain-handoff:${record.handoffId}`;
  if (record.$schema === schemaUri("handoff-teachback") && typeof record.handoffId === "string") return `domain-handoff-decision:${record.handoffId}`;
  return null;
}
