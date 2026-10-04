import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { createId } from "@nosh/core";
import { ArtifactStore } from "@nosh/evidence";
import { EventStore, type RegisteredProject } from "@nosh/persistence";
import type { AgentInspection, PiAdapter, PiSessionOptions } from "@nosh/pi-adapter";
import { isTaskTerminalRecord, schemaUri, sha256, type EventDraft, type JsonValue } from "@nosh/wire";
import { describe, expect, it, vi } from "vitest";
import { DirectionSupervisor } from "./direction-supervisor.js";
import { MissionSupervisor } from "./mission-supervisor.js";
import { ResearchControl } from "./research-control.js";

class FixtureAgents {
  private readonly sessions = new Map<string, PiSessionOptions>();
  constructor(private readonly append: (draft: EventDraft) => void, private readonly failGeneralWorkers = false, private readonly deferLastCriterion = false) {}
  inspect(): AgentInspection[] { return [...this.sessions.values()].map((session) => ({ ...session, piSessionId: `pi_${session.agentId}`, status: "running", currentTool: null, activeToolIds: [], startedAt: new Date().toISOString(), lastEventAt: new Date().toISOString(), modelProvider: session.model?.provider ?? null, modelId: session.model?.id ?? null, modelName: null, thinkingLevel: session.thinkingLevel ?? "medium", contextTokens: null, contextWindow: null, contextPercent: null })); }
  async start(options: PiSessionOptions): Promise<AgentInspection> { this.sessions.set(options.agentId, options); return this.inspect().find((agent) => agent.agentId === options.agentId)!; }
  async prompt(agentId: string, prompt: string): Promise<void> {
    const session = this.sessions.get(agentId)!; const taskId = session.taskId; if (!taskId) throw new Error("Fixture session requires a Task ID"); const target = /target\.targetId=([a-z0-9_]+)/.exec(prompt)?.[1] ?? ""; const packetText = prompt.split("Task Packet: ")[1]?.split(". Complete only")[0]; const packet = packetText ? JSON.parse(packetText) as { attempt: number; workspace: { startingCommit: string; branch: string }; lease: { leaseId: string }; requiredOutputs: Array<{ outputId: string }>; acceptanceCriteria: Array<{ criterionId: string }> } : null; const reviewRequestText = prompt.split("Review Request: ")[1]?.split(". Submit")[0]; const reviewRequest = reviewRequestText ? JSON.parse(reviewRequestText) as { reviewId: string; reviewRequestId: string; reviewType: string; target: { targetType: string; targetId: string; targetVersion: number }; criteria: Array<{ criterionId: string; required: boolean }>; requiredArtifactIds: string[]; requiredEvidenceIds: string[] } : null; const now = new Date().toISOString();
    let record: JsonValue;
    if (session.role === "mission_director") record = { $schema: schemaUri("mission-director-cycle"), cycleId: /"cycleId":"(cycle_[0-9a-f]+)"/.exec(prompt)?.[1], directorAgentId: agentId, missionId: session.missionId, observedGraphVersion: Number(/graph version (\d+)/.exec(prompt)?.[1]), northStarCheck: { currentWorkContributes: true } };
    else if (session.role === "research_director") record = { $schema: schemaUri("research-director-cycle"), cycleId: /"cycleId":"(cycle_[0-9a-f]+)"/.exec(prompt)?.[1], directorAgentId: agentId, directionId: session.directionId, observedGraphVersion: Number(/graph version (\d+)/.exec(prompt)?.[1]), evaluationContractHash: /sha256:[0-9a-f]{64}/.exec(prompt)?.[0] ?? "", questionCheck: { currentWorkContributes: true } };
    else if (session.role === "reviewer") { const reviewId = reviewRequest?.reviewId ?? createId("rev"); record = { $schema: schemaUri("review-verdict"), schemaVersion: 1, templateVersion: "1.0.0", reviewId, reviewRequestId: reviewRequest?.reviewRequestId ?? `request_${reviewId.slice(4)}`, reviewType: reviewRequest?.reviewType ?? (target.startsWith("mis_") ? "mission_completion" : target.startsWith("dir_") ? "direction_closure" : "task"), target: reviewRequest?.target ?? { targetType: "graph_node", targetId: target, targetVersion: 1 }, reviewerAgentId: agentId, independenceCheck: "pass", verdict: "PASS", summary: "Fixture review passed", criteria: reviewRequest?.criteria.map((criterion) => ({ criterionId: criterion.criterionId, required: criterion.required, status: "PASS", finding: "All fixture inputs passed", evidenceRefs: [], confidence: "high", defectIds: [] })) ?? [{ criterionId: "criterion_fixture", status: "PASS", finding: "All fixture inputs passed", evidenceRefs: [], confidence: "high", defectIds: [] }], defects: [], missingRequiredInputs: [], scientificIntegrityFlags: [], recommendedGraphAction: "accept_node", recommendedPromotion: "not_applicable", reviewedArtifactIds: reviewRequest?.requiredArtifactIds ?? [], reviewedEvidenceIds: reviewRequest?.requiredEvidenceIds ?? [], submittedAt: now }; }
    else if (session.role === "librarian_researcher" && packet) { const artifacts = new ArtifactStore(join(projectRoot(session.cwd), ".nosh", "artifacts")); const sourcePath = join(session.cwd, `.fixture-source-${taskId}.txt`); const reportPath = join(session.cwd, `.fixture-report-${taskId}.md`); const bibliographyPath = join(session.cwd, `.fixture-bibliography-${taskId}.bib`); writeFileSync(sourcePath, "Primary fixture source\n"); writeFileSync(reportPath, "# Fixture report\n"); writeFileSync(bibliographyPath, "@misc{fixture}\n"); const sourceArtifact = artifacts.add({ artifactId: createId("art"), projectId: session.projectId, kind: "source", mediaType: "text/plain", sourcePath, retentionClass: "accepted_evidence" }); const reportArtifact = artifacts.add({ artifactId: createId("art"), projectId: session.projectId, kind: "report", mediaType: "text/markdown", sourcePath: reportPath, retentionClass: "accepted_evidence" }); const bibliographyArtifact = artifacts.add({ artifactId: createId("art"), projectId: session.projectId, kind: "bibliography", mediaType: "application/x-bibtex", sourcePath: bibliographyPath, retentionClass: "accepted_evidence" }); record = { $schema: schemaUri("librarian-completion"), schemaVersion: 1, taskOutcome: "completed", researchQuestion: "Fixture literature question", searchCoverage: { databases: ["fixture"], queries: ["fixture query"], dateRange: { from: null, to: null }, language: ["en"], inclusionCriteria: ["primary"], exclusionCriteria: ["none"] }, sources: [{ sourceId: `source_${taskId.slice(4)}`, sourceType: "primary_source", title: "Fixture source", authors: ["Fixture Author"], publicationDate: null, canonicalUrl: "https://example.com/fixture", persistentId: null, version: "1", primarySource: true, accessedAt: now, artifactId: sourceArtifact.artifactId, relevance: "Direct fixture support" }], findings: [{ findingId: `finding_${taskId.slice(4)}`, statement: "Fixture finding", support: [{ sourceId: `source_${taskId.slice(4)}`, locator: "line 1", artifactId: sourceArtifact.artifactId }], confidence: "high", noveltyImplication: "Fixture only" }], contradictions: [], evaluationDifferences: [], knowledgeGaps: [], candidateClaimEffects: [], bibliographyArtifactId: bibliographyArtifact.artifactId, reportArtifactId: reportArtifact.artifactId, readyForReview: true }; }
    else if (session.role === "general_worker" && packet) { let endingCommit = packet.workspace.startingCommit; const changedPaths: string[] = []; let artifactId: string | null = null; if (!this.failGeneralWorkers) { const name = `fixture-${taskId}.txt`; const outputPath = join(session.cwd, name); writeFileSync(outputPath, "Reviewed fixture output\n"); git(session.cwd, ["add", "--", name]); git(session.cwd, ["commit", "-m", `fixture ${taskId}`]); endingCommit = git(session.cwd, ["rev-parse", "HEAD"]); changedPaths.push(name); artifactId = new ArtifactStore(join(projectRoot(session.cwd), ".nosh", "artifacts")).add({ artifactId: createId("art"), projectId: session.projectId, kind: "fixture_output", mediaType: "text/plain", sourcePath: outputPath, retentionClass: "accepted_evidence" }).artifactId; } record = { $schema: schemaUri("general-worker-completion"), schemaVersion: 1, taskOutcome: this.failGeneralWorkers ? "failed" : "completed", workPerformed: [{ action: "action_implementation", subject: "Fixture output", artifactIds: artifactId ? [artifactId] : [] }], codeChanges: { startingCommit: packet.workspace.startingCommit, endingCommit, changedPaths, diffArtifactId: null, branch: packet.workspace.branch }, commands: [{ commandId: `validator_${taskId.slice(4)}`, displayCommand: "fixture validator", exitCode: this.failGeneralWorkers ? 1 : 0, resultArtifactId: null }], criteria: packet.acceptanceCriteria.map((criterion, index, all) => ({ criterionId: criterion.criterionId, ...(this.deferLastCriterion && index === all.length - 1 ? { workerClaim: "deferred_to_review", validatorRunIds: [] } : { workerClaim: this.failGeneralWorkers ? "unsatisfied" : "satisfied", validatorRunIds: [`validator_${taskId.slice(4)}`] }), artifactIds: artifactId ? [artifactId] : [], notes: "Fixture validator" })), scientificImpact: { claimIds: [], evidenceIds: [], interpretation: "Fixture impact" }, deviations: [], newRisks: [], unresolvedItems: [], suggestedNextActions: [], readyForDeterministicPostflight: !this.failGeneralWorkers, readyForReview: !this.failGeneralWorkers };
    } else if (session.role === "librarian_researcher") record = { $schema: schemaUri("librarian-completion"), taskOutcome: "completed", readyForReview: true }; else record = { $schema: schemaUri("general-worker-completion"), taskOutcome: "completed", readyForReview: true, readyForDeterministicPostflight: true, codeChanges: { endingCommit: git(session.cwd, ["rev-parse", "HEAD"]) } };
    this.submit(session, record);
    if (session.directionId && ["general_worker", "librarian_researcher"].includes(session.role)) this.submit(session, { $schema: schemaUri("evidence"), evidenceId: createId("evd"), statement: "Fixture result", sourceRefs: [{ refType: "task", refId: session.taskId, locator: "fixture" }] });
  }
  stop(agentId: string): void { this.sessions.delete(agentId); }
  private submit(session: PiSessionOptions, record: JsonValue): void { const scope = { projectId: session.projectId, missionId: session.missionId, directionId: session.directionId, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: session.agentId }; this.append({ $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "record.submitted", source: "fixture", scope, correlationId: `task:${session.taskId}`, causationId: null, payload: record }); }
}

describe("MissionSupervisor", { timeout: 90_000 }, () => { // Real git worktrees per task; slower under parallel load.
  it("runs typed Director, worker, independent Review, and final Review gates", async () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-supervisor-")); const repositoryRoot = join(directory, "repository"); mkdirSync(repositoryRoot); initializeRepository(repositoryRoot); const projectId = createId("prj"); const project: RegisteredProject = { projectId, repositoryRoot, databasePath: join(directory, "project.sqlite"), registeredAt: new Date().toISOString() }; const store = new EventStore(project.databasePath); const publish = () => undefined; const research = new ResearchControl(() => store, () => project, publish); const fixture = new FixtureAgents((draft) => { appendFixtureEvent(store, draft); }); const supervisor = new MissionSupervisor(research, fixture as unknown as PiAdapter, () => [project], join(directory, "pi-package"), (draft) => { appendFixtureEvent(store, draft); });
    try {
      let mission = research.createMission(projectId, { title: "Fixture", objective: "Produce a reviewed result", deliverables: ["Reviewed result"], successCriteria: ["Deliverable artifact is preserved", "Independent review accepts every contract criterion"], idempotencyKey: "fixture-create" }); for (const next of ["planning", "awaiting_approval", "running"] as const) mission = research.transitionMission(projectId, mission.entityId, mission.version, next, `fixture-${next}`);
      for (const deadline = Date.now() + 60_000; Date.now() < deadline && research.mission(projectId, mission.entityId).state !== "completed";) { await supervisor.tick(); await new Promise((resolve) => setTimeout(resolve, 20)); }
      const completed = research.mission(projectId, mission.entityId); const failures = research.scopedEvents(projectId, mission.entityId).filter((entry) => ["task.execution_failed", "task.postflight_failed", "mission.supervisor_error", "mission.completion_rejected", "focus.alarm"].includes(entry.type)); expect({ state: completed.state, failures }).toEqual({ state: "completed", failures: [] }); expect(completed.value.nodes.every((node) => node.state === "accepted")).toBe(true); expect(research.records(projectId, "mission-director-cycle").length).toBeGreaterThanOrEqual(3); expect(research.records(projectId, "task-packet").length).toBeGreaterThanOrEqual(3); expect(research.records(projectId, "task-acknowledgement")).toEqual([]); expect(research.records(projectId, "review-request").length).toBeGreaterThanOrEqual(4); expect(research.records(projectId, "review-verdict").length).toBeGreaterThanOrEqual(4); expect(research.submitted(projectId).some((entry) => entry.event.scope.missionId === mission.entityId && (entry.record as { $schema?: string }).$schema === schemaUri("mission-completion-packet"))).toBe(true); expect(store.operationIntents(projectId, "pending")).toEqual([]);
      const basis = research.missionCompletionBasis(projectId, mission.entityId);
      // Accepted worker commits are integrated on the Mission's own branch, and later tasks start from that work.
      const branch = `nosh/mission-${mission.entityId.slice(4, 16)}`;
      const integrated = git(repositoryRoot, ["ls-tree", "-r", "--name-only", branch]).split(/\r?\n/);
      const endings = basis.completions.map((entry) => (entry.completion as { codeChanges?: { endingCommit?: string; startingCommit?: string } }).codeChanges).filter((code) => code?.endingCommit && code.endingCommit !== code.startingCommit);
      expect(endings.length).toBeGreaterThan(0);
      for (const code of endings) expect(spawnSync("git", ["-C", repositoryRoot, "merge-base", "--is-ancestor", code!.endingCommit!, branch], { windowsHide: true }).status).toBe(0);
      expect(integrated.filter((name) => name.startsWith("fixture-tsk_")).length).toBe(endings.length);
      const packets = research.records(projectId, "task-packet") as Array<{ scope: { missionId?: string }; workspace: { startingCommit: string } }>;
      const firstEnding = endings[0]!.endingCommit!; const later = packets.filter((packet) => packet.scope.missionId === mission.entityId && spawnSync("git", ["-C", repositoryRoot, "merge-base", "--is-ancestor", firstEnding, packet.workspace.startingCommit], { windowsHide: true }).status === 0);
      expect(later.length).toBeGreaterThan(0);
      expect(git(repositoryRoot, ["branch", "--show-current"])).toBe("main");
      const completion = research.records(projectId, "mission-completion-packet").at(-1) as { criteria?: unknown[]; deliverableArtifactIds?: string[]; openDefectIds?: string[] };
      expect(basis.criteria).toHaveLength(2);
      expect(basis.criteria.every((criterion) => criterion.validatorRunIds.length > 0 && criterion.artifactIds.length > 0 && Boolean(criterion.reviewId))).toBe(true);
      expect(completion.criteria).toHaveLength(2);
      expect(completion.deliverableArtifactIds).toEqual(basis.artifactIds);
      expect(completion.openDefectIds).toEqual(basis.openDefectIds);
      const entries = research.submitted(projectId);
      const accepted = basis.completions[0]!;
      const workerPacket = entries.find((entry) => entry.record.$schema === schemaUri("task-packet") && entry.record.taskId === accepted.taskId)!.record;
      const request = entries.find((entry) => entry.record.$schema === schemaUri("review-request") && entry.record.reviewId === accepted.review.reviewId)!;
      expect(request.event.correlationId).not.toBe(`task:${accepted.taskId}`);
      expect(request.record.contractRefs).toContainEqual({ kind: "contract_task.packet", id: accepted.taskId, hash: sha256(workerPacket) });
      // Corrupted legacy records must not become valid merely because a Review says PASS.
      for (const corruption of ["packet_hash", "stale_attempt", "self_review", "missing_inputs", "duplicate_packet"] as const) {
        const corrupted = structuredClone(entries);
        const packet = corrupted.find((entry) => entry.record.taskId === accepted.taskId && entry.record.$schema === schemaUri("task-packet"))!;
        const reviewRequest = corrupted.find((entry) => entry.record.reviewId === accepted.review.reviewId && entry.record.$schema === schemaUri("review-request"))!.record;
        const verdict = corrupted.find((entry) => entry.record.reviewId === accepted.review.reviewId && entry.record.$schema === schemaUri("review-verdict"))!.record;
        if (corruption === "packet_hash") reviewRequest.contractRefs = [{ kind: "contract_task.packet", id: accepted.taskId, hash: sha256("wrong packet") }];
        else if (corruption === "stale_attempt") packet.record.attempt = Number(packet.record.attempt) + 1;
        else if (corruption === "self_review") { reviewRequest.reviewerAgentId = accepted.agentId; verdict.reviewerAgentId = accepted.agentId; }
        else if (corruption === "missing_inputs") verdict.reviewedArtifactIds = [];
        else corrupted.push(structuredClone(packet));
        const read = vi.spyOn(research, "submitted").mockImplementation((_projectId, correlationId) => corrupted.filter((entry) => !correlationId || entry.event.correlationId === correlationId));
        try { expect(research.missionCompletionBasis(projectId, mission.entityId).issues.some((issue) => issue.startsWith(`accepted node ${accepted.nodeId} `)), corruption).toBe(true); }
        finally { read.mockRestore(); }
      }
    } finally { store.close(); rmSync(directory, { recursive: true, force: true }); }
  });

  it("completes a Mission whose review-only criterion the worker defers, on the reviewer's criterion PASS alone", async () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-supervisor-")); const repositoryRoot = join(directory, "repository"); mkdirSync(repositoryRoot); initializeRepository(repositoryRoot); const projectId = createId("prj"); const project: RegisteredProject = { projectId, repositoryRoot, databasePath: join(directory, "project.sqlite"), registeredAt: new Date().toISOString() }; const store = new EventStore(project.databasePath); const publish = () => undefined; const research = new ResearchControl(() => store, () => project, publish); const fixture = new FixtureAgents((draft) => { appendFixtureEvent(store, draft); }, false, true); const supervisor = new MissionSupervisor(research, fixture as unknown as PiAdapter, () => [project], join(directory, "pi-package"), (draft) => { appendFixtureEvent(store, draft); });
    try {
      let mission = research.createMission(projectId, { title: "Fixture", objective: "Produce a reviewed result", deliverables: ["Reviewed result"], successCriteria: ["Deliverable artifact is preserved", "Independent review accepts every contract criterion"], idempotencyKey: "fixture-create" }); for (const next of ["planning", "awaiting_approval", "running"] as const) mission = research.transitionMission(projectId, mission.entityId, mission.version, next, `fixture-${next}`);
      for (const deadline = Date.now() + 60_000; Date.now() < deadline && research.mission(projectId, mission.entityId).state !== "completed";) { await supervisor.tick(); await new Promise((resolve) => setTimeout(resolve, 20)); }
      expect(research.mission(projectId, mission.entityId).state).toBe("completed");
      const basis = research.missionCompletionBasis(projectId, mission.entityId);
      const deferred = basis.criteria.find((criterion) => criterion.criterionId === "criterion_2")!;
      expect(deferred.validatorRunIds).toEqual([deferred.reviewId]);
    } finally { store.close(); rmSync(directory, { recursive: true, force: true }); }
  });

  it("reports a safe pause as paused only after every Mission agent has stopped at its boundary", async () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-supervisor-")); const repositoryRoot = join(directory, "repository"); mkdirSync(repositoryRoot); initializeRepository(repositoryRoot); const projectId = createId("prj"); const project: RegisteredProject = { projectId, repositoryRoot, databasePath: join(directory, "project.sqlite"), registeredAt: new Date().toISOString() }; const store = new EventStore(project.databasePath); const publish = () => undefined; const research = new ResearchControl(() => store, () => project, publish); const fixture = new FixtureAgents((draft) => { appendFixtureEvent(store, draft); }); const supervisor = new MissionSupervisor(research, fixture as unknown as PiAdapter, () => [project], join(directory, "pi-package"), (draft) => { appendFixtureEvent(store, draft); });
    try {
      let mission = research.createMission(projectId, { title: "Pause", objective: "Pause safely", deliverables: ["Note"], successCriteria: ["Paused"], idempotencyKey: "pause-create" });
      for (const next of ["planning", "awaiting_approval", "running", "pausing"] as const) mission = research.transitionMission(projectId, mission.entityId, mission.version, next, `pause-${next}`);
      await fixture.start({ ...({} as PiSessionOptions), projectId, missionId: mission.entityId, agentId: createId("agt"), taskId: createId("tsk"), role: "general_worker" } as PiSessionOptions);
      await supervisor.tick(); await new Promise((resolve) => setTimeout(resolve, 50));
      expect(research.mission(projectId, mission.entityId).state).toBe("pausing");
      for (const agent of fixture.inspect()) fixture.stop(agent.agentId);
      await supervisor.tick(); await new Promise((resolve) => setTimeout(resolve, 50));
      expect(research.mission(projectId, mission.entityId).state).toBe("paused");
    } finally { store.close(); rmSync(directory, { recursive: true, force: true }); }
  });

  it("runs typed Direction Director, immutable baseline, Evidence, and closure Review gates", async () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-direction-supervisor-")); const repositoryRoot = join(directory, "repository"); mkdirSync(repositoryRoot); initializeRepository(repositoryRoot); const projectId = createId("prj"); const project: RegisteredProject = { projectId, repositoryRoot, databasePath: join(directory, "project.sqlite"), registeredAt: new Date().toISOString() }; const store = new EventStore(project.databasePath); const research = new ResearchControl(() => store, () => project, () => undefined); const fixture = new FixtureAgents((draft) => { appendFixtureEvent(store, draft); }); const supervisor = new DirectionSupervisor(research, fixture as unknown as PiAdapter, () => [project], join(directory, "pi-package"), (draft) => { appendFixtureEvent(store, draft); });
    try {
      let direction = research.createDirection(projectId, { question: "Does the bounded change improve the metric?", decisionUse: "Choose whether to adopt it", idempotencyKey: "direction-create" }); for (const next of ["proposed", "active"] as const) direction = research.transitionDirection(projectId, direction.entityId, direction.version, next, `direction-${next}`);
      // The hypothesis node waits for the Direction's Autoresearch instead of dispatching a worker.
      const awaiting = () => research.scopedEvents(projectId, direction.value.missionId ?? "", "direction.awaiting_autoresearch").length || research.eventsAfter(projectId, 0).some((event) => event.type === "direction.awaiting_autoresearch" && event.scope.directionId === direction.entityId);
      for (const deadline = Date.now() + 60_000; Date.now() < deadline && !awaiting();) { await supervisor.tick(); await new Promise((resolve) => setTimeout(resolve, 20)); }
      expect(awaiting()).toBeTruthy(); expect(research.direction(projectId, direction.entityId).value.acceptedBaseline?.commit).toMatch(/^[0-9a-f]{40}$/);
      const original = research.autoresearch.bind(research);
      const finished = vi.spyOn(research, "autoresearch").mockImplementation((id) => [...original(id), { entityId: createId("ar"), version: 9, state: "completed", value: { directionId: direction.entityId } } as never]);
      for (const deadline = Date.now() + 60_000; Date.now() < deadline && research.direction(projectId, direction.entityId).state !== "closed";) { await supervisor.tick(); await new Promise((resolve) => setTimeout(resolve, 20)); }
      finished.mockRestore();
      const closed = research.direction(projectId, direction.entityId); expect(closed.state).toBe("closed"); expect(closed.value.acceptedBaseline?.commit).toMatch(/^[0-9a-f]{40}$/); expect(git(repositoryRoot, ["cat-file", "-t", closed.value.acceptedBaseline!.commit])).toBe("commit"); expect(git(repositoryRoot, ["merge-base", "--is-ancestor", "HEAD", closed.value.acceptedBaseline!.commit])).toBe(""); /* Task work stays on its worktree branch; the main branch is not advanced by workers. */ expect(closed.value.nodes.every((node) => node.state === "accepted")).toBe(true); expect(research.submitted(projectId).some((entry) => entry.event.scope.directionId === direction.entityId && (entry.record as { $schema?: string }).$schema === schemaUri("evidence"))).toBe(true); expect(research.records(projectId, "task-packet").some((record) => (record as { scope?: { directionId?: string } }).scope?.directionId === direction.entityId)).toBe(true); expect(research.records(projectId, "review-request").some((record) => (record as { scope?: { directionId?: string } }).scope?.directionId === direction.entityId)).toBe(true); /* The hypothesis node is resolved from Autoresearch without a director turn. */ expect(research.records(projectId, "research-director-cycle").length).toBeGreaterThanOrEqual(2); expect(research.submitted(projectId).some((entry) => entry.event.scope.directionId === direction.entityId && (entry.record as { $schema?: string }).$schema === schemaUri("direction-closure-packet"))).toBe(true); expect(store.operationIntents(projectId, "pending")).toEqual([]);
    } finally { store.close(); rmSync(directory, { recursive: true, force: true }); }
  });

  it("reconstructs Focus fingerprints when the supervisor process state is replaced", async () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-focus-recovery-")); const repositoryRoot = join(directory, "repository"); mkdirSync(repositoryRoot); initializeRepository(repositoryRoot); const projectId = createId("prj"); const project: RegisteredProject = { projectId, repositoryRoot, databasePath: join(directory, "project.sqlite"), registeredAt: new Date().toISOString() }; const store = new EventStore(project.databasePath); const research = new ResearchControl(() => store, () => project, () => undefined); const fixture = new FixtureAgents((draft) => { appendFixtureEvent(store, draft); }, true);
    try { let mission = research.createMission(projectId, { title: "Focus", objective: "Reject tunnel vision", deliverables: ["Reviewed result"], successCriteria: ["Repeated work is stopped"], idempotencyKey: "focus-create" }); for (const next of ["planning", "awaiting_approval", "running"] as const) mission = research.transitionMission(projectId, mission.entityId, mission.version, next, `focus-${next}`); for (const deadline = Date.now() + 60_000; Date.now() < deadline && !research.scopedEvents(projectId, mission.entityId, "focus.alarm").length;) { const supervisor = new MissionSupervisor(research, fixture as unknown as PiAdapter, () => [project], join(directory, "pi-package"), (draft) => { appendFixtureEvent(store, draft); }); await supervisor.tick(); await new Promise((resolve) => setTimeout(resolve, 20)); } expect(research.scopedEvents(projectId, mission.entityId, "focus.attempt_recorded").length).toBeGreaterThanOrEqual(2); expect(research.scopedEvents(projectId, mission.entityId, "focus.alarm")[0]?.payload).toMatchObject({ reasons: ["repeated_attempt_fingerprint"] }); expect(research.mission(projectId, mission.entityId).value.nodes.some((node) => node.state === "blocked")).toBe(true); } finally { store.close(); rmSync(directory, { recursive: true, force: true }); }
  });

  it("reconciles persisted Pi token use against the Mission ceiling", () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-budget-")); const repositoryRoot = join(directory, "repository"); mkdirSync(repositoryRoot); const projectId = createId("prj"); const project: RegisteredProject = { projectId, repositoryRoot, databasePath: join(directory, "project.sqlite"), registeredAt: new Date().toISOString() }; const store = new EventStore(project.databasePath); const research = new ResearchControl(() => store, () => project, () => undefined);
    try { const mission = research.createMission(projectId, { title: "Budget", objective: "Honor the ceiling", deliverables: ["Bounded result"], successCriteria: ["No overrun"], idempotencyKey: "budget-create" }); const scope = { projectId, missionId: mission.entityId, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: createId("agt") }; store.append({ $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "agent.completed", source: "pi", scope, correlationId: null, causationId: null, payload: { modelTokens: mission.value.budgets.maximumModelTokens + 1 } }); store.append({ $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "job.state_changed", source: "jobs", scope: { ...scope, jobId: createId("job") }, correlationId: null, causationId: null, payload: { usesGpu: true, startedAt: "2026-07-18T00:00:00.000Z", finishedAt: "2026-07-18T00:00:07.000Z" } }); const use = research.missionBudgetUse(projectId, mission.entityId); expect(use.modelTokens).toBe(mission.value.budgets.maximumModelTokens + 1); expect(use.gpuSeconds).toBe(7); expect(research.missionCompletionIssues(projectId, mission.entityId)).toContain("Mission model-token budget is exceeded"); } finally { store.close(); rmSync(directory, { recursive: true, force: true }); }
  });

  it("releases an orphaned working lease after supervisor restart", async () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-lease-recovery-")); const repositoryRoot = join(directory, "repository"); mkdirSync(repositoryRoot); initializeRepository(repositoryRoot); const projectId = createId("prj"); const project: RegisteredProject = { projectId, repositoryRoot, databasePath: join(directory, "project.sqlite"), registeredAt: new Date().toISOString() }; const store = new EventStore(project.databasePath); const research = new ResearchControl(() => store, () => project, () => undefined); const fixture = new FixtureAgents((draft) => { appendFixtureEvent(store, draft); });
    try { let mission = research.createMission(projectId, { title: "Lease recovery", objective: "Resume without a lost owner", deliverables: ["Recovered result"], successCriteria: ["Every node is accepted"], idempotencyKey: "lease-create" }); for (const next of ["planning", "awaiting_approval", "running"] as const) mission = research.transitionMission(projectId, mission.entityId, mission.version, next, `lease-${next}`); const node = mission.value.nodes[0]!; const leaseId = createId("tsk"); mission = research.transitionMissionNode(projectId, mission.entityId, mission.version, node.id, "leased", "orphan-lease", { leaseId, ownerId: createId("agt"), version: mission.version, expiresAt: new Date(Date.now() + 60_000).toISOString() }); research.transitionMissionNode(projectId, mission.entityId, mission.version, node.id, "working", "orphan-working"); for (const deadline = Date.now() + 60_000; Date.now() < deadline && research.mission(projectId, mission.entityId).state !== "completed";) { const supervisor = new MissionSupervisor(research, fixture as unknown as PiAdapter, () => [project], join(directory, "pi-package"), (draft) => { appendFixtureEvent(store, draft); }); await supervisor.tick(); await new Promise((resolve) => setTimeout(resolve, 20)); } expect(research.mission(projectId, mission.entityId).state).toBe("completed"); expect(research.scopedEvents(projectId, mission.entityId, "mission.node_recovered")[0]?.payload).toMatchObject({ nodeId: node.id, priorState: "working", disposition: "ready_for_new_lease" }); } finally { store.close(); rmSync(directory, { recursive: true, force: true }); }
  });
  it("uses scoped graph resolution rather than every historical blocker at Mission completion", () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-blocker-resolution-")); const repositoryRoot = join(directory, "repository"); mkdirSync(repositoryRoot); const projectId = createId("prj"); const project: RegisteredProject = { projectId, repositoryRoot, databasePath: join(directory, "project.sqlite"), registeredAt: new Date().toISOString() }; const store = new EventStore(project.databasePath); const research = new ResearchControl(() => store, () => project, () => undefined);
    try {
      let mission = research.createMission(projectId, { title: "Blocker scope", objective: "Track live defects", deliverables: ["Bounded result"], successCriteria: ["No unresolved blocker"], idempotencyKey: "blocker-create" });
      for (const next of ["planning", "awaiting_approval", "running"] as const) mission = research.transitionMission(projectId, mission.entityId, mission.version, next, `blocker-${next}`);
      const resolvedNode = mission.value.nodes[0]!; const resolvedId = createId("blk"); const openId = createId("blk"); const scope = { projectId, missionId: mission.entityId, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: createId("agt") };
      for (const blocker of [{ blockerId: resolvedId, scopeType: "graph_node", scopeId: resolvedNode.id }, { blockerId: openId, scopeType: "mission", scopeId: mission.entityId }]) store.append({ $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "record.submitted", source: "fixture", scope, correlationId: null, causationId: null, payload: { $schema: schemaUri("blocker"), ...blocker } });
      mission = research.transitionMissionNode(projectId, mission.entityId, mission.version, resolvedNode.id, "leased", "blocker-lease", { leaseId: createId("tsk"), ownerId: scope.agentId, version: mission.version, expiresAt: new Date(Date.now() + 60_000).toISOString() });
      for (const next of ["working", "postflight", "reviewing", "accepted"] as const) mission = research.transitionMissionNode(projectId, mission.entityId, mission.version, resolvedNode.id, next, `blocker-${next}`);
      expect(research.missionCompletionBasis(projectId, mission.entityId).openDefectIds).toEqual([openId]);
    } finally { store.close(); rmSync(directory, { recursive: true, force: true }); }
  });

  it("emits canonical contracts only at Mission approval and Direction activation", () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-canonical-records-"));
    const repositoryRoot = join(directory, "repository");
    mkdirSync(repositoryRoot);
    const projectId = createId("prj");
    const project: RegisteredProject = { projectId, repositoryRoot, databasePath: join(directory, "project.sqlite"), registeredAt: new Date().toISOString() };
    const store = new EventStore(project.databasePath);
    const research = new ResearchControl(() => store, () => project, () => undefined);
    try {
      let mission = research.createMission(projectId, { title: "Canonical", objective: "Preserve wire authority", deliverables: ["Contract"], successCriteria: ["Status is valid"], idempotencyKey: "canonical-mission" });
      const direction = research.createDirection(projectId, { question: "Does status follow projection?", decisionUse: "Choose a contract", missionId: mission.entityId, idempotencyKey: "canonical-direction" });
      expect(mission.value.canonicalContract).toBeNull();
      expect(direction.value.canonicalContract).toBeNull();
      expect(research.records(projectId, "mission-contract")).toHaveLength(0);
      expect(research.records(projectId, "direction-contract")).toHaveLength(0);

      mission = research.transitionMission(projectId, mission.entityId, mission.version, "planning", "canonical-planning");
      mission = research.transitionMission(projectId, mission.entityId, mission.version, "awaiting_approval", "canonical-awaiting");
      mission = research.transitionMission(projectId, mission.entityId, mission.version, "running", "canonical-running");
      let activated = research.transitionDirection(projectId, direction.entityId, direction.version, "proposed", "canonical-proposed");
      activated = research.transitionDirection(projectId, activated.entityId, activated.version, "active", "canonical-active");

      const missionContracts = research.records(projectId, "mission-contract") as Array<{ approvedAt: string; approvedBy: string }>;
      const directionContracts = research.records(projectId, "direction-contract") as Array<{ activatedAt: string; approvedBy: string; evaluationContractId: string; baselineExperimentId: string }>;
      expect(missionContracts).toHaveLength(1);
      expect(directionContracts).toHaveLength(1);
      expect(missionContracts[0]).toMatchObject({ approvedAt: mission.value.approvedAt, approvedBy: "user" });
      expect(directionContracts[0]).toMatchObject({ activatedAt: activated.value.activatedAt, approvedBy: "user", evaluationContractId: activated.value.evaluationContractId, baselineExperimentId: activated.value.plannedBaselineExperimentId });
      expect(mission.value.canonicalContract).toEqual(missionContracts[0]);
      expect(activated.value.canonicalContract).toEqual(directionContracts[0]);

      const resumed = new ResearchControl(() => store, () => project, () => undefined);
      expect(resumed.mission(projectId, mission.entityId).value.canonicalContract).toEqual(missionContracts[0]);
      expect(resumed.direction(projectId, activated.entityId).value.canonicalContract).toEqual(directionContracts[0]);
      expect(resumed.records(projectId, "mission-contract")).toHaveLength(1);
      expect(resumed.records(projectId, "direction-contract")).toHaveLength(1);

    } finally {
      store.close();
      rmSync(directory, { recursive: true, force: true });
    }
  });
  it("rejects a Mission contract criterion missing from all task and Review mappings", () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-missing-criterion-")); const repositoryRoot = join(directory, "repository"); mkdirSync(repositoryRoot); const projectId = createId("prj"); const project: RegisteredProject = { projectId, repositoryRoot, databasePath: join(directory, "project.sqlite"), registeredAt: new Date().toISOString() }; const store = new EventStore(project.databasePath); const research = new ResearchControl(() => store, () => project, () => undefined);
    try {
      const mission = research.createMission(projectId, { title: "Two criteria", objective: "Require complete provenance", deliverables: ["Mapping"], successCriteria: ["Criterion one", "Criterion two"], idempotencyKey: "missing-criterion" });
      const basis = research.missionCompletionBasis(projectId, mission.entityId);
      expect(basis.issues).toContain("Mission contract criterion criterion_2 is absent from an independently reviewed accepted-node Task Packet");
      expect(basis.issues).toContain("every Mission contract criterion requires an exact task, validator, and independent Review mapping");
    } finally { store.close(); rmSync(directory, { recursive: true, force: true }); }
  });
  it("uses only the highest unambiguous valid Claim version with independently reviewed Evidence", () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-claim-version-")); const repositoryRoot = join(directory, "repository"); mkdirSync(repositoryRoot); const projectId = createId("prj"); const project: RegisteredProject = { projectId, repositoryRoot, databasePath: join(directory, "project.sqlite"), registeredAt: new Date().toISOString() }; const store = new EventStore(project.databasePath); const research = new ResearchControl(() => store, () => project, () => undefined);
    try {
      const mission = research.createMission(projectId, { title: "Claim authority", objective: "Use the latest Claim", deliverables: ["Claim audit"], successCriteria: ["Claims are current"], idempotencyKey: "claim-version" }); const claimId = createId("clm"); const scope = { projectId, missionId: mission.entityId, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: null }; const now = new Date().toISOString();
      const evidenceId = createId("evd"); const reviewId = createId("rev"); const reviewerAgentId = createId("agt"); const producerAgentId = createId("agt"); const reviewTaskId = createId("tsk");
      const claim = (claimVersion: number, status: "under_test" | "supported", text: string): JsonValue => ({ $schema: schemaUri("claim"), schemaVersion: 1, claimId, projectId, claimVersion, text, claimType: "claim_result", status, supportingEvidenceIds: status === "supported" ? [evidenceId] : [], contradictingEvidenceIds: [], qualifyingEvidenceIds: [], limitations: status === "under_test" ? ["Awaiting the next valid Claim version"] : [], paperLocations: [], requiredReviewId: status === "supported" ? reviewId : null, supersedesClaimVersion: claimVersion === 1 ? null : 1, updatedAt: now });
      for (const record of [claim(1, "under_test", "Initial hypothesis"), claim(2, "supported", "Validated conclusion")]) store.append({ $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "record.submitted", source: "fixture", scope, correlationId: null, causationId: null, payload: record });
      // A supported label alone does not prove the Claim, even at the latest version.
      expect(research.missionCompletionBasis(projectId, mission.entityId).unresolvedClaimIds).toContain(claimId);
      const request = { $schema: schemaUri("review-request"), schemaVersion: 1, reviewRequestId: `request_${reviewId.slice(4)}`, reviewId, reviewType: "claim", target: { targetType: "claim_record", targetId: claimId, targetVersion: 2 }, scope: { projectId, missionId: mission.entityId, directionId: null, autoresearchId: null }, producerAgentIds: [producerAgentId], reviewerAgentId, independenceCheck: "pass", contractRefs: [{ kind: "contract_claim", id: claimId, hash: sha256(claim(2, "supported", "Validated conclusion")) }], requiredArtifactIds: [], requiredEvidenceIds: [evidenceId], deterministicPostflight: { status: "pass", validatorRunIds: ["validator_claim.fixture"] }, criteria: [{ criterionId: "criterion_support", statement: "Evidence supports the Claim", required: true, severityIfFailed: "blocking" }], allowedVerdicts: ["PASS", "REVISE", "REDESIGN", "BLOCKED"], issuedAt: now };
      research.submitDaemonRecord(projectId, { ...scope, agentId: reviewerAgentId }, `task:${reviewTaskId}`, request, "claim-request");
      research.submitDaemonRecord(projectId, { ...scope, agentId: reviewerAgentId }, `task:${reviewTaskId}`, { $schema: schemaUri("review-verdict"), schemaVersion: 1, templateVersion: "1.0.0", reviewId, reviewRequestId: request.reviewRequestId, reviewType: request.reviewType, target: request.target, reviewerAgentId, independenceCheck: "pass", verdict: "PASS", summary: "The supporting Evidence was independently checked", criteria: [{ criterionId: "criterion_support", required: true, status: "PASS", finding: "The bounded result supports the Claim", evidenceRefs: [], confidence: "high", defectIds: [] }], defects: [], missingRequiredInputs: [], scientificIntegrityFlags: [], recommendedGraphAction: "accept_node", recommendedPromotion: "not_applicable", reviewedArtifactIds: [], reviewedEvidenceIds: [evidenceId], submittedAt: now }, "claim-verdict");
      research.submitDaemonRecord(projectId, scope, evidenceId, { $schema: schemaUri("evidence"), schemaVersion: 1, evidenceId, projectId, evidenceType: "evidence_result", statement: "The bounded fixture result supports the Claim", polarity: "supports", sourceRefs: [{ refType: "source_fixture", refId: "fixture_result", locator: "result" }], evaluationContractHash: null, scopeLimitations: ["Fixture only"], quality: { status: "reviewed", reviewId, confidence: "high" }, createdBy: "agent", createdByAgentId: producerAgentId, createdAt: now }, "claim-evidence");
      let basis = research.missionCompletionBasis(projectId, mission.entityId);
      expect(basis.unresolvedClaimIds).not.toContain(claimId);
      expect(basis.claims).toHaveLength(1);
      expect(basis.claims[0]).toMatchObject({ claimId, claimVersion: 2, status: "supported" });
      store.append({ $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "record.submitted", source: "fixture", scope, correlationId: null, causationId: null, payload: claim(2, "supported", "Conflicting version two") });
      basis = research.missionCompletionBasis(projectId, mission.entityId);
      expect(basis.issues).toContain(`ambiguous Claim version ${claimId}@2`);
    } finally { store.close(); rmSync(directory, { recursive: true, force: true }); }
  });
  it("enforces scoped task authority for progress, delegation, and exact handoff teach-back", () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-domain-effects-")); const repositoryRoot = join(directory, "repository"); mkdirSync(repositoryRoot); const projectId = createId("prj"); const project: RegisteredProject = { projectId, repositoryRoot, databasePath: join(directory, "project.sqlite"), registeredAt: new Date().toISOString() }; const store = new EventStore(project.databasePath); const research = new ResearchControl(() => store, () => project, () => undefined);
    try {
      let mission = research.createMission(projectId, { title: "Domain effects", objective: "Enforce effect authority", deliverables: ["State"], successCriteria: ["Effects are scoped"], idempotencyKey: "effects-create" });
      for (const next of ["planning", "awaiting_approval", "running"] as const) mission = research.transitionMission(projectId, mission.entityId, mission.version, next, `effects-${next}`);
      const taskId = createId("tsk"); const agentId = createId("agt"); const node = mission.value.nodes[0]!; mission = research.transitionMissionNode(projectId, mission.entityId, mission.version, node.id, "leased", "effects-lease", { leaseId: taskId, ownerId: agentId, version: mission.version, expiresAt: new Date(Date.now() + 60_000).toISOString() });
      mission = research.transitionMissionNode(projectId, mission.entityId, mission.version, node.id, "working", "effects-working");
      const scope = { projectId, missionId: mission.entityId, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId };
      const session = { ...scope, taskId, role: "general_worker" as const };
      store.append({ $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "record.submitted", source: "fixture", scope, correlationId: `task:${taskId}`, causationId: null, payload: { $schema: schemaUri("task-packet"), taskId, assignedRole: "general_worker", assignedAgentId: agentId, scope: { projectId, missionId: mission.entityId, directionId: null, autoresearchId: null, experimentId: null, graphNodeId: node.id }, observedVersions: { missionGraphVersion: mission.value.graphVersion, leaseVersion: mission.version }, lease: { leaseId: taskId, expiresAt: new Date(Date.now() + 60_000).toISOString() }, budget: { maximumModelTokens: 100, maximumWallClockSeconds: 100 }, permissions: { delegation: "request_only" } } });
      research.applyDomainEffect(projectId, session, { $schema: schemaUri("progress-update"), progressId: "progress_effects", taskId, agentId });
      research.applyDomainEffect(projectId, session, { $schema: schemaUri("delegation-request"), requestId: "delegation_effects", requestingTaskId: taskId, requestingAgentId: agentId, budgetEstimate: { modelTokens: 10, wallClockSeconds: 10 } });
      const recipient = { ...session, agentId: createId("agt"), taskId: null }; const handoffId = createId("hnd"); const handoff = { $schema: schemaUri("handoff"), handoffId, logicalOwnerId: "owner_effects", fromAgentId: agentId, toAgentId: recipient.agentId, scope, goalStack: { projectGoalId: projectId, missionCriterionIds: [], directionQuestionId: null, currentGraphNodeId: node.id }, observedVersions: { missionGraphVersion: mission.value.graphVersion }, branch: { head: "main" }, openDefects: [], openBlockerIds: [], readyNodeIds: [node.id], oldLeaseReleaseId: taskId };
      store.append({ $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "record.submitted", source: "fixture", scope, correlationId: `task:${taskId}`, causationId: null, payload: handoff });
      research.applyDomainEffect(projectId, session, handoff);
      const teachback = { $schema: schemaUri("handoff-teachback"), handoffId, logicalOwnerId: handoff.logicalOwnerId, toAgentId: recipient.agentId, decision: "accepted", understoodGoalStack: handoff.goalStack, observedVersions: handoff.observedVersions, observedBranchHead: "main", acknowledgedDefectIds: [], acknowledgedBlockerIds: [], selectedNextNodeId: node.id, conflicts: [] };
      store.append({ $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "record.submitted", source: "fixture", scope: { ...scope, agentId: recipient.agentId }, correlationId: handoffId, causationId: null, payload: teachback });
      expect(new ResearchControl(() => store, () => project, () => undefined).reconcileAcceptedDomainEffects(projectId)).toEqual({ recovered: 1, failed: 0 });
      expect(research.mission(projectId, mission.entityId).value.nodes.find((entry) => entry.id === node.id)?.state).toBe("ready");
      expect(store.projection(projectId, "logical_ownership", handoff.logicalOwnerId)?.value).toMatchObject({ agentId: recipient.agentId, handoffId });
      expect(store.replay(projectId).map((event) => event.type)).toEqual(expect.arrayContaining(["task.progress_recorded", "delegation.requested", "handoff.pending", "handoff.accepted"]));
      const handoffTerminal = store.replay(projectId).filter((event) => event.type === "handoff.accepted" && event.correlationId === handoffId);
      expect(handoffTerminal).toHaveLength(1);
      expect(handoffTerminal[0]?.payload).toMatchObject({ decisionHash: sha256(teachback) });
      const duplicatePacket = store.replay(projectId).find((event) => event.type === "record.submitted" && (event.payload as { taskId?: string }).taskId === taskId)?.payload;
      if (!duplicatePacket) throw new Error("task packet fixture missing");
      store.append({ $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "record.submitted", source: "fixture", scope, correlationId: `task:${taskId}:duplicate`, causationId: null, payload: duplicatePacket });
      expect(() => research.applyDomainEffect(projectId, session, { $schema: schemaUri("progress-update"), progressId: "progress_ambiguous_authority", taskId, agentId })).toThrow("exactly one matching durable Task Packet");
    } finally { store.close(); rmSync(directory, { recursive: true, force: true }); }
  });
  it("recovers accepted domain effects after the submit-to-effect crash window", () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-domain-recovery-"));
    const repositoryRoot = join(directory, "repository");
    mkdirSync(repositoryRoot);
    const projectId = createId("prj");
    const project: RegisteredProject = { projectId, repositoryRoot, databasePath: join(directory, "project.sqlite"), registeredAt: new Date().toISOString() };
    const store = new EventStore(project.databasePath);
    const research = new ResearchControl(() => store, () => project, () => undefined);
    try {
      let mission = research.createMission(projectId, { title: "Crash recovery", objective: "Recover accepted effects", deliverables: ["Recovery"], successCriteria: ["Accepted records remain durable"], idempotencyKey: "recovery-create" });
      for (const state of ["planning", "awaiting_approval", "running"] as const) mission = research.transitionMission(projectId, mission.entityId, mission.version, state, `recovery-${state}`);
      const taskId = createId("tsk");
      const agentId = createId("agt");
      const node = mission.value.nodes[0]!;
      mission = research.transitionMissionNode(projectId, mission.entityId, mission.version, node.id, "leased", "recovery-lease", { leaseId: taskId, ownerId: agentId, version: mission.version, expiresAt: new Date(Date.now() + 60_000).toISOString() });
      const scope = { projectId, missionId: mission.entityId, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId };
      store.append({ $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "record.submitted", source: "fixture", scope, correlationId: `task:${taskId}`, causationId: null, payload: { $schema: schemaUri("task-packet"), taskId, assignedRole: "general_worker", assignedAgentId: agentId, scope: { projectId, missionId: mission.entityId, directionId: null, autoresearchId: null, experimentId: null, graphNodeId: node.id }, observedVersions: { missionGraphVersion: mission.value.graphVersion, leaseVersion: mission.version }, lease: { leaseId: taskId, expiresAt: new Date(Date.now() + 60_000).toISOString() }, budget: { maximumModelTokens: 100, maximumWallClockSeconds: 100 }, permissions: { delegation: "request_only" } } });
      const accepted = store.append({ $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "record.submitted", source: "fixture", scope, correlationId: `task:${taskId}`, causationId: null, payload: { $schema: schemaUri("progress-update"), progressId: "progress_recovery", taskId, agentId } });
      const invalid = store.append({ $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "record.submitted", source: "fixture", scope, correlationId: "task:missing", causationId: null, payload: { $schema: schemaUri("progress-update"), progressId: "progress_unrecoverable", taskId: createId("tsk"), agentId } });

      const resumed = new ResearchControl(() => store, () => project, () => undefined);
      expect(resumed.reconcileAcceptedDomainEffects(projectId)).toEqual({ recovered: 1, failed: 1 });
      expect(store.replay(projectId).filter((event) => event.type === "task.progress_recorded")).toHaveLength(1);
      expect(store.replay(projectId).filter((event) => event.type === "recovery.domain_effect_failed")).toHaveLength(1);
      expect(resumed.submitted(projectId).map((entry) => entry.event.eventId)).toEqual(expect.arrayContaining([accepted.eventId, invalid.eventId]));
      expect(new ResearchControl(() => store, () => project, () => undefined).reconcileAcceptedDomainEffects(projectId)).toEqual({ recovered: 0, failed: 1 });
      expect(store.replay(projectId).filter((event) => event.type === "recovery.domain_effect_failed")).toHaveLength(1);
    } finally {
      store.close();
      rmSync(directory, { recursive: true, force: true });
    }
  });

  it("requires explicit valid lease acquisition for Mission and Direction nodes", () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-explicit-leases-")); const repositoryRoot = join(directory, "repository"); mkdirSync(repositoryRoot); const projectId = createId("prj"); const project: RegisteredProject = { projectId, repositoryRoot, databasePath: join(directory, "project.sqlite"), registeredAt: new Date().toISOString() }; const store = new EventStore(project.databasePath); const research = new ResearchControl(() => store, () => project, () => undefined);
    try {
      let mission = research.createMission(projectId, { title: "Explicit Mission lease", objective: "Lease explicitly", deliverables: ["Lease"], successCriteria: ["Lease validation"], idempotencyKey: "explicit-mission" });
      for (const state of ["planning", "awaiting_approval", "running"] as const) mission = research.transitionMission(projectId, mission.entityId, mission.version, state, `explicit-mission-${state}`);
      const missionNode = mission.value.nodes[0]!;
      expect(() => research.transitionMissionNode(projectId, mission.entityId, mission.version, missionNode.id, "leased", "mission-no-lease")).toThrow("leased transitions require one");
      expect(() => research.transitionMissionNode(projectId, mission.entityId, mission.version, missionNode.id, "working", "mission-wrong-next", { leaseId: createId("tsk"), ownerId: createId("agt"), version: mission.version, expiresAt: new Date(Date.now() + 60_000).toISOString() })).toThrow("Only a leased transition");
      expect(() => research.transitionMissionNode(projectId, mission.entityId, mission.version, missionNode.id, "leased", "mission-expired", { leaseId: createId("tsk"), ownerId: createId("agt"), version: mission.version, expiresAt: new Date(Date.now() - 1).toISOString() })).toThrow("expire in the future");
      mission = research.transitionMissionNode(projectId, mission.entityId, mission.version, missionNode.id, "leased", "mission-valid", { leaseId: createId("tsk"), ownerId: createId("agt"), version: mission.version, expiresAt: new Date(Date.now() + 60_000).toISOString() });
      expect(mission.value.nodes[0]?.lease?.ownerId).toBeTruthy();
      let direction = research.createDirection(projectId, { question: "Can a Direction lease explicitly?", decisionUse: "Validate node control", idempotencyKey: "explicit-direction" });
      for (const state of ["proposed", "active"] as const) direction = research.transitionDirection(projectId, direction.entityId, direction.version, state, `explicit-direction-${state}`);
      const directionNode = direction.value.nodes[0]!;
      expect(() => research.transitionDirectionNode(projectId, direction.entityId, direction.version, directionNode.id, "leased", "direction-no-lease")).toThrow("leased transitions require one");
      expect(() => research.transitionDirectionNode(projectId, direction.entityId, direction.version, directionNode.id, "postflight", "direction-wrong-next", { leaseId: createId("tsk"), ownerId: createId("agt"), version: direction.version, expiresAt: new Date(Date.now() + 60_000).toISOString() })).toThrow("Only a leased transition");
      direction = research.transitionDirectionNode(projectId, direction.entityId, direction.version, directionNode.id, "leased", "direction-valid", { leaseId: createId("tsk"), ownerId: createId("agt"), version: direction.version, expiresAt: new Date(Date.now() + 60_000).toISOString() });
      expect(direction.value.nodes[0]?.lease?.leaseId).toBeTruthy();
      direction = research.transitionDirectionNode(projectId, direction.entityId, direction.version, directionNode.id, "blocked", "direction-node-blocked");
      direction = research.transitionDirection(projectId, direction.entityId, direction.version, "blocked", "direction-blocked");
      // A blocked Direction accepts only the repair retry (blocked/failed node -> ready), never other node work.
      expect(() => research.transitionDirectionNode(projectId, direction.entityId, direction.version, directionNode.id, "cancelled", "direction-blocked-cancel")).toThrow("only while the Direction is active");
      direction = research.transitionDirectionNode(projectId, direction.entityId, direction.version, directionNode.id, "ready", "direction-node-retry");
      expect(direction.state).toBe("blocked"); expect(direction.value.nodes[0]?.state).toBe("ready");
      // Exhaust the attempt budget; only an explicit user retry grants one more attempt.
      direction = research.transitionDirection(projectId, direction.entityId, direction.version, "active", "direction-reactivate");
      for (let attempt = direction.value.nodes[0]!.attempt; attempt < direction.value.nodes[0]!.maximumAttempts; attempt++) {
        direction = research.transitionDirectionNode(projectId, direction.entityId, direction.version, directionNode.id, "leased", `exhaust-lease-${attempt}`, { leaseId: createId("tsk"), ownerId: createId("agt"), version: direction.version, expiresAt: new Date(Date.now() + 60_000).toISOString() });
        direction = research.transitionDirectionNode(projectId, direction.entityId, direction.version, directionNode.id, "working", `exhaust-working-${attempt}`);
        direction = research.transitionDirectionNode(projectId, direction.entityId, direction.version, directionNode.id, "failed", `exhaust-failed-${attempt}`);
        if (direction.value.nodes[0]!.attempt < direction.value.nodes[0]!.maximumAttempts) direction = research.transitionDirectionNode(projectId, direction.entityId, direction.version, directionNode.id, "ready", `exhaust-ready-${attempt}`);
      }
      expect(() => research.transitionDirectionNode(projectId, direction.entityId, direction.version, directionNode.id, "ready", "exhausted-auto")).toThrow("attempt budget is exhausted");
      const exhausted = direction.value.nodes[0]!;
      direction = research.transitionDirectionNode(projectId, direction.entityId, direction.version, directionNode.id, "ready", "exhausted-user", undefined, true);
      expect(direction.value.nodes[0]).toMatchObject({ state: "ready", maximumAttempts: exhausted.attempt + 1 });
    } finally { store.close(); rmSync(directory, { recursive: true, force: true }); }
  });
});

function initializeRepository(path: string): void { git(path, ["init", "-b", "main"]); git(path, ["config", "user.email", "fixture@nosh.test"]); git(path, ["config", "user.name", "NOSH Fixture"]); writeFileSync(join(path, ".gitignore"), ".nosh/\n.fixture-*\n"); writeFileSync(join(path, "README.md"), "# Fixture\n"); git(path, ["add", "--", ".gitignore", "README.md"]); git(path, ["commit", "-m", "fixture baseline"]); }
function git(path: string, args: string[]): string { const result = spawnSync("git", ["-C", path, ...args], { encoding: "utf8", windowsHide: true }); if (result.status !== 0) throw new Error(result.stderr.trim() || `git ${args[0]} failed`); return result.stdout.trim(); }

function appendFixtureEvent(store: EventStore, draft: EventDraft): void {
  if (draft.type === "record.submitted" && isTaskTerminalRecord(draft.payload)) {
    const taskId = /^task:(tsk_[0-9a-f]{32})$/.exec(draft.correlationId ?? "")?.[1];
    if (!taskId) throw new Error("Fixture terminal submission requires an exact Task correlation");
    store.appendTerminalSubmission(taskId, "fixture", (draft.payload as { $schema: string }).$schema, draft.payload, draft);
  } else store.append(draft);
}

// Task sessions run in .nosh/worktrees/<id>; the artifact store is Project-level.
function projectRoot(cwd: string): string { const marker = join(".nosh", "worktrees"); return cwd.includes(marker) ? cwd.slice(0, cwd.indexOf(marker)) : cwd; }
