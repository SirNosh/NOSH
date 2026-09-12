import { EventEmitter } from "node:events";
import { existsSync, mkdirSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { timingSafeEqual } from "node:crypto";
import { createId } from "@nosh/core";
import { EventStore, HostRegistry, applyScheduledRestores, createProjectBackup, listProjectBackups, scheduleProjectRestore, type RegisteredProject } from "@nosh/persistence";
import { StructuredSubmissionGate } from "@nosh/agent-runtime";
import { PiAdapter, terminalSchemaTools, type TerminalContext, type TerminalReceipt, type PiSessionOptions } from "@nosh/pi-adapter";
import { OrchestrationRuntime } from "@nosh/orchestration-runtime";
import { JobSupervisor, type JobRecord, type JobSpec } from "@nosh/jobs";
import { approveProjectContract, ArtifactStore, initializeResearchProject, readProjectContract, type ProjectContract, type ProjectInitialization } from "@nosh/evidence";
import { canonicalJson, validateRecord, episodeDraftSchema, isTaskTerminalRecord, schemaUri, sha256, type EventDraft, type EventEnvelope, type JsonValue, type ModelSelection, type RemoteCommandEnvelope } from "@nosh/wire";
import { SingleInstanceLock } from "./single-instance.js";
import { ResearchControl } from "./research-control.js";
import { MissionSupervisor } from "./mission-supervisor.js";
import { DirectionSupervisor } from "./direction-supervisor.js";
import { AutoresearchSupervisor } from "./autoresearch-supervisor.js";

export type NoshDaemonOptions = {
  dataDirectory: string;
  bootstrapToken: string;
  packagePath?: string;
  maximumActivePiSessions?: number;
};

export class NoshDaemon {
  private readonly stores = new Map<string, EventStore>();
  private readonly lock: SingleInstanceLock;
  private readonly submissionGate = new StructuredSubmissionGate();
  private readonly maintenanceProjects = new Set<string>();
  private maintenanceInspectionProject: string | null = null;
  private started = false;
  private readonly delegationRuns = new Set<string>();
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
    this.research = new ResearchControl((projectId) => this.storeFor(projectId), (projectId) => { const project = this.projects().find((entry) => entry.projectId === projectId); if (!project) throw new Error(`Project ${projectId} is not registered on this host`); return project; }, (event) => this.events.emit("event", event), (projectId) => this.assertProjectWritable(projectId));
    this.agents = new PiAdapter((event) => this.appendDraft(event), (tool, projectId, attemptKey, record, agentId) => this.submitTool(tool, projectId, attemptKey, record, agentId), undefined, { admit: (context) => this.admitTerminal(context), submit: (context, records) => this.submitTerminal(context, records), reject: (context, reason) => this.rejectTerminal(context, reason) });
    const packagePath = options.packagePath ?? resolve(import.meta.dirname, "..", "..", "..", "pi-package");
    this.runtime = new OrchestrationRuntime({ storeFor: (projectId) => { this.assertProjectWritable(projectId); return this.storeFor(projectId); }, projectFor: (projectId) => { const project = this.projects().find((entry) => entry.projectId === projectId); if (!project) throw new Error(`Project ${projectId} is not registered on this host`); return project; }, sessions: this.agents, packagePath, publish: (event) => this.events.emit("event", event) });
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
    try {
      applyScheduledRestores(this.options.dataDirectory);
      this.jobs.recover();
      for (const project of this.projects()) this.reconcileProject(project);
      this.missions.start();
      this.directions.start();
      this.experiments.start();
      this.started = true;
    } catch (error) {
      this.lock.release();
      throw error;
    }
  }

  stop(): void {
    this.started = false;
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
    this.assertProjectWritable(project.projectId);
    const registered = this.registry.register(project); this.reconcileProject(registered); return registered;
  }
  initializeProject(input: Omit<ProjectInitialization, "dataDirectory">): RegisteredProject {
    const metadataPath = join(resolve(input.path), ".nosh", "project.json");
    if (existsSync(metadataPath)) {
      const existingProjectId = (JSON.parse(readFileSync(metadataPath, "utf8")) as { projectId?: unknown }).projectId;
      if (typeof existingProjectId === "string") this.assertProjectWritable(existingProjectId);
    }
    return this.registerProject(initializeResearchProject({ ...input, dataDirectory: this.options.dataDirectory }));
  }

  projectContract(projectId: string): ProjectContract { const project = this.projects().find((entry) => entry.projectId === projectId); if (!project) throw new Error(`Project ${projectId} is not registered on this host`); return readProjectContract(project.repositoryRoot); }
  notificationAcks(projectId: string): string[] { return [...new Set(this.storeFor(projectId).replay(projectId).filter((event) => event.type === "notification.acknowledged").flatMap((event) => typeof event.payload === "object" && event.payload !== null && !Array.isArray(event.payload) && Array.isArray(event.payload.eventIds) ? event.payload.eventIds.filter((id): id is string => typeof id === "string") : []))]; }
  acknowledgeNotifications(projectId: string, eventIds: string[], idempotencyKey: string): EventEnvelope {
    this.assertProjectWritable(projectId);
    if (!eventIds.length || new Set(eventIds).size !== eventIds.length) throw new Error("Notification acknowledgement requires unique event IDs");
    const canonicalEventIds = [...eventIds].sort();
    const store = this.storeFor(projectId); const prior = store.commandReceipt(projectId, idempotencyKey);
    if (prior) {
      const payload = prior.event.payload;
      const priorIds = payload && typeof payload === "object" && !Array.isArray(payload) && Array.isArray(payload.eventIds) ? payload.eventIds : null;
      if (prior.event.type !== "notification.acknowledged" || prior.event.source !== "noshd" || prior.event.scope.projectId !== projectId || !priorIds || JSON.stringify(priorIds) !== JSON.stringify(canonicalEventIds)) throw new Error("Notification acknowledgement idempotency key was reused for a different Project or event set");
      return prior.event;
    }
    const known = new Set(store.replay(projectId).map((event) => event.eventId));
    if (canonicalEventIds.some((eventId) => !known.has(eventId))) throw new Error("Notification acknowledgement references an unknown Project event");
    const stored = store.appendIdempotent(idempotencyKey, { $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "notification.acknowledged", source: "noshd", scope: { projectId, missionId: null, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: null }, correlationId: null, causationId: null, payload: { eventIds: canonicalEventIds } });
    if (!stored.replayed) this.events.emit("event", stored.receipt.event);
    return stored.receipt.event;
  }

  async beginProjectIntake(projectId: string, selection?: ModelSelection): Promise<void> {
    const project = this.projects().find((entry) => entry.projectId === projectId); if (!project) throw new Error(`Project ${projectId} is not registered on this host`); await this.validateModelSelection(selection); const contract = this.projectContract(projectId); if (contract.approvedAt) return;
    const agent = await this.startAgent({ projectId, missionId: null, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, taskId: null, agentId: createId("agt"), role: "nosh", cwd: project.repositoryRoot, packagePath: this.options.packagePath ?? resolve(import.meta.dirname, "..", "..", "..", "pi-package"), ...(selection ? { model: { provider: selection.provider, id: selection.id }, ...(selection.thinkingLevel === undefined ? {} : { thinkingLevel: selection.thinkingLevel }) } : {}) });
    await this.agents.prompt(agent.agentId, intakePrompt(contract));
  }

  projects(): RegisteredProject[] {
    return this.registry.list();
  }

  backups(projectId: string) {
    if (!this.projects().some((project) => project.projectId === projectId)) throw new Error("Project is not registered on this host");
    return listProjectBackups(this.options.dataDirectory, projectId);
  }

  assertProjectWritable(projectId: string): void {
    if (this.maintenanceProjects.has(projectId) && this.maintenanceInspectionProject !== projectId) throw new Error("Project is in backup maintenance mode");
  }
  backupMaintenanceInProgress(): boolean { return this.maintenanceProjects.size > 0; }

  async createBackup(projectId: string) {
    const project = this.projects().find((entry) => entry.projectId === projectId);
    if (!project) throw new Error("Project is not registered on this host");
    this.assertProjectWritable(projectId);
    this.maintenanceProjects.add(projectId);
    this.missions.stop();
    this.directions.stop();
    this.experiments.stop();
    try {
      const active = this.backupActivityReason(projectId);
      if (active) throw new Error(active);
      return await createProjectBackup(this.options.dataDirectory, project);
    } finally {
      this.maintenanceProjects.delete(projectId);
      if (this.started && !this.maintenanceProjects.size) {
        this.missions.start();
        this.directions.start();
        this.experiments.start();
      }
    }
  }

  scheduleBackupRestore(projectId: string, backupId: string): void {
    const project = this.projects().find((entry) => entry.projectId === projectId);
    if (!project) throw new Error("Project is not registered on this host");
    const active = this.backupActivityReason(projectId);
    if (active) throw new Error(`Restore rejected: ${active.slice("Backup rejected: ".length)}`);
    this.assertProjectWritable(projectId);
    scheduleProjectRestore(this.options.dataDirectory, project, backupId);
  }


  private readonly terminalTurns = new Map<string, Promise<unknown>>();

  private async withTerminalTurn(context: TerminalContext, action: () => Promise<TerminalReceipt>): Promise<TerminalReceipt> {
    const key = `${context.projectId}:${context.turnId}`;
    const prior = this.terminalTurns.get(key) ?? Promise.resolve();
    const current = prior.catch(() => undefined).then(action);
    this.terminalTurns.set(key, current);
    try { return await current; } finally { if (this.terminalTurns.get(key) === current) this.terminalTurns.delete(key); }
  }

  async admitTerminal(context: TerminalContext): Promise<TerminalReceipt | undefined> {
    let frozen: Record<string, JsonValue>[] | undefined;
    const result = await this.withTerminalTurn(context, async () => {
      this.assertProjectWritable(context.projectId);
      const store = this.storeFor(context.projectId);
      const journal = store.terminalTurn(context.projectId, context.turnId, context as unknown as JsonValue);
      if (journal.records) { frozen = journal.records as Record<string, JsonValue>[]; return { accepted: false, retryAllowed: false }; }
      // A generation without a durable rejection may have executed tools before a crash.
      // Do not start a replacement generation for that uncertain turn.
      if (journal.closed || (journal.generations ?? 0) >= 2 || (journal.generations ?? 0) > journal.failures) {
        journal.closed = true; journal.status = "rejected";
        store.saveTerminalTurn(context.projectId, context.turnId, journal);
        return { accepted: false, retryAllowed: false, status: "rejected", error: "Terminal generation budget exhausted" };
      }
      journal.generations = (journal.generations ?? 0) + 1;
      store.saveTerminalTurn(context.projectId, context.turnId, journal);
      return { accepted: false, retryAllowed: true, admitted: true };
    });
    if (frozen) return this.submitTerminal(context, frozen);
    return result.admitted ? undefined : result;
  }

  async rejectTerminal(context: TerminalContext, reason: string): Promise<TerminalReceipt> {
    return this.withTerminalTurn(context, async () => this.rejectTerminalAttempt(context, reason));
  }

  private rejectTerminalAttempt(context: TerminalContext, reason: string): TerminalReceipt {
    this.assertProjectWritable(context.projectId);
    const store = this.storeFor(context.projectId);
    const receipt = store.terminalTurn(context.projectId, context.turnId, context as unknown as JsonValue);
    if (!receipt.closed && !receipt.records) {
      receipt.failures += 1;
      receipt.closed = receipt.failures >= 2 || reason.startsWith("Non-successful provider stop:") || reason.startsWith("Terminal policy:");
      receipt.status = receipt.closed ? "rejected" : "open";
      store.saveTerminalTurn(context.projectId, context.turnId, receipt);
    }
    return { accepted: false, retryAllowed: !receipt.closed && !receipt.records, status: receipt.status, error: reason };
  }

  async submitTerminal(context: TerminalContext, records: Record<string, JsonValue>[]): Promise<TerminalReceipt> {
    return this.withTerminalTurn(context, async () => {
      this.assertProjectWritable(context.projectId);
      const store = this.storeFor(context.projectId);
      const journal = store.terminalTurn(context.projectId, context.turnId, context as unknown as JsonValue);
      if (journal.records && canonicalJson(journal.records) !== canonicalJson(records)) return { accepted: false, retryAllowed: false, status: journal.status, error: "Terminal envelope replay conflict" };
      if (journal.closed && !journal.records) return { accepted: false, retryAllowed: false, status: journal.status, error: "Terminal correction budget exhausted" };
      const receipt = (): TerminalReceipt => ({ accepted: journal.status === "completed", retryAllowed: false, status: journal.status, results: journal.results, effect: { state: journal.results.some((item) => isObject(item) && isObject(item.effect) && item.effect.state === "failed") ? "failed" : journal.status === "completed" ? "completed" : "pending" } });
      if (journal.closed) return receipt();
      if (!journal.records) {
        try { this.validateTerminal(context, records); }
        catch (error) { const message = error instanceof Error ? error.message : "Terminal validation failed"; const correctable = message.startsWith("Invalid terminal record:") || message.startsWith("Unexpected terminal episode type") || message.startsWith("Episode references") || message.startsWith("Changed file escapes") || message.startsWith("Terminal output requires"); return this.rejectTerminalAttempt(context, correctable ? message : `Terminal policy: ${message}`); }
        // Freeze the complete validated intent before ANY record acceptance or effect.
        journal.records = structuredClone(records);
        journal.status = "pending";
        store.saveTerminalTurn(context.projectId, context.turnId, journal);
      }
      for (let index = journal.results.length; index < records.length; index++) {
        const record = journal.records[index] as Record<string, JsonValue>;
        const tool = terminalSchemaTools[String(record.$schema)]!;
        // Task terminal authority and supervisor lookup require the exact task correlation.
        const attempt = tool === "nosh_episode_submit" && context.instructionId ? `instruction:${context.instructionId}` : context.taskId ? `task:${context.taskId}` : `terminal:${context.turnId}:${index}`;
        try {
          // These seven terminal schemas have no submitTool domain/runtime effects.
          // Recover the append/journal crash window before reauthorizing a now-stale session.
          const acceptedEvent = store.replay(context.projectId).find((event) => event.type === "record.submitted" && event.scope.projectId === context.projectId && event.scope.agentId === context.agentId && event.correlationId === attempt && canonicalJson(event.payload) === canonicalJson(record));
          const result = acceptedEvent ? { accepted: true, event: acceptedEvent } as unknown as JsonValue
            : !this.agents.terminalTurnActive(context) ? { accepted: false, retryAllowed: false, error: "Terminal session is cancelled or inactive" }
            : await this.submitTool(tool, context.projectId, attempt, record, context.agentId) as JsonValue;
          journal.results.push(result);
          if (!isObject(result) || result.accepted !== true) {
            journal.status = journal.results.some((item) => isObject(item) && item.accepted === true) ? "partial" : "rejected";
            journal.closed = true;
            store.saveTerminalTurn(context.projectId, context.turnId, journal);
            return receipt();
          }
          journal.status = index + 1 === records.length ? "completed" : "partial";
          journal.closed = journal.status === "completed";
          store.saveTerminalTurn(context.projectId, context.turnId, journal);
        } catch (error) {
          // The accepted prefix is resumable; no rewritten correction is safe now.
          journal.status = journal.results.length ? "partial" : "pending";
          store.saveTerminalTurn(context.projectId, context.turnId, journal);
          return { ...receipt(), error: error instanceof Error ? error.message : "Terminal effect interrupted" };
        }
      }
      return receipt();
    });
  }

  private validateTerminal(context: TerminalContext, records: Record<string, JsonValue>[]): void {
    const agent = this.agents.inspect().find((entry) => entry.agentId === context.agentId && entry.projectId === context.projectId);
    if (!agent || agent.taskId !== context.taskId) throw new Error("Terminal output requires its active daemon-bound session");
    const store = this.storeFor(context.projectId);
    if (context.threadId) {
      const thread = store.projection<Record<string, JsonValue>>(context.projectId, "execution_thread", context.threadId);
      if (!thread || thread.version !== context.expectedVersion || thread.value.activeInstructionId !== context.instructionId || thread.value.currentAgentId !== context.agentId || thread.value.state !== "running") throw new Error("Stale terminal instruction or thread version");
    } else if (context.instructionId !== null || context.expectedVersion !== null) throw new Error("Incomplete terminal version scope");
    if (records.length < 1 || records.length > 2) throw new Error("Terminal output requires one or two records");
    let terminals = 0; let episodes = 0;
    const roles: Record<string, string> = { [schemaUri("general-worker-completion")]: "general_worker", [schemaUri("librarian-completion")]: "librarian_researcher", [schemaUri("review-verdict")]: "reviewer", [schemaUri("mission-director-cycle")]: "mission_director", [schemaUri("research-director-cycle")]: "research_director" };
    for (const record of records) {
      const schema = String(record.$schema); const tool = terminalSchemaTools[schema];
      if (!tool || !context.allowedTools.includes(tool)) throw new Error("Terminal schema is outside the host tool contract");
      const parsed = validateRecord(schema, record);
      if (!parsed.ok) throw new Error(`Invalid terminal record: ${JSON.stringify(parsed.errors)}`);
      const scopeError = submissionScopeError(record, context.projectId, context.agentId);
      if (scopeError) throw new Error(scopeError.message);
      if (roles[schema] && roles[schema] !== agent.role) throw new Error("Terminal record role does not match its session");
      if (record.taskId !== undefined && record.taskId !== context.taskId) throw new Error("Terminal task scope mismatch");
      for (const field of ["missionId", "directionId", "autoresearchId", "experimentId", "runId", "jobId"] as const) {
        if (record[field] !== undefined && record[field] !== agent[field]) throw new Error(`Terminal ${field} scope mismatch`);
        if (isObject(record.scope) && record.scope[field] !== undefined && record.scope[field] !== agent[field]) throw new Error(`Terminal nested ${field} scope mismatch`);
      }
      if (schema === schemaUri("episode-draft")) {
        episodes++;
        if (!context.expectedEpisodeType || record.episodeType !== context.expectedEpisodeType) throw new Error("Unexpected terminal episode type");
        this.runtime.validateDraft(context.projectId, episodeDraftSchema.parse(record));
      } else {
        terminals++;
        if (!context.taskId) throw new Error("Task terminal output requires a daemon-bound task");
        const prior = this.research.terminalRecord(context.projectId, context.taskId);
        if (prior && canonicalJson(prior) !== canonicalJson(record)) throw new Error("Conflicting task terminal replay");
      }
      this.research.validateDomainRecord(context.projectId, agent, record);
      this.research.canonicalDomainRecord(context.projectId, record);
    }
    if (terminals > 1 || episodes > 1 || (context.expectedEpisodeType && episodes !== 1) || (context.taskId && !context.instructionId && terminals !== 1)) throw new Error("Terminal output requires unique task result and expected episode");
  }

  submitRecord(tool: string, projectId: string, attemptKey: string, record: unknown, agentId?: string):
    | { accepted: true; event: EventEnvelope; effect?: { state: "failed"; error: string } }
    | { accepted: false; retryAllowed: boolean; errors: Array<{ pointer: string; code: string; message: string }> } {
    this.assertProjectWritable(projectId);
    const policyError = submissionScopeError(record, projectId, agentId); if (policyError) return { accepted: false, retryAllowed: false, errors: [{ pointer: policyError.pointer, code: "scope_policy", message: policyError.message }] };
    const result = this.submissionGate.submit(tool, `${attemptKey}:${tool}`, record);
    if (!result.ok) return { accepted: false, retryAllowed: result.retryAllowed, errors: result.errors };
    const agent = agentId ? this.agents.inspect().find((entry) => entry.agentId === agentId && entry.projectId === projectId) : undefined; if (agentId && !agent) return { accepted: false, retryAllowed: false, errors: [{ pointer: "/", code: "scope_policy", message: "Submitting Pi session is no longer active in this Project" }] }; if (tool === "nosh_project_contract_submit") { if (agent?.role !== "nosh") return { accepted: false, retryAllowed: false, errors: [{ pointer: "/", code: "scope_policy", message: "Only the Project Nosh session may submit the approved Project contract" }] }; const project = this.projects().find((entry) => entry.projectId === projectId)!; try { approveProjectContract(project.repositoryRoot, result.record); } catch (error) { return { accepted: false, retryAllowed: false, errors: [{ pointer: "/", code: "contract_policy", message: error instanceof Error ? error.message : "Project contract approval failed" }] }; } }
    if (this.research.isDomainEffectRecord(result.record as JsonValue) && !agent) return { accepted: false, retryAllowed: false, errors: [{ pointer: "/", code: "domain_policy", message: "Domain-effect records require an active matching Pi agent" }] };
    if (agent) {
      try { this.research.validateDomainRecord(projectId, agent, result.record as JsonValue); } catch (error) {
        return { accepted: false, retryAllowed: false, errors: [{ pointer: "/", code: "domain_policy", message: error instanceof Error ? error.message : "Domain record violates task authority" }] };
      }
    }
    let canonical: { event: EventEnvelope } | null = null;
    try {
      canonical = this.research.canonicalDomainRecord(projectId, result.record as JsonValue);
    } catch (error) {
      return { accepted: false, retryAllowed: false, errors: [{ pointer: "/", code: "domain_policy", message: error instanceof Error ? error.message : "Domain record conflicts with an accepted canonical payload" }] };
    }
    const draft: EventDraft = {
      $schema: "https://nosh.dev/schemas/event/v1", schemaVersion: 1, retention: "persistent", type: "record.submitted", source: "pi",
      scope: { projectId, missionId: agent?.missionId ?? null, directionId: agent?.directionId ?? null, autoresearchId: agent?.autoresearchId ?? null, experimentId: agent?.experimentId ?? null, runId: agent?.runId ?? null, jobId: agent?.jobId ?? null, agentId: agent?.agentId ?? null },
      correlationId: attemptKey, causationId: null, payload: result.record as JsonValue,
    };
    const schema = String((result.record as { $schema?: unknown }).$schema ?? "");
    const terminalTaskId = terminalOutcomeTaskId(tool, schema, attemptKey, result.record);
    let stored: { receipt: { event: EventEnvelope }; replayed: boolean };
    try {
      stored = canonical
        ? { receipt: { event: canonical.event }, replayed: true }
        : terminalTaskId
          ? this.storeFor(projectId).appendTerminalSubmission(terminalTaskId, tool, schema, result.record as JsonValue, draft)
          : this.storeFor(projectId).appendIdempotent(`record:${attemptKey}:${tool}:${sha256(result.record as JsonValue)}`, draft);
    } catch (error) {
      return { accepted: false, retryAllowed: false, errors: [{ pointer: "/", code: "terminal_conflict", message: error instanceof Error ? error.message : "Terminal submission conflicts with an accepted record" }] };
    }
    const event = stored.receipt.event;
    if (!stored.replayed) this.events.emit("event", event);
    if (agent) {
      try { this.research.applyDomainEffect(projectId, agent, result.record as JsonValue); }
      catch (error) { return { accepted: true, event, effect: { state: "failed", error: error instanceof Error ? error.message : "Domain effect failed after record acceptance" } }; }
    }
    return { accepted: true, event };
  }

  async submitTool(tool: string, projectId: string, attemptKey: string, record: unknown, agentId?: string): Promise<unknown> {
    const submitted = this.submitRecord(tool, projectId, attemptKey, record, agentId);
    if (!submitted.accepted) return submitted;
    if (tool === "nosh_delegation_request" && record && typeof record === "object" && !Array.isArray(record)) {
      try { return { ...submitted, delegation: await this.authorizeDelegation(projectId, record as Record<string, JsonValue>) }; }
      catch (error) { return { ...submitted, delegation: { state: "failed", error: error instanceof Error ? error.message : "Delegation authorization failed" } }; }
    }
    if (tool !== "nosh_runtime_instruct") return submitted;
    const request = { instruction: record as JsonValue };
    const operation = this.storeFor(projectId).beginOperation(projectId, "runtime.instruction", `tool-effect:${attemptKey}:${tool}`, request);
    if (operation.state === "completed" || operation.state === "failed") return { ...submitted, effect: { state: operation.state, ...(operation.result === null ? { error: operation.error } : { result: operation.result }) } };
    try {
      const result = await this.executeRuntimeEffect(operation.operationType, request);
      this.storeFor(projectId).completeOperation(projectId, operation.intentId, result);
      return { ...submitted, effect: { state: "completed", result } };
    } catch (error) {
      const message = error instanceof Error ? error.message : "Runtime effect failed";
      this.storeFor(projectId).failOperation(projectId, operation.intentId, message);
      return { ...submitted, effect: { state: "failed", error: message } };
    }
  }


  private async executeRuntimeEffect(operationType: string, request: JsonValue): Promise<JsonValue> {
    if (!request || typeof request !== "object" || Array.isArray(request)) throw new Error("Runtime effect request is invalid");
    const value = request as Record<string, JsonValue>;
    if (operationType === "runtime.instruction") return this.runtime.execute(value.instruction);
    throw new Error(`Unsupported runtime effect ${operationType}`);
  }

  async startAgent(options: PiSessionOptions): Promise<ReturnType<PiAdapter["inspect"]>[number]> {
    if (!this.projects().some((project) => project.projectId === options.projectId)) throw new Error(`Project ${options.projectId} is not registered on this host`);
    this.assertProjectWritable(options.projectId);
    return this.agents.start(options);
  }

  async validateModelSelection(selection?: ModelSelection): Promise<void> {
    await this.agents.validateModelSelection(selection);
  }

  async chat(projectId: string, message: string, idempotencyKey: string, selection?: ModelSelection): Promise<{ event: EventEnvelope; agentId: string; replayed: boolean }> {
    const text = message.trim(); if (!text || Buffer.byteLength(text) > 100_000) throw new Error("Chat message must contain 1 to 100000 UTF-8 bytes");
    const project = this.projects().find((entry) => entry.projectId === projectId); if (!project) throw new Error(`Project ${projectId} is not registered on this host`);
    this.assertProjectWritable(projectId);
    const store = this.storeFor(projectId); const prior = store.commandReceipt(projectId, idempotencyKey);
    if (prior) {
      const payload = prior.event.payload;
      if (prior.event.type !== "chat.user_message" || prior.event.source !== "user" || prior.event.scope.projectId !== projectId || !payload || typeof payload !== "object" || Array.isArray(payload) || payload.message !== text || typeof payload.agentId !== "string" || canonicalJson(payload.selection ?? null) !== canonicalJson(chatSelection(selection))) throw new Error("Chat idempotency key was reused for a different Project, message, or model selection");
      return { event: prior.event, agentId: payload.agentId, replayed: true };
    }
    await this.validateModelSelection(selection);
    let agent = this.agents.inspect().find((entry) => entry.projectId === projectId && entry.role === "nosh");
    if (agent && selection) {
      const sameModel = agent.modelProvider === selection.provider && agent.modelId === selection.id;
      if (!sameModel) { if (agent.status === "running") throw new Error("Stop the current Nosh turn before changing its Pi model"); this.agents.stop(agent.agentId); agent = undefined; }
      else if (selection.thinkingLevel !== undefined && selection.thinkingLevel !== agent.thinkingLevel) this.agents.setThinkingLevel(agent.agentId, selection.thinkingLevel);
    }
    const fresh = !agent;
    if (!agent) agent = await this.startAgent({ projectId, missionId: null, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, taskId: null, agentId: deterministicChatAgentId(projectId), role: "nosh", cwd: project.repositoryRoot, packagePath: this.options.packagePath ?? resolve(import.meta.dirname, "..", "..", "..", "pi-package"), ...(selection ? { model: { provider: selection.provider, id: selection.id }, ...(selection.thinkingLevel === undefined ? {} : { thinkingLevel: selection.thinkingLevel }) } : {}) });
    const stored = store.appendIdempotent(idempotencyKey, { $schema: "https://nosh.dev/schemas/event/v1", schemaVersion: 1, retention: "persistent", type: "chat.user_message", source: "user", scope: { projectId, missionId: null, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: null }, correlationId: idempotencyKey, causationId: null, payload: { message: text, selection: chatSelection(selection), agentId: agent.agentId } });
    if (stored.replayed) return { event: stored.receipt.event, agentId: agent.agentId, replayed: true };
    this.events.emit("event", stored.receipt.event);
    const intake = this.projectContract(projectId);
    const prompt = fresh && !intake.approvedAt ? `${intakePrompt(intake)}\n\nThe user's first Project-discovery response is:\n${text}` : text;
    const action = agent.status === "running" ? this.agents.followUp(agent.agentId, prompt) : this.agents.prompt(agent.agentId, prompt);
    void action.catch((error) => this.appendDraft({ $schema: "https://nosh.dev/schemas/event/v1", schemaVersion: 1, retention: "persistent", type: "agent.failed", source: "pi", scope: { projectId, missionId: null, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: agent!.agentId }, correlationId: idempotencyKey, causationId: stored.receipt.event.eventId, payload: { message: error instanceof Error ? error.message : "Chat agent failed" } }));
    return { event: stored.receipt.event, agentId: agent.agentId, replayed: false };
  }
  private authorizeDelegation(projectId: string, record: Record<string, JsonValue>): JsonValue {
    this.assertProjectWritable(projectId);
    if (record.$schema !== schemaUri("delegation-request") || typeof record.requestId !== "string" || typeof record.requestingTaskId !== "string" || typeof record.requestingAgentId !== "string" || typeof record.requestedRole !== "string" || typeof record.proposedObjective !== "string" || !record.proposedObjective.trim() || typeof record.submittedAt !== "string" || !record.budgetEstimate || typeof record.budgetEstimate !== "object" || Array.isArray(record.budgetEstimate)) throw new Error("Delegation request is malformed");
    const budget = record.budgetEstimate as Record<string, JsonValue>;
    if (typeof budget.modelTokens !== "number" || !Number.isInteger(budget.modelTokens) || typeof budget.wallClockSeconds !== "number" || !Number.isInteger(budget.wallClockSeconds) || budget.modelTokens <= 0 || budget.wallClockSeconds <= 0 || !["librarian_researcher", "general_worker", "reviewer"].includes(record.requestedRole)) throw new Error("Delegation request budget or role is invalid");
    const store = this.storeFor(projectId); const requestId = record.requestId; const requestHash = sha256(record);
    const existing = store.projection<JsonValue>(projectId, "delegation", requestId);
    if (existing) {
      const value = isDelegationValue(existing.value);
      if (value.requestHash !== requestHash) throw new Error("Delegation request ID already has different durable authority");
      if (existing.state === "rejected") return assertedRejectedDelegation(value, requestHash);
      if (existing.state === "completed" || existing.state === "failed") return assertedDelegationTerminal(value, existing.state);
      if (existing.state !== "authorized") throw new Error(`Delegation request has unexpected ${existing.state} durable authority`);
      this.resumeDelegation(projectId, requestId);
      return value;
    }
    const packets = this.research.submitted(projectId, `task:${record.requestingTaskId}`).map((entry) => entry.record).filter((entry): entry is Record<string, JsonValue> => typeof entry === "object" && entry !== null && !Array.isArray(entry) && entry.$schema === schemaUri("task-packet") && entry.taskId === record.requestingTaskId);
    if (packets.length !== 1) return this.rejectDelegation(projectId, record, requestHash, "Delegation request does not bind exactly one durable parent Task Packet");
    const parent = packets[0]!; const permissions = parent.permissions; const parentBudget = parent.budget; const parentScope = parent.scope;
    if (!permissions || typeof permissions !== "object" || Array.isArray(permissions) || permissions.delegation !== "request_only" || !parentBudget || typeof parentBudget !== "object" || Array.isArray(parentBudget) || !parentScope || typeof parentScope !== "object" || Array.isArray(parentScope) || parent.assignedAgentId !== record.requestingAgentId || Number(budget.modelTokens) > Number((parentBudget as Record<string, JsonValue>).maximumModelTokens) || Number(budget.wallClockSeconds) > Number((parentBudget as Record<string, JsonValue>).maximumWallClockSeconds)) return this.rejectDelegation(projectId, record, requestHash, "Delegation request exceeds durable parent Task Packet authority");
    const scope = parentScope as Record<string, JsonValue>;
    try { delegationEventScope(projectId, scope, record.requestingAgentId); this.assertDelegationParentLease(projectId, parent, scope, record.requestingAgentId); } catch (error) { return this.rejectDelegation(projectId, record, requestHash, error instanceof Error ? error.message : "Delegation parent authority is invalid", scope); }
    const missionId = nullableId(scope.missionId);
    const priorReservations = store.operationIntents(projectId).flatMap((intent) => intent.operationType === "delegation.reservation" && (intent.state === "pending" || intent.state === "completed") ? [delegationReservation(intent.request)] : []).filter((reservation): reservation is DelegationReservation => reservation !== null && reservation.parentTaskId === record.requestingTaskId && reservation.requestId !== requestId);
    const reservedTokens = priorReservations.reduce((total, reservation) => total + reservation.modelTokens, 0);
    const reservedSeconds = priorReservations.reduce((total, reservation) => total + reservation.wallClockSeconds, 0);
    if (reservedTokens + Number(budget.modelTokens) > Number((parentBudget as Record<string, JsonValue>).maximumModelTokens) || reservedSeconds + Number(budget.wallClockSeconds) > Number((parentBudget as Record<string, JsonValue>).maximumWallClockSeconds)) return this.rejectDelegation(projectId, record, requestHash, "Delegation siblings exceed the durable parent Task Packet budget", scope);
    if (missionId) {
      const mission = this.research.mission(projectId, missionId);
      const use = this.research.missionBudgetUse(projectId, missionId);
      const pendingMissionTokens = store.operationIntents(projectId, "pending").flatMap((intent) => intent.operationType === "delegation.reservation" ? [delegationReservation(intent.request)] : []).filter((reservation): reservation is DelegationReservation => reservation !== null && reservation.missionId === missionId).reduce((total, reservation) => total + reservation.modelTokens, 0);
      if (use.modelTokens + pendingMissionTokens + Number(budget.modelTokens) > mission.value.budgets.maximumModelTokens) return this.rejectDelegation(projectId, record, requestHash, "Delegation siblings exceed the Mission model-token budget", scope);
      if (Number(budget.wallClockSeconds) > mission.value.budgets.maximumWallClockSeconds - use.wallClockSeconds) return this.rejectDelegation(projectId, record, requestHash, "Delegation request exceeds the remaining Mission wall-clock deadline", scope);
    }
    const hostMaximum = this.options.maximumActivePiSessions ?? 8;
    if (!Number.isInteger(hostMaximum) || hostMaximum < 1 || this.activeAuthorizedSessions(projectId).length >= hostMaximum) return this.rejectDelegation(projectId, record, requestHash, "Host active Pi-session capacity is exhausted", scope);
    if (missionId && this.activeAuthorizedSessions(projectId, missionId).length >= this.research.mission(projectId, missionId).value.budgets.maximumConcurrentAgents) return this.rejectDelegation(projectId, record, requestHash, "Mission maximum concurrent-agent budget is exhausted", scope);
    const reservation = store.beginOperation(projectId, "delegation.reservation", `delegation-reservation:${requestId}`, { requestId, parentTaskId: record.requestingTaskId, modelTokens: budget.modelTokens, wallClockSeconds: budget.wallClockSeconds, scope });
    if (reservation.state !== "pending") return this.rejectDelegation(projectId, record, requestHash, "Delegation reservation is already terminal without an authorization", scope);
    try {
      const childTaskId = deterministicDelegationId("tsk", projectId, requestId); const childAgentId = deterministicDelegationId("agt", projectId, requestId); const childThreadId = deterministicDelegationId("thr", projectId, requestId);
      const runtimeScope = delegationEventScope(projectId, scope, childAgentId);
      const runtimeRequest = delegationRuntimeRequest(projectId, record, scope, childTaskId, childAgentId, childThreadId);
      const authorized = store.mutateProjection(`delegation-authorize:${requestId}`, 0, { $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "delegation.authorized", source: "noshd", scope: { ...runtimeScope, agentId: record.requestingAgentId }, correlationId: requestId, causationId: null, payload: { requestId, childTaskId, childAgentId, childThreadId, reservationIntentId: reservation.intentId } }, { entityType: "delegation", entityId: requestId, state: "authorized", value: { requestHash, parentTaskId: record.requestingTaskId, childTaskId, childAgentId, childThreadId, runtimeScope, runtimeRequest, reservationIntentId: reservation.intentId, authorizedAt: new Date().toISOString() } });
      if (!authorized.replayed) this.events.emit("event", authorized.event);
      this.resumeDelegation(projectId, requestId);
      return authorized.projection.value;
    } catch (error) {
      this.failPendingDelegationReservation(projectId, reservation.intentId, error);
      throw error;
    }
  }
  private resumeDelegation(projectId: string, requestId: string): void {
    const store = this.storeFor(projectId);
    const delegation = store.projection<JsonValue>(projectId, "delegation", requestId);
    if (!delegation || delegation.state !== "authorized" || typeof delegation.value !== "object" || delegation.value === null || Array.isArray(delegation.value) || !("runtimeRequest" in delegation.value)) return;
    const operation = store.beginOperation(projectId, "delegation.runtime", `delegation-runtime:${requestId}`, delegation.value.runtimeRequest as JsonValue);
    const runKey = delegationRunKey(projectId, requestId);
    if (operation.state === "pending" && !this.delegationRuns.has(runKey)) {
      this.delegationRuns.add(runKey);
      void this.runDelegation(projectId, requestId, operation.intentId, operation.request).finally(() => this.delegationRuns.delete(runKey));
    }
  }
  private async runDelegation(projectId: string, requestId: string, intentId: string, request: JsonValue): Promise<void> {
    const store = this.storeFor(projectId);
    try {
      if (typeof request !== "object" || request === null || Array.isArray(request) || typeof request.open !== "object" || request.open === null || typeof request.step !== "object" || request.step === null) throw new Error("Delegation runtime intent is corrupt");
      await this.runtime.execute(request.open);
      const runtimeOutcome = await this.runtime.execute(request.step);
      if (!runtimeOutcome || typeof runtimeOutcome !== "object" || Array.isArray(runtimeOutcome) || !("episode" in runtimeOutcome) || !runtimeOutcome.episode || typeof runtimeOutcome.episode !== "object" || Array.isArray(runtimeOutcome.episode) || typeof runtimeOutcome.episode.status !== "string") throw new Error("Delegation runtime returned no Episode outcome");
      const episode = runtimeOutcome.episode as Record<string, JsonValue>;
      const completed = episode.status === "completed";
      const current = store.projection<JsonValue>(projectId, "delegation", requestId);
      if (!current || current.state !== "authorized") throw new Error("Delegation authority is no longer active");
      const value = isDelegationValue(current.value); const runtimeScope = delegationScope(value.runtimeScope ?? null);
      const terminalState = completed ? "completed" : "failed"; const terminalValue = { ...value, runtimeOutcome, episode, completedAt: new Date().toISOString(), terminalHash: delegationTerminalHash(value, terminalState, runtimeOutcome as JsonValue, episode) };
      const outcome = store.mutateProjection(`delegation-outcome:${requestId}`, current.version, { $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: completed ? "delegation.completed" : "delegation.failed", source: "noshd", scope: runtimeScope, correlationId: requestId, causationId: null, payload: { requestId, childTaskId: value.childTaskId, runtimeOutcome, episode, terminalHash: terminalValue.terminalHash } }, { entityType: "delegation", entityId: requestId, state: terminalState, value: terminalValue });
      if (typeof value.reservationIntentId === "string") this.completePendingOperation(projectId, value.reservationIntentId, { requestId, terminalState: completed ? "completed" : "failed" });
      this.completePendingOperation(projectId, intentId, runtimeOutcome as JsonValue);
      if (!outcome.replayed) this.events.emit("event", outcome.event);
    } catch (error) {
      const message = error instanceof Error ? error.message : "Delegated child failed";
      const current = store.projection<JsonValue>(projectId, "delegation", requestId);
      if (current && current.state === "authorized") {
        const value = isDelegationValue(current.value); const runtimeScope = delegationScope(value.runtimeScope ?? null);
        const terminalValue = { ...value, error: message, completedAt: new Date().toISOString(), terminalHash: delegationTerminalHash(value, "failed", null, null, message) };
        const failed = store.mutateProjection(`delegation-outcome:${requestId}`, current.version, { $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "delegation.failed", source: "noshd", scope: runtimeScope, correlationId: requestId, causationId: null, payload: { requestId, error: message, terminalHash: terminalValue.terminalHash } }, { entityType: "delegation", entityId: requestId, state: "failed", value: terminalValue });
        if (typeof value.reservationIntentId === "string") this.completePendingOperation(projectId, value.reservationIntentId, { requestId, terminalState: "failed" });
        if (!failed.replayed) this.events.emit("event", failed.event);
      }
      this.failPendingOperation(projectId, intentId, message);
    }
  }
  private assertDelegationParentLease(projectId: string, parent: Record<string, JsonValue>, scope: Record<string, JsonValue>, agentId: string): void {
    if (!isObject(parent.lease) || typeof parent.lease.leaseId !== "string" || typeof parent.lease.expiresAt !== "string" || Date.parse(parent.lease.expiresAt) <= Date.now() || !isObject(parent.observedVersions) || typeof scope.graphNodeId !== "string") throw new Error("Delegation parent Task Packet lacks a live lease authority");
    const sessions = this.agents.inspect().filter((session) => session.projectId === projectId && session.agentId === agentId && session.taskId === parent.taskId && session.missionId === nullableId(scope.missionId) && session.directionId === nullableId(scope.directionId) && session.autoresearchId === nullableId(scope.autoresearchId) && session.experimentId === nullableId(scope.experimentId));
    if (sessions.length !== 1) throw new Error("Delegation parent Pi session is not uniquely live in the Task Packet scope");
    const graphNodeId = scope.graphNodeId; const leaseId = parent.lease.leaseId;
    if (typeof scope.missionId === "string") {
      const mission = this.research.mission(projectId, scope.missionId); const leaseVersion = parent.observedVersions.leaseVersion;
      const node = mission.value.nodes.find((entry) => entry.id === graphNodeId);
      if (!Number.isInteger(leaseVersion) || mission.version !== leaseVersion || !node?.lease || node.lease.leaseId !== leaseId || node.lease.ownerId !== agentId || !["leased", "working"].includes(node.state)) throw new Error("Delegation parent Mission lease is no longer exact and live");
      return;
    }
    if (typeof scope.directionId === "string") {
      const direction = this.research.direction(projectId, scope.directionId); const leaseVersion = parent.observedVersions.directionProjectionVersion;
      const node = direction.value.nodes.find((entry) => entry.id === graphNodeId);
      if (!Number.isInteger(leaseVersion) || direction.version !== leaseVersion || !node?.lease || node.lease.leaseId !== leaseId || node.lease.ownerId !== agentId || !["leased", "working"].includes(node.state)) throw new Error("Delegation parent Direction lease is no longer exact and live");
      return;
    }
    throw new Error("Delegation parent Task Packet has no Mission or Direction graph scope");
  }
  private activeAuthorizedSessions(projectId: string, missionId?: string): string[] {
    const store = this.storeFor(projectId);
    const delegatedAgentIds = new Set(store.projections<JsonValue>(projectId, "delegation").filter((delegation) => delegation.state === "authorized").flatMap((delegation) => isDelegationValue(delegation.value).childAgentId && typeof isDelegationValue(delegation.value).childAgentId === "string" ? [isDelegationValue(delegation.value).childAgentId] : []));
    const live = this.agents.inspect().filter((agent) => agent.projectId === projectId && (!missionId || agent.missionId === missionId) && !delegatedAgentIds.has(agent.agentId)).map((agent) => `agent:${agent.agentId}`);
    const reserved = store.operationIntents(projectId, "pending").filter((intent) => intent.operationType === "delegation.reservation").flatMap((intent) => {
      const reservation = delegationReservation(intent.request);
      return reservation && (!missionId || reservation.missionId === missionId) ? [`reservation:${reservation.requestId}`] : [];
    });
    return [...new Set([...live, ...reserved])];
  }
  private completePendingOperation(projectId: string, intentId: string, result: JsonValue): void {
    const store = this.storeFor(projectId); const intent = store.operationIntents(projectId).find((candidate) => candidate.intentId === intentId);
    if (intent?.state === "pending") store.completeOperation(projectId, intentId, result);
  }
  private failPendingOperation(projectId: string, intentId: string, error: unknown): void {
    const store = this.storeFor(projectId); const intent = store.operationIntents(projectId).find((candidate) => candidate.intentId === intentId);
    if (intent?.state === "pending") store.failOperation(projectId, intentId, error instanceof Error ? error.message : "Delegation operation failed");
  }
  private reconcileDelegationReservations(projectId: string): void {
    const store = this.storeFor(projectId);
    for (const intent of store.operationIntents(projectId, "pending").filter((candidate) => candidate.operationType === "delegation.reservation")) {
      const reservation = delegationReservation(intent.request);
      if (!reservation) { this.failPendingOperation(projectId, intent.intentId, "Delegation reservation is corrupt"); continue; }
      const delegation = store.projection<JsonValue>(projectId, "delegation", reservation.requestId);
      if (!delegation) { this.failPendingOperation(projectId, intent.intentId, "Delegation reservation has no durable authorization"); continue; }
      if (delegation.state === "authorized") continue;
      if (delegation.state === "completed" || delegation.state === "failed") this.completePendingOperation(projectId, intent.intentId, { requestId: reservation.requestId, terminalState: delegation.state });
      else this.failPendingOperation(projectId, intent.intentId, `Delegation reservation has unexpected ${delegation.state} authority`);
    }
  }
  private failPendingDelegationReservation(projectId: string, intentId: string, error: unknown): void {
    this.failPendingOperation(projectId, intentId, error);
  }
  private rejectDelegation(projectId: string, record: Record<string, JsonValue>, requestHash: string, message: string, parentScope?: Record<string, JsonValue>): never {
    const requestId = record.requestId as string; const store = this.storeFor(projectId);
    const runtimeScope = delegationRejectionScope(projectId, parentScope, record.requestingAgentId as string);
    const terminal = { requestHash, state: "rejected", runtimeScope, error: message };
    const terminalHash = sha256(terminal);
    const existing = store.projection<JsonValue>(projectId, "delegation", requestId);
    if (existing) {
      if (existing.state === "rejected") throw new Error(String(assertedRejectedDelegation(isDelegationValue(existing.value), requestHash).error));
      throw new Error("Delegation request ID already has a non-rejected durable authority");
    }
    this.failPendingDelegationReservationForRequest(projectId, requestId, message);
    const rejected = store.mutateProjection(`delegation-reject:${requestId}`, 0, {
      $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "delegation.rejected", source: "noshd", scope: runtimeScope,
      correlationId: requestId, causationId: null, payload: { requestId, requestHash, terminalHash, error: message },
    }, { entityType: "delegation", entityId: requestId, state: "rejected", value: { ...terminal, terminalHash } });
    if (!rejected.replayed) this.events.emit("event", rejected.event);
    throw new Error(message);
  }
  private failPendingDelegationReservationForRequest(projectId: string, requestId: string, error: unknown): void {
    const intent = this.storeFor(projectId).operationIntents(projectId).find((candidate) => candidate.idempotencyKey === `delegation-reservation:${requestId}`);
    if (intent) this.failPendingDelegationReservation(projectId, intent.intentId, error);
  }

  async steerMission(projectId: string, missionId: string, expectedVersion: number, message: string, idempotencyKey: string): Promise<void> {
    const mission = this.research.mission(projectId, missionId); const text = message.trim(); if (mission.version !== expectedVersion) throw new Error("Mission version conflict"); if (mission.state !== "running") throw new Error("Only a running Mission may be steered"); if (!text) throw new Error("Mission steering message is required"); const directors = this.agents.inspect().filter((agent) => agent.projectId === projectId && agent.missionId === missionId && ["mission_director", "research_director"].includes(agent.role)); if (!directors.length) throw new Error("Mission has no active Director to steer"); await Promise.all(directors.map((agent) => this.agents.steer(agent.agentId, text))); this.appendDraft({ $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "mission.steering_applied", source: "user", scope: { projectId, missionId, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: null }, correlationId: idempotencyKey, causationId: null, payload: { message: text, directorAgentIds: directors.map((agent) => agent.agentId) } });
  }

  async controlMission(projectId: string, missionId: string, expectedVersion: number, action: "pause" | "resume" | "stop", mode: "safe" | "checkpoint" | "immediate", idempotencyKey: string) {
    const mission = this.research.mission(projectId, missionId); if (mission.version !== expectedVersion) throw new Error("Mission version conflict"); const agents = this.agents.inspect().filter((agent) => agent.projectId === projectId && agent.missionId === missionId);
    if (action === "resume") return this.research.transitionMission(projectId, missionId, mission.version, "running", `${idempotencyKey}:running`);
    if (action === "pause") { const pausing = this.research.transitionMission(projectId, missionId, mission.version, "pausing", `${idempotencyKey}:pausing`); if (mode === "immediate") await Promise.all(agents.map((agent) => this.agents.abort(agent.agentId))); else await Promise.all(agents.map((agent) => this.agents.steer(agent.agentId, "Pause at the next safe boundary and persist a canonical handoff."))); if (mode === "checkpoint") for (const job of this.jobs.list().filter((entry) => missionJobMatches(entry, projectId, missionId))) this.jobs.checkpoint(job.jobId); return this.research.transitionMission(projectId, missionId, pausing.version, "paused", `${idempotencyKey}:paused`); }
    const stopping = this.research.transitionMission(projectId, missionId, mission.version, "stopping", `${idempotencyKey}:stopping`); for (const job of this.jobs.list().filter((entry) => missionJobMatches(entry, projectId, missionId))) this.jobs.cancel(job.jobId, "mission_stopped"); await Promise.all(agents.map((agent) => this.agents.abort(agent.agentId))); return this.research.transitionMission(projectId, missionId, stopping.version, "stopped", `${idempotencyKey}:stopped`);
  }
  controlJob(projectId: string, jobId: string, action: "checkpoint" | "cancel", idempotencyKey: string): JobRecord {
    this.assertProjectWritable(projectId);
    const job = this.jobs.get(jobId); if (job.projectId !== projectId) throw new Error("Job is outside the requested Project");
    const store = this.storeFor(projectId); const key = `job-control:${idempotencyKey}`; const request = { jobId, action };
    const prior = store.operationIntents(projectId).find((intent) => intent.idempotencyKey === key);
    const intent = store.beginOperation(projectId, "job.control", key, request);
    if (intent.state === "completed") return intent.result as unknown as JobRecord;
    if (intent.state === "failed") throw new Error(intent.error ?? "Job control previously failed");
    if (prior) throw new Error("Job control outcome is unresolved after interruption; refusing to repeat the action");
    try {
      const result = action === "checkpoint" ? this.jobs.checkpoint(jobId) : this.jobs.cancel(jobId);
      const stored = store.appendIdempotent(`job-control-event:${idempotencyKey}`, { $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "job.control_completed", source: "noshd", scope: { projectId, missionId: result.missionId, directionId: result.directionId, autoresearchId: result.autoresearchId, experimentId: result.experimentId, runId: result.runId, jobId, agentId: null }, correlationId: jobId, causationId: null, payload: { action, job: result as unknown as JsonValue } });
      store.completeOperation(projectId, intent.intentId, result as unknown as JsonValue);
      if (!stored.replayed) this.events.emit("event", stored.receipt.event);
      return result;
    } catch (error) {
      const message = error instanceof Error ? error.message : "Job control failed";
      store.failOperation(projectId, intent.intentId, message);
      throw error;
    }
  }

  startJob(spec: JobSpec): JobRecord {
    if (!this.projects().some((project) => project.projectId === spec.projectId)) throw new Error(`Project ${spec.projectId} is not registered on this host`);
    this.assertProjectWritable(spec.projectId);
    const store = this.storeFor(spec.projectId);
    const intent = store.beginOperation(spec.projectId, "job.launch", `job-launch:${spec.jobId}`, spec as unknown as JsonValue);
    if (intent.state === "completed") { const result = intent.result as unknown as JobRecord; assertJobIdentity(result, spec, "Stored Job launch result"); return result; }
    if (intent.state === "failed") throw new Error(intent.error ?? "Job launch previously failed");
    this.jobs.start(spec);
    const verified = this.jobs.get(spec.jobId);
    assertJobIdentity(verified, spec, "Job launch verification");
    store.completeOperation(spec.projectId, intent.intentId, verified as unknown as JsonValue);
    return verified;
  }

  replay(projectId: string, afterSequence: number): EventEnvelope[] {
    return this.storeFor(projectId).replay(projectId, afterSequence);
  }

  replayPage(projectId: string, afterSequence = 0, limit = 300, recent = false): { events: EventEnvelope[]; nextCursor: number; hasMore: boolean } {
    return this.storeFor(projectId).replayPage(projectId, afterSequence, limit, recent);
  }

  unresolvedRemoteCommands(projectId: string): Array<{ commandId: string; type: string; targetType: string; targetId: string; expectedVersion: number; acceptedAt: string }> {
    const events = this.storeFor(projectId).replay(projectId); const terminal = new Set(events.filter((event) => ["remote.command_completed", "remote.command_failed"].includes(event.type) && event.correlationId).map((event) => event.correlationId!));
    return events.filter((event) => event.type === "remote.command_accepted" && event.correlationId && !terminal.has(event.correlationId)).map((event) => { const payload = event.payload as { type?: string; targetType?: string; targetId?: string; expectedVersion?: number }; return { commandId: event.correlationId!, type: payload.type ?? "unknown", targetType: payload.targetType ?? "unknown", targetId: payload.targetId ?? "unknown", expectedVersion: payload.expectedVersion ?? 0, acceptedAt: event.timestamp }; });
  }

  resolveRemoteCommand(projectId: string, commandId: string, outcome: "applied" | "not_applied", note: string, idempotencyKey: string): EventEnvelope {
    this.assertProjectWritable(projectId);
    const store = this.storeFor(projectId); const receipt = store.commandReceipt(projectId, `remote-resolution:${idempotencyKey}`); if (receipt) return receipt.event; const events = store.replay(projectId); const accepted = events.find((event) => event.type === "remote.command_accepted" && event.correlationId === commandId); if (!accepted) throw new Error("Unknown remote command"); if (events.some((event) => ["remote.command_completed", "remote.command_failed"].includes(event.type) && event.correlationId === commandId)) throw new Error("Remote command already has a terminal outcome"); if (!note.trim()) throw new Error("A local resolution note is required"); const payload = accepted.payload as { type?: string }; const stored = store.appendIdempotent(`remote-resolution:${idempotencyKey}`, { $schema: "https://nosh.dev/schemas/event/v1", schemaVersion: 1, retention: "persistent", type: outcome === "applied" ? "remote.command_completed" : "remote.command_failed", source: "local_user", scope: accepted.scope, correlationId: commandId, causationId: accepted.eventId, payload: { commandId, type: payload.type ?? "unknown", resolution: outcome, note: note.trim() } }); if (!stored.replayed) this.events.emit("event", stored.receipt.event); return stored.receipt.event;
  }

  remoteVersion(command: RemoteCommandEnvelope): number { if (command.targetType === "mission") return this.research.mission(command.projectId, command.targetId).version; if (command.targetType === "thread") return this.runtime.thread(command.projectId, command.targetId).version; return this.storeFor(command.projectId).currentSequence(command.projectId); }
  remoteReplay(command: RemoteCommandEnvelope): boolean { const store = this.storeFor(command.projectId); const receipt = store.commandReceipt(command.projectId, `remote:${command.idempotencyKey}`); if (!receipt) return false; const payload = receipt.event.payload as { envelopeHash?: string }; if (payload.envelopeHash !== sha256(command as unknown as JsonValue)) throw new Error("Idempotency key was reused for a different remote command"); const terminal = store.replay(command.projectId).find((event) => event.correlationId === command.commandId && ["remote.command_completed", "remote.command_failed"].includes(event.type)); if (!terminal) throw new Error("Remote command was accepted but its outcome is unresolved after interruption"); if (terminal.type === "remote.command_failed") throw new Error("Remote command previously failed"); return true; }

  async executeRemoteCommand(command: RemoteCommandEnvelope, payload: JsonValue): Promise<{ replayed: boolean; currentVersion: number }> {
    if (!this.projects().some((project) => project.projectId === command.projectId)) throw new Error("Remote command Project is not registered on this host");
    this.assertProjectWritable(command.projectId);
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
    this.assertProjectWritable(draft.scope.projectId);
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
      if (command.type === "mission.pause") { const pausing = this.research.transitionMission(command.projectId, command.targetId, mission.version, "pausing", `${command.idempotencyKey}:pausing`); const mode = (payload as { mode: string }).mode; if (mode === "immediate") await Promise.all(agents.map((entry) => this.agents.abort(entry.agentId))); else await Promise.all(agents.map((entry) => this.agents.steer(entry.agentId, "Pause at the next safe boundary and persist a canonical handoff."))); if (mode === "checkpoint") for (const job of this.jobs.list().filter((entry) => missionJobMatches(entry, command.projectId, command.targetId))) this.jobs.checkpoint(job.jobId); this.research.transitionMission(command.projectId, command.targetId, pausing.version, "paused", `${command.idempotencyKey}:paused`); }
      if (command.type === "mission.stop") { const stopping = this.research.transitionMission(command.projectId, command.targetId, mission.version, "stopping", `${command.idempotencyKey}:stopping`); for (const job of this.jobs.list().filter((entry) => missionJobMatches(entry, command.projectId, command.targetId))) this.jobs.cancel(job.jobId, "mission_stopped_remotely"); await Promise.all(agents.map((entry) => this.agents.abort(entry.agentId))); this.research.transitionMission(command.projectId, command.targetId, stopping.version, "stopped", `${command.idempotencyKey}:stopped`); }
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

  private backupActivityReason(projectId: string): string | null {
    this.maintenanceInspectionProject = projectId;
    try {
      const agents = this.agents.inspect().filter((agent) => agent.projectId === projectId && agent.status !== "idle").map((agent) => `${agent.agentId}:${agent.status}`);
      const jobs = this.jobs.list().filter((job) => job.projectId === projectId && ["queued", "starting", "running", "checkpointing", "finishing"].includes(job.state)).map((job) => `${job.jobId}:${job.state}`);
      const threads = this.runtime.threads(projectId).filter((thread) => ["open", "running", "awaiting", "awaiting_user", "paused"].includes(thread.value.state)).map((thread) => `${thread.entityId}:${thread.value.state}`);
      const missions = this.research.missions(projectId).filter((mission) => !["draft", "completed", "stopped", "failed"].includes(mission.state)).map((mission) => `${mission.entityId}:${mission.state}`);
      const directions = this.research.directions(projectId).filter((direction) => !["draft", "closed", "rejected", "stopped"].includes(direction.state)).map((direction) => `${direction.entityId}:${direction.state}`);
      const executions = this.research.autoresearch(projectId).filter((execution) => !["draft", "completed", "stopped", "failed"].includes(execution.state)).map((execution) => `${execution.entityId}:${execution.state}`);
      const active = [...agents.map((entry) => `agent ${entry}`), ...jobs.map((entry) => `job ${entry}`), ...threads.map((entry) => `runtime thread ${entry}`), ...missions.map((entry) => `Mission ${entry}`), ...directions.map((entry) => `Direction ${entry}`), ...executions.map((entry) => `Autoresearch ${entry}`)];
      return active.length ? `Backup rejected: active Project state (${active.join(", ")})` : null;
    } finally {
      this.maintenanceInspectionProject = null;
    }
  }

  private reconcileProject(project: RegisteredProject): void {
    const store = this.storeFor(project.projectId);
    const papers = this.research.reconcilePendingOperations(project.projectId);
    const domainEffects = this.research.reconcileAcceptedDomainEffects(project.projectId);
    let recoveredDelegations = 0; let rejectedDelegations = 0;
    for (const entry of this.research.submitted(project.projectId)) if (entry.record && typeof entry.record === "object" && !Array.isArray(entry.record) && entry.record.$schema === schemaUri("delegation-request")) {
      const record = entry.record as Record<string, JsonValue>;
      try {
        this.authorizeDelegation(project.projectId, record);
        const state = store.projection(project.projectId, "delegation", String(record.requestId))?.state;
        if (state === "authorized") recoveredDelegations += 1;
        if (state === "rejected") rejectedDelegations += 1;
      } catch (error) {
        const terminal = store.projection(project.projectId, "delegation", String(record.requestId));
        if (terminal?.state !== "rejected") this.appendDraft({ $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "recovery.delegation_failed", source: "noshd", scope: entry.event.scope, correlationId: String(record.requestId), causationId: entry.event.eventId, payload: { message: error instanceof Error ? error.message : "Delegation recovery failed" } });
      }
    }
    this.reconcileDelegationReservations(project.projectId);
    let jobs = 0;
    for (const intent of store.operationIntents(project.projectId, "pending")) if (intent.operationType === "job.launch") {
      const spec = intent.request as unknown as JobSpec;
      this.jobs.start(spec);
      const verified = this.jobs.get(spec.jobId);
      assertJobIdentity(verified, spec, "Recovered Job launch");
      store.completeOperation(project.projectId, intent.intentId, verified as unknown as JsonValue);
      jobs += 1;
    }
    const runtime = store.operationIntents(project.projectId, "pending").filter((intent) => intent.operationType === "runtime.instruction");
    for (const intent of runtime) void this.executeRuntimeEffect(intent.operationType, intent.request)
      .then((result) => store.completeOperation(project.projectId, intent.intentId, result))
      .catch((error) => {
        const message = error instanceof Error ? error.message : "Runtime recovery failed";
        store.failOperation(project.projectId, intent.intentId, message);
        this.appendDraft({ $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "recovery.runtime_failed", source: "noshd", scope: { projectId: project.projectId, missionId: null, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: null }, correlationId: intent.intentId, causationId: null, payload: { operationType: intent.operationType, message } });
      });
    const programs = this.runtime.programStates(project.projectId).filter(({ value }) => value.status === "running");
    for (const { entityId } of programs) void this.runtime.runProgram(project.projectId, entityId).catch((error) => this.appendDraft({ $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "recovery.program_failed", source: "noshd", scope: { projectId: project.projectId, missionId: null, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: null }, correlationId: entityId, causationId: null, payload: { message: error instanceof Error ? error.message : "Program recovery failed" } }));
    const unresolved = store.operationIntents(project.projectId, "pending").map((intent) => ({ intentId: intent.intentId, operationType: intent.operationType }));
    const unresolvedRemoteCommands = this.unresolvedRemoteCommands(project.projectId).map((command) => command.commandId);
    this.appendDraft({ $schema: "https://nosh.dev/schemas/event/v1", schemaVersion: 1, retention: "persistent", type: "recovery.report", source: "noshd", scope: { projectId: project.projectId, missionId: null, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: null }, correlationId: null, causationId: null, payload: { reconciledPaperOperations: papers, recoveredDomainEffects: domainEffects.recovered, failedDomainEffects: domainEffects.failed, recoveredDelegations, rejectedDelegations, reconciledJobLaunches: jobs, resumingRuntimeInstructions: runtime.length, resumingPrograms: programs.length, unresolvedIntents: unresolved, unresolvedRemoteCommands } });
  }
}

export function missionJobMatches(job: Pick<JobRecord, "projectId" | "missionId">, projectId: string, missionId: string): boolean {
  return job.projectId === projectId && job.missionId === missionId;
}
function intakePrompt(contract: ProjectContract): string {
  return `This Project has a draft, unapproved Project contract for "${contract.workingTitle}". Conduct a facilitated research-discovery conversation: ask exactly one focused question per turn; do not invent requirements; challenge vague, contradictory, unfalsifiable, or overly broad claims; and periodically reflect the shared understanding. Establish the research problem, motivation, decision use, contribution, scope, datasets and licenses, baselines, evaluation and falsification criteria, reproducibility, risks, intended paper outcome, and realistic GPU and time budgets before proposing an approved contract. Current north-star question: ${contract.northStar.question}`;
}
function terminalOutcomeTaskId(tool: string, schema: string, attemptKey: string, record: unknown): string | null {
  if (!isTaskTerminalRecord(record) || !((tool === "nosh_response_submit" && schema !== schemaUri("review-verdict")) || (tool === "nosh_review_submit" && schema === schemaUri("review-verdict")))) return null;
  const task = /^task:(tsk_[0-9a-f]{32})$/.exec(attemptKey)?.[1];
  if (task) return task;
  if (!record || typeof record !== "object" || Array.isArray(record)) return null;
  const taskId = (record as { taskId?: unknown }).taskId;
  return typeof taskId === "string" && /^tsk_[0-9a-f]{32}$/.test(taskId) ? taskId : null;
}

function submissionScopeError(record: unknown, projectId: string, agentId?: string): { pointer: string; message: string } | null {
  if (!record || typeof record !== "object" || Array.isArray(record)) return null; const value = record as Record<string, unknown>; if (typeof value.projectId === "string" && value.projectId !== projectId) return { pointer: "/projectId", message: "Record Project does not match the daemon-bound session" }; const scope = value.scope; if (scope && typeof scope === "object" && !Array.isArray(scope) && typeof (scope as Record<string, unknown>).projectId === "string" && (scope as Record<string, unknown>).projectId !== projectId) return { pointer: "/scope/projectId", message: "Record scope does not match the daemon-bound session" };
  if (!agentId) return null;
  const schema = typeof value.$schema === "string" ? value.$schema : "";
  const actorBySchema: Record<string, string> = {
    [schemaUri("response-envelope")]: "agentId", [schemaUri("task-acknowledgement")]: "agentId", [schemaUri("progress-update")]: "agentId", [schemaUri("delegation-request")]: "requestingAgentId", [schemaUri("review-verdict")]: "reviewerAgentId", [schemaUri("experiment-proposal")]: "proposedByAgentId", [schemaUri("evidence")]: "createdByAgentId", [schemaUri("handoff")]: "fromAgentId", [schemaUri("handoff-teachback")]: "toAgentId", [schemaUri("mission-director-cycle")]: "directorAgentId", [schemaUri("research-director-cycle")]: "directorAgentId", [schemaUri("runtime-instruction")]: "proposedByAgentId",
  };
  const field = actorBySchema[schema];
  if (field && value[field] !== agentId) return { pointer: `/${field}`, message: "Record actor does not match the daemon-bound Pi session" };
  return null;
}
type DelegationReservation = { requestId: string; parentTaskId: string; modelTokens: number; wallClockSeconds: number; missionId: string | null };
function deterministicDelegationId(prefix: "tsk" | "agt" | "thr" | "ins", projectId: string, requestId: string): string { return `${prefix}_${sha256({ prefix, projectId, requestId }).slice(7, 39)}`; }
function delegationRunKey(projectId: string, requestId: string): string { return `${projectId}:${requestId}`; }
function nullableId(value: JsonValue | undefined): string | null { return typeof value === "string" ? value : null; }
function isObject(value: unknown): value is Record<string, JsonValue> { return typeof value === "object" && value !== null && !Array.isArray(value); }
function delegationEventScope(projectId: string, scope: Record<string, JsonValue>, agentId: string): EventDraft["scope"] {
  if (scope.projectId !== projectId) throw new Error("Delegation parent scope is outside the Project");
  return { projectId, missionId: nullableId(scope.missionId), directionId: nullableId(scope.directionId), autoresearchId: nullableId(scope.autoresearchId), experimentId: nullableId(scope.experimentId), runId: nullableId(scope.runId), jobId: nullableId(scope.jobId), agentId };
}
function delegationScope(value: JsonValue): EventDraft["scope"] {
  if (!isObject(value) || typeof value.projectId !== "string" || ["missionId", "directionId", "autoresearchId", "experimentId", "runId", "jobId", "agentId"].some((field) => value[field] !== null && typeof value[field] !== "string")) throw new Error("Delegation authority has an invalid durable runtime scope");
  return value as unknown as EventDraft["scope"];
}
function isDelegationValue(value: JsonValue): Record<string, JsonValue> { if (!isObject(value)) throw new Error("Delegation projection is corrupt"); return value; }
function delegationRejectionScope(projectId: string, parentScope: Record<string, JsonValue> | undefined, agentId: string): EventDraft["scope"] {
  if (parentScope) try { return delegationEventScope(projectId, parentScope, agentId); } catch { /* use the Project scope when the parent scope is itself invalid */ }
  return { projectId, missionId: null, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId };
}
function delegationTerminalHash(value: Record<string, JsonValue>, state: "completed" | "failed", runtimeOutcome: JsonValue | null, episode: JsonValue | null, error: string | null = null): string {
  return sha256({ requestHash: value.requestHash, state, runtimeScope: delegationScope(value.runtimeScope ?? null), runtimeOutcome, episode, error });
}
function assertedDelegationTerminal(value: Record<string, JsonValue>, state: "completed" | "failed"): Record<string, JsonValue> {
  if (typeof value.requestHash !== "string" || typeof value.terminalHash !== "string" || value.terminalHash !== delegationTerminalHash(value, state, value.runtimeOutcome ?? null, value.episode ?? null, typeof value.error === "string" ? value.error : null)) throw new Error("Delegation terminal authority hash is invalid");
  return value;
}
function assertedRejectedDelegation(value: Record<string, JsonValue>, requestHash: string): Record<string, JsonValue> {
  const runtimeScope = delegationScope(value.runtimeScope ?? null);
  if (value.requestHash !== requestHash || value.state !== "rejected" || typeof value.error !== "string" || typeof value.terminalHash !== "string" || value.terminalHash !== sha256({ requestHash, state: "rejected", runtimeScope, error: value.error })) throw new Error("Delegation rejection authority hash is invalid");
  return value;
}
function delegationRuntimeRequest(projectId: string, record: Record<string, JsonValue>, scope: Record<string, JsonValue>, childTaskId: string, childAgentId: string, childThreadId: string): JsonValue {
  const role = record.requestedRole as "librarian_researcher" | "general_worker" | "reviewer"; const budget = record.budgetEstimate as Record<string, JsonValue>; const issuedAt = record.submittedAt as string;
  const permissions = { network: "disabled", subprocess: "disabled", gitCommit: false, gitPush: false, delegation: "request_only", networkAllowlist: [], allowedToolIds: ["tool_nosh.episode.submit"] };
  const open = { $schema: schemaUri("runtime-instruction"), schemaVersion: 1, instructionId: deterministicDelegationId("ins", projectId, `${record.requestId}:open`), projectId, idempotencyKey: `delegation:${String(record.requestId)}:open`, proposedByAgentId: null, issuedAt, operation: "THREAD_OPEN", threadId: childThreadId, taskId: childTaskId, initialAgentId: childAgentId, ownerScope: { missionId: nullableId(scope.missionId), directionId: nullableId(scope.directionId), autoresearchId: nullableId(scope.autoresearchId), experimentId: nullableId(scope.experimentId), graphNodeId: nullableId(scope.graphNodeId) }, role, purpose: record.proposedObjective as string, executionMode: "background", parentThreadId: null, inputRefs: [], skillIds: [], capabilities: [], taskPermissions: permissions, taskWorkspace: null, budget: { maximumToolCalls: 50, maximumModelTokens: budget.modelTokens, maximumWallClockSeconds: budget.wallClockSeconds } };
  const step = { $schema: schemaUri("runtime-instruction"), schemaVersion: 1, instructionId: deterministicDelegationId("ins", projectId, `${record.requestId}:step`), projectId, idempotencyKey: `delegation:${String(record.requestId)}:step`, proposedByAgentId: null, issuedAt, operation: "THREAD_STEP", threadId: childThreadId, objective: record.proposedObjective as string, expectedEpisodeType: "episode_delegation", inputRefs: [], skillIds: [] };
  return { open, step };
}
function delegationReservation(value: JsonValue): DelegationReservation | null {
  if (!isObject(value) || !isObject(value.scope)) return null;
  const missionId = nullableId(value.scope.missionId);
  return typeof value.requestId === "string" && typeof value.parentTaskId === "string" && typeof value.modelTokens === "number" && Number.isInteger(value.modelTokens) && value.modelTokens > 0 && typeof value.wallClockSeconds === "number" && Number.isInteger(value.wallClockSeconds) && value.wallClockSeconds > 0 ? { requestId: value.requestId, parentTaskId: value.parentTaskId, modelTokens: value.modelTokens, wallClockSeconds: value.wallClockSeconds, missionId } : null;
}
function assertJobIdentity(record: JobRecord, spec: JobSpec, context: string): void {
  if (record.jobId !== spec.jobId || record.projectId !== spec.projectId || record.commandDigest !== sha256(spec.command) || record.commitSha !== spec.commitSha || record.evaluationContractHash !== spec.evaluationContractHash) throw new Error(`${context} does not match its durable launch specification`);
}
function chatSelection(selection?: ModelSelection): JsonValue { return selection ? { provider: selection.provider, id: selection.id, thinkingLevel: selection.thinkingLevel ?? null } : null; }
function deterministicChatAgentId(projectId: string): string { return `agt_${sha256({ projectId, role: "nosh_chat" }).slice(7, 39)}`; }
