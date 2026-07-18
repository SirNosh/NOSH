import { createId, nowUtc } from "@nosh/core";
import { createEpisode, episodeIntegrity, fallbackEpisodeDraft } from "@nosh/episodes";
import { type EntityProjection, type EventStore } from "@nosh/persistence";
import type { PiAdapter, PiSessionOptions } from "@nosh/pi-adapter";
import { compileProgram, SkillRegistry, validateState } from "@nosh/skills";
import { createThread, ensureCanStep, renderThreadContext } from "@nosh/threads";
import {
  episodeDraftSchema, episodeSchema, executionThreadSchema, programStateSchema, runtimeInstructionSchema, runtimeInterventionSchema, schemaUri, sha256, skillManifestSchema,
  type Episode, type EpisodeDraft, type EventDraft, type ExecutionThread, type JsonValue, type OrchestrationProgram, type ProgramState, type RuntimeInstruction, type SkillManifest,
} from "@nosh/wire";
import { isAbsolute, relative, resolve, sep } from "node:path";

export type RuntimeProject = { projectId: string; repositoryRoot: string };
export type RuntimeHookDecision = { assignment: "execute" | "suppress" | "replace"; reasons: string[]; replacement?: RuntimeInstruction };
export type RuntimeHook = (instruction: RuntimeInstruction) => RuntimeHookDecision | undefined;
export type RuntimeDependencies = {
  storeFor(projectId: string): EventStore;
  projectFor(projectId: string): RuntimeProject;
  sessions: Pick<PiAdapter, "start" | "prompt" | "followUp" | "steer" | "abort" | "stop" | "inspect">;
  packagePath: string;
  publish?(event: ReturnType<EventStore["append"]>): void;
  directAction?(action: string, payload: Record<string, JsonValue>, instruction: RuntimeInstruction): Promise<JsonValue> | JsonValue;
  skillCheck?(checkId: string, phase: "preflight" | "postflight", thread: ExecutionThread, episode?: Episode): boolean;
};

export class OrchestrationRuntime {
  private readonly skillRegistries = new Map<string, SkillRegistry>();
  private readonly programs = new Map<string, OrchestrationProgram>();
  private readonly hooks: RuntimeHook[] = [];
  private readonly inFlight = new Map<string, Promise<Episode>>();
  private readonly executions = new Map<string, Promise<JsonValue>>();
  private readonly programRuns = new Map<string, Promise<JsonValue[]>>();

  constructor(private readonly dependencies: RuntimeDependencies) {}

  registerHook(hook: RuntimeHook): void { this.hooks.push(hook); }

  registerSkill(projectId: string, input: unknown, idempotencyKey: string): SkillManifest {
    const skill = skillManifestSchema.parse(input); const store = this.dependencies.storeFor(projectId); const existing = store.projection(projectId, "skill", skill.skillId);
    const result = store.mutateProjection(idempotencyKey, existing?.version ?? 0, draft(projectId, "runtime.skill_registered", skill.skillId, skill), { entityType: "skill", entityId: skill.skillId, state: "active", value: skill });
    if (!result.replayed) this.dependencies.publish?.(result.event); this.skillRegistry(projectId).add(skill); return skill;
  }

  registerProgram(projectId: string, input: unknown, idempotencyKey: string): OrchestrationProgram {
    const program = compileProgram(input); const store = this.dependencies.storeFor(projectId); const existing = store.projection<OrchestrationProgram>(projectId, "orchestration_program", program.programId); const priorState = store.projection<ProgramState>(projectId, "program_state", program.programId); if (priorState?.value.status === "running" && existing && sha256(existing.value) !== sha256(program)) throw new Error(`Running program ${program.programId} cannot be replaced`); const now = nowUtc();
    const state = programStateSchema.parse(priorState ? { ...priorState.value, namespace: program.state.namespace, fieldTypes: program.state.schema, updatedAt: now } : { $schema: schemaUri("program-state"), schemaVersion: 1, projectId, programId: program.programId, namespace: program.state.namespace, fieldTypes: program.state.schema, values: program.state.initial, status: "idle", currentStepId: null, stepsExecuted: 0, executionCount: 0, usage: { toolCalls: 0, modelTokens: 0, wallClockSeconds: 0 }, checkpoint: null, startedAt: null, updatedAt: now }); validateState(program.state.schema, state.values);
    const result = store.mutateProjection(idempotencyKey, existing?.version ?? 0, draft(projectId, "runtime.program_registered", program.programId, program), { entityType: "orchestration_program", entityId: program.programId, state: "active", value: program });
    if (!result.replayed) this.dependencies.publish?.(result.event); const saved = store.mutateProjection(`${idempotencyKey}:state`, priorState?.version ?? 0, draft(projectId, "runtime.program_state_declared", program.programId, state), { entityType: "program_state", entityId: program.programId, state: state.status, value: state }); if (!saved.replayed) this.dependencies.publish?.(saved.event); this.programs.set(`${projectId}:${program.programId}`, program); return program;
  }

  async execute(input: unknown): Promise<JsonValue> {
    const proposed = runtimeInstructionSchema.parse(input); const key = `${proposed.projectId}:${proposed.idempotencyKey}`; const running = this.executions.get(key); if (running) return running; const execution = this.executeOne(proposed); this.executions.set(key, execution); try { return await execution; } finally { this.executions.delete(key); }
  }

  private async executeOne(proposed: RuntimeInstruction): Promise<JsonValue> {
    const store = this.dependencies.storeFor(proposed.projectId);
    const intent = store.beginOperation(proposed.projectId, `runtime.${proposed.operation.toLowerCase()}`, proposed.idempotencyKey, proposed as JsonValue);
    if (intent.state === "completed") return intent.result ?? {};
    if (intent.state === "failed") throw new Error(intent.error ?? "Runtime instruction previously failed");
    const started = Date.now(); const before = this.threadState(proposed); let instruction = proposed; let assignment: RuntimeHookDecision["assignment"] = "execute"; const reasons: string[] = [];
    try {
      this.authorize(instruction);
      for (const hook of this.hooks) {
        const decision = hook(instruction); if (!decision) continue; assignment = decision.assignment; reasons.push(...decision.reasons);
        if (decision.assignment === "replace") { if (!decision.replacement) throw new Error("Replacement hook omitted its instruction"); instruction = runtimeInstructionSchema.parse(decision.replacement); if (instruction.projectId !== proposed.projectId || instruction.instructionId !== proposed.instructionId || instruction.idempotencyKey !== proposed.idempotencyKey || instruction.proposedByAgentId !== proposed.proposedByAgentId) throw new Error("Replacement hook changed the immutable instruction envelope"); this.authorize(instruction); }
        if (decision.assignment === "suppress") break;
      }
      const result = assignment === "suppress" ? { suppressed: true, reasons } : await this.dispatch(instruction);
      this.recordIntervention(proposed, assignment, reasons, true, assignment !== "suppress", before, this.threadState(instruction), result, Date.now() - started);
      store.completeOperation(proposed.projectId, intent.intentId, result);
      return result;
    } catch (error) {
      const message = error instanceof Error ? error.message : "Runtime instruction failed";
      if (instruction.operation === "THREAD_STEP") this.markStepFailed(instruction);
      try { this.recordIntervention(proposed, assignment, [...reasons, message], false, false, before, this.threadState(instruction), { error: message }, Date.now() - started); } catch { /* the primary failure remains authoritative */ }
      store.failOperation(proposed.projectId, intent.intentId, message);
      throw error;
    }
  }

  threads(projectId: string): Array<EntityProjection<ExecutionThread>> { return this.dependencies.storeFor(projectId).projections<ExecutionThread>(projectId, "execution_thread"); }
  thread(projectId: string, threadId: string): EntityProjection<ExecutionThread> { const value = this.dependencies.storeFor(projectId).projection<ExecutionThread>(projectId, "execution_thread", threadId); if (!value) throw new Error(`Unknown thread ${threadId}`); return value; }
  episodes(projectId: string): Array<EntityProjection<Episode>> { const episodes = this.dependencies.storeFor(projectId).projections<Episode>(projectId, "episode"); for (const { value } of episodes) if (!episodeIntegrity(value)) throw new Error(`Episode ${value.episodeId} failed its integrity check`); return episodes; }
  skills(projectId: string): Array<EntityProjection<SkillManifest>> { return this.dependencies.storeFor(projectId).projections<SkillManifest>(projectId, "skill"); }
  programRecords(projectId: string): Array<EntityProjection<OrchestrationProgram>> { return this.dependencies.storeFor(projectId).projections<OrchestrationProgram>(projectId, "orchestration_program"); }
  programStates(projectId: string): Array<EntityProjection<ProgramState>> { return this.dependencies.storeFor(projectId).projections<ProgramState>(projectId, "program_state"); }
  episode(projectId: string, episodeId: string): EntityProjection<Episode> { const value = this.dependencies.storeFor(projectId).projection<Episode>(projectId, "episode", episodeId); if (!value) throw new Error(`Unknown episode ${episodeId}`); if (!episodeIntegrity(value.value)) throw new Error(`Episode ${episodeId} failed its integrity check`); return value; }
  trace(projectId: string, episodeId: string) { const episode = this.episode(projectId, episodeId).value; return this.dependencies.storeFor(projectId).replay(projectId, episode.trace.firstSequence - 1).filter((event) => event.sequence !== null && event.sequence <= episode.trace.lastSequence); }
  hasActiveFork(projectId: string, missionId: string | null, directionId: string | null): boolean { return this.threads(projectId).some(({ value }) => value.executionMode === "foreground_fork" && ["open", "running", "awaiting_user", "awaiting"].includes(value.state) && value.ownerScope.missionId === missionId && value.ownerScope.directionId === directionId); }

  async rotate(projectId: string, threadId: string, idempotencyKey: string): Promise<ExecutionThread> {
    const stored = this.thread(projectId, threadId); const thread = structuredClone(stored.value); if (terminalThread(thread) || thread.state === "running" || thread.activeInstructionId || this.activeChildFork(projectId, thread)) throw new Error(`Thread ${threadId} cannot rotate from ${thread.state}`); this.endSession(thread, "rotated"); await this.ensureSession(thread);
    return this.saveThread(thread, stored.version, idempotencyKey, "runtime.thread_session_rotated").value;
  }

  async forkMessage(projectId: string, threadId: string, message: string, idempotencyKey: string): Promise<void> {
    const text = message.trim(); if (!text) throw new Error("Fork message is required"); const stored = this.thread(projectId, threadId); const thread = structuredClone(stored.value);
    if (thread.executionMode !== "foreground_fork" || thread.state !== "awaiting_user") throw new Error("Thread is not awaiting foreground interaction");
    await this.execute({ $schema: schemaUri("runtime-instruction"), schemaVersion: 1, instructionId: createId("ins"), projectId, idempotencyKey, proposedByAgentId: null, issuedAt: nowUtc(), operation: "THREAD_STEP", threadId, objective: text, expectedEpisodeType: "episode_foreground", inputRefs: [], skillIds: [] });
  }

  async runProgram(projectId: string, programId: string): Promise<JsonValue[]> {
    const key = `${projectId}:${programId}`; const running = this.programRuns.get(key); if (running) return running; const run = this.runProgramOne(projectId, programId); this.programRuns.set(key, run); try { return await run; } finally { this.programRuns.delete(key); }
  }

  private async runProgramOne(projectId: string, programId: string): Promise<JsonValue[]> {
    const program = this.program(projectId, programId); const store = this.dependencies.storeFor(projectId); let stored = store.projection<ProgramState>(projectId, "program_state", programId); if (!stored) throw new Error(`Program ${programId} has no declared state`); let state = structuredClone(stored.value);
    if (state.status !== "running") { state.status = "running"; state.currentStepId = program.startStepId; state.stepsExecuted = 0; state.executionCount += 1; state.usage = { toolCalls: 0, modelTokens: 0, wallClockSeconds: 0 }; state.checkpoint = null; state.startedAt = nowUtc(); state.updatedAt = state.startedAt; stored = this.saveProgramState(state, stored.version, `program:${programId}:${state.executionCount}:start`, "runtime.program_started"); }
    const results: JsonValue[] = []; const started = Date.parse(state.startedAt!);
    while (state.currentStepId) {
      if (state.stepsExecuted >= program.maximumSteps) return this.failProgram(stored, state, `Program ${programId} exceeded maximumSteps`);
      if (Date.now() - started > program.maximumWallClockSeconds * 1_000) return this.failProgram(stored, state, `Program ${programId} exceeded its wall-clock bound`);
      if (state.usage.modelTokens >= program.maximumModelTokens || state.usage.toolCalls >= program.maximumToolCalls) return this.failProgram(stored, state, `Program ${programId} exhausted its agent budget`);
      const step = program.steps.find((candidate) => candidate.stepId === state.currentStepId); if (!step) return this.failProgram(stored, state, `Program step ${state.currentStepId} is missing`);
      if (!this.guard(projectId, step.guard, state.values)) { state.currentStepId = step.failureStepId; state.stepsExecuted += 1; state.checkpoint = { stepId: step.stepId, outcome: "guard_false" }; state.updatedAt = nowUtc(); stored = this.saveProgramState(state, stored.version, `program:${programId}:${state.executionCount}:step:${state.stepsExecuted}`, "runtime.program_checkpointed"); continue; }
      const body = structuredClone(step.instruction); if (body.operation === "THREAD_STEP" && typeof body.objective === "string") body.objective = `${body.objective}\n\nProgram state (${state.namespace}): ${JSON.stringify(state.values)}`;
      const instruction = runtimeInstructionSchema.parse({ ...body, $schema: schemaUri("runtime-instruction"), schemaVersion: 1, instructionId: deterministicId("ins", `${programId}:${state.executionCount}:${state.stepsExecuted}:${step.stepId}`), projectId, idempotencyKey: `program:${programId}:${state.executionCount}:${state.stepsExecuted}:${step.stepId}`, proposedByAgentId: null, issuedAt: state.startedAt });
      try {
        if (step.background) void this.execute(instruction).catch(() => undefined); else { const result = await this.execute(instruction); results.push(result); if (step.outputStateKey) { const values = { ...state.values, [step.outputStateKey]: result }; validateState(state.fieldTypes, values); state.values = values; } if (instruction.operation === "THREAD_STEP") this.addUsage(state, result); else if (instruction.operation === "SKILL_APPLY" && isRecord(result) && "programResults" in result) this.addUsage(state, result.programResults); }
        state.currentStepId = step.nextStepId; state.stepsExecuted += 1; state.checkpoint = { stepId: step.stepId, outcome: step.background ? "spawned" : "completed" }; state.updatedAt = nowUtc(); stored = this.saveProgramState(state, stored.version, `program:${programId}:${state.executionCount}:step:${state.stepsExecuted}`, "runtime.program_checkpointed");
      } catch (error) {
        if (!step.failureStepId) return this.failProgram(stored, state, error instanceof Error ? error.message : "Program step failed"); state.currentStepId = step.failureStepId; state.stepsExecuted += 1; state.checkpoint = { stepId: step.stepId, outcome: "failed", error: error instanceof Error ? error.message : "Program step failed" }; state.updatedAt = nowUtc(); stored = this.saveProgramState(state, stored.version, `program:${programId}:${state.executionCount}:step:${state.stepsExecuted}`, "runtime.program_checkpointed");
      }
    }
    const prefix = `program:${programId}:${state.executionCount}:`; for (const intent of store.operationIntents(projectId, "pending").filter((item) => item.idempotencyKey.startsWith(prefix))) try { await this.execute(intent.request); } catch (error) { return this.failProgram(stored, state, error instanceof Error ? error.message : "Background program step failed"); }
    const intents = store.operationIntents(projectId).filter((item) => item.idempotencyKey.startsWith(prefix)); const failed = intents.find((intent) => { const step = program.steps.find((candidate) => candidate.background && intent.idempotencyKey.endsWith(`:${candidate.stepId}`)); const request = intent.request; if (!step || intent.state !== "failed" || !isRecord(request) || typeof request.threadId !== "string") return false; const threadId = request.threadId; return !program.steps.some((candidate) => isRecord(candidate.instruction) && candidate.instruction.operation === "THREAD_AWAIT" && Array.isArray(candidate.instruction.threadIds) && candidate.instruction.threadIds.includes(threadId) && intents.some((join) => join.idempotencyKey.endsWith(`:${candidate.stepId}`))); }); if (failed) return this.failProgram(stored, state, failed.error ?? "Background program step failed"); const durableResults = intents.flatMap((intent) => intent.result ?? []); for (const intent of intents) { const step = program.steps.find((candidate) => candidate.background && intent.idempotencyKey.endsWith(`:${candidate.stepId}`)); const operation = isRecord(intent.request) && typeof intent.request.operation === "string" ? intent.request.operation : ""; if (step && intent.result !== null && operation === "THREAD_STEP") this.addUsage(state, intent.result); else if (step && isRecord(intent.result) && operation === "SKILL_APPLY" && "programResults" in intent.result) this.addUsage(state, intent.result.programResults); } if (state.usage.modelTokens > program.maximumModelTokens || state.usage.toolCalls > program.maximumToolCalls || Date.now() - started > program.maximumWallClockSeconds * 1_000) return this.failProgram(stored, state, `Program ${programId} exceeded its final budget`);
    state.status = "completed"; state.checkpoint = { outcome: "completed", results: durableResults.length }; state.usage.wallClockSeconds = Math.max(state.usage.wallClockSeconds, Math.ceil((Date.now() - started) / 1_000)); state.updatedAt = nowUtc(); this.saveProgramState(state, stored.version, `program:${programId}:${state.executionCount}:completed`, "runtime.program_completed"); return durableResults.length ? durableResults : results;
  }

  private async dispatch(instruction: RuntimeInstruction): Promise<JsonValue> {
    switch (instruction.operation) {
      case "THREAD_OPEN": return { thread: await this.open(instruction) };
      case "THREAD_FORK": return { thread: await this.open(instruction) };
      case "THREAD_STEP": {
        const promise = this.step(instruction); const key = `${instruction.projectId}:${instruction.threadId}`; this.inFlight.set(key, promise);
        try { return { episode: await promise }; } finally { this.inFlight.delete(key); }
      }
      case "THREAD_AWAIT": return this.awaitThreads(instruction);
      case "THREAD_COMPOSE": return { thread: this.compose(instruction) };
      case "THREAD_PAUSE": return { thread: await this.pause(instruction) };
      case "THREAD_CANCEL": return { thread: await this.cancel(instruction) };
      case "SKILL_APPLY": return this.applySkillInstruction(instruction);
      case "DIRECT_ACTION": return { result: await this.directAction(instruction) };
      case "STOP": return this.stop(instruction);
    }
  }

  private async open(instruction: Extract<RuntimeInstruction, { operation: "THREAD_OPEN" | "THREAD_FORK" }>): Promise<ExecutionThread> {
    const store = this.dependencies.storeFor(instruction.projectId); const prior = store.projection<ExecutionThread>(instruction.projectId, "execution_thread", instruction.threadId); if (prior) return prior.value;
    const parentId = instruction.operation === "THREAD_FORK" ? instruction.controllingThreadId : instruction.parentThreadId; if (parentId) { const parent = this.thread(instruction.projectId, parentId).value; if (terminalThread(parent) || this.activeChildFork(instruction.projectId, parent)) throw new Error(`Thread ${parentId} cannot accept another child from ${parent.state}`); }
    const thread = createThread(instruction); await this.ensureSession(thread, instruction.initialAgentId ?? undefined); const saved = this.saveThread(thread, 0, `${instruction.idempotencyKey}:thread`, "runtime.thread_opened");
    if (thread.parentThreadId) this.addChild(instruction.projectId, thread.parentThreadId, thread.threadId, `${instruction.idempotencyKey}:parent`);
    return saved.value;
  }

  private async step(instruction: Extract<RuntimeInstruction, { operation: "THREAD_STEP" }>): Promise<Episode> {
    let stored = this.thread(instruction.projectId, instruction.threadId); const thread = structuredClone(stored.value); const priorEpisode = this.episodes(instruction.projectId).find(({ value }) => value.instructionId === instruction.instructionId)?.value;
    if (priorEpisode) { if (!thread.episodeIds.includes(priorEpisode.episodeId)) { if (thread.state === "running" && thread.activeInstructionId !== instruction.instructionId) throw new Error(`Thread ${thread.threadId} is running another instruction`); if (thread.currentAgentId && !this.active(thread.currentAgentId)) this.endSession(thread, "failed"); thread.episodeIds.push(priorEpisode.episodeId); thread.nextStepNumber = Math.max(thread.nextStepNumber, priorEpisode.stepNumber + 1); thread.usage.toolCalls += priorEpisode.usage.toolCalls; thread.usage.modelTokens += priorEpisode.usage.modelTokens; thread.usage.wallClockSeconds += priorEpisode.usage.wallClockSeconds; thread.activeInstructionId = null; thread.state = priorEpisode.status === "completed" ? thread.executionMode === "foreground_fork" ? "awaiting_user" : "open" : "failed"; thread.updatedAt = nowUtc(); this.saveThread(thread, stored.version, `${instruction.idempotencyKey}:episode-recovery`, "runtime.thread_step_recovered"); } return priorEpisode; }
    if (thread.state === "running") { if (thread.activeInstructionId !== instruction.instructionId || thread.currentAgentId && this.active(thread.currentAgentId)) throw new Error(`Thread ${thread.threadId} is already running another instruction`); this.endSession(thread, "failed"); thread.activeInstructionId = null; thread.state = "open"; thread.updatedAt = nowUtc(); stored = this.saveThread(thread, stored.version, `${instruction.idempotencyKey}:session-recovery`, "runtime.thread_session_recovered"); }
    ensureCanStep(thread);
    if (this.activeChildFork(instruction.projectId, thread)) throw new Error(`Thread ${thread.threadId} is blocked by an active foreground fork`);
    const activeSkills = [...new Set([...thread.activeSkillIds, ...instruction.skillIds])].map((skillId) => this.skill(projectId(instruction), skillId, thread));
    const inputEpisodeTypes = [...thread.inputRefs, ...instruction.inputRefs].flatMap((ref) => this.dependencies.storeFor(instruction.projectId).projection<Episode>(instruction.projectId, "episode", ref)?.value.episodeType ?? []);
    for (const skill of activeSkills) { if (skill.outputEpisodeType !== instruction.expectedEpisodeType) throw new Error(`Skill ${skill.skillId} outputs ${skill.outputEpisodeType}, not ${instruction.expectedEpisodeType}`); if (skill.activation.episodeTypes.length && !skill.activation.episodeTypes.includes(instruction.expectedEpisodeType) && !skill.activation.episodeTypes.some((type) => inputEpisodeTypes.includes(type))) throw new Error(`Skill ${skill.skillId} is not activated for this episode context`); this.runChecks(skill.preflightChecks, "preflight", thread); for (const type of skill.inputEpisodeTypes) if (!inputEpisodeTypes.includes(type)) throw new Error(`Skill ${skill.skillId} requires an input episode of type ${type}`); }
    thread.activeSkillIds = [...new Set([...thread.activeSkillIds, ...instruction.skillIds])]; const permittedTools = activeSkills.length ? activeSkills.map((skill) => skill.permittedTools).reduce((allowed, tools) => allowed.filter((tool) => tools.includes(tool))) : undefined; if (permittedTools && !permittedTools.includes("nosh_episode_submit")) throw new Error("Active skills must jointly permit nosh_episode_submit"); if (permittedTools && thread.currentAgentId) this.endSession(thread, "rotated"); await this.ensureSession(thread, undefined, permittedTools); thread.state = "running"; thread.activeInstructionId = instruction.instructionId; thread.updatedAt = nowUtc();
    stored = this.saveThread(thread, stored.version, `${instruction.idempotencyKey}:running:${thread.sessionHistory.length}`, "runtime.thread_step_started"); const firstSequence = stored.value ? this.dependencies.storeFor(instruction.projectId).currentSequence(instruction.projectId) : 1;
    const allEpisodes = this.episodes(instruction.projectId).map(({ value }) => value); const prompts = thread.activeSkillIds.map((id) => this.skill(instruction.projectId, id, thread).promptFragment);
    const context = renderThreadContext(thread, allEpisodes, instruction.inputRefs, prompts); const startedAt = nowUtc(); const started = Date.now(); let failure: string | undefined;
    const prompt = `${instruction.objective}\n\n${context}\n\nBefore finishing, call nosh_episode_submit exactly once with an episode-draft of type ${instruction.expectedEpisodeType}. Include only verified facts, decisions, artifact/evidence IDs, changed repository-relative files, unresolved questions, and recommended next actions.`;
    try { await this.dependencies.sessions.prompt(thread.currentAgentId!, prompt); } catch (error) { failure = error instanceof Error ? error.message : "Pi session failed"; }
    const observed = this.thread(instruction.projectId, thread.threadId); let pausedAtBoundary = false; if (observed.version !== stored.version) { if (["running", "paused"].includes(observed.value.state) && observed.value.activeInstructionId === instruction.instructionId) { stored = observed; thread.childThreadIds = [...new Set([...thread.childThreadIds, ...observed.value.childThreadIds])]; pausedAtBoundary = observed.value.state === "paused"; } else throw new Error(`Thread ${thread.threadId} changed to ${observed.value.state} during its step`); }
    const store = this.dependencies.storeFor(instruction.projectId); const lastSequence = Math.max(firstSequence, store.currentSequence(instruction.projectId)); const events = store.replay(instruction.projectId, firstSequence - 1).filter((event) => event.sequence !== null && event.sequence <= lastSequence && event.scope.agentId === thread.currentAgentId);
    const submissions = events.filter((event) => event.type === "record.submitted" && isRecord(event.payload) && event.payload.$schema === schemaUri("episode-draft")); const submitted = submissions.at(-1)?.payload; if (submissions.length > 1) throw new Error("Thread step submitted more than one distinct episode-draft");
    if (!submitted && !failure) throw new Error("Thread step did not submit its required typed episode-draft");
    const episodeDraft = submitted ? episodeDraftSchema.parse(submitted) : fallbackEpisodeDraft(instruction.expectedEpisodeType, failure!);
    if (episodeDraft.episodeType !== instruction.expectedEpisodeType) throw new Error(`Episode type ${episodeDraft.episodeType} does not match expected ${instruction.expectedEpisodeType}`);
    this.validateDraft(instruction.projectId, episodeDraft);
    const modelTokens = events.reduce((total, event) => total + (event.type === "agent.completed" && isRecord(event.payload) && typeof event.payload.modelTokens === "number" ? event.payload.modelTokens : 0), 0);
    const toolCalls = events.filter((event) => event.type === "agent.tool_completed").length; const elapsed = Math.max(0, Math.ceil((Date.now() - started) / 1_000));
    if (activeSkills.length) { const used = events.filter((event) => event.type === "agent.tool_completed" && isRecord(event.payload) && typeof event.payload.toolName === "string").map((event) => String((event.payload as Record<string, JsonValue>).toolName)); const forbidden = used.filter((tool) => activeSkills.some((skill) => !skill.permittedTools.includes(tool))); if (forbidden.length) throw new Error(`Skill tool policy rejected: ${[...new Set(forbidden)].join(", ")}`); }
    const totalUsage = { toolCalls: thread.usage.toolCalls + toolCalls, modelTokens: thread.usage.modelTokens + modelTokens, wallClockSeconds: thread.usage.wallClockSeconds + elapsed };
    failure ??= totalUsage.toolCalls > thread.budget.maximumToolCalls ? `Thread ${thread.threadId} exceeded its tool-call budget` : totalUsage.modelTokens > thread.budget.maximumModelTokens ? `Thread ${thread.threadId} exceeded its model-token budget` : totalUsage.wallClockSeconds > thread.budget.maximumWallClockSeconds ? `Thread ${thread.threadId} exceeded its wall-clock budget` : undefined;
    let episode = createEpisode({ projectId: instruction.projectId, threadId: thread.threadId, instructionId: instruction.instructionId, stepNumber: thread.nextStepNumber, episodeType: episodeDraft.episodeType, objective: instruction.objective, status: failure ? "failed" : "completed", summary: episodeDraft.summary, facts: episodeDraft.facts, decisions: episodeDraft.decisions, artifactIds: episodeDraft.artifactIds, evidenceIds: episodeDraft.evidenceIds, changedFiles: episodeDraft.changedFiles, unresolvedQuestions: episodeDraft.unresolvedQuestions, recommendedNextActions: episodeDraft.recommendedNextActions, contextInputRefs: [...new Set([...thread.inputRefs, ...instruction.inputRefs])], trace: { firstSequence, lastSequence }, usage: { toolCalls, modelTokens, wallClockSeconds: elapsed }, startedAt, completedAt: nowUtc() });
    if (!failure) try { for (const skill of activeSkills) this.runChecks(skill.postflightChecks, "postflight", thread, episode); } catch (error) { failure = error instanceof Error ? error.message : "Skill postflight check failed"; const { episodeHash: _hash, ...unhashed } = episode; const failed = { ...unhashed, status: "failed" as const, completedAt: nowUtc() }; episode = episodeSchema.parse({ ...failed, episodeHash: sha256(failed) }); }
    this.saveEpisode(episode, `${instruction.idempotencyKey}:episode`); thread.episodeIds.push(episode.episodeId); thread.nextStepNumber += 1; thread.inputRefs = [...new Set([...thread.inputRefs, ...instruction.inputRefs])]; thread.activeSkillIds = [];
    thread.usage = totalUsage; thread.activeInstructionId = null; if (activeSkills.length) this.endSession(thread, "completed"); thread.state = failure ? "failed" : pausedAtBoundary ? "paused" : thread.executionMode === "foreground_fork" ? "awaiting_user" : "open"; thread.updatedAt = nowUtc();
    try { this.saveThread(thread, stored.version, `${instruction.idempotencyKey}:completed`, failure ? "runtime.thread_step_failed" : "runtime.thread_step_completed"); } catch (error) { const current = this.thread(instruction.projectId, thread.threadId); if (!["running", "paused"].includes(current.value.state) || current.value.activeInstructionId !== instruction.instructionId) throw error; thread.childThreadIds = [...new Set([...thread.childThreadIds, ...current.value.childThreadIds])]; thread.state = failure ? "failed" : current.value.state === "paused" ? "paused" : thread.executionMode === "foreground_fork" ? "awaiting_user" : "open"; this.saveThread(thread, current.version, `${instruction.idempotencyKey}:completed-after-merge`, failure ? "runtime.thread_step_failed" : "runtime.thread_step_completed"); }
    if (failure) throw new Error(failure);
    return episode;
  }

  private async awaitThreads(instruction: Extract<RuntimeInstruction, { operation: "THREAD_AWAIT" }>): Promise<JsonValue> {
    const waits = instruction.threadIds.map((threadId) => this.inFlight.get(`${instruction.projectId}:${threadId}`) ?? this.waitForEpisode(instruction.projectId, threadId, instruction.deadline));
    let episodes: Episode[];
    if (instruction.policy === "first_success") episodes = [await Promise.any(waits)];
    else if (instruction.policy === "minimum_count") episodes = await minimumSuccess(waits, instruction.minimumCount!);
    else {
      const settled = instruction.policy === "deadline" && instruction.deadline ? await Promise.race([Promise.allSettled(waits), deadline(instruction.deadline)]) : await Promise.allSettled(waits);
      episodes = settled.filter((item): item is PromiseFulfilledResult<Episode> => item.status === "fulfilled").map((item) => item.value);
      if (instruction.policy === "all" && episodes.length !== waits.length) throw new Error("Not all awaited threads completed successfully");
    }
    return { episodeIds: episodes.map((episode) => episode.episodeId) };
  }

  private compose(instruction: Extract<RuntimeInstruction, { operation: "THREAD_COMPOSE" }>): ExecutionThread {
    const stored = this.thread(instruction.projectId, instruction.targetThreadId); const thread = structuredClone(stored.value);
    if (terminalThread(thread) || thread.state === "running" || thread.activeInstructionId || this.activeChildFork(instruction.projectId, thread)) throw new Error(`Thread ${thread.threadId} cannot compose from ${thread.state}`);
    for (const id of instruction.episodeIds) this.episode(instruction.projectId, id); thread.inputRefs = [...new Set([...thread.inputRefs, ...instruction.episodeIds])]; thread.state = "open"; thread.updatedAt = nowUtc();
    return this.saveThread(thread, stored.version, `${instruction.idempotencyKey}:compose`, "runtime.thread_composed").value;
  }

  private async pause(instruction: Extract<RuntimeInstruction, { operation: "THREAD_PAUSE" }>): Promise<ExecutionThread> {
    const stored = this.thread(instruction.projectId, instruction.threadId); const thread = structuredClone(stored.value);
    if (terminalThread(thread)) throw new Error(`Thread ${thread.threadId} cannot pause from ${thread.state}`);
    if (thread.currentAgentId && this.active(thread.currentAgentId)) await this.dependencies.sessions.steer(thread.currentAgentId, `Pause at the next safe boundary. Reason: ${instruction.reason}`);
    thread.state = "paused"; thread.updatedAt = nowUtc(); return this.saveThread(thread, stored.version, `${instruction.idempotencyKey}:pause`, "runtime.thread_paused").value;
  }

  private async cancel(instruction: Extract<RuntimeInstruction, { operation: "THREAD_CANCEL" }>): Promise<ExecutionThread> {
    const stored = this.thread(instruction.projectId, instruction.threadId); const thread = structuredClone(stored.value);
    if (terminalThread(thread) || this.activeChildFork(instruction.projectId, thread)) throw new Error(`Thread ${thread.threadId} cannot cancel from ${thread.state}`);
    if (thread.currentAgentId && this.active(thread.currentAgentId)) await this.dependencies.sessions.abort(thread.currentAgentId); this.endSession(thread, "cancelled"); thread.state = "cancelled"; thread.activeInstructionId = null; thread.updatedAt = nowUtc();
    return this.saveThread(thread, stored.version, `${instruction.idempotencyKey}:cancel`, "runtime.thread_cancelled").value;
  }

  private applySkill(instruction: Extract<RuntimeInstruction, { operation: "SKILL_APPLY" }>): ExecutionThread {
    const stored = this.thread(instruction.projectId, instruction.threadId); const thread = structuredClone(stored.value); if (terminalThread(thread) || thread.state === "running" || thread.activeInstructionId || this.activeChildFork(instruction.projectId, thread)) throw new Error(`Thread ${thread.threadId} cannot apply a skill from ${thread.state}`); thread.inputRefs = [...new Set([...thread.inputRefs, ...instruction.inputRefs])]; this.skill(instruction.projectId, instruction.skillId, thread);
    thread.activeSkillIds = [...new Set([...thread.activeSkillIds, instruction.skillId])]; thread.updatedAt = nowUtc(); return this.saveThread(thread, stored.version, `${instruction.idempotencyKey}:skill`, "runtime.skill_applied").value;
  }

  private async applySkillInstruction(instruction: Extract<RuntimeInstruction, { operation: "SKILL_APPLY" }>): Promise<JsonValue> {
    const skill = this.skill(instruction.projectId, instruction.skillId, this.thread(instruction.projectId, instruction.threadId).value); if (skill.executionMode === "program" && skill.programId && this.programRuns.has(`${instruction.projectId}:${skill.programId}`)) throw new Error(`Program ${skill.programId} is already running and cannot be invoked by SKILL_APPLY`); const thread = this.applySkill(instruction); if (skill.executionMode !== "program" || !skill.programId) return { thread };
    let programResults: JsonValue[]; try { programResults = await this.runProgram(instruction.projectId, skill.programId); } finally { const stored = this.thread(instruction.projectId, instruction.threadId); if (stored.value.activeSkillIds.includes(skill.skillId)) { const cleared = structuredClone(stored.value); cleared.activeSkillIds = cleared.activeSkillIds.filter((id) => id !== skill.skillId); cleared.updatedAt = nowUtc(); this.saveThread(cleared, stored.version, `${instruction.idempotencyKey}:program-skill-cleared`, "runtime.skill_cleared"); } } return { thread: this.thread(instruction.projectId, instruction.threadId).value, programResults };
  }

  private async directAction(instruction: Extract<RuntimeInstruction, { operation: "DIRECT_ACTION" }>): Promise<JsonValue> {
    if (instruction.threadId) { const thread = this.thread(instruction.projectId, instruction.threadId).value; if (this.activeChildFork(instruction.projectId, thread)) throw new Error(`Thread ${thread.threadId} is blocked by an active foreground fork`); if (!thread.capabilities.includes(instruction.action)) throw new Error(`Thread lacks capability ${instruction.action}`); }
    if (!this.dependencies.directAction) throw new Error(`No direct action handler for ${instruction.action}`); return this.dependencies.directAction(instruction.action, instruction.payload, instruction);
  }

  private async stop(instruction: Extract<RuntimeInstruction, { operation: "STOP" }>): Promise<JsonValue> {
    let programState: ProgramState | undefined; if (instruction.programId) { const stored = this.dependencies.storeFor(instruction.projectId).projection<ProgramState>(instruction.projectId, "program_state", instruction.programId); if (!stored) throw new Error(`Unknown program ${instruction.programId}`); programState = structuredClone(stored.value); programState.status = "stopped"; programState.currentStepId = null; programState.checkpoint = { outcome: "stopped", reason: instruction.reason }; programState.updatedAt = nowUtc(); programState = this.saveProgramState(programState, stored.version, `${instruction.idempotencyKey}:program-stop`, "runtime.program_stopped").value; }
    if (!instruction.threadId) return { programState: programState! };
    const stored = this.thread(instruction.projectId, instruction.threadId); const thread = structuredClone(stored.value); if (terminalThread(thread)) return { thread, ...(programState ? { programState } : {}) }; if (this.activeChildFork(instruction.projectId, thread)) throw new Error(`Thread ${thread.threadId} is blocked by an active foreground fork`); this.endSession(thread, "completed", instruction.proposedByAgentId === thread.currentAgentId); thread.state = "completed"; thread.activeInstructionId = null; thread.updatedAt = nowUtc();
    return { thread: this.saveThread(thread, stored.version, `${instruction.idempotencyKey}:thread-stop`, "runtime.thread_stopped").value, ...(programState ? { programState } : {}) };
  }

  private async ensureSession(thread: ExecutionThread, requestedAgentId?: string, tools?: string[]): Promise<void> {
    if (thread.currentAgentId && this.active(thread.currentAgentId)) return;
    if (thread.currentAgentId) this.endSession(thread, "failed"); const index = thread.sessionHistory.length; const agentId = index === 0 && requestedAgentId ? requestedAgentId : deterministicId("agt", `${thread.threadId}:${index}`);
    const project = this.dependencies.projectFor(thread.projectId); const scope = thread.ownerScope;
    const inspection = await this.dependencies.sessions.start({ projectId: thread.projectId, missionId: scope.missionId, directionId: scope.directionId, autoresearchId: scope.autoresearchId, experimentId: scope.experimentId, runId: null, jobId: null, taskId: thread.taskId, agentId, role: thread.role, cwd: project.repositoryRoot, packagePath: this.dependencies.packagePath, ...(tools ? { tools } : {}) } satisfies PiSessionOptions);
    thread.currentAgentId = agentId; thread.currentPiSessionId = inspection.piSessionId; thread.sessionHistory.push({ agentId, piSessionId: inspection.piSessionId, startedAt: inspection.startedAt, endedAt: null, endReason: null }); thread.updatedAt = nowUtc();
  }

  private endSession(thread: ExecutionThread, reason: "rotated" | "completed" | "cancelled" | "failed", defer = false): void {
    if (thread.currentAgentId && this.active(thread.currentAgentId)) { const agentId = thread.currentAgentId; if (defer) { const timer = setTimeout(() => this.dependencies.sessions.stop(agentId), 100); timer.unref(); } else this.dependencies.sessions.stop(agentId); } const current = thread.sessionHistory.at(-1); if (current && !current.endedAt) { current.endedAt = nowUtc(); current.endReason = reason; }
    thread.currentAgentId = null; thread.currentPiSessionId = null; thread.updatedAt = nowUtc();
  }

  private active(agentId: string): boolean { return this.dependencies.sessions.inspect().some((agent) => agent.agentId === agentId); }
  private authorize(instruction: RuntimeInstruction): void {
    if (!instruction.proposedByAgentId) return; const agent = this.dependencies.sessions.inspect().find((entry) => entry.agentId === instruction.proposedByAgentId && entry.projectId === instruction.projectId); if (!agent) throw new Error("Instruction proposer is not an active Pi session");
    const privileged = ["nosh", "mission_director", "research_director"].includes(agent.role); const sameScope = (thread: ExecutionThread) => thread.ownerScope.missionId === agent.missionId && thread.ownerScope.directionId === agent.directionId && thread.ownerScope.autoresearchId === agent.autoresearchId && thread.ownerScope.experimentId === agent.experimentId;
    const controls = (threadId: string) => { const thread = this.thread(instruction.projectId, threadId).value; if (!sameScope(thread)) return false; if (privileged || thread.currentAgentId === agent.agentId) return true; return thread.parentThreadId ? this.thread(instruction.projectId, thread.parentThreadId).value.currentAgentId === agent.agentId : false; };
    if (instruction.operation === "THREAD_OPEN") { if (!privileged || instruction.ownerScope.missionId !== agent.missionId || instruction.ownerScope.directionId !== agent.directionId || instruction.ownerScope.autoresearchId !== agent.autoresearchId || instruction.ownerScope.experimentId !== agent.experimentId || instruction.parentThreadId && !controls(instruction.parentThreadId)) throw new Error("Agent is not authorized to open this logical thread"); return; }
    if (instruction.operation === "THREAD_FORK") { if (!controls(instruction.controllingThreadId)) throw new Error("Agent does not control the foreground fork parent"); return; }
    if (instruction.operation === "THREAD_AWAIT") { if (!instruction.threadIds.every(controls)) throw new Error("Agent does not control every awaited thread"); return; }
    if (instruction.operation === "THREAD_COMPOSE") { if (!controls(instruction.targetThreadId)) throw new Error("Agent does not control the compose target"); return; }
    if (instruction.operation === "STOP" && instruction.threadId === null || instruction.operation === "DIRECT_ACTION" && instruction.threadId === null) { if (!privileged) throw new Error("Only a director may issue an unscoped instruction"); return; }
    if ("threadId" in instruction && instruction.threadId && !controls(instruction.threadId)) throw new Error("Agent does not control the target thread");
  }
  private markStepFailed(instruction: Extract<RuntimeInstruction, { operation: "THREAD_STEP" }>): void { const stored = this.dependencies.storeFor(instruction.projectId).projection<ExecutionThread>(instruction.projectId, "execution_thread", instruction.threadId); if (!stored || !["running", "paused"].includes(stored.value.state) || stored.value.activeInstructionId !== instruction.instructionId) return; const thread = structuredClone(stored.value); this.endSession(thread, "failed"); thread.state = "failed"; thread.activeInstructionId = null; thread.updatedAt = nowUtc(); try { this.saveThread(thread, stored.version, `${instruction.idempotencyKey}:runtime-failure`, "runtime.thread_step_failed"); } catch { /* preserve the original failure */ } }
  private saveThread(thread: ExecutionThread, version: number, key: string, type: string): EntityProjection<ExecutionThread> { const value = executionThreadSchema.parse(thread); const result = this.dependencies.storeFor(thread.projectId).mutateProjection(key, version, draft(thread.projectId, type, thread.threadId, value, thread), { entityType: "execution_thread", entityId: thread.threadId, state: thread.state, value }); if (!result.replayed) this.dependencies.publish?.(result.event); return result.projection; }
  private saveEpisode(episode: Episode, key: string): void { const result = this.dependencies.storeFor(episode.projectId).mutateProjection(key, 0, draft(episode.projectId, "runtime.episode_created", episode.threadId, episode), { entityType: "episode", entityId: episode.episodeId, state: episode.status, value: episode }); if (!result.replayed) this.dependencies.publish?.(result.event); }
  private saveProgramState(state: ProgramState, version: number, key: string, type: string): EntityProjection<ProgramState> { validateState(state.fieldTypes, state.values); const value = programStateSchema.parse(state); const result = this.dependencies.storeFor(state.projectId).mutateProjection(key, version, draft(state.projectId, type, state.programId, value), { entityType: "program_state", entityId: state.programId, state: state.status, value }); if (!result.replayed) this.dependencies.publish?.(result.event); return result.projection; }
  private failProgram(stored: EntityProjection<ProgramState>, state: ProgramState, message: string): never { state.status = "failed"; state.checkpoint = { outcome: "failed", error: message }; state.updatedAt = nowUtc(); this.saveProgramState(state, stored.version, `program:${state.programId}:${state.executionCount}:failed:${state.stepsExecuted}`, "runtime.program_failed"); throw new Error(message); }
  private addUsage(state: ProgramState, result: JsonValue): void { for (const ref of collectRefs(result).filter((value) => value.startsWith("epi_"))) { const episode = this.dependencies.storeFor(state.projectId).projection<Episode>(state.projectId, "episode", ref)?.value; if (!episode) continue; state.usage.toolCalls += episode.usage.toolCalls; state.usage.modelTokens += episode.usage.modelTokens; state.usage.wallClockSeconds += episode.usage.wallClockSeconds; } }
  private addChild(projectId: string, parentId: string, childId: string, key: string): void { for (let attempt = 0; attempt < 3; attempt += 1) { const stored = this.thread(projectId, parentId); const parent = structuredClone(stored.value); parent.childThreadIds = [...new Set([...parent.childThreadIds, childId])]; parent.updatedAt = nowUtc(); try { this.saveThread(parent, stored.version, key, "runtime.thread_child_added"); return; } catch (error) { if (attempt === 2) throw error; } } }
  private activeChildFork(projectId: string, thread: ExecutionThread): boolean { return thread.childThreadIds.some((id) => { const child = this.dependencies.storeFor(projectId).projection<ExecutionThread>(projectId, "execution_thread", id)?.value; return child?.executionMode === "foreground_fork" && ["open", "running", "awaiting", "awaiting_user", "paused"].includes(child.state); }); }
  private async waitForEpisode(projectId: string, threadId: string, deadlineAt: string | null): Promise<Episode> { for (;;) { const running = this.inFlight.get(`${projectId}:${threadId}`); if (running) return running; const thread = this.thread(projectId, threadId).value; if (thread.state !== "running") { const id = thread.episodeIds.at(-1); if (!id) throw new Error(`Thread ${threadId} has no completed episode`); const episode = this.episode(projectId, id).value; if (episode.status !== "completed") throw new Error(`Thread ${threadId} ended with a ${episode.status} Episode`); return episode; } const recovered = thread.activeInstructionId ? this.dependencies.storeFor(projectId).projections<Episode>(projectId, "episode").find(({ value }) => value.instructionId === thread.activeInstructionId)?.value : undefined; if (recovered) { if (!episodeIntegrity(recovered)) throw new Error(`Episode ${recovered.episodeId} failed its integrity check`); if (recovered.status !== "completed") throw new Error(`Thread ${threadId} ended with a ${recovered.status} Episode`); return recovered; } if (deadlineAt && Date.now() >= Date.parse(deadlineAt)) throw new Error(`Thread ${threadId} did not finish before the deadline`); await new Promise((resolvePromise) => setTimeout(resolvePromise, 25)); } }

  private skill(projectId: string, skillId: string, thread: ExecutionThread): SkillManifest {
    const registry = this.skillRegistry(projectId); try { return registry.applicable(skillId, thread); } catch (error) {
      const stored = this.dependencies.storeFor(projectId).projection<SkillManifest>(projectId, "skill", skillId); if (!stored) throw error; registry.add(skillManifestSchema.parse(stored.value)); return registry.applicable(skillId, thread);
    }
  }

  private skillRegistry(projectId: string): SkillRegistry { let registry = this.skillRegistries.get(projectId); if (!registry) { registry = new SkillRegistry(); this.skillRegistries.set(projectId, registry); } return registry; }

  private runChecks(checks: string[], phase: "preflight" | "postflight", thread: ExecutionThread, episode?: Episode): void { for (const check of checks) { const passed = check === "check_budget" ? (() => { try { ensureCanStep(thread); return true; } catch { return false; } })() : check === "check_episode" ? episode?.status === "completed" : this.dependencies.skillCheck?.(check, phase, thread, episode); if (passed !== true) throw new Error(`Skill ${phase} check failed or is unavailable: ${check}`); } }

  private program(projectId: string, programId: string): OrchestrationProgram { const key = `${projectId}:${programId}`; const active = this.programs.get(key); if (active) return active; const stored = this.dependencies.storeFor(projectId).projection<OrchestrationProgram>(projectId, "orchestration_program", programId); if (!stored) throw new Error(`Unknown program ${programId}`); const program = compileProgram(stored.value); this.programs.set(key, program); return program; }
  private guard(projectId: string, guard: OrchestrationProgram["steps"][number]["guard"], values: Record<string, JsonValue>): boolean { if (!guard) return true; const value = guard.ref in values ? values[guard.ref] : this.referenceValue(projectId, guard.ref); if (guard.operator === "exists") return value !== undefined; return guard.operator === "equals" ? JSON.stringify(value) === JSON.stringify(guard.value) : JSON.stringify(value) !== JSON.stringify(guard.value); }
  private validateDraft(projectId: string, value: EpisodeDraft): void { for (const id of [...value.artifactIds, ...value.evidenceIds, ...value.facts.flatMap((fact) => fact.evidenceRefs), ...value.decisions.flatMap((decision) => decision.evidenceRefs)]) if (!this.referenceExists(projectId, id)) throw new Error(`Episode references unknown record ${id}`); const root = resolve(this.dependencies.projectFor(projectId).repositoryRoot); for (const file of value.changedFiles) { const resolved = resolve(root, file); const path = relative(root, resolved); if (!path || isAbsolute(file) || isAbsolute(path) || path === ".." || path.startsWith(`..${sep}`)) throw new Error(`Changed file escapes the repository: ${file}`); } }
  private referenceExists(projectId: string, ref: string): boolean { return this.referenceValue(projectId, ref) !== undefined; }
  private referenceValue(projectId: string, ref: string): JsonValue | undefined { const store = this.dependencies.storeFor(projectId); for (const type of ["episode", "execution_thread", "skill", "orchestration_program", "mission", "direction", "autoresearch"]) { const projection = store.projection(projectId, type, ref); if (projection) return projection.value; } const expected: Record<string, [string, string]> = { art: [schemaUri("artifact"), "artifactId"], evd: [schemaUri("evidence"), "evidenceId"], clm: [schemaUri("claim"), "claimId"], rev: [schemaUri("review-verdict"), "reviewId"] }; const rule = expected[ref.slice(0, 3)]; return store.replay(projectId).filter((event) => event.type === "record.submitted" && isRecord(event.payload) && event.payload.$schema !== schemaUri("episode-draft")).map((event) => event.payload).find((payload) => rule ? isRecord(payload) && payload.$schema === rule[0] && payload[rule[1]] === ref : containsId(payload, ref)); }
  private threadState(instruction: RuntimeInstruction): JsonValue { const id = "threadId" in instruction ? instruction.threadId : "targetThreadId" in instruction ? instruction.targetThreadId : null; if (!id) return null; return this.dependencies.storeFor(instruction.projectId).projection(instruction.projectId, "execution_thread", id)?.value ?? null; }

  private recordIntervention(instruction: RuntimeInstruction, assignment: RuntimeHookDecision["assignment"], reasons: string[], eligible: boolean, executed: boolean, before: JsonValue, after: JsonValue, result: JsonValue, latency: number): void {
    const store = this.dependencies.storeFor(instruction.projectId); const interventionId = deterministicId("ivn", instruction.instructionId); const outputRefs = collectRefs(result); const inputRefs = collectRefs(instruction as JsonValue).filter((ref) => ref !== instruction.instructionId && ref !== instruction.projectId);
    const episodeCost = outputRefs.filter((ref) => ref.startsWith("epi_")).flatMap((ref) => store.projection<Episode>(instruction.projectId, "episode", ref)?.value ?? []);
    const value = runtimeInterventionSchema.parse({ $schema: schemaUri("runtime-intervention"), schemaVersion: 1, interventionId, projectId: instruction.projectId, instructionId: instruction.instructionId, operation: instruction.operation, proposed: instruction, eligibility: { eligible, reasons }, assignment, executed, inputRefs, outputRefs, stateBefore: before, stateAfter: after, cost: { toolCalls: episodeCost.reduce((total, episode) => total + episode.usage.toolCalls, 0), modelTokens: episodeCost.reduce((total, episode) => total + episode.usage.modelTokens, 0), wallClockSeconds: episodeCost.reduce((total, episode) => total + episode.usage.wallClockSeconds, 0) || Math.ceil(latency / 1_000) }, latencyMilliseconds: latency, downstreamEpisodeIds: outputRefs.filter((ref) => ref.startsWith("epi_")), recordedAt: nowUtc() });
    const prior = store.projection(instruction.projectId, "runtime_intervention", interventionId); const saved = store.mutateProjection(`${instruction.idempotencyKey}:intervention`, prior?.version ?? 0, draft(instruction.projectId, "runtime.intervention_recorded", instruction.instructionId, value), { entityType: "runtime_intervention", entityId: interventionId, state: executed ? "executed" : "not_executed", value }); if (!saved.replayed) this.dependencies.publish?.(saved.event);
  }
}

function draft(projectId: string, type: string, correlationId: string, payload: JsonValue, thread?: ExecutionThread): EventDraft { return { $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type, source: "orchestration_runtime", scope: { projectId, missionId: thread?.ownerScope.missionId ?? null, directionId: thread?.ownerScope.directionId ?? null, autoresearchId: thread?.ownerScope.autoresearchId ?? null, experimentId: thread?.ownerScope.experimentId ?? null, runId: null, jobId: null, agentId: thread?.currentAgentId ?? null }, correlationId, causationId: null, payload }; }
function deterministicId(prefix: "agt" | "ivn" | "ins", seed: string): string { return `${prefix}_${sha256(seed).slice(7, 39)}`; }
function isRecord(value: JsonValue): value is Record<string, JsonValue> { return typeof value === "object" && value !== null && !Array.isArray(value); }
function containsId(value: JsonValue, id: string): boolean { if (value === id) return true; if (Array.isArray(value)) return value.some((item) => containsId(item, id)); if (isRecord(value)) return Object.values(value).some((item) => containsId(item, id)); return false; }
function collectRefs(value: JsonValue): string[] { const refs: string[] = []; const visit = (item: JsonValue): void => { if (typeof item === "string" && /^[a-z][a-z0-9]*_[a-z0-9][a-z0-9.:-]*$/.test(item)) refs.push(item); else if (Array.isArray(item)) item.forEach(visit); else if (isRecord(item)) Object.values(item).forEach(visit); }; visit(value); return [...new Set(refs)]; }
function projectId(instruction: RuntimeInstruction): string { return instruction.projectId; }
function terminalThread(thread: ExecutionThread): boolean { return ["completed", "cancelled", "failed"].includes(thread.state); }
async function deadline(value: string): Promise<PromiseSettledResult<Episode>[]> { const milliseconds = Math.max(0, Date.parse(value) - Date.now()); return new Promise((resolvePromise) => setTimeout(() => resolvePromise([]), milliseconds)); }
function minimumSuccess(waits: Array<Promise<Episode>>, count: number): Promise<Episode[]> { return new Promise((resolvePromise, reject) => { const episodes: Episode[] = []; let settled = 0; for (const wait of waits) void wait.then((episode) => { episodes.push(episode); if (episodes.length === count) resolvePromise(episodes); }).catch(() => undefined).finally(() => { settled += 1; if (settled === waits.length && episodes.length < count) reject(new Error("Awaited threads did not reach minimum_count")); }); }); }
