import { GitWorkspaceManager, worktreeGitEnvironment } from "@nosh/git-workspaces";
import { createId } from "@nosh/core";
import { ArtifactStore } from "@nosh/evidence";
import { existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { VersionedDag, type GraphNode } from "@nosh/graph";
import type { PiAdapter, PiSessionOptions } from "@nosh/pi-adapter";
import type { OrchestrationRuntime } from "@nosh/orchestration-runtime";
import { FocusGovernor, Scheduler, type AttemptInput } from "@nosh/scheduler";
import { canonicalJson, schemaUri, sha256, validateRecord, type EventDraft, type JsonValue, type TaskPermissions, episodeTypeForRole } from "@nosh/wire";
import type { RegisteredProject } from "@nosh/persistence";
import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { SPAWN_NODE_TYPES, daemonTaskFacts, declaredTaskCommands, missionOutcomes, librarianNetworkAllowed, librarianResearchHosts, taskCommandInstructions, taskCommandRuns, type MissionProjection, type ResearchControl, type Stored } from "./research-control.js";
import { DirectorSlip, directorRetryAllowed, missionBrief, type SpawnBriefContext } from "./director-brief.js";
import { createTaskWorktree, taskWorktreePath } from "./task-worktree.js";
import { completionInstructions, generalWorkerCompletionTemplate, librarianInstructions, reviewerInstructions } from "./task-templates.js";
import { completionRunIssues, validateGitCompletion, writeUpOnly } from "./task-postflight.js";
import { CHILD_BLOCKED_MS, allocateBudget, probeEvaluation, freezeContract, spawnIssues, spawnMissionChild, type JobRunner, type MissionSpawn } from "./research-spawner.js";
/**
 * What a failed attempt produced, for the focus governor: a session that never delivered a result is a transient retry;
 * otherwise the changed commit and the issues or review that rejected it, so only a genuinely repeated failure alarms.
 */
type FailureFingerprint = { transient: true } | { diffHash: string | null; evidence: JsonValue };
/** A research child blocked or paused this long, with nothing of its own running, is stopped so its spawn node can retry. */
/** Agents of the Mission itself, not of the Directions and Autoresearch it spawned. */
function ownAgent(agent: { missionId: string | null; directionId: string | null; autoresearchId: string | null }, missionId: string): boolean { return agent.missionId === missionId && !agent.directionId && !agent.autoresearchId; }
/** A spawn node leased to the research child it started. */
function childOwned(node: GraphNode): boolean { return SPAWN_NODE_TYPES.includes(node.type) && /^(dir|ar)_/.test(node.lease?.ownerId ?? ""); }
function changedCommit(completion: unknown): string | null {
  const code = (completion as { codeChanges?: { startingCommit?: string; endingCommit?: string } }).codeChanges;
  return code?.endingCommit && code.endingCommit !== code.startingCommit ? code.endingCommit : null;
}
export class MissionSupervisor {
  private timer: NodeJS.Timeout | undefined; private readonly busy = new Set<string>(); private readonly scheduler = new Scheduler();
  constructor(private readonly research: ResearchControl, private readonly agents: PiAdapter, private readonly projects: () => RegisteredProject[], private readonly packagePath: string, private readonly emit: (draft: EventDraft) => void, private readonly runtime?: OrchestrationRuntime, private readonly jobRunner?: JobRunner) {}

  // Each cycle records its own failures as events; only failures after shutdown (closed stores) reach here.
  start(): void { this.timer = setInterval(() => void this.tick().catch(() => undefined), 1_000); this.timer.unref(); }
  stop(): void { if (this.timer) clearInterval(this.timer); this.timer = undefined; }
  async tick(): Promise<void> { for (const project of this.projects()) for (const mission of this.research.missions(project.projectId)) { if (["completed", "stopped", "failed"].includes(mission.state)) this.scheduler.releaseMission(project.projectId, mission.entityId); else if (mission.state === "running" || mission.state === "pausing") void this.cycle(project, mission.entityId).catch(() => undefined); else if (mission.state === "reviewing") this.recoverInterruptedReview(project.projectId, mission.entityId); } }

  /** A final review cut short by a daemon restart leaves the Mission in reviewing, which no cycle handles; return it to running so the review runs again. */
  private recoverInterruptedReview(projectId: string, missionId: string): void {
    if (this.busy.has(missionId) || this.agents.inspect().some((agent) => agent.missionId === missionId && !agent.directionId && !agent.autoresearchId)) return;
    const mission = this.research.mission(projectId, missionId); if (mission.state !== "reviewing") return;
    this.research.transitionMission(projectId, missionId, mission.version, "running", `review-interrupted-${mission.version}`);
    this.emit(event(projectId, missionId, "mission.review_recovered", { fromVersion: mission.version }));
  }

  private async cycle(project: RegisteredProject, missionId: string): Promise<void> {
    // A safe pause is reported as paused only once every Mission agent has actually stopped at its boundary.
    const current = this.research.mission(project.projectId, missionId);
    if (current.state === "pausing") { if (!this.busy.has(missionId) && !this.agents.inspect().some((agent) => agent.missionId === missionId)) this.research.transitionMission(project.projectId, missionId, current.version, "paused", `mission-paused-at-boundary-${current.version}`); return; }
    if (this.busy.has(missionId) || this.agents.inspect().some((agent) => ownAgent(agent, missionId)) || this.runtime?.hasActiveFork(project.projectId, missionId, null)) return; this.busy.add(missionId);
    try {
      let mission = this.research.mission(project.projectId, missionId); if (mission.state !== "running") return; mission = this.reconcileOrphanedNodes(project.projectId, mission); if (mission.state !== "running") return; mission = await this.reconcileChildNodes(project.projectId, mission); if (mission.state !== "running") return; this.scheduler.activateMission(project.projectId, missionId); this.scheduler.restoreAllocations(Object.fromEntries(mission.value.nodes.map((node) => [node.id, node.attempt])));
      const use = this.research.missionBudgetUse(project.projectId, missionId); const budget = mission.value.budgets; const exhausted = [use.modelTokens >= budget.maximumModelTokens ? "model_tokens" : null, use.wallClockSeconds >= budget.maximumWallClockSeconds ? "wall_clock" : null, use.gpuSeconds > budget.maximumGpuSeconds ? "gpu_seconds" : null, use.diskBytes >= budget.maximumDiskBytes ? "disk_bytes" : null].filter((value): value is string => Boolean(value)); if (exhausted.length) { this.emit(event(project.projectId, missionId, "mission.budget_exhausted", { exhausted, use: use as unknown as JsonValue, limits: budget as unknown as JsonValue })); this.research.transitionMission(project.projectId, missionId, mission.version, "blocked", `budget-exhausted-${createId("cmd")}`); return; }
      const graph = new VersionedDag(missionId, mission.value.nodes, "hydrated", mission.value.graphVersion);
      if (graph.completionReady()) { await this.finalReview(project, mission); return; }
      const frontier = this.scheduler.frontier(graph, { paused: false, approvals: new Set(), workspaceConflicts: new Set(), availableRoles: new Set(["librarian_researcher", "general_worker", "reviewer"]), available: { agents: Math.max(0, budget.maximumConcurrentAgents - this.agents.inspect().filter((agent) => ownAgent(agent, missionId)).length), gpuJobs: budget.maximumGpuSeconds > use.gpuSeconds ? 1 : 0, gpuSeconds: Math.max(0, budget.maximumGpuSeconds - use.gpuSeconds), modelTokens: Math.max(0, budget.maximumModelTokens - use.modelTokens), diskBytes: Math.max(0, budget.maximumDiskBytes - use.diskBytes) } });
      const node = frontier[0]; if (!node) { if (!mission.value.nodes.some((item) => ["leased", "working", "postflight", "reviewing"].includes(item.state))) this.research.transitionMission(project.projectId, missionId, mission.version, "blocked", `supervisor-blocked-${createId("cmd")}`); return; }
      if (SPAWN_NODE_TYPES.includes(node.type)) { await this.spawnNode(project, mission, node); return; }
      await this.directorCycle(project, mission, node); mission = this.research.mission(project.projectId, missionId); if (mission.state === "running") await this.runNode(project, mission, node.id);
    } catch (error) {
      let mission = this.research.mission(project.projectId, missionId);
      // Work cut short by a user Stop is the expected effect of that Stop, not a supervisor failure.
      if (["stopping", "stopped", "completed", "failed"].includes(mission.state)) return;
      // An invalid cycle record is a model slip, not a stuck Mission: the next tick runs a fresh cycle.
      if (error instanceof DirectorSlip && directorRetryAllowed(this.research.scopedEvents(project.projectId, missionId).filter((entry) => !entry.scope.directionId && !entry.scope.autoresearchId), schemaUri("mission-director-cycle"), "mission.director_retry")) { this.emit(event(project.projectId, missionId, "mission.director_retry", { message: error.message })); return; }
      this.emit(event(project.projectId, missionId, "mission.supervisor_error", { message: error instanceof Error ? error.message : "Mission supervisor failed" })); if (mission.state === "reviewing") mission = this.research.transitionMission(project.projectId, missionId, mission.version, "running", `supervisor-review-failed-${createId("cmd")}`); if (mission.state === "running") this.research.transitionMission(project.projectId, missionId, mission.version, "blocked", `supervisor-error-${createId("cmd")}`);
    } finally { this.busy.delete(missionId); }
  }

  private async directorCycle(project: RegisteredProject, mission: Stored<MissionProjection>, node: GraphNode, spawn?: SpawnBriefContext): Promise<{ spawn?: MissionSpawn }> {
    const agentId = createId("agt"); const taskId = createId("tsk");
    const brief = missionBrief({ research: this.research, project, mission, node, agentId, ...(spawn ? { spawn } : {}) });
    try { await this.runSession(project, { agentId, taskId, missionId: mission.entityId, role: "mission_director" }, brief.prompt); }
    catch (error) { if (error instanceof Error && error.message.startsWith("Terminal output rejected")) throw new DirectorSlip(error.message); throw error; }
    const record = this.research.terminalRecord(project.projectId, taskId, "mission-director-cycle") as { cycleId?: string; directorAgentId?: string; missionId?: string; observedGraphVersion?: number; northStarCheck?: { currentWorkContributes?: boolean }; spawn?: MissionSpawn } | undefined;
    if (!record || record.cycleId !== brief.ids.cycleId || record.directorAgentId !== agentId || record.missionId !== mission.entityId || record.observedGraphVersion !== mission.value.graphVersion) throw new DirectorSlip("Mission Director did not submit the current cycle record");
    if (record.northStarCheck?.currentWorkContributes !== true) throw new Error("Mission Director halted the selected node; user attention required");
    return record.spawn ? { spawn: record.spawn } : {};
  }

  /**
   * A research_direction or autoresearch node: the Mission Director specifies the child, the daemon probes and freezes
   * its evaluation, then creates and starts it. The node stays working, leased to the child, until the child finishes.
   */
  private async spawnNode(project: RegisteredProject, mission: Stored<MissionProjection>, node: GraphNode): Promise<void> {
    const projectId = project.projectId; const attempt = node.attempt + 1; const key = `child-spawn:${mission.entityId}:${node.id}:${attempt}`;
    // A spawn interrupted by a restart resumes from its durable intent instead of asking the Director again.
    const prior = this.research.externalOperation(projectId, key);
    let childId: string;
    if (prior) {
      const request = prior.request as { spawn: MissionSpawn; evaluationContract: JsonValue; baseCommit: string; budget: { maximumModelTokens: number; maximumWallClockSeconds: number } };
      childId = spawnMissionChild(this.research, projectId, { mission, nodeId: node.id, attempt, spawn: request.spawn, evaluationContract: request.evaluationContract, baseCommit: request.baseCommit, budget: request.budget });
    } else {
      if (!this.jobRunner) throw new Error("Spawning research children requires the daemon's job runner");
      const allocation = allocateBudget(this.research, projectId, mission, node.type as MissionSpawn["kind"]);
      if (!allocation.ok) { this.emit(event(projectId, mission.entityId, "research.spawn_rejected", { nodeId: node.id, issues: [allocation.reason] })); this.research.transitionMissionNode(projectId, mission.entityId, mission.version, node.id, "blocked", `spawn-budget-${node.id}-${attempt}`); return; }
      const rejections = this.research.scopedEvents(projectId, mission.entityId, "research.spawn_rejected").filter((entry) => (entry.payload as { nodeId?: string; attempt?: number }).nodeId === node.id && (entry.payload as { attempt?: number }).attempt === attempt);
      const { spawn } = await this.directorCycle(project, mission, node, { commands: declaredTaskCommands(project).map(({ commandId, description, argv }) => ({ commandId, description, argv })), siblings: this.spawnSiblings(projectId, mission, node.id), rejections: rejections.map((entry) => ((entry.payload as { issues?: string[] }).issues ?? []).join("; ")) });
      mission = this.research.mission(projectId, mission.entityId); if (mission.state !== "running") return;
      const baseCommit = this.integrationHead(project, mission.entityId);
      const issues = spawn ? spawnIssues(project, spawn, node.id, node.type) : ["the Mission Director did not fill spawn for this node"];
      const probe = spawn && !issues.length ? await probeEvaluation(this.research, this.jobRunner, project, { missionId: mission.entityId, directionId: null, spawn, baseCommit }) : null;
      if (!spawn || !probe?.ok) {
        const failure = probe && !probe.ok ? probe : null;
        this.emit(event(projectId, mission.entityId, "research.spawn_rejected", { nodeId: node.id, attempt, issues: [...issues, ...(failure?.issues ?? [])], metricKeys: failure?.metricKeys ?? [], writtenJson: failure?.writtenJson ?? [] }));
        // Repeated rejections cost the node one attempt, so a Director that cannot specify a child does not loop forever.
        if (rejections.length + 1 >= 3) { mission = this.research.mission(projectId, mission.entityId); const leaseId = createId("tsk"); mission = this.research.transitionMissionNode(projectId, mission.entityId, mission.version, node.id, "leased", `spawn-giveup-lease-${node.id}-${attempt}`, { leaseId, ownerId: "agt_spawn", version: mission.version, expiresAt: new Date(Date.now() + 60_000).toISOString() }); this.research.transitionMissionNode(projectId, mission.entityId, mission.version, node.id, "working", `spawn-giveup-working-${node.id}-${attempt}`); await this.retryOrBlock(projectId, mission.entityId, node.id, leaseId, { diffHash: null, evidence: { spawnRejected: node.id, attempt } }); }
        return;
      }
      const evaluationContract = freezeContract(spawn, probe, { missionId: mission.entityId, baseCommit });
      this.emit(event(projectId, mission.entityId, "research.contract_probed", { nodeId: node.id, attempt, probeJobId: probe.probeJobId, baselineMetrics: probe.metrics }));
      childId = spawnMissionChild(this.research, projectId, { mission, nodeId: node.id, attempt, spawn, evaluationContract, baseCommit, budget: { maximumModelTokens: allocation.maximumModelTokens, maximumWallClockSeconds: allocation.maximumWallClockSeconds } });
    }
    mission = this.research.mission(projectId, mission.entityId);
    const current = mission.value.nodes.find((item) => item.id === node.id);
    if (current?.state === "ready") { mission = this.research.transitionMissionNode(projectId, mission.entityId, mission.version, node.id, "leased", `${key}:leased`, { leaseId: `lease_${childId.slice(childId.indexOf("_") + 1)}`, ownerId: childId, version: mission.version, expiresAt: new Date(Date.now() + mission.value.budgets.maximumWallClockSeconds * 1_000).toISOString() }); }
    if (mission.value.nodes.find((item) => item.id === node.id)?.state === "leased") { this.research.transitionMissionNode(projectId, mission.entityId, mission.version, node.id, "working", `${key}:working`); this.emit(event(projectId, mission.entityId, "mission.child_spawned", { nodeId: node.id, attempt, childId })); }
  }

  /** One line per research child already specified for this Mission, so the Director keeps them distinct. */
  private spawnSiblings(projectId: string, mission: Stored<MissionProjection>, nodeId: string): string[] {
    return mission.value.nodes.filter((item) => item.id !== nodeId && SPAWN_NODE_TYPES.includes(item.type)).flatMap((item) => { const child = this.research.spawnedChild(projectId, mission.entityId, item.id); if (!child) return []; const question = child.kind === "direction" ? this.research.direction(projectId, child.entityId).value.question : this.research.autoresearchExecution(projectId, child.entityId).value.decisionQuestion; return [`${item.id} (${child.kind} ${child.entityId}, ${child.state}): ${question}`]; });
  }

  /**
   * Maps each running research child onto its spawn node: a reviewed closure accepts the node; a stopped or failed
   * child fails it (respawned while attempts remain); a child blocked for 10 minutes with nothing running is stopped.
   */
  private async reconcileChildNodes(projectId: string, mission: Stored<MissionProjection>): Promise<Stored<MissionProjection>> {
    for (const node of mission.value.nodes.filter((item) => item.state === "working" && childOwned(item))) {
      const childId = node.lease!.ownerId; const child = childId.startsWith("dir_") ? this.research.direction(projectId, childId) : this.research.autoresearchExecution(projectId, childId);
      const done = childId.startsWith("dir_") ? child.state === "closed" : child.state === "completed";
      if (done) {
        for (const next of ["postflight", "reviewing", "accepted"] as const) mission = this.research.transitionMissionNode(projectId, mission.entityId, mission.version, node.id, next, `child-resolve:${childId}:${next}`);
        this.emit(event(projectId, mission.entityId, "mission.child_resolved", { nodeId: node.id, childId, outcome: "accepted" })); continue;
      }
      const stale = ["blocked", "paused"].includes(child.state) && Date.now() - Date.parse(child.value.updatedAt) > CHILD_BLOCKED_MS && !this.agents.inspect().some((agent) => agent.directionId === childId || agent.autoresearchId === childId);
      if (stale) { if (childId.startsWith("dir_")) this.research.transitionDirection(projectId, childId, child.version, "stopped", `child-stop:${childId}:${child.version}`); else this.research.transitionAutoresearch(projectId, childId, child.version, "stopped", `child-stop:${childId}:${child.version}`); this.emit(event(projectId, mission.entityId, "mission.child_stopped", { nodeId: node.id, childId, priorState: child.state })); }
      const ended = stale || (childId.startsWith("dir_") ? ["rejected", "stopped"] : ["failed", "stopped"]).includes(child.state);
      if (ended) { this.emit(event(projectId, mission.entityId, "mission.child_resolved", { nodeId: node.id, childId, outcome: "failed", childState: stale ? "stopped" : child.state })); await this.retryOrBlock(projectId, mission.entityId, node.id, `child_${childId.slice(childId.indexOf("_") + 1)}`, { diffHash: null, evidence: { childId, state: child.state } }); mission = this.research.mission(projectId, mission.entityId); }
    }
    return mission;
  }

  private reconcileOrphanedNodes(projectId: string, mission: Stored<MissionProjection>): Stored<MissionProjection> { for (const stale of mission.value.nodes.filter((node) => ["leased", "working", "postflight", "reviewing"].includes(node.state) && !childOwned(node))) { if (stale.state === "leased") mission = this.research.transitionMissionNode(projectId, mission.entityId, mission.version, stale.id, "ready", `recovery-release-${stale.lease?.leaseId ?? createId("cmd")}`); else { mission = this.research.transitionMissionNode(projectId, mission.entityId, mission.version, stale.id, "failed", `recovery-failed-${stale.lease?.leaseId ?? createId("cmd")}`); const current = mission.value.nodes.find((node) => node.id === stale.id)!; if (current.attempt < current.maximumAttempts) mission = this.research.transitionMissionNode(projectId, mission.entityId, mission.version, stale.id, "ready", `recovery-ready-${stale.lease?.leaseId ?? createId("cmd")}`); else { this.emit(event(projectId, mission.entityId, "mission.node_recovered", { nodeId: stale.id, priorState: stale.state, disposition: "attempt_budget_exhausted" })); return this.research.transitionMission(projectId, mission.entityId, mission.version, "blocked", `recovery-blocked-${stale.lease?.leaseId ?? createId("cmd")}`); } } this.emit(event(projectId, mission.entityId, "mission.node_recovered", { nodeId: stale.id, priorState: stale.state, disposition: "ready_for_new_lease" })); } return mission; }

  private async runNode(project: RegisteredProject, mission: Stored<MissionProjection>, nodeId: string): Promise<void> {
    let node = mission.value.nodes.find((item) => item.id === nodeId); if (!node || node.state !== "ready") return; this.scheduler.recordAllocation(nodeId);
    const role = roleFor(node); const agentId = createId("agt"); const taskId = createId("tsk"); const expiresAt = new Date(Date.now() + 30 * 60_000).toISOString();
    mission = this.research.transitionMissionNode(project.projectId, mission.entityId, mission.version, nodeId, "leased", `lease-${taskId}`, { leaseId: taskId, ownerId: agentId, version: mission.version, expiresAt });
    mission = this.research.transitionMissionNode(project.projectId, mission.entityId, mission.version, nodeId, "working", `working-${taskId}`); node = mission.value.nodes.find((item) => item.id === nodeId)!;
    const schema = role === "reviewer" ? "review-verdict" : role === "librarian_researcher" ? "librarian-completion" : "general-worker-completion"; let packet: TaskPacketRecord; let directReview: ReviewRequestRecord | null = null; let outcomes: { artifactIds: string[]; evidenceIds: string[]; digest: string } = { artifactIds: [], evidenceIds: [], digest: "" };
    try { packet = this.issueTaskPacket(project, mission, node, agentId, taskId, role, schema, expiresAt); if (role === "reviewer") { const producer = [...this.research.records(project.projectId, "mission-director-cycle")].reverse().find((record) => (record as { missionId?: string }).missionId === mission.entityId) as { directorAgentId?: string } | undefined; if (!producer?.directorAgentId) throw new Error("Review requires a durable producer identity"); outcomes = missionOutcomes(this.research, project.projectId, mission.entityId); directReview = this.issueReviewRequest(project, mission, taskId, agentId, [producer.directorAgentId], "task", "graph_node", node.id, mission.version, node.id, node.title, [{ kind: "contract_task.packet", id: taskId, hash: sha256(packet as unknown as JsonValue) }], outcomes.artifactIds, outcomes.evidenceIds); } await this.runSession(project, { agentId, taskId, missionId: mission.entityId, role, taskPermissions: packet.permissions, taskWorkspace: packet.workspace }, fitDigest(`Daemon-issued Task Packet: ${JSON.stringify(packet)}. Complete only graph node ${node.id}: ${node.title}. ${directReview ? `${outcomes.digest ? `This is an independent review of the Mission's accepted work against its success criteria. Accepted work (daemon-authored; required Artifacts/Evidence hold these records): ${DIGEST_SLOT} ` : ""}Review Request: ${JSON.stringify(directReview)}. ` : ""}${role === "general_worker" ? `You work in an isolated worktree on branch ${packet.workspace.branch}. ${completionInstructions(generalWorkerCompletionTemplate(packet, null))} ` : role === "reviewer" && directReview ? `${reviewerInstructions(directReview)} Your worktree is the Mission integration branch at ${packet.workspace.startingCommit}, holding every accepted change: read the files there (nosh_workspace_read) to verify what accepted work claims. Artifacts captured before a change was accepted can predate it; judge the current branch, citing its diff and run-result Artifacts. ` : role === "librarian_researcher" ? `${librarianInstructions(taskId, packet.permissions.network === "allowlisted")} ` : `Submit exactly one ${schemaUri(schema)} in the host terminal JSON envelope. `}${role === "reviewer" ? `Set reviewerAgentId=${agentId} and target.targetId=${node.id}.` : "Prose cannot change node state; deterministic postflight checks the packet, Git state, validators, and Artifact references before Review."}`, outcomes.digest)); } catch (error) { this.emit(event(project.projectId, mission.entityId, "task.execution_failed", { taskId, nodeId, message: error instanceof Error ? error.message : "Task execution failed" })); await this.retryOrBlock(project.projectId, mission.entityId, node.id, taskId, { transient: true }); return; }
    if (role === "reviewer") { mission = this.research.transitionMissionNode(project.projectId, mission.entityId, mission.version, node.id, "postflight", `postflight-${taskId}`); mission = this.research.transitionMissionNode(project.projectId, mission.entityId, mission.version, node.id, "reviewing", `reviewing-${taskId}`); await this.applyReview(project.projectId, mission, node.id, taskId, agentId, directReview!); return; }
    const completion = this.research.terminalRecord(project.projectId, taskId, schema); const postflight = completion ? [...this.postflight(project, packet, schema, completion), ...completionRunIssues(taskCommandRuns(this.research)(project.projectId), taskId, completion, [], writeUpOnly(project.repositoryRoot))] : ["missing completion record"];
    if (postflight.length) { this.emit(event(project.projectId, mission.entityId, "task.postflight_failed", { taskId, nodeId, issues: postflight })); await this.retryOrBlock(project.projectId, mission.entityId, node.id, taskId, completion ? { diffHash: changedCommit(completion), evidence: [...postflight].sort() } : { transient: true }); return; }
    mission = this.research.transitionMissionNode(project.projectId, mission.entityId, mission.version, node.id, "postflight", `postflight-${taskId}`); mission = this.research.transitionMissionNode(project.projectId, mission.entityId, mission.version, node.id, "reviewing", `reviewing-${taskId}`);
    const reviewerId = createId("agt"); const reviewTask = createId("tsk"); const required = completionRefs(completion!); const reviewRequest = this.issueReviewRequest(project, mission, reviewTask, reviewerId, [agentId], "task", "graph_node", node.id, mission.version, node.id, node.title, [{ kind: "contract_task.packet", id: taskId, hash: sha256(packet as unknown as JsonValue) }], required.artifactIds, required.evidenceIds); await this.runSession(project, { agentId: reviewerId, taskId: reviewTask, missionId: mission.entityId, role: "reviewer" }, `Independently review node ${node.id} (${node.title}) at Mission projection version ${mission.version}. ${role === "librarian_researcher" ? "The worker is a librarian: judge the sources and citations it was asked to collect (are they real, relevant, and cited without overclaiming?). It cannot run commands, so do not require run outputs or implementation results from it; those belong to the implementation node. " : ""}Worker ${agentId} submitted: ${JSON.stringify(completion)}. ${daemonTaskFacts(this.research, project.projectId, taskId)} Review Request: ${JSON.stringify(reviewRequest)}. ${reviewerInstructions(reviewRequest)}`).catch((error: unknown) => {
      // A reviewer that never produced a valid verdict is one failed review attempt (retry or block below), not a Mission-wide supervisor error.
      if (!(error instanceof Error && error.message.startsWith("Terminal output rejected"))) throw error;
      this.emit(event(project.projectId, mission.entityId, "task.review_failed", { nodeId: node.id, taskId: reviewTask, message: error.message.slice(0, 2_000) }));
    }); await this.applyReview(project.projectId, mission, node.id, reviewTask, reviewerId, reviewRequest);
  }

  private async applyReview(projectId: string, mission: Stored<MissionProjection>, nodeId: string, taskId: string, reviewerId: string, request: ReviewRequestRecord): Promise<void> {
    const review = this.research.terminalRecord(projectId, taskId, "review-verdict") as ReviewVerdictRecord | undefined;
    if (review && reviewPassesRequest(review, request, reviewerId)) { this.research.transitionMissionNode(projectId, mission.entityId, mission.version, nodeId, "accepted", `accepted-${taskId}`); return; }
    if (review?.verdict === "BLOCKED" && review.reviewId === request.reviewId && review.reviewRequestId === request.reviewRequestId) { this.research.transitionMissionNode(projectId, mission.entityId, mission.version, nodeId, "blocked", `review-blocked-${taskId}`); return; }
    await this.retryOrBlock(projectId, mission.entityId, nodeId, taskId, review ? { diffHash: null, evidence: { verdict: review.verdict, review: review as unknown as JsonValue } } : { transient: true });
  }

  private async retryOrBlock(projectId: string, missionId: string, nodeId: string, taskId: string, failure: FailureFingerprint): Promise<void> {
    let mission = this.research.mission(projectId, missionId); let node = mission.value.nodes.find((item) => item.id === nodeId)!; if (["working", "reviewing"].includes(node.state)) mission = this.research.transitionMissionNode(projectId, missionId, mission.version, nodeId, "failed", `failed-${taskId}`); node = mission.value.nodes.find((item) => item.id === nodeId)!;
    if (node.attempt >= node.maximumAttempts) { this.research.transitionMission(projectId, missionId, mission.version, "blocked", `attempts-exhausted-${taskId}`); return; }
    const attempt: AttemptInput = { projectId, missionId, directionId: null, nodeId, parentExperimentId: null, taskType: node.type, hypothesisFamilyTags: [], commandClass: node.type, inScopeFiles: [], inputArtifactHashes: "evidence" in failure ? [sha256(failure.evidence)] : [], diffHash: "diffHash" in failure ? failure.diffHash : null, evaluationContractHash: null, intendedDecision: node.title, expectedOutputType: "reviewed_node" }; const overrideReason = "transient" in failure ? "transient_retry" as const : null; const governor = new FocusGovernor(); for (const prior of this.research.scopedEvents(projectId, missionId, "focus.attempt_recorded")) { const payload = prior.payload as unknown as { input?: AttemptInput; delta?: {}; overrideReason?: "replication" | "transient_retry" | "controlled_seed" | null }; if (payload.input) governor.record(payload.input, payload.delta ?? {}, payload.overrideReason ?? null); } const focus = governor.record(attempt, {}, overrideReason); this.emit(event(projectId, missionId, "focus.attempt_recorded", { input: attempt as unknown as JsonValue, delta: {}, overrideReason, fingerprint: focus.fingerprint, progress: focus.progress }));
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
      gitHead: this.integrationHead(project, mission.entityId),
      integrationBranch: integrationBranch(mission.entityId),
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
    await this.runSession(project, { agentId: reviewerId, taskId, missionId: mission.entityId, role: "reviewer" }, `Perform the independent final Mission review for ${mission.entityId}, projection version ${mission.version}. Audit accepted task completions, their exact independent Reviews, stored deliverable artifacts (read them with nosh_artifact_read when available), evidence, validators, claims, and defects. Accepted work (daemon-authored digest): ${missionOutcomes(this.research, project.projectId, mission.entityId, 9_000).digest}. Completion packet: ${JSON.stringify({ deliverableArtifactIds: completion.deliverableArtifactIds, openDefectIds: completion.openDefectIds, deterministicPreReview: completion.deterministicPreReview, unresolvedClaimIds: basis.unresolvedClaimIds })}. Review Request: ${JSON.stringify(request)}. ${reviewerInstructions(request)}`);
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
    const branch = git(project.repositoryRoot, ["branch", "--show-current"]);
    if (!branch || git(project.repositoryRoot, ["status", "--porcelain"])) throw new Error("Task preflight requires a clean checked-out Git branch");
    // Every task builds on the Mission's accepted work, not on the user's checkout alone.
    const head = this.integrationHead(project, mission.entityId);
    const tree = createTaskWorktree(this.research, project, taskId, head);
    const use = this.research.missionBudgetUse(project.projectId, mission.entityId);
    const remainingWall = Math.max(1, mission.value.budgets.maximumWallClockSeconds - use.wallClockSeconds);
    const remainingTokens = Math.max(1, mission.value.budgets.maximumModelTokens - use.modelTokens);
    const outputKind = role === "general_worker" ? "output_code.commit" : role === "librarian_researcher" ? "output_research.report" : "output_review.verdict";
    const acceptanceCriteria = node.criterionIds.length ? node.criterionIds.map((criterionId) => {
      const index = Number(criterionId.slice("criterion_".length)) - 1;
      return { criterionId, statement: mission.value.successCriteria[index]!, validatorIds: ["validator_task.postflight"], reviewRubricIds: ["rubric_scientific.correctness"], required: true };
    }) : [{ criterionId: node.id, statement: node.title, validatorIds: ["validator_task.postflight"], reviewRubricIds: ["rubric_scientific.correctness"], required: true }];
    const libraryNetwork = role === "librarian_researcher" && librarianNetworkAllowed(project);
    const commands = role === "general_worker" ? declaredTaskCommands(project) : [];
    const permissions: TaskPermissions = {
      network: libraryNetwork ? "allowlisted" : "disabled",
      subprocess: commands.length ? "allowlisted" : "disabled",
      gitCommit: role === "general_worker",
      gitPush: false,
      delegation: "request_only",
      networkAllowlist: libraryNetwork ? [...librarianResearchHosts] : [],
      allowedToolIds: [
        role === "reviewer" ? "tool_nosh.review.submit" : "tool_nosh.response.submit",
        "tool_nosh.blocker.submit",
        ...(role === "reviewer" ? [] : ["tool_nosh.delegation.request"]),
        "tool_pi.read",
        ...(role === "reviewer" ? [] : ["tool_pi.edit", "tool_pi.write"]),
        ...(role === "general_worker" ? ["tool_nosh.git.commit"] : []),
        ...(libraryNetwork ? ["tool_nosh.network.read"] : []),
        ...(commands.length ? ["tool_nosh.run"] : []),
      ],
    };
    // Research children this node depends on: their Evidence and outcomes reach the worker, not only the final gate.
    const research = this.research.researchInputs(project.projectId, mission.entityId, node.hardDependencies);
    // Answers and code diffs exceed a packet instruction's 1,000 characters, so they travel as an Artifact the worker reads.
    const researchArtifact = research.summaries.length ? this.completionArtifact(project, mission, "research-inputs", { nodeId: node.id, children: research.summaries } as unknown as JsonValue) : null;
    const packet: TaskPacketRecord = {
      $schema: schemaUri("task-packet"), schemaVersion: 1, templateVersion: "1.0.0",
      taskId, attempt: node.attempt, taskType: `task_${node.type.replaceAll("_", ".")}`, assignedRole: role, assignedAgentId: agentId,
      scope: { projectId: project.projectId, missionId: mission.entityId, directionId: null, autoresearchId: null, experimentId: null, graphNodeId: node.id },
      goalStack: { projectGoalId: project.projectId, projectGoalSummary: mission.value.objective, missionCriterionIds: node.criterionIds, missionCriterionSummary: acceptanceCriteria.map((criterion) => criterion.statement).join("; "), directionQuestionId: null, directionQuestionSummary: null, nodeObjective: node.title },
      whyNow: "The deterministic Mission frontier selected this ready node.",
      instructions: [`Complete only ${node.title}`, "Do not modify protected NOSH contracts", ...(researchArtifact ? [`Build on this Mission's finished research: read Artifact ${researchArtifact} with nosh_artifact_read (each research child's question, answer, and promoted experiments with their measured results and code changes; port a promoted change rather than re-deriving it, and cite its results). Their Evidence is in inputEvidenceIds.`] : []), ...(mission.value.startingEvidence.length ? [`Read the starting Evidence (${mission.value.startingEvidence.join(", ")}) with nosh_artifact_read; each record cites the Artifacts that hold its measurements`] : []), ...(commands.length ? taskCommandInstructions(commands, true) : ["You cannot run commands: report only what you verified by reading, and claim a criterion satisfied only as far as that evidence supports"])],
      inScope: ["**"], outOfScope: [".nosh/contracts/**"], inputArtifactIds: researchArtifact ? [researchArtifact] : [], inputEvidenceIds: [...new Set([...mission.value.startingEvidence, ...research.evidenceIds])], // the researcher's starting Evidence and this node's research children's Evidence (readable with nosh_artifact_read)
      requiredOutputs: [{ outputId: `out_${taskId.slice(4)}`, kind: outputKind, required: true }],
      acceptanceCriteria,
      workspace: { worktreeId: tree.worktreeId, branch: tree.branch, startingCommit: head, writeScopes: ["**"], protectedScopes: [".nosh/contracts/**"] },
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
  /**
   * Accepted worker commits are merged onto the Mission's own branch (never the user's), so later tasks build on accepted
   * work and the Mission ends as one reviewable branch for the user to merge. Idempotent and crash-safe: Git ancestry is
   * the record of what is integrated. A conflict blocks the Mission for attention instead of guessing a resolution.
   */
  private integrationHead(project: RegisteredProject, missionId: string): string {
    const base = (this.research.records(project.projectId, "task-packet") as unknown as TaskPacketRecord[]).find((packet) => packet.scope.missionId === missionId)?.workspace.startingCommit;
    if (!base) return git(project.repositoryRoot, ["rev-parse", "HEAD"]);
    const branch = integrationBranch(missionId);
    const path = new GitWorkspaceManager(project.repositoryRoot, join(project.repositoryRoot, ".nosh", "worktrees"), join(project.repositoryRoot, ".nosh", "git")).create(branch, base, `wt_mission_${missionId.slice(4)}`);
    const run = (args: string[]) => spawnSync("git", ["-C", path, ...args], { encoding: "utf8", windowsHide: true, env: { ...worktreeGitEnvironment(path), GIT_AUTHOR_NAME: "NOSH", GIT_AUTHOR_EMAIL: "noshd@nosh.invalid", GIT_COMMITTER_NAME: "NOSH", GIT_COMMITTER_EMAIL: "noshd@nosh.invalid" } });
    for (const accepted of this.research.missionCompletionBasis(project.projectId, missionId).completions) {
      const code = (accepted.completion as { codeChanges?: { startingCommit?: string; endingCommit?: string } }).codeChanges;
      if (!code?.endingCommit || code.endingCommit === code.startingCommit || run(["merge-base", "--is-ancestor", code.endingCommit, "HEAD"]).status === 0) continue;
      const merge = run(["merge", "--no-ff", "--no-edit", "-m", `NOSH: integrate accepted node ${accepted.nodeId} (${accepted.taskId})`, code.endingCommit]);
      if (merge.status !== 0) {
        run(["merge", "--abort"]);
        this.emit(event(project.projectId, missionId, "mission.integration_conflict", { nodeId: accepted.nodeId, taskId: accepted.taskId, commit: code.endingCommit, branch, detail: `${merge.stdout}${merge.stderr}`.trim().slice(0, 2_000) }));
        throw new Error(`Accepted node ${accepted.nodeId} conflicts with Mission branch ${branch}; resolve it in ${path} or retry the node`);
      }
    }
    return git(path, ["rev-parse", "HEAD"]);
  }
  private postflight(project: RegisteredProject, packet: TaskPacketRecord, schema: string, record: JsonValue): string[] { const parsed = validateRecord(schemaUri(schema), record); if (!parsed.ok) return parsed.errors.map((error) => `${error.pointer} ${error.message}`); const value = parsed.value as Record<string, unknown>; const issues: string[] = []; if (value.taskOutcome !== "completed" || value.readyForReview !== true) issues.push("completion is not ready for Review"); if (schema === "general-worker-completion") { if (value.readyForDeterministicPostflight !== true) issues.push("worker did not request deterministic postflight"); const code = value.codeChanges as { startingCommit: string; endingCommit: string; branch: string; changedPaths: string[]; diffArtifactId: string | null }; issues.push(...validateGitCompletion(taskWorktreePath(project.repositoryRoot, packet.workspace.worktreeId), packet.workspace, code)); const commands = value.commands as Array<{ exitCode: number | null }>; if (commands.some((command) => command.exitCode !== 0)) issues.push("a declared command did not pass"); const criteria = value.criteria as Array<{ criterionId: string; workerClaim: string; validatorRunIds: string[]; artifactIds: string[] }>; for (const criterion of packet.acceptanceCriteria.filter((entry) => entry.required)) { const result = criteria.find((entry) => entry.criterionId === criterion.criterionId); // deferred_to_review: only the independent task Review can establish it, and completion counts it only on that Review's criterion PASS.
      if (!result || !(result.workerClaim === "deferred_to_review" || (result.workerClaim === "satisfied" && result.validatorRunIds.length))) issues.push(`criterion ${criterion.criterionId} lacks a passing validator`); } const ids = [code.diffArtifactId, ...((value.workPerformed as Array<{ artifactIds: string[] }>).flatMap((work) => work.artifactIds)), ...criteria.flatMap((criterion) => criterion.artifactIds)].filter((id): id is string => Boolean(id)); const artifacts = new ArtifactStore(join(project.repositoryRoot, ".nosh", "artifacts")); for (const id of ids) try { artifacts.resolve(id); } catch { issues.push(`artifact ${id} does not resolve`); } } else { const librarian = value as unknown as { bibliographyArtifactId: string; reportArtifactId: string; sources: Array<{ artifactId: string }>; findings: Array<{ support: Array<{ artifactId: string }> }> }; const artifacts = new ArtifactStore(join(project.repositoryRoot, ".nosh", "artifacts")); for (const id of [librarian.bibliographyArtifactId, librarian.reportArtifactId, ...librarian.sources.map((source) => source.artifactId), ...librarian.findings.flatMap((finding) => finding.support.map((support) => support.artifactId))]) try { artifacts.resolve(id); } catch { issues.push(`artifact ${id} does not resolve`); } } return [...new Set(issues)]; }

  private async runSession(project: RegisteredProject, input: { agentId: string; taskId: string; missionId: string; role: PiSessionOptions["role"]; taskPermissions?: PiSessionOptions["taskPermissions"]; taskWorkspace?: PiSessionOptions["taskWorkspace"] }, prompt: string): Promise<void> {
    if (!this.runtime) {
      const options: PiSessionOptions = { projectId: project.projectId, missionId: input.missionId, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, taskId: input.taskId, agentId: input.agentId, role: input.role, cwd: input.taskWorkspace ? taskWorktreePath(project.repositoryRoot, input.taskWorkspace.worktreeId) : project.repositoryRoot, packagePath: this.packagePath, ...(input.taskPermissions ? { taskPermissions: input.taskPermissions } : {}), ...(input.taskWorkspace ? { taskWorkspace: input.taskWorkspace } : {}) };
      await this.agents.start(options);
      try { await this.agents.prompt(input.agentId, prompt); } finally { this.agents.stop(input.agentId); }
      return;
    }
    const threadId = createId("thr"); const issuedAt = new Date().toISOString(); const key = `runtime:${input.taskId}:${threadId}`;
    await this.runtime.execute({ $schema: schemaUri("runtime-instruction"), schemaVersion: 1, instructionId: createId("ins"), projectId: project.projectId, idempotencyKey: `${key}:open`, proposedByAgentId: null, issuedAt, operation: "THREAD_OPEN", threadId, taskId: input.taskId, initialAgentId: input.agentId, ownerScope: { missionId: input.missionId, directionId: null, autoresearchId: null, experimentId: null, graphNodeId: null }, role: input.role, purpose: `Execute Mission task ${input.taskId}`, executionMode: "background", parentThreadId: null, inputRefs: [], skillIds: [], capabilities: [], taskPermissions: input.taskPermissions ?? null, taskWorkspace: input.taskWorkspace ?? null, budget: { maximumToolCalls: 200, maximumModelTokens: 100_000, maximumWallClockSeconds: 7_200 } });
    try {
      await this.runtime.execute({ $schema: schemaUri("runtime-instruction"), schemaVersion: 1, instructionId: createId("ins"), projectId: project.projectId, idempotencyKey: `${key}:step`, proposedByAgentId: null, issuedAt, operation: "THREAD_STEP", threadId, objective: prompt, expectedEpisodeType: episodeTypeForRole(input.role), inputRefs: [], skillIds: [] });
    } finally {
      await this.runtime.execute({ $schema: schemaUri("runtime-instruction"), schemaVersion: 1, instructionId: createId("ins"), projectId: project.projectId, idempotencyKey: `${key}:stop`, proposedByAgentId: null, issuedAt: new Date().toISOString(), operation: "STOP", threadId, programId: null, reason: "Task session reached its boundary" });
    }
  }
}

type TaskPacketRecord = { $schema: string; schemaVersion: number; templateVersion: string; taskId: string; attempt: number; taskType: string; assignedRole: "librarian_researcher" | "general_worker" | "reviewer"; assignedAgentId: string; scope: { projectId: string; missionId: string; directionId: null; autoresearchId: null; experimentId: null; graphNodeId: string }; goalStack: { projectGoalId: string; projectGoalSummary: string; missionCriterionIds: string[]; missionCriterionSummary: string; directionQuestionId: null; directionQuestionSummary: null; nodeObjective: string }; whyNow: string; instructions: string[]; inScope: string[]; outOfScope: string[]; inputArtifactIds: string[]; inputEvidenceIds: string[]; requiredOutputs: Array<{ outputId: string; kind: string; required: boolean }>; acceptanceCriteria: Array<{ criterionId: string; statement: string; validatorIds: string[]; reviewRubricIds: string[]; required: boolean }>; workspace: { worktreeId: string; branch: string; startingCommit: string; writeScopes: string[]; protectedScopes: string[] }; permissions: TaskPermissions; budget: { deadline: string; maximumWallClockSeconds: number; maximumModelTokens: number; maximumToolCalls: number; maximumRepairAttempts: number }; progressPolicy: { milestoneIntervalSeconds: number; emitOnFirstDurableDelta: boolean; emitOnBlocker: boolean; emitOnAnomaly: boolean; maximumSilentSeconds: number }; responseSchema: string; observedVersions: Record<string, number>; lease: { leaseId: string; expiresAt: string; heartbeatSeconds: number }; issuedBy: "nosh"; issuedAt: string };
type ReviewRequestRecord = { $schema: string; schemaVersion: number; reviewRequestId: string; reviewId: string; reviewType: "task" | "experiment" | "experiment_round" | "autoresearch_closure" | "direction_closure" | "claim" | "paper_section" | "mission_completion"; target: { targetType: string; targetId: string; targetVersion: number }; scope: { projectId: string; missionId: string; directionId: null; autoresearchId: null }; producerAgentIds: string[]; reviewerAgentId: string; independenceCheck: "pass"; contractRefs: Array<{ kind: string; id: string; hash: string }>; requiredArtifactIds: string[]; requiredEvidenceIds: string[]; deterministicPostflight: { status: "pass"; validatorRunIds: string[] }; criteria: Array<{ criterionId: string; statement: string; required: true; severityIfFailed: "blocking" }>; allowedVerdicts: Array<"PASS" | "REVISE" | "REDESIGN" | "BLOCKED">; issuedAt: string };
type ReviewVerdictRecord = { $schema?: string; reviewId?: string; reviewRequestId?: string; reviewType?: string; target?: { targetType?: string; targetId?: string; targetVersion?: number }; reviewerAgentId?: string; independenceCheck?: string; verdict?: string; criteria?: Array<{ criterionId?: string; status?: string }>; reviewedArtifactIds?: string[]; reviewedEvidenceIds?: string[] };
function completionRefs(record: JsonValue): { artifactIds: string[]; evidenceIds: string[] } { const value = record as unknown as { workPerformed?: Array<{ artifactIds?: string[] }>; codeChanges?: { diffArtifactId?: string | null }; criteria?: Array<{ artifactIds?: string[] }>; scientificImpact?: { evidenceIds?: string[] }; bibliographyArtifactId?: string; reportArtifactId?: string; sources?: Array<{ artifactId?: string }>; findings?: Array<{ support?: Array<{ artifactId?: string }> }> }; const artifactIds = [value.codeChanges?.diffArtifactId, value.bibliographyArtifactId, value.reportArtifactId, ...(value.workPerformed ?? []).flatMap((entry) => entry.artifactIds ?? []), ...(value.criteria ?? []).flatMap((entry) => entry.artifactIds ?? []), ...(value.sources ?? []).map((entry) => entry.artifactId), ...(value.findings ?? []).flatMap((entry) => (entry.support ?? []).map((support) => support.artifactId))].filter((id): id is string => Boolean(id)); return { artifactIds: [...new Set(artifactIds)], evidenceIds: [...new Set(value.scientificImpact?.evidenceIds ?? [])] }; }
function reviewPassesRequest(review: ReviewVerdictRecord, request: ReviewRequestRecord, reviewerId: string): boolean { if (!validateRecord(schemaUri("review-verdict"), review as JsonValue).ok || review.reviewId !== request.reviewId || review.reviewRequestId !== request.reviewRequestId || review.reviewType !== request.reviewType || review.reviewerAgentId !== reviewerId || review.independenceCheck !== "pass" || review.verdict !== "PASS" || review.target?.targetType !== request.target.targetType || review.target.targetId !== request.target.targetId || review.target.targetVersion !== request.target.targetVersion) return false; if (!request.criteria.every((required) => review.criteria?.some((criterion) => criterion.criterionId === required.criterionId && criterion.status === "PASS"))) return false; return request.requiredArtifactIds.every((id) => review.reviewedArtifactIds?.includes(id)) && request.requiredEvidenceIds.every((id) => review.reviewedEvidenceIds?.includes(id)); }
function git(cwd: string, args: string[]): string { const result = spawnSync("git", ["-C", cwd, ...args], { encoding: "utf8", windowsHide: true, env: worktreeGitEnvironment(cwd) }); if (result.status !== 0) throw new Error(result.stderr.trim() || `git ${args[0]} failed`); return result.stdout.trim(); }

/** The Mission's own integration branch; the user merges it when the Mission is accepted. */
function integrationBranch(missionId: string): string { return `nosh/mission-${missionId.slice(4, 16)}`; }
/** A runtime step objective holds at most this many characters (wire `text`); the accepted-work digest is the part that grows. */
const OBJECTIVE_LIMIT = 20_000;
export const DIGEST_SLOT = " DIGEST ";
/** Puts as much of the accepted-work digest into the prompt as fits the step objective limit (the digest is already a bounded excerpt). */
export function fitDigest(prompt: string, digest: string): string {
  if (!prompt.includes(DIGEST_SLOT)) return prompt;
  const room = OBJECTIVE_LIMIT - 200 - (prompt.length - DIGEST_SLOT.length);
  return prompt.replace(DIGEST_SLOT, digest.length <= room ? digest : `${digest.slice(0, Math.max(0, room - 60))}… [digest truncated to fit; read the required Artifacts]`);
}
function roleFor(node: GraphNode): "librarian_researcher" | "general_worker" | "reviewer" { if (node.type === "literature_review") return "librarian_researcher"; if (["claim_review", "final_review", "approval"].includes(node.type)) return "reviewer"; return "general_worker"; }
function event(projectId: string, missionId: string, type: string, payload: JsonValue): EventDraft { return { $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type, source: "mission_supervisor", scope: { projectId, missionId, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: null }, correlationId: missionId, causationId: null, payload }; }
