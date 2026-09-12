import { createId } from "@nosh/core";
import { ArtifactStore } from "@nosh/evidence";
import { existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { VersionedDag, type GraphNode } from "@nosh/graph";
import type { PiAdapter, PiSessionOptions } from "@nosh/pi-adapter";
import type { OrchestrationRuntime } from "@nosh/orchestration-runtime";
import { FocusGovernor, Scheduler, type AttemptInput } from "@nosh/scheduler";
import { canonicalJson, schemaUri, sha256, validateRecord, type EventDraft, type JsonValue, type TaskPermissions } from "@nosh/wire";
import type { RegisteredProject } from "@nosh/persistence";
import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { librarianResearchHosts, type MissionProjection, type ResearchControl, type Stored } from "./research-control.js";
import { validateGitCompletion } from "./task-postflight.js";
export class MissionSupervisor {
  private timer: NodeJS.Timeout | undefined; private readonly busy = new Set<string>(); private readonly scheduler = new Scheduler();
  constructor(private readonly research: ResearchControl, private readonly agents: PiAdapter, private readonly projects: () => RegisteredProject[], private readonly packagePath: string, private readonly emit: (draft: EventDraft) => void, private readonly runtime?: OrchestrationRuntime) {}

  start(): void { this.timer = setInterval(() => void this.tick(), 1_000); this.timer.unref(); }
  stop(): void { if (this.timer) clearInterval(this.timer); this.timer = undefined; }
  async tick(): Promise<void> { for (const project of this.projects()) for (const mission of this.research.missions(project.projectId)) { if (["completed", "stopped", "failed"].includes(mission.state)) this.scheduler.releaseMission(project.projectId, mission.entityId); else if (mission.state === "running") void this.cycle(project, mission.entityId); } }

  private async cycle(project: RegisteredProject, missionId: string): Promise<void> {
    if (this.busy.has(missionId) || this.agents.inspect().some((agent) => agent.missionId === missionId) || this.runtime?.hasActiveFork(project.projectId, missionId, null)) return; this.busy.add(missionId);
    try {
      let mission = this.research.mission(project.projectId, missionId); if (mission.state !== "running") return; mission = this.reconcileOrphanedNodes(project.projectId, mission); if (mission.state !== "running") return; this.scheduler.activateMission(project.projectId, missionId); this.scheduler.restoreAllocations(Object.fromEntries(mission.value.nodes.map((node) => [node.id, node.attempt])));
      const use = this.research.missionBudgetUse(project.projectId, missionId); const budget = mission.value.budgets; const exhausted = [use.modelTokens >= budget.maximumModelTokens ? "model_tokens" : null, use.wallClockSeconds >= budget.maximumWallClockSeconds ? "wall_clock" : null, use.gpuSeconds > budget.maximumGpuSeconds ? "gpu_seconds" : null, use.diskBytes >= budget.maximumDiskBytes ? "disk_bytes" : null].filter((value): value is string => Boolean(value)); if (exhausted.length) { this.emit(event(project.projectId, missionId, "mission.budget_exhausted", { exhausted, use: use as unknown as JsonValue, limits: budget as unknown as JsonValue })); this.research.transitionMission(project.projectId, missionId, mission.version, "blocked", `budget-exhausted-${createId("cmd")}`); return; }
      const graph = new VersionedDag(missionId, mission.value.nodes, "hydrated", mission.value.graphVersion);
      if (graph.completionReady()) { await this.finalReview(project, mission); return; }
      const frontier = this.scheduler.frontier(graph, { paused: false, approvals: new Set(), workspaceConflicts: new Set(), availableRoles: new Set(["librarian_researcher", "general_worker", "reviewer"]), available: { agents: Math.max(0, budget.maximumConcurrentAgents - this.agents.inspect().filter((agent) => agent.missionId === missionId).length), gpuJobs: budget.maximumGpuSeconds > use.gpuSeconds ? 1 : 0, gpuSeconds: Math.max(0, budget.maximumGpuSeconds - use.gpuSeconds), modelTokens: Math.max(0, budget.maximumModelTokens - use.modelTokens), diskBytes: Math.max(0, budget.maximumDiskBytes - use.diskBytes) } });
      const node = frontier[0]; if (!node) { if (!mission.value.nodes.some((item) => ["leased", "working", "postflight", "reviewing"].includes(item.state))) this.research.transitionMission(project.projectId, missionId, mission.version, "blocked", `supervisor-blocked-${createId("cmd")}`); return; }
      await this.directorCycle(project, mission, node); mission = this.research.mission(project.projectId, missionId); if (mission.state === "running") await this.runNode(project, mission, node.id);
    } catch (error) {
      let mission = this.research.mission(project.projectId, missionId); this.emit(event(project.projectId, missionId, "mission.supervisor_error", { message: error instanceof Error ? error.message : "Mission supervisor failed" })); if (mission.state === "reviewing") mission = this.research.transitionMission(project.projectId, missionId, mission.version, "running", `supervisor-review-failed-${createId("cmd")}`); if (mission.state === "running") this.research.transitionMission(project.projectId, missionId, mission.version, "blocked", `supervisor-error-${createId("cmd")}`);
    } finally { this.busy.delete(missionId); }
  }

  private async directorCycle(project: RegisteredProject, mission: Stored<MissionProjection>, node: GraphNode): Promise<void> {
    const agentId = createId("agt"); const taskId = createId("tsk");
    await this.runSession(project, { agentId, taskId, missionId: mission.entityId, role: "mission_director" }, `Inspect Mission ${mission.entityId} graph version ${mission.value.graphVersion}. The deterministic scheduler selected ready node ${node.id} (${node.title}). Confirm it advances objective ${JSON.stringify(mission.value.objective)}. Submit exactly one ${schemaUri("mission-director-cycle")} record through nosh_response_submit; directorAgentId=${agentId}, missionId=${mission.entityId}, observedGraphVersion=${mission.value.graphVersion}. Do not claim completion or mutate state in prose.`);
    const record = this.research.terminalRecord(project.projectId, taskId, "mission-director-cycle") as { directorAgentId?: string; missionId?: string; observedGraphVersion?: number; northStarCheck?: { currentWorkContributes?: boolean } } | undefined;
    if (!record || record.directorAgentId !== agentId || record.missionId !== mission.entityId || record.observedGraphVersion !== mission.value.graphVersion || record.northStarCheck?.currentWorkContributes !== true) throw new Error("Mission Director did not submit a current, goal-aligned cycle record");
  }

  private reconcileOrphanedNodes(projectId: string, mission: Stored<MissionProjection>): Stored<MissionProjection> { for (const stale of mission.value.nodes.filter((node) => ["leased", "working", "postflight", "reviewing"].includes(node.state))) { if (stale.state === "leased") mission = this.research.transitionMissionNode(projectId, mission.entityId, mission.version, stale.id, "ready", `recovery-release-${stale.lease?.leaseId ?? createId("cmd")}`); else { mission = this.research.transitionMissionNode(projectId, mission.entityId, mission.version, stale.id, "failed", `recovery-failed-${stale.lease?.leaseId ?? createId("cmd")}`); const current = mission.value.nodes.find((node) => node.id === stale.id)!; if (current.attempt < current.maximumAttempts) mission = this.research.transitionMissionNode(projectId, mission.entityId, mission.version, stale.id, "ready", `recovery-ready-${stale.lease?.leaseId ?? createId("cmd")}`); else { this.emit(event(projectId, mission.entityId, "mission.node_recovered", { nodeId: stale.id, priorState: stale.state, disposition: "attempt_budget_exhausted" })); return this.research.transitionMission(projectId, mission.entityId, mission.version, "blocked", `recovery-blocked-${stale.lease?.leaseId ?? createId("cmd")}`); } } this.emit(event(projectId, mission.entityId, "mission.node_recovered", { nodeId: stale.id, priorState: stale.state, disposition: "ready_for_new_lease" })); } return mission; }

  private async runNode(project: RegisteredProject, mission: Stored<MissionProjection>, nodeId: string): Promise<void> {
    let node = mission.value.nodes.find((item) => item.id === nodeId); if (!node || node.state !== "ready") return; this.scheduler.recordAllocation(nodeId);
    const role = roleFor(node); const agentId = createId("agt"); const taskId = createId("tsk"); const expiresAt = new Date(Date.now() + 30 * 60_000).toISOString();
    mission = this.research.transitionMissionNode(project.projectId, mission.entityId, mission.version, nodeId, "leased", `lease-${taskId}`, { leaseId: taskId, ownerId: agentId, version: mission.version, expiresAt });
    mission = this.research.transitionMissionNode(project.projectId, mission.entityId, mission.version, nodeId, "working", `working-${taskId}`); node = mission.value.nodes.find((item) => item.id === nodeId)!;
    const schema = role === "reviewer" ? "review-verdict" : role === "librarian_researcher" ? "librarian-completion" : "general-worker-completion"; let packet: TaskPacketRecord; let directReview: ReviewRequestRecord | null = null;
    try { packet = this.issueTaskPacket(project, mission, node, agentId, taskId, role, schema, expiresAt); if (role === "reviewer") { const producer = [...this.research.records(project.projectId, "mission-director-cycle")].reverse().find((record) => (record as { missionId?: string }).missionId === mission.entityId) as { directorAgentId?: string } | undefined; if (!producer?.directorAgentId) throw new Error("Review requires a durable producer identity"); directReview = this.issueReviewRequest(project, mission, taskId, agentId, [producer.directorAgentId], "task", "graph_node", node.id, mission.version, node.id, node.title, [{ kind: "contract_task.packet", id: taskId, hash: sha256(packet as unknown as JsonValue) }], [], []); } await this.runSession(project, { agentId, taskId, missionId: mission.entityId, role, taskPermissions: packet.permissions, taskWorkspace: packet.workspace }, `Acknowledge this daemon-issued Task Packet with ${schemaUri("task-acknowledgement")} before work: ${JSON.stringify(packet)}. Then complete only graph node ${node.id}: ${node.title}. ${directReview ? `Review Request: ${JSON.stringify(directReview)}. ` : ""}Submit exactly one ${schemaUri(schema)} record with the matching NOSH tool. ${role === "reviewer" ? `Set reviewerAgentId=${agentId} and target.targetId=${node.id}.` : "Prose cannot change node state; deterministic postflight checks the packet, Git state, validators, and Artifact references before Review."}`); } catch (error) { this.emit(event(project.projectId, mission.entityId, "task.execution_failed", { taskId, nodeId, message: error instanceof Error ? error.message : "Task execution failed" })); await this.retryOrBlock(project.projectId, mission.entityId, node.id, taskId); return; }
    if (!this.acknowledged(project.projectId, taskId, agentId, packet)) { await this.retryOrBlock(project.projectId, mission.entityId, node.id, taskId); return; }
    if (role === "reviewer") { mission = this.research.transitionMissionNode(project.projectId, mission.entityId, mission.version, node.id, "postflight", `postflight-${taskId}`); mission = this.research.transitionMissionNode(project.projectId, mission.entityId, mission.version, node.id, "reviewing", `reviewing-${taskId}`); await this.applyReview(project.projectId, mission, node.id, taskId, agentId, directReview!); return; }
    const completion = this.research.terminalRecord(project.projectId, taskId, schema); const postflight = completion ? this.postflight(project, packet, schema, completion) : ["missing completion record"];
    if (postflight.length) { this.emit(event(project.projectId, mission.entityId, "task.postflight_failed", { taskId, nodeId, issues: postflight })); await this.retryOrBlock(project.projectId, mission.entityId, node.id, taskId); return; }
    mission = this.research.transitionMissionNode(project.projectId, mission.entityId, mission.version, node.id, "postflight", `postflight-${taskId}`); mission = this.research.transitionMissionNode(project.projectId, mission.entityId, mission.version, node.id, "reviewing", `reviewing-${taskId}`);
    const reviewerId = createId("agt"); const reviewTask = createId("tsk"); const required = completionRefs(completion!); const reviewRequest = this.issueReviewRequest(project, mission, reviewTask, reviewerId, [agentId], "task", "graph_node", node.id, mission.version, node.id, node.title, [{ kind: "contract_task.packet", id: taskId, hash: sha256(packet as unknown as JsonValue) }], required.artifactIds, required.evidenceIds); await this.runSession(project, { agentId: reviewerId, taskId: reviewTask, missionId: mission.entityId, role: "reviewer" }, `Independently review node ${node.id} (${node.title}) at Mission projection version ${mission.version}. Worker ${agentId} submitted: ${JSON.stringify(completion)}. Review Request: ${JSON.stringify(reviewRequest)}. Submit exactly one ${schemaUri("review-verdict")} using nosh_review_submit with the exact Review Request identities and criterion-level evidence.`); await this.applyReview(project.projectId, mission, node.id, reviewTask, reviewerId, reviewRequest);
  }

  private async applyReview(projectId: string, mission: Stored<MissionProjection>, nodeId: string, taskId: string, reviewerId: string, request: ReviewRequestRecord): Promise<void> {
    const review = this.research.terminalRecord(projectId, taskId, "review-verdict") as ReviewVerdictRecord | undefined;
    if (review && reviewPassesRequest(review, request, reviewerId)) { this.research.transitionMissionNode(projectId, mission.entityId, mission.version, nodeId, "accepted", `accepted-${taskId}`); return; }
    if (review?.verdict === "BLOCKED" && review.reviewId === request.reviewId && review.reviewRequestId === request.reviewRequestId) { this.research.transitionMissionNode(projectId, mission.entityId, mission.version, nodeId, "blocked", `review-blocked-${taskId}`); return; }
    await this.retryOrBlock(projectId, mission.entityId, nodeId, taskId);
  }

  private async retryOrBlock(projectId: string, missionId: string, nodeId: string, taskId: string): Promise<void> {
    let mission = this.research.mission(projectId, missionId); let node = mission.value.nodes.find((item) => item.id === nodeId)!; if (["working", "reviewing"].includes(node.state)) mission = this.research.transitionMissionNode(projectId, missionId, mission.version, nodeId, "failed", `failed-${taskId}`); node = mission.value.nodes.find((item) => item.id === nodeId)!;
    if (node.attempt >= node.maximumAttempts) { this.research.transitionMission(projectId, missionId, mission.version, "blocked", `attempts-exhausted-${taskId}`); return; }
    const attempt: AttemptInput = { projectId, missionId, directionId: null, nodeId, parentExperimentId: null, taskType: node.type, hypothesisFamilyTags: [], commandClass: node.type, inScopeFiles: [], inputArtifactHashes: [], diffHash: null, evaluationContractHash: null, intendedDecision: node.title, expectedOutputType: "reviewed_node" }; const governor = new FocusGovernor(); for (const prior of this.research.scopedEvents(projectId, missionId, "focus.attempt_recorded")) { const payload = prior.payload as unknown as { input?: AttemptInput; delta?: {}; overrideReason?: "replication" | "transient_retry" | "controlled_seed" | null }; if (payload.input) governor.record(payload.input, payload.delta ?? {}, payload.overrideReason ?? null); } const focus = governor.record(attempt, {}); this.emit(event(projectId, missionId, "focus.attempt_recorded", { input: attempt as unknown as JsonValue, delta: {}, overrideReason: null, fingerprint: focus.fingerprint, progress: focus.progress }));
    mission = this.research.transitionMissionNode(projectId, missionId, mission.version, nodeId, "ready", `retry-${taskId}`); if (focus.alarm) { this.emit(event(projectId, missionId, "focus.alarm", focus.alarm as unknown as JsonValue)); this.research.transitionMissionNode(projectId, missionId, mission.version, nodeId, "blocked", `focus-blocked-${taskId}`); }
  }

  private async finalReview(project: RegisteredProject, mission: Stored<MissionProjection>): Promise<void> {
    const basis = this.research.missionCompletionBasis(project.projectId, mission.entityId);
    const issues = [...new Set([...this.research.missionCompletionIssues(project.projectId, mission.entityId), ...basis.issues])];
    if (issues.length) {
      this.emit(event(project.projectId, mission.entityId, "mission.completion_rejected", { issues }));
      this.research.transitionMission(project.projectId, mission.entityId, mission.version, "blocked", `completion-gate-${createId("cmd")}`);
      return;
    }
    mission = this.research.transitionMission(project.projectId, mission.entityId, mission.version, "reviewing", `final-review-${createId("cmd")}`);
    const reviewerId = createId("agt");
    const taskId = createId("tsk");
    const contract = { objective: mission.value.objective, deliverables: mission.value.deliverables, successCriteria: mission.value.successCriteria, finalReviewRubric: mission.value.finalReviewRubric };
    const claims = basis.claims.map((claim) => ({ claimId: String(claim.claimId), status: String(claim.status) }));
    const summaryArtifactId = this.completionArtifact(project, mission, "accepted-task-evidence", {
      contract,
      acceptedCompletions: basis.completions.map(({ nodeId, taskId: acceptedTaskId, completion, review, artifactIds, evidenceIds, validatorRunIds }) => ({ nodeId, taskId: acceptedTaskId, completion, review, artifactIds, evidenceIds, validatorRunIds })),
      unresolvedClaimIds: basis.unresolvedClaimIds,
      openDefectIds: basis.openDefectIds,
    });
    const reproducibilityArtifactId = this.completionArtifact(project, mission, "reproducibility", {
      graphVersion: mission.value.graphVersion,
      gitHead: git(project.repositoryRoot, ["rev-parse", "HEAD"]),
      acceptedTaskIds: basis.completions.map((completion) => completion.taskId),
      validatorRunIds: basis.validatorRunIds,
      deliverableArtifactIds: basis.artifactIds,
      evidenceIds: basis.evidenceIds,
    });
    const completionAttempt = this.research.submitted(project.projectId).filter((entry) => entry.event.scope.missionId === mission.entityId && (entry.record as { $schema?: string }).$schema === schemaUri("mission-completion-packet")).length + 1;
    const completion: JsonValue = {
      $schema: schemaUri("mission-completion-packet"),
      schemaVersion: 1,
      missionId: mission.entityId,
      objectiveVersion: 1,
      graphVersion: mission.value.graphVersion,
      completionAttempt,
      requiredNodeDisposition: mission.value.nodes.filter((node) => node.required).map((node) => ({ nodeId: node.id, state: node.state, attempt: node.attempt })),
      criteria: basis.criteria.map(({ criterionId, statement, taskId: criterionTaskId, reviewId, validatorRunIds, artifactIds, evidenceIds }) => ({ criterionId, statement, status: "pass", taskId: criterionTaskId, reviewId, validatorRunIds, artifactIds, evidenceIds })),
      deliverableArtifactIds: basis.artifactIds,
      claimAudit: {
        supportedClaimIds: claims.filter((claim) => ["supported", "partially_supported"].includes(claim.status)).map((claim) => claim.claimId),
        qualifiedClaimIds: claims.filter((claim) => ["qualified", "limited"].includes(claim.status)).map((claim) => claim.claimId),
        contradictedClaimIds: claims.filter((claim) => ["contradicted", "not_supported"].includes(claim.status)).map((claim) => claim.claimId),
        unresolvedClaimIds: basis.unresolvedClaimIds,
      },
      openDefectIds: basis.openDefectIds,
      policyExceptions: [],
      budgetReconciliation: this.research.missionBudgetUse(project.projectId, mission.entityId) as unknown as JsonValue,
      reproducibilityManifestArtifactId: reproducibilityArtifactId,
      directorSummaryArtifactId: summaryArtifactId,
      deterministicPreReview: { status: "pass", validatorRunIds: basis.validatorRunIds },
      builtAt: new Date().toISOString(),
    };
    this.research.submitDaemonRecord(project.projectId, { projectId: project.projectId, missionId: mission.entityId, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: null }, `mission:${mission.entityId}`, completion, `mission-completion:${mission.entityId}:${completionAttempt}`);
    const producers = [...new Set(basis.completions.map((completion) => completion.agentId))];
    const request = this.issueReviewRequest(project, mission, taskId, reviewerId, producers, "mission_completion", "mission_record", mission.entityId, mission.version, mission.entityId, mission.value.successCriteria.join("; "), [{ kind: "contract_mission", id: mission.entityId, hash: sha256(contract as unknown as JsonValue) }, { kind: "contract_mission.completion", id: mission.entityId, hash: sha256(completion) }], [...new Set([...basis.artifactIds, summaryArtifactId, reproducibilityArtifactId])], basis.evidenceIds);
    await this.runSession(project, { agentId: reviewerId, taskId, missionId: mission.entityId, role: "reviewer" }, `Perform the independent final Mission review for ${mission.entityId}, projection version ${mission.version}. Audit accepted task completions, their exact independent Reviews, stored deliverable artifacts, evidence, validators, claims, and defects: ${JSON.stringify({ basis, completion })}. Review Request: ${JSON.stringify(request)}. Submit ${schemaUri("review-verdict")} through nosh_review_submit with the exact Review Request identities.`);
    const review = this.research.terminalRecord(project.projectId, taskId, "review-verdict") as ReviewVerdictRecord | undefined;
    if (review && reviewPassesRequest(review, request, reviewerId)) this.research.transitionMission(project.projectId, mission.entityId, mission.version, "completed", `completed-${taskId}`);
    else {
      mission = this.research.transitionMission(project.projectId, mission.entityId, mission.version, "running", `final-repair-${taskId}`);
      this.research.transitionMission(project.projectId, mission.entityId, mission.version, "blocked", `final-review-blocked-${taskId}`);
    }
  }

  private completionArtifact(project: RegisteredProject, mission: Stored<MissionProjection>, name: string, value: JsonValue): string { const path = join(project.repositoryRoot, ".nosh", "mission", mission.entityId, `${name}.json`); const content = `${canonicalJson(value)}\n`; const digest = sha256(content); const key = digest.slice(7, 23); const write = this.research.beginExternalOperation(project.projectId, "filesystem.write", `mission-file:${mission.entityId}:${name}:${key}`, { path, content, digest }); mkdirSync(join(project.repositoryRoot, ".nosh", "mission", mission.entityId), { recursive: true }); if (!existsSync(path) || sha256(readFileSync(path, "utf8")) !== digest) { const temporary = `${path}.${write.intentId}.tmp`; writeFileSync(temporary, content, "utf8"); renameSync(temporary, path); } if (sha256(readFileSync(path, "utf8")) !== digest) throw new Error("Mission completion file verification failed"); this.research.completeExternalOperation(project.projectId, write.intentId, { path, digest }); const artifactId = `art_${sha256({ missionId: mission.entityId, name, digest }).slice(7, 39)}`; const copy = this.research.beginExternalOperation(project.projectId, "artifact.copy", `mission-artifact:${artifactId}`, { artifactId, path, digest }); const artifact = new ArtifactStore(join(project.repositoryRoot, ".nosh", "artifacts")).add({ artifactId, projectId: project.projectId, kind: `artifact_mission.${name}`, mediaType: "application/json", sourcePath: path, retentionClass: "accepted_evidence" }); this.research.completeExternalOperation(project.projectId, copy.intentId, { artifactId, contentHash: artifact.contentHash, version: artifact.version }); const record: JsonValue = { $schema: schemaUri("artifact"), schemaVersion: 1, artifactId, projectId: project.projectId, kind: `artifact_mission.${name}`, mediaType: "application/json", contentHash: artifact.contentHash, sizeBytes: artifact.sizeBytes, version: artifact.version, producer: { type: "noshd" }, scope: { missionId: mission.entityId, directionId: null, autoresearchId: null, experimentId: null }, git: null, evaluationContractHash: null, retentionClass: "accepted_evidence", remotePreviewPolicy: "encrypted_allowed", redactionStatus: "checked", createdAt: artifact.createdAt }; this.research.submitDaemonRecord(project.projectId, { projectId: project.projectId, missionId: mission.entityId, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: null }, `artifact:${artifactId}`, record, `mission-artifact-record:${artifactId}`); return artifactId; }

  private issueReviewRequest(project: RegisteredProject, mission: Stored<MissionProjection>, taskId: string, reviewerAgentId: string, producerAgentIds: string[], reviewType: ReviewRequestRecord["reviewType"], targetType: string, targetId: string, targetVersion: number, criterionId: string, criterion: string, contractRefs: ReviewRequestRecord["contractRefs"], requiredArtifactIds: string[], requiredEvidenceIds: string[]): ReviewRequestRecord {
    if (!producerAgentIds.length || producerAgentIds.includes(reviewerAgentId)) throw new Error("Review requires an independent producer");
    const packetReference = contractRefs.find((reference) => reference.kind === "contract_task.packet");
    const packet = packetReference ? this.research.submitted(project.projectId, `task:${packetReference.id}`).find((entry) => (entry.record as { $schema?: string }).$schema === schemaUri("task-packet"))?.record as TaskPacketRecord | undefined : undefined;
    const criteria = reviewType === "mission_completion"
      ? mission.value.successCriteria.map((statement, index) => ({ criterionId: `criterion_${index + 1}`, statement, required: true as const, severityIfFailed: "blocking" as const }))
      : packet?.acceptanceCriteria.map(({ criterionId: packetCriterionId, statement }) => ({ criterionId: packetCriterionId, statement, required: true as const, severityIfFailed: "blocking" as const })) ?? [{ criterionId, statement: criterion, required: true as const, severityIfFailed: "blocking" as const }];
    const reviewId = createId("rev");
    const request: ReviewRequestRecord = {
      $schema: schemaUri("review-request"), schemaVersion: 1, reviewRequestId: `request_${reviewId.slice(4)}`, reviewId, reviewType, target: { targetType, targetId, targetVersion }, scope: { projectId: project.projectId, missionId: mission.entityId, directionId: null, autoresearchId: null },
      producerAgentIds: [...new Set(producerAgentIds)], reviewerAgentId, independenceCheck: "pass", contractRefs, requiredArtifactIds: [...new Set(requiredArtifactIds)], requiredEvidenceIds: [...new Set(requiredEvidenceIds)],
      deterministicPostflight: { status: "pass", validatorRunIds: [reviewType === "mission_completion" ? "validator_mission.completion" : "validator_task.postflight"] }, criteria, allowedVerdicts: ["PASS", "REVISE", "REDESIGN", "BLOCKED"], issuedAt: new Date().toISOString(),
    };
    this.research.submitDaemonRecord(project.projectId, { projectId: project.projectId, missionId: mission.entityId, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: reviewerAgentId }, `task:${taskId}`, request as unknown as JsonValue, `review-request:${taskId}`);
    return request;
  }
  private issueTaskPacket(project: RegisteredProject, mission: Stored<MissionProjection>, node: GraphNode, agentId: string, taskId: string, role: "librarian_researcher" | "general_worker" | "reviewer", responseSchema: string, expiresAt: string): TaskPacketRecord {
    const head = git(project.repositoryRoot, ["rev-parse", "HEAD"]);
    const branch = git(project.repositoryRoot, ["branch", "--show-current"]);
    if (!branch || git(project.repositoryRoot, ["status", "--porcelain"])) throw new Error("Task preflight requires a clean checked-out Git branch");
    const use = this.research.missionBudgetUse(project.projectId, mission.entityId);
    const remainingWall = Math.max(1, mission.value.budgets.maximumWallClockSeconds - use.wallClockSeconds);
    const remainingTokens = Math.max(1, mission.value.budgets.maximumModelTokens - use.modelTokens);
    const outputKind = role === "general_worker" ? "output_code.commit" : role === "librarian_researcher" ? "output_research.report" : "output_review.verdict";
    const acceptanceCriteria = node.criterionIds.length ? node.criterionIds.map((criterionId) => {
      const index = Number(criterionId.slice("criterion_".length)) - 1;
      return { criterionId, statement: mission.value.successCriteria[index]!, validatorIds: ["validator_task.postflight"], reviewRubricIds: ["rubric_scientific.correctness"], required: true };
    }) : [{ criterionId: node.id, statement: node.title, validatorIds: ["validator_task.postflight"], reviewRubricIds: ["rubric_scientific.correctness"], required: true }];
    const permissions: TaskPermissions = {
      network: role === "librarian_researcher" ? "allowlisted" : "disabled",
      subprocess: "disabled",
      gitCommit: role === "general_worker",
      gitPush: false,
      delegation: "request_only",
      networkAllowlist: role === "librarian_researcher" ? [...librarianResearchHosts] : [],
      allowedToolIds: [
        "tool_nosh.task.acknowledge",
        role === "reviewer" ? "tool_nosh.review.submit" : "tool_nosh.response.submit",
        "tool_nosh.progress.emit",
        "tool_nosh.blocker.submit",
        ...(role === "reviewer" ? [] : ["tool_nosh.delegation.request"]),
        "tool_pi.read",
        ...(role === "reviewer" ? [] : ["tool_pi.edit", "tool_pi.write"]),
        ...(role === "general_worker" ? ["tool_nosh.git.commit"] : []),
        ...(role === "librarian_researcher" ? ["tool_nosh.network.read"] : []),
      ],
    };
    const packet: TaskPacketRecord = {
      $schema: schemaUri("task-packet"), schemaVersion: 1, templateVersion: "1.0.0",
      taskId, attempt: node.attempt, taskType: `task_${node.type.replaceAll("_", ".")}`, assignedRole: role, assignedAgentId: agentId,
      scope: { projectId: project.projectId, missionId: mission.entityId, directionId: null, autoresearchId: null, experimentId: null, graphNodeId: node.id },
      goalStack: { projectGoalId: project.projectId, projectGoalSummary: mission.value.objective, missionCriterionIds: node.criterionIds, missionCriterionSummary: acceptanceCriteria.map((criterion) => criterion.statement).join("; "), directionQuestionId: null, directionQuestionSummary: null, nodeObjective: node.title },
      whyNow: "The deterministic Mission frontier selected this ready node.",
      instructions: [`Complete only ${node.title}`, "Do not modify protected NOSH contracts", "Run and report deterministic validators", ...(role === "general_worker" ? ["Create commits only with nosh_git_commit and explicit changed paths."] : [])],
      inScope: ["**"], outOfScope: [".nosh/contracts/**"], inputArtifactIds: [], inputEvidenceIds: [],
      requiredOutputs: [{ outputId: `out_${taskId.slice(4)}`, kind: outputKind, required: true }],
      acceptanceCriteria,
      workspace: { worktreeId: `wt_${taskId.slice(4)}`, branch, startingCommit: head, writeScopes: ["**"], protectedScopes: [".nosh/contracts/**"] },
      permissions,
      budget: { deadline: new Date(Date.now() + Math.min(7_200, remainingWall) * 1_000).toISOString(), maximumWallClockSeconds: Math.min(7_200, remainingWall), maximumModelTokens: Math.min(60_000, remainingTokens), maximumToolCalls: 100, maximumRepairAttempts: Math.max(0, node.maximumAttempts - node.attempt) },
      progressPolicy: { milestoneIntervalSeconds: 900, emitOnFirstDurableDelta: true, emitOnBlocker: true, emitOnAnomaly: true, maximumSilentSeconds: 1_200 },
      responseSchema: schemaUri(responseSchema),
      observedVersions: { missionGraphVersion: mission.value.graphVersion, missionProjectionVersion: mission.version, leaseVersion: mission.version },
      lease: { leaseId: taskId, expiresAt, heartbeatSeconds: 15 }, issuedBy: "nosh", issuedAt: new Date().toISOString(),
    };
    this.research.submitDaemonRecord(project.projectId, { projectId: project.projectId, missionId: mission.entityId, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId }, `task:${taskId}`, packet as unknown as JsonValue, `task-packet:${taskId}`);
    return packet;
  }
  private acknowledged(projectId: string, taskId: string, agentId: string, packet: TaskPacketRecord): boolean { const record = this.record(projectId, taskId, "task-acknowledgement") as { taskId?: string; attempt?: number; agentId?: string; decision?: string; understoodOutputIds?: string[]; understoodCriterionIds?: string[]; observedLeaseId?: string; observedStartingCommit?: string } | undefined; return Boolean(record && validateRecord(schemaUri("task-acknowledgement"), record).ok && record.taskId === taskId && record.attempt === packet.attempt && record.agentId === agentId && record.decision === "accepted" && equal(record.understoodOutputIds, packet.requiredOutputs.map((output) => output.outputId)) && equal(record.understoodCriterionIds, packet.acceptanceCriteria.map((criterion) => criterion.criterionId)) && record.observedLeaseId === packet.lease.leaseId && record.observedStartingCommit === packet.workspace.startingCommit); }
  private postflight(project: RegisteredProject, packet: TaskPacketRecord, schema: string, record: JsonValue): string[] { const parsed = validateRecord(schemaUri(schema), record); if (!parsed.ok) return parsed.errors.map((error) => `${error.pointer} ${error.message}`); const value = parsed.value as Record<string, unknown>; const issues: string[] = []; if (value.taskOutcome !== "completed" || value.readyForReview !== true) issues.push("completion is not ready for Review"); if (schema === "general-worker-completion") { if (value.readyForDeterministicPostflight !== true) issues.push("worker did not request deterministic postflight"); const code = value.codeChanges as { startingCommit: string; endingCommit: string; branch: string; changedPaths: string[]; diffArtifactId: string | null }; issues.push(...validateGitCompletion(project.repositoryRoot, packet.workspace, code)); const commands = value.commands as Array<{ exitCode: number | null }>; if (commands.some((command) => command.exitCode !== 0)) issues.push("a declared command did not pass"); const criteria = value.criteria as Array<{ criterionId: string; workerClaim: string; validatorRunIds: string[]; artifactIds: string[] }>; for (const criterion of packet.acceptanceCriteria.filter((entry) => entry.required)) { const result = criteria.find((entry) => entry.criterionId === criterion.criterionId); if (!result || result.workerClaim !== "satisfied" || !result.validatorRunIds.length) issues.push(`criterion ${criterion.criterionId} lacks a passing validator`); } const ids = [code.diffArtifactId, ...((value.workPerformed as Array<{ artifactIds: string[] }>).flatMap((work) => work.artifactIds)), ...criteria.flatMap((criterion) => criterion.artifactIds)].filter((id): id is string => Boolean(id)); const artifacts = new ArtifactStore(join(project.repositoryRoot, ".nosh", "artifacts")); for (const id of ids) try { artifacts.resolve(id); } catch { issues.push(`artifact ${id} does not resolve`); } } else { const librarian = value as unknown as { bibliographyArtifactId: string; reportArtifactId: string; sources: Array<{ artifactId: string }>; findings: Array<{ support: Array<{ artifactId: string }> }> }; const artifacts = new ArtifactStore(join(project.repositoryRoot, ".nosh", "artifacts")); for (const id of [librarian.bibliographyArtifactId, librarian.reportArtifactId, ...librarian.sources.map((source) => source.artifactId), ...librarian.findings.flatMap((finding) => finding.support.map((support) => support.artifactId))]) try { artifacts.resolve(id); } catch { issues.push(`artifact ${id} does not resolve`); } } return [...new Set(issues)]; }

  private async runSession(project: RegisteredProject, input: { agentId: string; taskId: string; missionId: string; role: PiSessionOptions["role"]; taskPermissions?: PiSessionOptions["taskPermissions"]; taskWorkspace?: PiSessionOptions["taskWorkspace"] }, prompt: string): Promise<void> {
    if (!this.runtime) {
      const options: PiSessionOptions = { projectId: project.projectId, missionId: input.missionId, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, taskId: input.taskId, agentId: input.agentId, role: input.role, cwd: project.repositoryRoot, packagePath: this.packagePath, ...(input.taskPermissions ? { taskPermissions: input.taskPermissions } : {}), ...(input.taskWorkspace ? { taskWorkspace: input.taskWorkspace } : {}) };
      await this.agents.start(options);
      try { await this.agents.prompt(input.agentId, prompt); } finally { this.agents.stop(input.agentId); }
      return;
    }
    const threadId = createId("thr"); const issuedAt = new Date().toISOString(); const key = `runtime:${input.taskId}`;
    await this.runtime.execute({ $schema: schemaUri("runtime-instruction"), schemaVersion: 1, instructionId: createId("ins"), projectId: project.projectId, idempotencyKey: `${key}:open`, proposedByAgentId: null, issuedAt, operation: "THREAD_OPEN", threadId, taskId: input.taskId, initialAgentId: input.agentId, ownerScope: { missionId: input.missionId, directionId: null, autoresearchId: null, experimentId: null, graphNodeId: null }, role: input.role, purpose: `Execute Mission task ${input.taskId}`, executionMode: "background", parentThreadId: null, inputRefs: [], skillIds: [], capabilities: [], taskPermissions: input.taskPermissions ?? null, taskWorkspace: input.taskWorkspace ?? null, budget: { maximumToolCalls: 200, maximumModelTokens: 100_000, maximumWallClockSeconds: 7_200 } });
    try {
      await this.runtime.execute({ $schema: schemaUri("runtime-instruction"), schemaVersion: 1, instructionId: createId("ins"), projectId: project.projectId, idempotencyKey: `${key}:step`, proposedByAgentId: null, issuedAt, operation: "THREAD_STEP", threadId, objective: prompt, expectedEpisodeType: `episode_${input.role}`, inputRefs: [], skillIds: [] });
    } finally {
      await this.runtime.execute({ $schema: schemaUri("runtime-instruction"), schemaVersion: 1, instructionId: createId("ins"), projectId: project.projectId, idempotencyKey: `${key}:stop`, proposedByAgentId: null, issuedAt: new Date().toISOString(), operation: "STOP", threadId, programId: null, reason: "Task session reached its boundary" });
    }
  }
  private record(projectId: string, taskId: string, schema: string): JsonValue | undefined { return this.research.submitted(projectId, `task:${taskId}`).find((entry) => (entry.record as { $schema?: string }).$schema === schemaUri(schema))?.record; }
}

type TaskPacketRecord = { $schema: string; schemaVersion: number; templateVersion: string; taskId: string; attempt: number; taskType: string; assignedRole: "librarian_researcher" | "general_worker" | "reviewer"; assignedAgentId: string; scope: { projectId: string; missionId: string; directionId: null; autoresearchId: null; experimentId: null; graphNodeId: string }; goalStack: { projectGoalId: string; projectGoalSummary: string; missionCriterionIds: string[]; missionCriterionSummary: string; directionQuestionId: null; directionQuestionSummary: null; nodeObjective: string }; whyNow: string; instructions: string[]; inScope: string[]; outOfScope: string[]; inputArtifactIds: string[]; inputEvidenceIds: string[]; requiredOutputs: Array<{ outputId: string; kind: string; required: boolean }>; acceptanceCriteria: Array<{ criterionId: string; statement: string; validatorIds: string[]; reviewRubricIds: string[]; required: boolean }>; workspace: { worktreeId: string; branch: string; startingCommit: string; writeScopes: string[]; protectedScopes: string[] }; permissions: TaskPermissions; budget: { deadline: string; maximumWallClockSeconds: number; maximumModelTokens: number; maximumToolCalls: number; maximumRepairAttempts: number }; progressPolicy: { milestoneIntervalSeconds: number; emitOnFirstDurableDelta: boolean; emitOnBlocker: boolean; emitOnAnomaly: boolean; maximumSilentSeconds: number }; responseSchema: string; observedVersions: Record<string, number>; lease: { leaseId: string; expiresAt: string; heartbeatSeconds: number }; issuedBy: "nosh"; issuedAt: string };
type ReviewRequestRecord = { $schema: string; schemaVersion: number; reviewRequestId: string; reviewId: string; reviewType: "task" | "experiment" | "experiment_round" | "autoresearch_closure" | "direction_closure" | "claim" | "paper_section" | "mission_completion"; target: { targetType: string; targetId: string; targetVersion: number }; scope: { projectId: string; missionId: string; directionId: null; autoresearchId: null }; producerAgentIds: string[]; reviewerAgentId: string; independenceCheck: "pass"; contractRefs: Array<{ kind: string; id: string; hash: string }>; requiredArtifactIds: string[]; requiredEvidenceIds: string[]; deterministicPostflight: { status: "pass"; validatorRunIds: string[] }; criteria: Array<{ criterionId: string; statement: string; required: true; severityIfFailed: "blocking" }>; allowedVerdicts: Array<"PASS" | "REVISE" | "REDESIGN" | "BLOCKED">; issuedAt: string };
type ReviewVerdictRecord = { $schema?: string; reviewId?: string; reviewRequestId?: string; reviewType?: string; target?: { targetType?: string; targetId?: string; targetVersion?: number }; reviewerAgentId?: string; independenceCheck?: string; verdict?: string; criteria?: Array<{ criterionId?: string; status?: string }>; reviewedArtifactIds?: string[]; reviewedEvidenceIds?: string[] };
function completionRefs(record: JsonValue): { artifactIds: string[]; evidenceIds: string[] } { const value = record as unknown as { workPerformed?: Array<{ artifactIds?: string[] }>; codeChanges?: { diffArtifactId?: string | null }; criteria?: Array<{ artifactIds?: string[] }>; scientificImpact?: { evidenceIds?: string[] }; bibliographyArtifactId?: string; reportArtifactId?: string; sources?: Array<{ artifactId?: string }>; findings?: Array<{ support?: Array<{ artifactId?: string }> }> }; const artifactIds = [value.codeChanges?.diffArtifactId, value.bibliographyArtifactId, value.reportArtifactId, ...(value.workPerformed ?? []).flatMap((entry) => entry.artifactIds ?? []), ...(value.criteria ?? []).flatMap((entry) => entry.artifactIds ?? []), ...(value.sources ?? []).map((entry) => entry.artifactId), ...(value.findings ?? []).flatMap((entry) => (entry.support ?? []).map((support) => support.artifactId))].filter((id): id is string => Boolean(id)); return { artifactIds: [...new Set(artifactIds)], evidenceIds: [...new Set(value.scientificImpact?.evidenceIds ?? [])] }; }
function reviewPassesRequest(review: ReviewVerdictRecord, request: ReviewRequestRecord, reviewerId: string): boolean { if (!validateRecord(schemaUri("review-verdict"), review as JsonValue).ok || review.reviewId !== request.reviewId || review.reviewRequestId !== request.reviewRequestId || review.reviewType !== request.reviewType || review.reviewerAgentId !== reviewerId || review.independenceCheck !== "pass" || review.verdict !== "PASS" || review.target?.targetType !== request.target.targetType || review.target.targetId !== request.target.targetId || review.target.targetVersion !== request.target.targetVersion) return false; if (!request.criteria.every((required) => review.criteria?.some((criterion) => criterion.criterionId === required.criterionId && criterion.status === "PASS"))) return false; return request.requiredArtifactIds.every((id) => review.reviewedArtifactIds?.includes(id)) && request.requiredEvidenceIds.every((id) => review.reviewedEvidenceIds?.includes(id)); }
function git(cwd: string, args: string[]): string { const result = spawnSync("git", ["-C", cwd, ...args], { encoding: "utf8", windowsHide: true }); if (result.status !== 0) throw new Error(result.stderr.trim() || `git ${args[0]} failed`); return result.stdout.trim(); }
function equal(left: string[] | undefined, right: string[]): boolean { return Boolean(left && left.length === right.length && left.every((value, index) => value === right[index])); }

function roleFor(node: GraphNode): "librarian_researcher" | "general_worker" | "reviewer" { if (node.type === "literature_review") return "librarian_researcher"; if (["claim_review", "final_review", "approval"].includes(node.type)) return "reviewer"; return "general_worker"; }
function event(projectId: string, missionId: string, type: string, payload: JsonValue): EventDraft { return { $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type, source: "mission_supervisor", scope: { projectId, missionId, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: null }, correlationId: missionId, causationId: null, payload }; }
