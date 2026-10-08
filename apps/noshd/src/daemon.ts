import { EventEmitter } from "node:events";
import { existsSync, lstatSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { spawnSync } from "node:child_process";
import { isAbsolute, join, relative, resolve } from "node:path";
import { createHash, timingSafeEqual } from "node:crypto";
import { createId } from "@nosh/core";
import { EventStore, HostRegistry, applyScheduledRestores, createProjectBackup, listProjectBackups, scheduleProjectRestore, type RegisteredProject } from "@nosh/persistence";
import { StructuredSubmissionGate } from "@nosh/agent-runtime";
import { PiAdapter, commitAllWorkspaceChanges, terminalSchemaTools, type AgentInspection, type TerminalContext, type TerminalReceipt, type PiSessionOptions } from "@nosh/pi-adapter";
import { OrchestrationRuntime } from "@nosh/orchestration-runtime";
import { worktreeGitEnvironment } from "@nosh/git-workspaces";
import { JobSupervisor, type JobRecord, type JobSpec } from "@nosh/jobs";
import { ArtifactStore, amendProjectContract, approveProjectContract, commitProjectContract, detectRunnableCommands, ensureNoshIgnore, initializeResearchProject, readProjectContract, type ProjectContract, type ProjectInitialization } from "@nosh/evidence";
import { parseModelSelection, canonicalJson, schemaDocumentPath, validateRecord, episodeDraftSchema, scopeSchema, isTaskTerminalRecord, schemaUri, sha256, type EventDraft, type EventEnvelope, type JsonValue, type ModelSelection, type TaskPermissions, type TaskWorkspace } from "@nosh/wire";
import { SingleInstanceLock } from "./single-instance.js";
import { gitChangedPaths, gitWorktreeState } from "./task-postflight.js";
import { taskWorktreePath } from "./task-worktree.js";
import { ResearchControl } from "./research-control.js";
import { MissionSupervisor } from "./mission-supervisor.js";
import { DirectionSupervisor } from "./direction-supervisor.js";
import { AutoresearchSupervisor } from "./autoresearch-supervisor.js";
import { buildResearchMap, type ResearchMap } from "./research-map.js";

export type NoshDaemonOptions = {
  dataDirectory: string;
  bootstrapToken: string;
  packagePath?: string;
  maximumActivePiSessions?: number;
  /** Model for every session without an explicit selection (from NOSH config, not Pi's shared global settings). */
  defaultModel?: ModelSelection;
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
    this.agents.defaultModel = options.defaultModel;
    const packagePath = options.packagePath ?? resolve(import.meta.dirname, "..", "..", "..", "pi-package");
    this.runtime = new OrchestrationRuntime({ storeFor: (projectId) => { this.assertProjectWritable(projectId); return this.storeFor(projectId); }, projectFor: (projectId) => { const project = this.projects().find((entry) => entry.projectId === projectId); if (!project) throw new Error(`Project ${projectId} is not registered on this host`); return project; }, sessions: this.agents, packagePath, publish: (event) => this.events.emit("event", event) });
    this.missions = new MissionSupervisor(this.research, this.agents, () => this.projects(), packagePath, (event) => this.appendDraft(event), this.runtime);
    // Jobs are constructed below; resolve lazily so baseline evaluation uses the live supervisor.
    this.directions = new DirectionSupervisor(this.research, this.agents, () => this.projects(), packagePath, (event) => this.appendDraft(event), this.runtime, { start: (spec) => this.startJob(spec), get: (jobId) => this.jobs.get(jobId) });
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
      // Projects opened before NOSH ignored its own runtime state get the ignore file on the next start.
      for (const project of this.projects()) { try { ensureNoshIgnore(project.repositoryRoot); } catch { /* An unreachable repository is reported by reconciliation. */ } this.reconcileProject(project); }
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
  /** User-only: write the next approved contract version (e.g. declaring nosh_run commands). Agents cannot reach this route. */
  amendProjectContract(projectId: string, input: unknown, idempotencyKey: string): ProjectContract {
    this.assertProjectWritable(projectId); const project = this.projects().find((entry) => entry.projectId === projectId); if (!project) throw new Error(`Project ${projectId} is not registered on this host`);
    const contract = amendProjectContract(project.repositoryRoot, input);
    this.recordContractCommit(projectId, project.repositoryRoot, `${idempotencyKey}:contract-commit`);
    this.appendDraft({ $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "project.contract_amended", source: "user", scope: { projectId, missionId: null, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: null }, correlationId: idempotencyKey, causationId: null, payload: { contractVersion: contract.contractVersion, commandIds: contract.execution?.commands.map((command) => command.commandId) ?? [] } });
    return contract;
  }
  /** Approved contract files are committed at once so the next task preflight sees a clean checkout; the outcome is an event either way. */
  private recordContractCommit(projectId: string, repositoryRoot: string, correlationId: string): void {
    const outcome = commitProjectContract(repositoryRoot);
    this.appendDraft({ $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: outcome.committed ? "project.contract_committed" : "project.contract_commit_skipped", source: "noshd", scope: { projectId, missionId: null, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: null }, correlationId, causationId: null, payload: outcome });
  }
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
    await this.agents.prompt(agent.agentId, intakePrompt(contract, project.repositoryRoot));
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
    if (frozen) return this.submitTerminal(context, frozen, true);
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

  async submitTerminal(context: TerminalContext, submitted: Record<string, JsonValue>[], frozen = false): Promise<TerminalReceipt> {
    let records = submitted;
    if (!frozen) {
      try { records = this.withDaemonFacts(context, submitted); }
      catch (error) { return this.rejectTerminal(context, error instanceof Error ? error.message : "Terminal policy: daemon facts failed"); }
    }
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
        catch (error) { const message = error instanceof Error ? error.message : "Terminal validation failed"; // A mis-copied ID of the session's own scope is a correctable model slip; the corrected record is checked by the same policy.
          const correctable = message.startsWith("Invalid terminal record:") || message.startsWith("Unexpected terminal episode type") || message.startsWith("Episode references") || message.startsWith("Changed file escapes") || message.startsWith("Terminal output requires") || message.startsWith("Scope field mismatch:"); return this.rejectTerminalAttempt(context, correctable ? message : `Terminal policy: ${message}`); }
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
        if (record[field] !== undefined && record[field] !== agent[field]) throw new Error(`Scope field mismatch: ${field} must be exactly ${agent[field]}`);
        if (isObject(record.scope) && record.scope[field] !== undefined && record.scope[field] !== agent[field]) throw new Error(`Scope field mismatch: scope.${field} must be exactly ${agent[field]}`);
      }
      if (schema === schemaUri("episode-draft")) {
        episodes++;
        if (!context.expectedEpisodeType || record.episodeType !== context.expectedEpisodeType) throw new Error("Unexpected terminal episode type");
        this.runtime.validateDraft(context.projectId, episodeDraftSchema.parse(record), context.agentId);
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
    const agent = agentId ? this.agents.inspect().find((entry) => entry.agentId === agentId && entry.projectId === projectId) : undefined; if (agentId && !agent) return { accepted: false, retryAllowed: false, errors: [{ pointer: "/", code: "scope_policy", message: "Submitting Pi session is no longer active in this Project" }] }; if (tool === "nosh_project_contract_submit") { if (agent?.role !== "nosh") return { accepted: false, retryAllowed: false, errors: [{ pointer: "/", code: "scope_policy", message: "Only the Project Nosh session may submit the approved Project contract" }] }; if (JSON.stringify(result.record).includes('"EDIT:')) return { accepted: false, retryAllowed: true, errors: [{ pointer: "/", code: "template_placeholder", message: "Replace every \"EDIT: …\" placeholder of the successor template with the agreed content before submitting" }] }; const project = this.projects().find((entry) => entry.projectId === projectId)!; try { approveProjectContract(project.repositoryRoot, result.record); this.recordContractCommit(projectId, project.repositoryRoot, `${attemptKey}:contract-commit`); } catch (error) { return { accepted: false, retryAllowed: false, errors: [{ pointer: "/", code: "contract_policy", message: error instanceof Error ? error.message : "Project contract approval failed" }] }; } }
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
    if (tool === "nosh_run") return this.runTaskCommand(projectId, agentId, record);
    if (tool === "nosh_progress_note") return this.recordProgressNote(projectId, attemptKey, agentId, record);
    if (tool === "nosh_artifact_read") return this.readTaskArtifact(projectId, agentId, record);
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
    // Reject an invalid active contract before changing a session or persisting chat.
    // Completed requests still replay above, even if the contract later changes.
    const intake = this.projectContract(projectId);
    await this.validateModelSelection(selection);
    let agent = this.agents.inspect().find((entry) => entry.projectId === projectId && entry.role === "nosh");
    // A session started before any model account was connected has no model; start afresh so it picks up `nosh login`.
    if (agent && agent.status !== "running" && (!agent.modelId || agent.modelId === "unknown")) { this.agents.stop(agent.agentId); agent = undefined; }
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
    // A fresh Pi session (daemon restart, model change) has no memory: restore the recent conversation so
    // discovery and chat continue where they left off instead of starting over.
    const history = fresh ? this.chatHistory(projectId, agent.agentId, stored.receipt.event.sequence ?? Number.MAX_SAFE_INTEGER) : "";
    const restored = history ? `Conversation so far in this Project (restored after a session restart; most recent last):\n${history}\n\n` : "";
    const prompt = fresh && !intake.approvedAt ? `${intakePrompt(intake, project.repositoryRoot)}\n\n${restored}The user's ${history ? "next" : "first"} Project-discovery response is:\n${text}` : `${restored}${text}`;
    const action = agent.status === "running" ? this.agents.followUp(agent.agentId, prompt) : this.agents.prompt(agent.agentId, prompt);
    void action.catch((error) => this.appendDraft({ $schema: "https://nosh.dev/schemas/event/v1", schemaVersion: 1, retention: "persistent", type: "agent.failed", source: "pi", scope: { projectId, missionId: null, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: agent!.agentId }, correlationId: idempotencyKey, causationId: stored.receipt.event.eventId, payload: { message: error instanceof Error ? error.message : "Chat agent failed" } }));
    return { event: stored.receipt.event, agentId: agent.agentId, replayed: false };
  }
  /** The Project chat before `beforeSequence`, newest turns kept within a character budget. */
  private chatHistory(projectId: string, agentId: string, beforeSequence: number, maximumCharacters = 12_000): string {
    const turns = this.storeFor(projectId).replay(projectId).filter((event) => (event.sequence ?? 0) < beforeSequence && isObject(event.payload) && (event.type === "chat.user_message" || (event.type === "agent.completed" && event.scope.agentId === agentId && !event.correlationId)))
      .map((event) => `${event.type === "chat.user_message" ? "User" : "NOSH"}: ${String((event.payload as Record<string, JsonValue>).message ?? "").trim().slice(0, 2_000)}`)
      .filter((turn) => !turn.endsWith(": "));
    const kept: string[] = []; let size = 0;
    for (const turn of turns.reverse()) { if (size + turn.length > maximumCharacters) break; kept.unshift(turn); size += turn.length + 1; }
    return kept.join("\n");
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
    const mission = this.research.assertInspectedMissionState(projectId, missionId, expectedVersion); const text = message.trim(); if (mission.state !== "running") throw new Error("Only a running Mission may be steered"); if (!text) throw new Error("Mission steering message is required"); const directors = this.agents.inspect().filter((agent) => agent.projectId === projectId && agent.missionId === missionId && ["mission_director", "research_director"].includes(agent.role)); // Steering is durable: it reaches any live Director now and every later Director cycle through the Mission brief.
    await Promise.all(directors.map((agent) => this.agents.steer(agent.agentId, text))); this.appendDraft({ $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "mission.steering_applied", source: "user", scope: { projectId, missionId, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: null }, correlationId: idempotencyKey, causationId: null, payload: { message: text, directorAgentIds: directors.map((agent) => agent.agentId) } });
  }

  async controlMission(projectId: string, missionId: string, expectedVersion: number, action: "pause" | "resume" | "stop", mode: "safe" | "checkpoint" | "immediate", idempotencyKey: string) {
    const mission = this.research.assertInspectedMissionState(projectId, missionId, expectedVersion); const agents = this.agents.inspect().filter((agent) => agent.projectId === projectId && agent.missionId === missionId);
    if (action === "resume") return this.research.transitionMission(projectId, missionId, mission.version, "running", `${idempotencyKey}:running`);
    if (action === "pause") { const pausing = this.research.transitionMission(projectId, missionId, mission.version, "pausing", `${idempotencyKey}:pausing`); if (mode === "immediate") await Promise.all(agents.map((agent) => this.agents.abort(agent.agentId))); else await Promise.all(agents.map((agent) => this.agents.steer(agent.agentId, "Pause at the next safe boundary and persist a canonical handoff."))); if (mode === "checkpoint") for (const job of this.jobs.list().filter((entry) => missionJobMatches(entry, projectId, missionId))) this.jobs.checkpoint(job.jobId); return mode === "immediate" || !agents.length ? this.research.transitionMission(projectId, missionId, pausing.version, "paused", `${idempotencyKey}:paused`) : pausing; }
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

  /**
   * Daemon-owned facts a model would otherwise spend tool calls on, filled into a task's terminal records before
   * validation: commands[] (its nosh_run Jobs), "artifact:<path>" citations (snapshotted as Artifacts), codeChanges
   * (the daemon commits the worktree on the worker's behalf), and the episode's changedFiles.
   * Throws a correctable reason for a bad citation and a "Terminal policy:" reason when the commit itself fails.
   */
  private withDaemonFacts(context: TerminalContext, submitted: Record<string, JsonValue>[]): Record<string, JsonValue>[] {
    let records = submitted.map((record) => withSourceUrls(this.withDaemonCommands(context, record)));
    const agent = context.taskId ? this.agents.inspect().find((entry) => entry.agentId === context.agentId && entry.projectId === context.projectId && entry.taskId === context.taskId) : undefined;
    const packet = agent ? this.taskPacketFacts(context.projectId, agent.taskId!) : undefined;
    if (!agent || !packet?.workspace) return records;
    records = records.map((record) => this.withEvidenceSources(context.projectId, agent, record));
    // One Artifact per cited path per envelope, with its most specific kind (a report cited again in the episode is the same report).
    const kinds = new Map<string, string>();
    for (const record of records) collectArtifactCitations(record, String(record.$schema), "", kinds);
    const resolved = new Map([...kinds].map(([path, kind]) => [path, this.snapshotTaskArtifact(context.projectId, agent, packet, path, kind)]));
    records = records.map((record) => replaceArtifactCitations(record, resolved) as Record<string, JsonValue>);
    records = records.map((record) => this.withRunArtifacts(context.projectId, agent, record));
    const completion = records.find((record) => record.$schema === schemaUri("general-worker-completion"));
    if (!completion || !packet.permissions?.gitCommit) return records;
    try { this.commitTaskWorktree(context.projectId, agent, packet, `nosh: ${agent.taskId} completion`); }
    catch (error) { throw new Error(`Terminal policy: daemon commit of your worktree failed: ${error instanceof Error ? error.message : "unknown error"}`); }
    const worktree = this.taskWorktree(context.projectId, packet.workspace);
    const endingCommit = gitWorktreeState(worktree).head; const changedPaths = gitChangedPaths(worktree, packet.workspace.startingCommit, endingCommit);
    const diffArtifactId = changedPaths.length ? this.snapshotTaskDiff(context.projectId, agent, worktree, packet.workspace.startingCommit, endingCommit) : null;
    const codeChanges = { ...(isObject(completion.codeChanges) ? completion.codeChanges : {}), startingCommit: packet.workspace.startingCommit, endingCommit, changedPaths, diffArtifactId, branch: packet.workspace.branch };
    return records.map((record) => record === completion ? { ...record, codeChanges } : record.$schema === schemaUri("episode-draft") ? { ...record, changedFiles: changedPaths } : record);
  }

  private taskPacketFacts(projectId: string, taskId: string): TaskPacketFacts | undefined {
    return this.research.records(projectId, "task-packet").filter((entry) => entry.taskId === taskId).at(-1) as TaskPacketFacts | undefined;
  }

  private taskWorktree(projectId: string, workspace: { worktreeId: string }): string {
    return taskWorktreePath(this.projects().find((entry) => entry.projectId === projectId)!.repositoryRoot, workspace.worktreeId);
  }

  /** Workers have no commit tool: the daemon commits every worktree change under the task's commit authority. Null when unauthorized or clean. */
  private commitTaskWorktree(projectId: string, agent: AgentInspection, packet: TaskPacketFacts, message: string): { commit: string; paths: string[] } | null {
    if (!packet.permissions?.gitCommit || !packet.workspace) return null;
    return commitAllWorkspaceChanges({ projectId, missionId: agent.missionId, directionId: agent.directionId, autoresearchId: agent.autoresearchId, experimentId: agent.experimentId, runId: null, jobId: null, taskId: agent.taskId, agentId: agent.agentId, role: agent.role, cwd: this.taskWorktree(projectId, packet.workspace), packagePath: "", taskPermissions: packet.permissions, taskWorkspace: packet.workspace }, message);
  }

  /** A `PROGRESS: done -> next` line in a live task's assistant text, recorded as a daemon-built progress-update (no model tool call). */
  private recordProgressNote(projectId: string, attemptKey: string, agentId: string | undefined, record: unknown): unknown {
    const agent = agentId ? this.agents.inspect().find((entry) => entry.agentId === agentId && entry.projectId === projectId) : undefined;
    const packet = agent?.taskId ? this.taskPacketFacts(projectId, agent.taskId) : undefined;
    const note = String((record as { note?: unknown } | null)?.note ?? "").trim();
    if (!agent?.taskId || !packet || attemptKey !== `task:${agent.taskId}` || !note) return { accepted: false, error: "Progress notes require an active daemon-bound task session" };
    const [summary = note, next = ""] = note.split(/\s*->\s*/, 2);
    const ordinal = this.research.submitted(projectId, attemptKey).filter((entry) => (entry.record as { $schema?: string }).$schema === schemaUri("progress-update")).length + 1;
    const progress = { $schema: schemaUri("progress-update"), schemaVersion: 1, progressId: `progress_${agent.taskId.slice(4)}_${ordinal}`, taskId: agent.taskId, attempt: packet.attempt, agentId: agent.agentId, kind: "milestone", summary: summary.slice(0, 2_000), goalStackIds: [agent.taskId], durableDelta: { commitIds: [], artifactIds: [], evidenceIds: [], graphNodeStateChanges: [], closedDefectIds: [] }, validation: [], currentOperation: summary.slice(0, 1_000), nextOperation: next.slice(0, 1_000), estimatedRemainingSeconds: null, newRisk: null, blockerId: null, attemptFingerprint: sha256({ taskId: agent.taskId, attempt: packet.attempt }), emittedAt: new Date().toISOString() };
    return this.submitRecord("nosh_progress_emit", projectId, attemptKey, progress, agent.agentId);
  }

  /**
   * Each nosh_run of the task becomes a hash-checked Artifact (command, commit, exit code, output tail): commands[]
   * cite it, and so does every criterion whose validator is that Job. Downstream reviewers, who cannot run commands,
   * then verify a run from the Artifact set they are required to read.
   */
  private withRunArtifacts(projectId: string, agent: AgentInspection, record: Record<string, JsonValue>): Record<string, JsonValue> {
    if (record.$schema !== schemaUri("general-worker-completion")) return record;
    const runs = this.storeFor(projectId).replay(projectId).filter((event) => event.type === "task.command_run" && isObject(event.payload) && event.payload.taskId === agent.taskId).map((event) => event.payload as Record<string, JsonValue>);
    if (!runs.length) return record;
    const byJob = new Map(runs.map((run) => {
      const directory = mkdtempSync(join(tmpdir(), "nosh-run-")); const file = join(directory, "run.json");
      try { writeFileSync(file, `${canonicalJson(run)}\n`, "utf8"); return [String(run.jobId), this.recordTaskArtifact(projectId, agent, file, `art_${sha256({ taskId: agent.taskId!, jobId: run.jobId }).slice(7, 39)}`, "artifact_run-result", "application/json", { type: "noshd", taskId: agent.taskId!, jobId: run.jobId })] as const; }
      finally { rmSync(directory, { recursive: true, force: true }); }
    }));
    const latest = new Map(latestRunPerCommand(runs).map((run) => [String(run.commandId), String(run.jobId)]));
    const commands = Array.isArray(record.commands) ? record.commands.map((command) => isObject(command) ? { ...command, resultArtifactId: byJob.get(latest.get(String(command.commandId)) ?? "") ?? null } : command) : record.commands;
    const criteria = Array.isArray(record.criteria) ? record.criteria.map((criterion) => {
      if (!isObject(criterion) || !Array.isArray(criterion.validatorRunIds)) return criterion;
      const cited = criterion.validatorRunIds.map((id) => byJob.get(String(id))).filter((id): id is string => Boolean(id));
      const artifactIds = Array.isArray(criterion.artifactIds) ? criterion.artifactIds.map(String) : [];
      return { ...criterion, artifactIds: [...new Set([...artifactIds, ...cited])] };
    }) : record.criteria;
    return { ...record, ...(commands === undefined ? {} : { commands }), ...(criteria === undefined ? {} : { criteria }) };
  }

  /** commands[] in a worker completion is factual and daemon-owned: when the task ran nosh_run, it is exactly those runs (models kept inventing its shape). */
  private withDaemonCommands(context: TerminalContext, record: Record<string, JsonValue>): Record<string, JsonValue> {
    if (record.$schema !== schemaUri("general-worker-completion") || !context.taskId) return record;
    const runs = latestRunPerCommand(this.storeFor(context.projectId).replay(context.projectId).filter((event) => event.type === "task.command_run" && isObject(event.payload) && event.payload.taskId === context.taskId).map((event) => event.payload as { commandId?: string; argv?: string[]; exitCode?: number | null }));
    if (!runs.length) return record;
    return { ...record, commands: runs.map((run) => ({ commandId: String(run.commandId), displayCommand: (run.argv ?? []).join(" ").slice(0, 2_000), exitCode: typeof run.exitCode === "number" ? run.exitCode : null, resultArtifactId: null })) };
  }

  /** Runs one command declared in the approved Project contract for a live task session, as a supervised Job in its worktree. The agent never spawns processes. */
  private async runTaskCommand(projectId: string, agentId: string | undefined, record: unknown): Promise<unknown> {
    try {
      this.assertProjectWritable(projectId);
      const agent = agentId ? this.agents.inspect().find((entry) => entry.agentId === agentId && entry.projectId === projectId) : undefined;
      if (!agent?.taskId) throw new Error("nosh_run requires an active daemon-bound task session");
      const taskId = agent.taskId;
      const packet = this.taskPacketFacts(projectId, taskId);
      if (packet?.permissions?.subprocess !== "allowlisted" || !packet.permissions.allowedToolIds?.includes("tool_nosh.run") || !packet.workspace?.worktreeId) throw new Error("This task is not authorized to run commands");
      const project = this.projects().find((entry) => entry.projectId === projectId)!;
      const execution = readProjectContract(project.repositoryRoot).execution;
      const commandId = (record as { commandId?: unknown } | null)?.commandId;
      const command = execution?.commands.find((entry) => entry.commandId === commandId);
      if (!execution || !command) throw new Error(`Unknown commandId; the approved Project contract declares: ${execution?.commands.map((entry) => entry.commandId).join(", ") || "none"}`);
      const worktree = taskWorktreePath(project.repositoryRoot, packet.workspace.worktreeId);
      // Each run is tied to an exact commit: the daemon commits the worker's current edits first (workers have no commit tool).
      try { this.commitTaskWorktree(projectId, agent, packet, `nosh: ${taskId} edits before ${command.commandId}`); }
      catch (error) { throw new Error(`Could not commit your edits before the run: ${error instanceof Error ? error.message : "unknown error"}`); }
      const { head, dirty } = gitWorktreeState(worktree);
      // The same contract command on the same clean commit has already been measured: answer from the record at once
      // instead of re-running it (models re-check results they already have; the harness should not pay for it twice).
      const prior = dirty ? undefined : this.storeFor(projectId).replay(projectId).filter((event) => event.type === "task.command_run" && isObject(event.payload) && event.payload.taskId === taskId && event.payload.commandId === command.commandId && event.payload.commit === head && event.payload.dirty === false).at(-1)?.payload as Record<string, JsonValue> | undefined;
      if (prior) return { accepted: true, ...prior, cached: true, citable: prior.state === "completed" && prior.exitCode === 0, note: `Already ran ${command.commandId} on this exact commit; returning that recorded result (cite ${String(prior.jobId)}).` };
      const jobId = createId("job");
      this.startJob({ jobId, projectId, missionId: agent.missionId, directionId: agent.directionId, autoresearchId: agent.autoresearchId, runId: createId("run"), experimentId: agent.experimentId, commitSha: head, workingDirectory: worktree, runner: execution.runner, distribution: null, command: command.argv, checkpointCommand: null, environmentLockHash: sha256({ node: process.version, platform: process.platform, arch: process.arch }), evaluationContractHash: sha256(execution as unknown as JsonValue), timeoutSeconds: command.timeoutSeconds, usesGpu: false });
      const terminal = ["completed", "failed", "cancelled", "lost"]; let job = this.jobs.get(jobId); const deadline = Date.now() + (command.timeoutSeconds + 60) * 1_000;
      while (!terminal.includes(job.state) && Date.now() < deadline) { await new Promise((done) => setTimeout(done, 250)); job = this.jobs.get(jobId); }
      // The record keeps a bounded stdout tail so reviewers can verify what a run printed without the logs.
      const result = { taskId, jobId, commandId: command.commandId, description: command.description, argv: command.argv, commit: head, dirty, state: job.state, exitCode: job.exitCode, failureReason: job.failureReason, stdoutTail: this.jobs.tail(jobId, "stdout", 1_500) };
      this.appendDraft({ $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "task.command_run", source: "noshd", scope: { projectId, missionId: agent.missionId, directionId: agent.directionId, autoresearchId: agent.autoresearchId, experimentId: agent.experimentId, runId: null, jobId, agentId: agent.agentId }, correlationId: `task:${taskId}`, causationId: null, payload: result as unknown as JsonValue });
      return { accepted: true, ...result, citable: job.state === "completed" && job.exitCode === 0 && !dirty, stderrTail: this.jobs.tail(jobId, "stderr", 2_000) };
    } catch (error) { return { accepted: false, error: error instanceof Error ? error.message : "nosh_run failed" }; }
  }

  /** Read-only view of one Project Artifact for a live task session (reviewers verify what workers registered). Hash-checked; text only, bounded. */
  private readTaskArtifact(projectId: string, agentId: string | undefined, record: unknown): unknown {
    try {
      const agent = agentId ? this.agents.inspect().find((entry) => entry.agentId === agentId && entry.projectId === projectId) : undefined;
      if (!agent?.taskId) throw new Error("nosh_artifact_read requires an active daemon-bound task session");
      const input = record as { artifactId?: unknown; offset?: unknown } | null; const offset = typeof input?.offset === "number" && input.offset > 0 ? Math.floor(input.offset) : 0;
      if (typeof input?.artifactId === "string" && /^evd_[0-9a-f]+$/.test(input.artifactId)) {
        // Review Requests cite Evidence by evd_ id; return the record so the reviewer can follow it to its art_ Artifacts.
        const recorded = this.research.records(projectId, "evidence").find((entry) => (entry as { evidenceId?: unknown }).evidenceId === input.artifactId);
        // A candidate frozen for an open Review is readable too; it is recorded with that Review's disposition.
        const evidence = recorded ?? this.research.candidateEvidence(projectId, input.artifactId);
        if (!evidence) throw new Error(`Evidence ${input.artifactId} is not recorded in this Project`);
        const text = JSON.stringify(evidence, null, 2); const content = text.slice(offset, offset + 12_000);
        return { accepted: true, evidenceId: input.artifactId, kind: recorded ? "evidence" : "evidence_candidate", mediaType: "application/json", offset, content, remainingCharacters: Math.max(0, text.length - offset - content.length) };
      }
      if (typeof input?.artifactId !== "string" || !/^art_[0-9a-f]+$/.test(input.artifactId)) throw new Error("artifactId (art_... or an evd_... Evidence id) is required");
      const project = this.projects().find((entry) => entry.projectId === projectId)!;
      const artifact = new ArtifactStore(join(project.repositoryRoot, ".nosh", "artifacts")).resolve(input.artifactId);
      if (artifact.projectId !== projectId) throw new Error("Artifact belongs to another Project");
      const text = readFileSync(artifact.storedPath, "utf8"); const content = text.slice(offset, offset + 12_000);
      return { accepted: true, artifactId: artifact.artifactId, kind: artifact.kind, mediaType: artifact.mediaType, contentHash: artifact.contentHash, sizeBytes: artifact.sizeBytes, offset, content, remainingCharacters: Math.max(0, text.length - offset - content.length) };
    } catch (error) { return { accepted: false, error: error instanceof Error ? error.message : "nosh_artifact_read failed" }; }
  }

  /**
   * Snapshots one regular file from a live task's own worktree into the Artifact store and records it. The ID is
   * derived from task, path, and content, so a corrected or replayed citation of the same file resolves to the same Artifact.
   */
  private snapshotTaskArtifact(projectId: string, agent: AgentInspection, packet: TaskPacketFacts, path: string, kind: string): string {
    this.assertProjectWritable(projectId);
    const worktree = this.taskWorktree(projectId, packet.workspace!);
    const target = resolve(worktree, path); const inside = relative(worktree, target).replaceAll("\\", "/");
    if (!inside || inside.startsWith("..") || isAbsolute(inside) || inside.split("/").includes(".git")) throw new Error(`Artifact citation artifact:${path} must name a relative file inside your worktree, outside .git`);
    let stat; try { stat = lstatSync(target); } catch { throw new Error(`Artifact citation artifact:${path} names no file in your worktree; cite a file you wrote or read`); }
    if (!stat.isFile() || stat.nlink !== 1) throw new Error(`Artifact citation artifact:${path} must be a regular, unlinked file`); if (stat.size > 5_000_000) throw new Error(`Artifact citation artifact:${path} exceeds 5 MB`);
    const mediaType = /\.md$/i.test(inside) ? "text/markdown" : /\.json$/i.test(inside) ? "application/json" : /\.bib$/i.test(inside) ? "application/x-bibtex" : "text/plain";
    const digest = `sha256:${createHash("sha256").update(readFileSync(target)).digest("hex")}`;
    return this.recordTaskArtifact(projectId, agent, target, `art_${sha256({ taskId: agent.taskId!, path: inside, digest, kind }).slice(7, 39)}`, kind, mediaType, { type: "agent", agentId: agent.agentId, taskId: agent.taskId!, path: inside });
  }

  /** The exact code change of a worker's completion (starting..ending commit) as a daemon-produced Artifact, so reviewers judge the change itself. */
  private snapshotTaskDiff(projectId: string, agent: AgentInspection, worktree: string, startingCommit: string, endingCommit: string): string {
    const diff = spawnSync("git", ["-C", worktree, "diff", "--no-color", "--no-ext-diff", "--find-renames", startingCommit, endingCommit], { encoding: "utf8", windowsHide: true, maxBuffer: 20_000_000, env: worktreeGitEnvironment(worktree) });
    if (diff.status !== 0) throw new Error(diff.stderr.trim() || "git diff failed");
    const limit = 1_000_000; const content = diff.stdout.length > limit ? `${diff.stdout.slice(0, limit)}\n[diff truncated by NOSH at ${limit} characters]\n` : diff.stdout;
    const directory = mkdtempSync(join(tmpdir(), "nosh-diff-")); const file = join(directory, "change.diff");
    try {
      writeFileSync(file, content, "utf8");
      return this.recordTaskArtifact(projectId, agent, file, `art_${sha256({ taskId: agent.taskId!, startingCommit, endingCommit }).slice(7, 39)}`, "artifact_diff", "text/x-diff", { type: "noshd", taskId: agent.taskId!, startingCommit, endingCommit });
    } finally { rmSync(directory, { recursive: true, force: true }); }
  }

  /** A librarian source may cite recorded Evidence (evd_): it becomes a snapshot Artifact of that record, at nosh://evidence/<id>. */
  private withEvidenceSources(projectId: string, agent: AgentInspection, record: Record<string, JsonValue>): Record<string, JsonValue> {
    if (record.$schema !== schemaUri("librarian-completion") || !Array.isArray(record.sources)) return record;
    const sources = record.sources.map((source) => {
      if (!isObject(source) || typeof source.artifactId !== "string" || !/^evd_[0-9a-f]+$/.test(source.artifactId)) return source;
      const evidenceId = source.artifactId;
      const evidence = this.research.records(projectId, "evidence").find((entry) => (entry as { evidenceId?: unknown }).evidenceId === evidenceId);
      if (!evidence) throw new Error(`Source cites ${evidenceId}, which is not recorded Evidence in this Project`);
      const directory = mkdtempSync(join(tmpdir(), "nosh-evidence-")); const file = join(directory, `${evidenceId}.json`);
      try {
        writeFileSync(file, `${JSON.stringify(evidence, null, 2)}\n`, "utf8");
        const artifactId = this.recordTaskArtifact(projectId, agent, file, `art_${sha256({ evidenceId, taskId: agent.taskId! }).slice(7, 39)}`, "artifact_source", "application/json", { type: "noshd", taskId: agent.taskId!, evidenceId });
        return { ...source, artifactId, canonicalUrl: typeof source.canonicalUrl === "string" && source.canonicalUrl.startsWith("http") ? source.canonicalUrl : `nosh://evidence/${evidenceId}` };
      } finally { rmSync(directory, { recursive: true, force: true }); }
    });
    return { ...record, sources };
  }

  private recordTaskArtifact(projectId: string, agent: AgentInspection, sourcePath: string, artifactId: string, kind: string, mediaType: string, producer: JsonValue): string {
    const project = this.projects().find((entry) => entry.projectId === projectId)!;
    const artifact = new ArtifactStore(join(project.repositoryRoot, ".nosh", "artifacts")).add({ artifactId, projectId, kind, mediaType, sourcePath, retentionClass: "accepted_evidence" });
    const scope = { missionId: agent.missionId, directionId: agent.directionId, autoresearchId: agent.autoresearchId, experimentId: agent.experimentId };
    const artifactRecord: JsonValue = { $schema: schemaUri("artifact"), schemaVersion: 1, artifactId, projectId, kind, mediaType, contentHash: artifact.contentHash, sizeBytes: artifact.sizeBytes, version: artifact.version, producer, scope, git: null, evaluationContractHash: null, retentionClass: "accepted_evidence", remotePreviewPolicy: "encrypted_allowed", redactionStatus: "checked", createdAt: artifact.createdAt };
    this.research.submitDaemonRecord(projectId, { projectId, ...scope, runId: null, jobId: null, agentId: agent.agentId }, `artifact:${artifactId}`, artifactRecord, `task-artifact:${artifactId}`);
    return artifactId;
  }

  startJob(spec: JobSpec): JobRecord {
    if (!this.projects().some((project) => project.projectId === spec.projectId)) throw new Error(`Project ${spec.projectId} is not registered on this host`);
    this.assertProjectWritable(spec.projectId);
    // Job events carry this scope; reject malformed IDs before a record or intent exists,
    // otherwise the saved Job stays "starting" forever when its first event is rejected.
    scopeSchema.parse({ projectId: spec.projectId, missionId: spec.missionId, directionId: spec.directionId, autoresearchId: spec.autoresearchId, experimentId: spec.experimentId, runId: spec.runId, jobId: spec.jobId, agentId: null });
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

  /** The default for every new session without an explicit selection (the CLI's `nosh model set`); running sessions keep theirs. */
  async setDefaultModel(model: string): Promise<ModelSelection> {
    const selection = parseModelSelection(model);
    await this.agents.validateModelSelection(selection);
    this.agents.defaultModel = selection;
    return selection;
  }

  /** What the Project is doing right now, as one tree (Missions, Directions, Autoresearch, workers, Jobs) for visual clients. */
  researchMap(projectId: string): ResearchMap {
    if (!this.projects().some((project) => project.projectId === projectId)) throw new Error(`Project ${projectId} is not registered on this host`);
    const events = this.storeFor(projectId).replay(projectId);
    const submitted = events.filter((event) => event.type === "record.submitted" && isObject(event.payload)).map((event) => ({ event, record: event.payload as Record<string, unknown> }));
    return buildResearchMap({ missions: this.research.missions(projectId), directions: this.research.directions(projectId), autoresearch: this.research.autoresearch(projectId), submitted, events, agents: this.agents.inspect().filter((agent) => agent.projectId === projectId), jobs: this.jobs.list().filter((job) => job.projectId === projectId) });
  }

  replay(projectId: string, afterSequence: number): EventEnvelope[] {
    return this.storeFor(projectId).replay(projectId, afterSequence);
  }

  replayPage(projectId: string, afterSequence = 0, limit = 300, recent = false): { events: EventEnvelope[]; nextCursor: number; hasMore: boolean } {
    return this.storeFor(projectId).replayPage(projectId, afterSequence, limit, recent);
  }

  private appendDraft(draft: EventDraft): EventEnvelope {
    this.assertProjectWritable(draft.scope.projectId);
    const event = this.storeFor(draft.scope.projectId).append(draft);
    this.events.emit("event", event);
    return event;
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

  /** Projects with non-terminal agents, Jobs, threads, Missions, Directions, or Autoresearch; `nosh stop --if-idle` reads this before an update. */
  activity(): Array<{ projectId: string; active: string[] }> {
    return this.projects().map((project) => ({ projectId: project.projectId, active: this.activeProjectState(project.projectId) })).filter((entry) => entry.active.length > 0);
  }

  private backupActivityReason(projectId: string): string | null {
    const active = this.activeProjectState(projectId);
    return active.length ? `Backup rejected: active Project state (${active.join(", ")})` : null;
  }

  private activeProjectState(projectId: string): string[] {
    this.maintenanceInspectionProject = projectId;
    try {
      const agents = this.agents.inspect().filter((agent) => agent.projectId === projectId && agent.status !== "idle").map((agent) => `${agent.agentId}:${agent.status}`);
      const jobs = this.jobs.list().filter((job) => job.projectId === projectId && ["queued", "starting", "running", "checkpointing", "finishing"].includes(job.state)).map((job) => `${job.jobId}:${job.state}`);
      const threads = this.runtime.threads(projectId).filter((thread) => ["open", "running", "awaiting", "awaiting_user", "paused"].includes(thread.value.state)).map((thread) => `${thread.entityId}:${thread.value.state}`);
      const missions = this.research.missions(projectId).filter((mission) => !["draft", "completed", "stopped", "failed"].includes(mission.state)).map((mission) => `${mission.entityId}:${mission.state}`);
      const directions = this.research.directions(projectId).filter((direction) => !["draft", "closed", "rejected", "stopped"].includes(direction.state)).map((direction) => `${direction.entityId}:${direction.state}`);
      const executions = this.research.autoresearch(projectId).filter((execution) => !["draft", "completed", "stopped", "failed"].includes(execution.state)).map((execution) => `${execution.entityId}:${execution.state}`);
      const active = [...agents.map((entry) => `agent ${entry}`), ...jobs.map((entry) => `job ${entry}`), ...threads.map((entry) => `runtime thread ${entry}`), ...missions.map((entry) => `Mission ${entry}`), ...directions.map((entry) => `Direction ${entry}`), ...executions.map((entry) => `Autoresearch ${entry}`)];
      return active;
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
    this.appendDraft({ $schema: "https://nosh.dev/schemas/event/v1", schemaVersion: 1, retention: "persistent", type: "recovery.report", source: "noshd", scope: { projectId: project.projectId, missionId: null, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: null }, correlationId: null, causationId: null, payload: { reconciledPaperOperations: papers, recoveredDomainEffects: domainEffects.recovered, failedDomainEffects: domainEffects.failed, recoveredDelegations, rejectedDelegations, reconciledJobLaunches: jobs, resumingRuntimeInstructions: runtime.length, resumingPrograms: programs.length, unresolvedIntents: unresolved } });
  }
}

export function missionJobMatches(job: Pick<JobRecord, "projectId" | "missionId">, projectId: string, missionId: string): boolean {
  return job.projectId === projectId && job.missionId === missionId;
}
// Generated by `pnpm schemas`; shipped with the release under packages/wire/src/schemas.
const PROJECT_CONTRACT_SCHEMA = schemaDocumentPath("project-contract");
function intakePrompt(contract: ProjectContract, repositoryRoot: string): string {
  const detected = detectRunnableCommands(repositoryRoot);
  // Commands approved here let workers run tests/evaluations as daemon Jobs from the first Direction, with no later amendment.
  const commands = detected.length ? `\n\nRunnable commands detected in this repository (already in the template's execution field): ${JSON.stringify(detected)}. Confirm with the user which are the right validators; remove the others, or remove the whole execution field if none. Keep each argv exactly as detected: Jobs launch executables directly with no shell, so never use npm or npx (shell shims on Windows); call node (or the tool) directly. A command must already work in the repository today; a flag that a later Mission adds belongs in a later contract amendment.` : "\n\nNo runnable test or evaluation commands were detected. If the user names some, add them as the optional execution field: { \"runner\": \"native\", \"commands\": [{ \"commandId\": \"command_test\", \"description\": \"…\", \"argv\": [\"node\", \"--test\"], \"timeoutSeconds\": 120 }] } (argv lists only, never shell strings; npm is not allowed, call node directly).";
  // The draft and a successor template are daemon facts: the model never has to find, read, or reconstruct them.
  return `This Project has a draft, unapproved Project contract for "${contract.workingTitle}". Conduct a facilitated research-discovery conversation: ask exactly one focused question per turn; do not invent requirements; challenge vague, contradictory, unfalsifiable, or overly broad claims; and periodically reflect the shared understanding. Establish the research problem, motivation, decision use, contribution, scope, datasets and licenses, baselines, evaluation and falsification criteria, reproducibility, risks, intended paper outcome, and realistic GPU and time budgets before proposing an approved contract. Current north-star question: ${contract.northStar.question}

Current draft contract (version ${contract.contractVersion}; do not search for the file): ${JSON.stringify(contract)}

When the user explicitly approves the concrete proposal, submit it with nosh_project_contract_submit using exactly this successor template: keep every value that is not an "EDIT: …" string (identity, version, provenance, branch, paper claim policy), replace each "EDIT: …" string with the agreed content, and fill the string lists (domainTags, scope.included/excluded, licensingConstraints, paper.requiredSections) with plain strings. datasets is a list of objects such as {"name": "…", "license": "…", "source": "…"}. Refs (goalId, contributionType, policy values) are a lowercase prefix, one underscore, then lowercase letters, digits, '.', ':' or '-'. For literature reads by the librarian set policies.network to "network_research.allowlisted"; otherwise keep it. Successor template: ${JSON.stringify(contractSuccessorTemplate(contract, detected))}

Only if a field is unclear, the canonical JSON Schema is ${PROJECT_CONTRACT_SCHEMA}.${commands}`;
}

/** The next contract version with daemon facts kept and research content marked for the agent to fill. */
export function contractSuccessorTemplate(contract: ProjectContract, detected: ReturnType<typeof detectRunnableCommands>): JsonValue {
  const pending = (value: string, prompt: string): string => value.startsWith("Pending collaborative") || value.endsWith("_pending") ? `EDIT: ${prompt}` : value;
  const draft = contract as unknown as Record<string, JsonValue>;
  return {
    ...draft, contractVersion: contract.contractVersion + 1, approvedAt: null,
    northStar: { ...contract.northStar, question: pending(contract.northStar.question, "the falsifiable research question"), decisionUse: pending(contract.northStar.decisionUse, "the decision this research informs, and for whom"), contributionType: pending(contract.northStar.contributionType, "a ref such as contribution_empirical.evaluation") },
    domainTags: contract.domainTags.length ? contract.domainTags : ["EDIT: a domain tag"],
    scope: { included: contract.scope.included.length ? contract.scope.included : ["EDIT: what the study covers"], excluded: contract.scope.excluded },
    ...(detected.length && !draft.execution ? { execution: { runner: "native", commands: detected } as unknown as JsonValue } : {}),
  } as JsonValue;
}

function terminalOutcomeTaskId(tool: string, schema: string, attemptKey: string, record: unknown): string | null {
  if (!isTaskTerminalRecord(record) || !((tool === "nosh_response_submit" && schema !== schemaUri("review-verdict")) || (tool === "nosh_review_submit" && schema === schemaUri("review-verdict")) || (tool === "nosh_experiment_propose" && schema === schemaUri("experiment-proposal")))) return null;
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

type TaskPacketFacts = { attempt?: number; permissions?: TaskPermissions; workspace?: TaskWorkspace };

const CITATION = /^artifact:(\S+)$/;
const KIND_RANK = ["artifact_file", "artifact_source", "artifact_report", "artifact_bibliography"];
/** Every whole-string "artifact:<path>" (no spaces) with the most specific kind any of its citing fields implies. */
function collectArtifactCitations(value: JsonValue, schema: string, key: string, kinds: Map<string, string>): void {
  if (typeof value === "string") { const path = CITATION.exec(value)?.[1]; if (path) { const kind = citationKind(key, schema); if (KIND_RANK.indexOf(kind) > KIND_RANK.indexOf(kinds.get(path) ?? "")) kinds.set(path, kind); } return; }
  if (Array.isArray(value)) { for (const item of value) collectArtifactCitations(item, schema, key, kinds); return; }
  if (isObject(value)) for (const [field, item] of Object.entries(value)) collectArtifactCitations(item, schema, field, kinds);
}
/** Replaces each citation with the art_ ID of its snapshotted worktree file. */
function replaceArtifactCitations(value: JsonValue, resolved: Map<string, string>): JsonValue {
  if (typeof value === "string") { const path = CITATION.exec(value)?.[1]; return path ? resolved.get(path) ?? value : value; }
  if (Array.isArray(value)) return value.map((item) => replaceArtifactCitations(item, resolved));
  if (isObject(value)) return Object.fromEntries(Object.entries(value).map(([field, item]) => [field, replaceArtifactCitations(item, resolved)]));
  return value;
}
function citationKind(key: string, schema: string): string {
  if (key === "bibliographyArtifactId") return "artifact_bibliography";
  if (key === "reportArtifactId") return "artifact_report";
  return schema === schemaUri("librarian-completion") ? "artifact_source" : "artifact_file";
}

/**
 * A completion's commands[] is each command's final state: its latest run. Earlier failing runs are the ordinary
 * edit-test iterations of the task (still recorded, and shown to reviewers), not the outcome.
 */
function latestRunPerCommand<T extends { commandId?: unknown }>(runs: T[]): T[] {
  const latest = new Map<string, T>(); for (const run of runs) { const key = String(run.commandId); latest.delete(key); latest.set(key, run); }
  return [...latest.values()];
}

/**
 * A literature source's canonical URL is derivable when the model omits it: a cited worktree file is file:///<path>,
 * a DOI is https://doi.org/<doi>. A missing derivable field must not void an otherwise valid review.
 */
function withSourceUrls(record: Record<string, JsonValue>): Record<string, JsonValue> {
  if (record.$schema !== schemaUri("librarian-completion") || !Array.isArray(record.sources)) return record;
  const sources = record.sources.map((source) => {
    if (!isObject(source) || (typeof source.canonicalUrl === "string" && source.canonicalUrl)) return source;
    const cited = typeof source.artifactId === "string" ? /^artifact:(\S+)$/.exec(source.artifactId)?.[1] : undefined;
    const doi = typeof source.persistentId === "string" ? /^(?:doi:|https?:\/\/(?:dx\.)?doi\.org\/)(10\.\S+)$/i.exec(source.persistentId)?.[1] : undefined;
    const url = cited ? `file:///${cited.replace(/^\.?\//, "")}` : doi ? `https://doi.org/${doi}` : undefined;
    return url ? { ...source, canonicalUrl: url } : source;
  });
  return { ...record, sources };
}
