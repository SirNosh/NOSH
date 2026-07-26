import { EventEmitter } from "node:events";
import { mkdirSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { timingSafeEqual } from "node:crypto";
import { createId } from "@nosh/core";
import { EventStore, HostRegistry, type RegisteredProject } from "@nosh/persistence";
import { StructuredSubmissionGate } from "@nosh/agent-runtime";
import { PiAdapter, type PiSessionOptions } from "@nosh/pi-adapter";
import { OrchestrationRuntime } from "@nosh/orchestration-runtime";
import { JobSupervisor, type JobRecord, type JobSpec } from "@nosh/jobs";
import { approveProjectContract, ArtifactStore, initializeResearchProject, readProjectContract, type ProjectContract, type ProjectInitialization } from "@nosh/evidence";
import { appendEventCommandSchema, schemaUri, sha256, type AppendEventCommand, type EventDraft, type EventEnvelope, type JsonValue, type RemoteCommandEnvelope } from "@nosh/wire";
import { SingleInstanceLock } from "./single-instance.js";
import { ResearchControl } from "./research-control.js";
import { MissionSupervisor } from "./mission-supervisor.js";
import { DirectionSupervisor } from "./direction-supervisor.js";
import { AutoresearchSupervisor } from "./autoresearch-supervisor.js";

export type NoshDaemonOptions = {
  dataDirectory: string;
  bootstrapToken: string;
  packagePath?: string;
};

export class NoshDaemon {
  private readonly stores = new Map<string, EventStore>();
  private readonly lock: SingleInstanceLock;
  private readonly submissionGate = new StructuredSubmissionGate();
  readonly events = new EventEmitter();
  readonly registry: HostRegistry;
  readonly agents: PiAdapter;
  readonly jobs: JobSupervisor;
  readonly research: ResearchControl;
  readonly runtime: OrchestrationRuntime;
  readonly missions: MissionSupervisor;
  readonly directions: DirectionSupervisor;
  readonly experiments: AutoresearchSupervisor;

  constructor(private readonly options: NoshDaemonOptions) {
    mkdirSync(options.dataDirectory, { recursive: true });
    this.lock = new SingleInstanceLock(join(options.dataDirectory, "noshd.lock"));
    this.registry = new HostRegistry(join(options.dataDirectory, "host.sqlite"));
    this.research = new ResearchControl((projectId) => this.storeFor(projectId), (projectId) => { const project = this.projects().find((entry) => entry.projectId === projectId); if (!project) throw new Error(`Project ${projectId} is not registered on this host`); return project; }, (event) => this.events.emit("event", event));
    this.agents = new PiAdapter((event) => this.appendDraft(event), (tool, projectId, attemptKey, record, agentId) => this.submitTool(tool, projectId, attemptKey, record, agentId));
    const packagePath = options.packagePath ?? resolve(import.meta.dirname, "..", "..", "..", "pi-package");
    this.runtime = new OrchestrationRuntime({ storeFor: (projectId) => this.storeFor(projectId), projectFor: (projectId) => { const project = this.projects().find((entry) => entry.projectId === projectId); if (!project) throw new Error(`Project ${projectId} is not registered on this host`); return project; }, sessions: this.agents, packagePath, publish: (event) => this.events.emit("event", event) });
    this.missions = new MissionSupervisor(this.research, this.agents, () => this.projects(), packagePath, (event) => this.appendDraft(event), this.runtime);
    this.directions = new DirectionSupervisor(this.research, this.agents, () => this.projects(), packagePath, (event) => this.appendDraft(event), this.runtime);
    this.jobs = new JobSupervisor(options.dataDirectory, (job) => {
      if (!this.projects().some((project) => project.projectId === job.projectId)) return;
      this.appendDraft({
        $schema: "https://nosh.dev/schemas/event/v1", schemaVersion: 1, retention: "persistent", type: "job.state_changed", source: "job_supervisor",
        scope: { projectId: job.projectId, missionId: job.missionId, directionId: job.directionId, autoresearchId: job.autoresearchId, experimentId: job.experimentId, runId: job.runId, jobId: job.jobId, agentId: null },
        correlationId: job.runId, causationId: null, payload: job as unknown as JsonValue,
      });
    });
    this.experiments = new AutoresearchSupervisor(this.research, this.agents, this.jobs, (spec) => this.startJob(spec), () => this.projects(), packagePath, (event) => this.appendDraft(event), this.runtime);
  }

  start(): void {
    this.lock.acquire();
    this.jobs.recover();
    for (const project of this.projects()) this.reconcileProject(project);
    this.missions.start();
    this.directions.start();
    this.experiments.start();
  }

  stop(): void {
    this.missions.stop();
    this.directions.stop();
    this.experiments.stop();
    for (const agent of this.agents.inspect()) this.agents.stop(agent.agentId);
    this.jobs.close();
    for (const store of this.stores.values()) store.close();
    this.stores.clear();
    this.registry.close();
    this.lock.release();
  }

  authenticate(token: string | undefined): boolean {
    if (!token) return false;
    const provided = Buffer.from(token);
    const expected = Buffer.from(this.options.bootstrapToken);
    return provided.length === expected.length && timingSafeEqual(provided, expected);
  }

  registerProject(project: Omit<RegisteredProject, "registeredAt">): RegisteredProject {
    const registered = this.registry.register(project); this.reconcileProject(registered); return registered;
  }

  initializeProject(input: Omit<ProjectInitialization, "dataDirectory">): RegisteredProject { return this.registerProject(initializeResearchProject({ ...input, dataDirectory: this.options.dataDirectory })); }

  projectContract(projectId: string): ProjectContract { const project = this.projects().find((entry) => entry.projectId === projectId); if (!project) throw new Error(`Project ${projectId} is not registered on this host`); return readProjectContract(project.repositoryRoot); }

  async beginProjectIntake(projectId: string, model?: { provider: string; id: string }): Promise<void> {
    const project = this.projects().find((entry) => entry.projectId === projectId); if (!project) throw new Error(`Project ${projectId} is not registered on this host`); const contract = this.projectContract(projectId); if (contract.approvedAt) return;
    const agent = await this.startAgent({ projectId, missionId: null, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, taskId: null, agentId: createId("agt"), role: "nosh", cwd: project.repositoryRoot, packagePath: this.options.packagePath ?? resolve(import.meta.dirname, "..", "..", "..", "pi-package"), ...(model ? { model } : {}) });
    await this.agents.prompt(agent.agentId, intakePrompt(contract));
  }

  projects(): RegisteredProject[] {
    return this.registry.list();
  }

  appendCommand(command: AppendEventCommand): { event: EventEnvelope; replayed: boolean } {
    const parsed = appendEventCommandSchema.parse(command);
    if (parsed.projectId !== parsed.payload.scope.projectId) throw new Error("Command project scope does not match event project scope");
    const result = this.storeFor(parsed.projectId).appendIdempotent(parsed.idempotencyKey, parsed.payload);
    if (!result.replayed) this.events.emit("event", result.receipt.event);
    return { event: result.receipt.event, replayed: result.replayed };
  }

  submitRecord(tool: string, projectId: string, attemptKey: string, record: unknown, agentId?: string):
    | { accepted: true; event: EventEnvelope }
    | { accepted: false; retryAllowed: boolean; errors: Array<{ pointer: string; code: string; message: string }> } {
    const policyError = submissionScopeError(record, projectId, agentId); if (policyError) return { accepted: false, retryAllowed: false, errors: [{ pointer: policyError.pointer, code: "scope_policy", message: policyError.message }] };
    const result = this.submissionGate.submit(tool, `${attemptKey}:${tool}`, record);
    if (!result.ok) return { accepted: false, retryAllowed: result.retryAllowed, errors: result.errors };
    const agent = agentId ? this.agents.inspect().find((entry) => entry.agentId === agentId && entry.projectId === projectId) : undefined; if (agentId && !agent) return { accepted: false, retryAllowed: false, errors: [{ pointer: "/", code: "scope_policy", message: "Submitting Pi session is no longer active in this Project" }] }; if (tool === "nosh_project_contract_submit") { if (agent?.role !== "nosh") return { accepted: false, retryAllowed: false, errors: [{ pointer: "/", code: "scope_policy", message: "Only the Project Nosh session may submit the approved Project contract" }] }; const project = this.projects().find((entry) => entry.projectId === projectId)!; try { approveProjectContract(project.repositoryRoot, result.record); } catch (error) { return { accepted: false, retryAllowed: false, errors: [{ pointer: "/", code: "contract_policy", message: error instanceof Error ? error.message : "Project contract approval failed" }] }; } }
    const draft: EventDraft = {
      $schema: "https://nosh.dev/schemas/event/v1", schemaVersion: 1, retention: "persistent", type: "record.submitted", source: "pi",
      scope: { projectId, missionId: agent?.missionId ?? null, directionId: agent?.directionId ?? null, autoresearchId: agent?.autoresearchId ?? null, experimentId: agent?.experimentId ?? null, runId: agent?.runId ?? null, jobId: agent?.jobId ?? null, agentId: agent?.agentId ?? null },
      correlationId: attemptKey, causationId: null, payload: result.record as JsonValue,
    };
    const stored = this.storeFor(projectId).appendIdempotent(`record:${attemptKey}:${tool}:${sha256(result.record as JsonValue)}`, draft);
    const event = stored.receipt.event;
    if (!stored.replayed) this.events.emit("event", event);
    return { accepted: true, event };
  }

  async submitTool(tool: string, projectId: string, attemptKey: string, record: unknown, agentId?: string): Promise<unknown> { const submitted = this.submitRecord(tool, projectId, attemptKey, record, agentId); if (!submitted.accepted || tool !== "nosh_runtime_instruct") return submitted; try { return { ...submitted, result: await this.runtime.execute(record) }; } catch (error) { return { accepted: false, retryAllowed: false, errors: [{ pointer: "/", code: "runtime_instruction_failed", message: error instanceof Error ? error.message : "Runtime instruction failed" }] }; } }

  async startAgent(options: PiSessionOptions): Promise<ReturnType<PiAdapter["inspect"]>[number]> {
    if (!this.projects().some((project) => project.projectId === options.projectId)) throw new Error(`Project ${options.projectId} is not registered on this host`);
    return this.agents.start(options);
  }

  async chat(projectId: string, message: string, idempotencyKey: string, model?: { provider: string; id: string }): Promise<{ event: EventEnvelope; agentId: string; replayed: boolean }> {
    const text = message.trim(); if (!text || Buffer.byteLength(text) > 100_000) throw new Error("Chat message must contain 1 to 100000 UTF-8 bytes");
    const project = this.projects().find((entry) => entry.projectId === projectId); if (!project) throw new Error(`Project ${projectId} is not registered on this host`);
    const stored = this.storeFor(projectId).appendIdempotent(idempotencyKey, { $schema: "https://nosh.dev/schemas/event/v1", schemaVersion: 1, retention: "persistent", type: "chat.user_message", source: "user", scope: { projectId, missionId: null, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: null }, correlationId: idempotencyKey, causationId: null, payload: { message: text } });
    if (!stored.replayed) this.events.emit("event", stored.receipt.event);
    let agent = this.agents.inspect().find((entry) => entry.projectId === projectId && entry.role === "nosh"); if (agent && model && (agent.modelProvider !== model.provider || agent.modelId !== model.id)) { if (agent.status === "running") throw new Error("Stop the current Nosh turn before changing its Pi model"); this.agents.stop(agent.agentId); agent = undefined; }
    const intake = this.projectContract(projectId); let fresh = false; if (!agent) { fresh = true; agent = await this.startAgent({ projectId, missionId: null, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, taskId: null, agentId: createId("agt"), role: "nosh", cwd: project.repositoryRoot, packagePath: this.options.packagePath ?? resolve(import.meta.dirname, "..", "..", "..", "pi-package"), ...(model ? { model } : {}) }); }
    if (!stored.replayed) { const running = agent.status === "running"; const prompt = fresh && !intake.approvedAt ? `${intakePrompt(intake)}\n\nThe user's first Project-discovery response is:\n${text}` : text; const action = running ? this.agents.followUp(agent.agentId, prompt) : this.agents.prompt(agent.agentId, prompt); void action.catch((error) => this.appendDraft({ $schema: "https://nosh.dev/schemas/event/v1", schemaVersion: 1, retention: "persistent", type: "agent.failed", source: "pi", scope: { projectId, missionId: null, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: agent!.agentId }, correlationId: idempotencyKey, causationId: stored.receipt.event.eventId, payload: { message: error instanceof Error ? error.message : "Pi prompt failed" } })); }
    return { event: stored.receipt.event, agentId: agent.agentId, replayed: stored.replayed };
  }

  async steerMission(projectId: string, missionId: string, expectedVersion: number, message: string, idempotencyKey: string): Promise<void> {
    const mission = this.research.mission(projectId, missionId); const text = message.trim(); if (mission.version !== expectedVersion) throw new Error("Mission version conflict"); if (mission.state !== "running") throw new Error("Only a running Mission may be steered"); if (!text) throw new Error("Mission steering message is required"); const directors = this.agents.inspect().filter((agent) => agent.projectId === projectId && agent.missionId === missionId && ["mission_director", "research_director"].includes(agent.role)); if (!directors.length) throw new Error("Mission has no active Director to steer"); await Promise.all(directors.map((agent) => this.agents.steer(agent.agentId, text))); this.appendDraft({ $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "mission.steering_applied", source: "user", scope: { projectId, missionId, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: null }, correlationId: idempotencyKey, causationId: null, payload: { message: text, directorAgentIds: directors.map((agent) => agent.agentId) } });
  }

  async controlMission(projectId: string, missionId: string, expectedVersion: number, action: "pause" | "resume" | "stop", mode: "safe" | "checkpoint" | "immediate", idempotencyKey: string) {
    const mission = this.research.mission(projectId, missionId); if (mission.version !== expectedVersion) throw new Error("Mission version conflict"); const agents = this.agents.inspect().filter((agent) => agent.projectId === projectId && agent.missionId === missionId);
    if (action === "resume") return this.research.transitionMission(projectId, missionId, mission.version, "running", `${idempotencyKey}:running`);
    if (action === "pause") { const pausing = this.research.transitionMission(projectId, missionId, mission.version, "pausing", `${idempotencyKey}:pausing`); if (mode === "immediate") await Promise.all(agents.map((agent) => this.agents.abort(agent.agentId))); else await Promise.all(agents.map((agent) => this.agents.steer(agent.agentId, "Pause at the next safe boundary and persist a canonical handoff."))); if (mode === "checkpoint") for (const job of this.jobs.list().filter((entry) => entry.projectId === projectId)) this.jobs.checkpoint(job.jobId); return this.research.transitionMission(projectId, missionId, pausing.version, "paused", `${idempotencyKey}:paused`); }
    const stopping = this.research.transitionMission(projectId, missionId, mission.version, "stopping", `${idempotencyKey}:stopping`); for (const job of this.jobs.list().filter((entry) => entry.projectId === projectId)) this.jobs.cancel(job.jobId, "mission_stopped"); await Promise.all(agents.map((agent) => this.agents.abort(agent.agentId))); return this.research.transitionMission(projectId, missionId, stopping.version, "stopped", `${idempotencyKey}:stopped`);
  }

  startJob(spec: JobSpec): JobRecord {
    if (!this.projects().some((project) => project.projectId === spec.projectId)) throw new Error(`Project ${spec.projectId} is not registered on this host`);
    const store = this.storeFor(spec.projectId); const intent = store.beginOperation(spec.projectId, "job.launch", `job-launch:${spec.jobId}`, spec as unknown as JsonValue); const job = this.jobs.start(spec); const verified = this.jobs.get(spec.jobId); if (verified.jobId !== spec.jobId || verified.commandDigest !== job.commandDigest || verified.commitSha !== spec.commitSha || verified.evaluationContractHash !== spec.evaluationContractHash) throw new Error("Job launch verification failed"); store.completeOperation(spec.projectId, intent.intentId, verified as unknown as JsonValue); return verified;
  }

  replay(projectId: string, afterSequence: number): EventEnvelope[] {
    return this.storeFor(projectId).replay(projectId, afterSequence);
  }

  unresolvedRemoteCommands(projectId: string): Array<{ commandId: string; type: string; targetType: string; targetId: string; expectedVersion: number; acceptedAt: string }> {
    const events = this.storeFor(projectId).replay(projectId); const terminal = new Set(events.filter((event) => ["remote.command_completed", "remote.command_failed"].includes(event.type) && event.correlationId).map((event) => event.correlationId!));
    return events.filter((event) => event.type === "remote.command_accepted" && event.correlationId && !terminal.has(event.correlationId)).map((event) => { const payload = event.payload as { type?: string; targetType?: string; targetId?: string; expectedVersion?: number }; return { commandId: event.correlationId!, type: payload.type ?? "unknown", targetType: payload.targetType ?? "unknown", targetId: payload.targetId ?? "unknown", expectedVersion: payload.expectedVersion ?? 0, acceptedAt: event.timestamp }; });
  }

  resolveRemoteCommand(projectId: string, commandId: string, outcome: "applied" | "not_applied", note: string, idempotencyKey: string): EventEnvelope {
    const store = this.storeFor(projectId); const receipt = store.commandReceipt(projectId, `remote-resolution:${idempotencyKey}`); if (receipt) return receipt.event; const events = store.replay(projectId); const accepted = events.find((event) => event.type === "remote.command_accepted" && event.correlationId === commandId); if (!accepted) throw new Error("Unknown remote command"); if (events.some((event) => ["remote.command_completed", "remote.command_failed"].includes(event.type) && event.correlationId === commandId)) throw new Error("Remote command already has a terminal outcome"); if (!note.trim()) throw new Error("A local resolution note is required"); const payload = accepted.payload as { type?: string }; const stored = store.appendIdempotent(`remote-resolution:${idempotencyKey}`, { $schema: "https://nosh.dev/schemas/event/v1", schemaVersion: 1, retention: "persistent", type: outcome === "applied" ? "remote.command_completed" : "remote.command_failed", source: "local_user", scope: accepted.scope, correlationId: commandId, causationId: accepted.eventId, payload: { commandId, type: payload.type ?? "unknown", resolution: outcome, note: note.trim() } }); if (!stored.replayed) this.events.emit("event", stored.receipt.event); return stored.receipt.event;
  }

  remoteVersion(command: RemoteCommandEnvelope): number { if (command.targetType === "mission") return this.research.mission(command.projectId, command.targetId).version; if (command.targetType === "thread") return this.runtime.thread(command.projectId, command.targetId).version; return this.storeFor(command.projectId).currentSequence(command.projectId); }
  remoteReplay(command: RemoteCommandEnvelope): boolean { const store = this.storeFor(command.projectId); const receipt = store.commandReceipt(command.projectId, `remote:${command.idempotencyKey}`); if (!receipt) return false; const payload = receipt.event.payload as { envelopeHash?: string }; if (payload.envelopeHash !== sha256(command as unknown as JsonValue)) throw new Error("Idempotency key was reused for a different remote command"); const terminal = store.replay(command.projectId).find((event) => event.correlationId === command.commandId && ["remote.command_completed", "remote.command_failed"].includes(event.type)); if (!terminal) throw new Error("Remote command was accepted but its outcome is unresolved after interruption"); if (terminal.type === "remote.command_failed") throw new Error("Remote command previously failed"); return true; }

  async executeRemoteCommand(command: RemoteCommandEnvelope, payload: JsonValue): Promise<{ replayed: boolean; currentVersion: number }> {
    if (!this.projects().some((project) => project.projectId === command.projectId)) throw new Error("Remote command Project is not registered on this host");
    const scope = { projectId: command.projectId, missionId: command.targetType === "mission" ? command.targetId : null, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: command.targetType === "job" ? command.targetId : null, agentId: command.targetType === "agent" ? command.targetId : null };
    const accepted = this.storeFor(command.projectId).appendIdempotent(`remote:${command.idempotencyKey}`, { $schema: "https://nosh.dev/schemas/event/v1", schemaVersion: 1, retention: "persistent", type: "remote.command_accepted", source: command.deviceId, scope, correlationId: command.commandId, causationId: null, payload: { accountId: command.accountId, commandId: command.commandId, deviceId: command.deviceId, type: command.type, targetType: command.targetType, targetId: command.targetId, expectedVersion: command.expectedVersion, requiredPermission: command.requiredPermission, issuedAt: command.issuedAt, envelopeHash: sha256(command as unknown as JsonValue) } });
    if (accepted.replayed) return { replayed: true, currentVersion: this.remoteVersion(command) };
    this.events.emit("event", accepted.receipt.event);
    try {
      await this.dispatchRemote(command, payload);
      this.appendDraft({ $schema: "https://nosh.dev/schemas/event/v1", schemaVersion: 1, retention: "persistent", type: "remote.command_completed", source: "noshd", scope, correlationId: command.commandId, causationId: accepted.receipt.event.eventId, payload: { commandId: command.commandId, type: command.type } });
      return { replayed: false, currentVersion: this.remoteVersion(command) };
    } catch (error) {
      this.appendDraft({ $schema: "https://nosh.dev/schemas/event/v1", schemaVersion: 1, retention: "persistent", type: "remote.command_failed", source: "noshd", scope, correlationId: command.commandId, causationId: accepted.receipt.event.eventId, payload: { commandId: command.commandId, type: command.type, error: error instanceof Error ? error.message : "remote_command_failed" } });
      throw error;
    }
  }

  private appendDraft(draft: EventDraft): EventEnvelope {
    const event = this.storeFor(draft.scope.projectId).append(draft);
    this.events.emit("event", event);
    return event;
  }

  private async dispatchRemote(command: RemoteCommandEnvelope, payload: JsonValue): Promise<void> {
    if (command.targetType === "agent") {
      const agent = this.agents.inspect().find((entry) => entry.agentId === command.targetId && entry.projectId === command.projectId); if (!agent) throw new Error("Remote agent target is outside the Project or no longer active");
      if (command.type === "agent.message") await this.agents.steer(command.targetId, (payload as { message: string }).message);
      else if (command.type === "agent.safe_pause") await this.agents.steer(command.targetId, "Pause at the next safe boundary, persist a canonical handoff, and do not start another tool call.");
      else if (command.type === "agent.cancel") await this.agents.abort(command.targetId);
      return;
    }
    if (command.targetType === "job") {
      const job = this.jobs.get(command.targetId); if (job.projectId !== command.projectId) throw new Error("Remote job target is outside the Project");
      if (command.type === "job.checkpoint") this.jobs.checkpoint(command.targetId); else if (command.type === "job.cancel") this.jobs.cancel(command.targetId, "remote_cancel");
      return;
    }
    if (command.targetType === "mission") {
      const mission = this.research.mission(command.projectId, command.targetId); const agents = this.agents.inspect().filter((entry) => entry.projectId === command.projectId && entry.missionId === command.targetId);
      if (command.type === "mission.steer") { if (mission.state !== "running") throw new Error("Only a running Mission may be steered"); await Promise.all(agents.filter((entry) => ["mission_director", "research_director"].includes(entry.role)).map((entry) => this.agents.steer(entry.agentId, (payload as { message: string }).message))); }
      if (command.type === "mission.pause") { const pausing = this.research.transitionMission(command.projectId, command.targetId, mission.version, "pausing", `${command.idempotencyKey}:pausing`); const mode = (payload as { mode: string }).mode; if (mode === "immediate") await Promise.all(agents.map((entry) => this.agents.abort(entry.agentId))); else await Promise.all(agents.map((entry) => this.agents.steer(entry.agentId, "Pause at the next safe boundary and persist a canonical handoff."))); if (mode === "checkpoint") for (const job of this.jobs.list().filter((entry) => entry.projectId === command.projectId)) this.jobs.checkpoint(job.jobId); this.research.transitionMission(command.projectId, command.targetId, pausing.version, "paused", `${command.idempotencyKey}:paused`); }
      if (command.type === "mission.stop") { const stopping = this.research.transitionMission(command.projectId, command.targetId, mission.version, "stopping", `${command.idempotencyKey}:stopping`); for (const job of this.jobs.list().filter((entry) => entry.projectId === command.projectId)) this.jobs.cancel(job.jobId, "mission_stopped_remotely"); await Promise.all(agents.map((entry) => this.agents.abort(entry.agentId))); this.research.transitionMission(command.projectId, command.targetId, stopping.version, "stopped", `${command.idempotencyKey}:stopped`); }
      if (command.type === "mission.resume") { if (!agents.length) throw new Error("Mission resume requires a persisted Director session to resume"); this.research.transitionMission(command.projectId, command.targetId, mission.version, "running", `${command.idempotencyKey}:running`); }
      return;
    }
    if (command.targetType === "thread") {
      const thread = this.runtime.thread(command.projectId, command.targetId).value;
      if (thread.executionMode !== "foreground_fork") throw new Error("Only a foreground fork may be controlled remotely");
      if (command.type === "thread.message") await this.runtime.forkMessage(command.projectId, command.targetId, (payload as { message: string }).message, `remote-fork-message:${command.idempotencyKey}`);
      else if (command.type === "thread.stop") await this.runtime.execute({ $schema: schemaUri("runtime-instruction"), schemaVersion: 1, instructionId: `ins_${sha256(command.commandId).slice(7, 39)}`, projectId: command.projectId, idempotencyKey: `remote-fork-stop:${command.idempotencyKey}`, proposedByAgentId: null, issuedAt: command.issuedAt, operation: "STOP", threadId: command.targetId, programId: null, reason: "Foreground interaction completed by the remote user" });
      return;
    }
    if (command.targetType === "review") { this.appendDraft({ $schema: "https://nosh.dev/schemas/event/v1", schemaVersion: 1, retention: "persistent", type: "review.user_responded", source: command.deviceId, scope: { projectId: command.projectId, missionId: null, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: null }, correlationId: command.targetId, causationId: command.commandId, payload: { reviewId: command.targetId, ...(payload as { verdict: string; comment: string }) } }); return; }
    if (command.targetType === "artifact") { const project = this.projects().find((entry) => entry.projectId === command.projectId)!; const artifact = new ArtifactStore(join(project.repositoryRoot, ".nosh", "artifacts")).resolve(command.targetId); const maximumBytes = (payload as { maximumBytes: number }).maximumBytes; const bytes = readFileSync(artifact.storedPath); this.appendDraft({ $schema: "https://nosh.dev/schemas/event/v1", schemaVersion: 1, retention: "persistent", type: "artifact.preview_ready", source: "noshd", scope: { projectId: command.projectId, missionId: null, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: null }, correlationId: command.targetId, causationId: command.commandId, payload: { artifactId: command.targetId, mediaType: artifact.mediaType, contentHash: artifact.contentHash, bytes: Math.min(bytes.length, maximumBytes), truncated: bytes.length > maximumBytes, base64: bytes.subarray(0, maximumBytes).toString("base64") } }); return; }
    throw new Error("Unsupported remote target");
  }

  private storeFor(projectId: string): EventStore {
    const existing = this.stores.get(projectId);
    if (existing) return existing;
    const project = this.projects().find((entry) => entry.projectId === projectId);
    if (!project) throw new Error(`Project ${projectId} is not registered on this host`);
    const store = new EventStore(project.databasePath);
    this.stores.set(projectId, store);
    return store;
  }

  private reconcileProject(project: RegisteredProject): void {
    const store = this.storeFor(project.projectId); const papers = this.research.reconcilePendingOperations(project.projectId); let jobs = 0;
    for (const intent of store.operationIntents(project.projectId, "pending")) if (intent.operationType === "job.launch") { const spec = intent.request as unknown as JobSpec; const record = this.jobs.start(spec); const verified = this.jobs.get(spec.jobId); if (verified.commandDigest !== record.commandDigest || verified.commitSha !== spec.commitSha || verified.evaluationContractHash !== spec.evaluationContractHash) throw new Error("Recovered Job launch did not match its durable intent"); store.completeOperation(project.projectId, intent.intentId, verified as unknown as JsonValue); jobs += 1; }
    const runtime = store.operationIntents(project.projectId, "pending").filter((intent) => intent.operationType.startsWith("runtime.")); for (const intent of runtime) void this.runtime.execute(intent.request).catch((error) => this.appendDraft({ $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "recovery.runtime_failed", source: "noshd", scope: { projectId: project.projectId, missionId: null, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: null }, correlationId: intent.intentId, causationId: null, payload: { operationType: intent.operationType, message: error instanceof Error ? error.message : "Runtime recovery failed" } }));
    const programs = this.runtime.programStates(project.projectId).filter(({ value }) => value.status === "running"); for (const { entityId } of programs) void this.runtime.runProgram(project.projectId, entityId).catch((error) => this.appendDraft({ $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "recovery.program_failed", source: "noshd", scope: { projectId: project.projectId, missionId: null, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: null }, correlationId: entityId, causationId: null, payload: { message: error instanceof Error ? error.message : "Program recovery failed" } }));
    const unresolved = store.operationIntents(project.projectId, "pending").map((intent) => ({ intentId: intent.intentId, operationType: intent.operationType })); const unresolvedRemoteCommands = this.unresolvedRemoteCommands(project.projectId).map((command) => command.commandId); this.appendDraft({ $schema: "https://nosh.dev/schemas/event/v1", schemaVersion: 1, retention: "persistent", type: "recovery.report", source: "noshd", scope: { projectId: project.projectId, missionId: null, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: null }, correlationId: null, causationId: null, payload: { reconciledPaperOperations: papers, reconciledJobLaunches: jobs, resumingRuntimeInstructions: runtime.length, resumingPrograms: programs.length, unresolvedIntents: unresolved, unresolvedRemoteCommands } });
  }
}

function intakePrompt(contract: ProjectContract): string {
  return `This Project has a draft, unapproved Project contract. Conduct a BMAD-inspired facilitated discovery conversation: draw the user's thinking out instead of inventing the project for them; ask exactly one focused question per turn; follow useful answers with sharper questions; challenge vague, contradictory, unfalsifiable, or overly broad claims; and periodically reflect the shared understanding in plain language. Establish both the letter and spirit of the research: problem and motivation, north-star question, decision use, defensible contribution and novelty, included and excluded scope, datasets and licenses, baselines, evaluation and falsification criteria, reproducibility, risks and policies, intended paper outcome, and only then realistic GPU-hours, disk, and hardware limits. Inspect an existing repository read-only when that would improve a question. Do not start a Mission, Direction, Autoresearch execution, job, or implementation during discovery. When the understanding is complete, present one concise proposed contract and ask for explicit approval. Only after an unambiguous user approval, submit the complete next-version ${schemaUri("project-contract")} record through nosh_project_contract_submit. Preserve projectId=${contract.projectId}, createdAt=${contract.createdAt}, createdBy=${contract.createdBy}, and canonicalDefaultBranch=${contract.canonicalDefaultBranch}; use contractVersion=${contract.contractVersion + 1}, a real approval timestamp, and the user's agreed values. Begin now with the single most important question. Current draft: ${JSON.stringify(contract)}`;
}

function submissionScopeError(record: unknown, projectId: string, agentId?: string): { pointer: string; message: string } | null {
  if (!record || typeof record !== "object" || Array.isArray(record)) return null; const value = record as Record<string, unknown>; if (typeof value.projectId === "string" && value.projectId !== projectId) return { pointer: "/projectId", message: "Record Project does not match the daemon-bound session" }; const scope = value.scope; if (scope && typeof scope === "object" && !Array.isArray(scope) && typeof (scope as Record<string, unknown>).projectId === "string" && (scope as Record<string, unknown>).projectId !== projectId) return { pointer: "/scope/projectId", message: "Record scope does not match the daemon-bound session" };
  if (!agentId) return null; const schema = typeof value.$schema === "string" ? value.$schema : ""; const actorBySchema: Record<string, string> = { ["https://nosh.dev/schemas/response-envelope/v1"]: "agentId", ["https://nosh.dev/schemas/task-acknowledgement/v1"]: "agentId", ["https://nosh.dev/schemas/delegation-request/v1"]: "requestingAgentId", ["https://nosh.dev/schemas/review-verdict/v1"]: "reviewerAgentId", ["https://nosh.dev/schemas/experiment-proposal/v1"]: "proposedByAgentId", ["https://nosh.dev/schemas/evidence/v1"]: "createdByAgentId", ["https://nosh.dev/schemas/mission-director-cycle/v1"]: "directorAgentId", ["https://nosh.dev/schemas/research-director-cycle/v1"]: "directorAgentId", ["https://nosh.dev/schemas/runtime-instruction/v1"]: "proposedByAgentId" }; const field = actorBySchema[schema]; if (field && value[field] !== agentId) return { pointer: `/${field}`, message: "Record actor does not match the daemon-bound Pi session" }; return null;
}
