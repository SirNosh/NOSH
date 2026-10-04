import { z } from "zod";
import { id, schemaUri, schemaVersion, sha256Digest, timestamp } from "./common.js";
import { jsonValueSchema } from "./json.js";
import { taskPermissionsSchema, taskWorkspaceSchema } from "./submissions.js";

// Validation receipts reach models; say what the format is, not just "Invalid".
const ref = z.string().regex(/^[a-z][a-z0-9]*_[a-z0-9][a-z0-9.:-]*$/, "Expected a reference: lowercase prefix, exactly one underscore, then lowercase letters, digits, '.', ':' or '-' (e.g. contribution_method-fixture)").max(128);
const text = z.string().min(1).max(20_000);
const refs = z.array(ref).max(500);
const strings = z.array(z.string().min(1).max(4_000)).max(200);
const stateKey = z.string().regex(/^[a-z][a-z0-9_]{0,127}$/);
const role = z.enum(["nosh", "mission_director", "research_director", "librarian_researcher", "general_worker", "reviewer"]);
const ownerScope = z.object({ missionId: id("mis").nullable(), directionId: id("dir").nullable(), autoresearchId: id("ar").nullable(), experimentId: id("exp").nullable(), graphNodeId: ref.nullable() }).strict();
const budget = z.object({ maximumToolCalls: z.number().int().positive(), maximumModelTokens: z.number().int().positive(), maximumWallClockSeconds: z.number().int().positive() }).strict();
const usage = z.object({ toolCalls: z.number().int().nonnegative(), modelTokens: z.number().int().nonnegative(), wallClockSeconds: z.number().int().nonnegative() }).strict();
const fact = z.object({ statement: text, evidenceRefs: refs, confidence: z.enum(["low", "medium", "high"]) }).strict();
const decision = z.object({ statement: text, rationale: text, evidenceRefs: refs }).strict();
const nextAction = z.object({ operation: z.enum(["THREAD_OPEN", "THREAD_STEP", "THREAD_FORK", "THREAD_AWAIT", "THREAD_COMPOSE", "THREAD_PAUSE", "THREAD_CANCEL", "SKILL_APPLY", "DIRECT_ACTION", "STOP"]), objective: text }).strict();

export const episodeDraftSchema = z.object({
  $schema: z.literal(schemaUri("episode-draft")), schemaVersion, episodeType: ref, summary: text,
  facts: z.array(fact).max(200), decisions: z.array(decision).max(100), artifactIds: z.array(id("art")).max(200), evidenceIds: z.array(id("evd")).max(200),
  changedFiles: strings, unresolvedQuestions: strings, recommendedNextActions: z.array(nextAction).max(50),
}).strict();

export const episodeSchema = z.object({
  $schema: z.literal(schemaUri("episode")), schemaVersion, episodeId: id("epi"), projectId: id("prj"), threadId: id("thr"), instructionId: id("ins"), stepNumber: z.number().int().positive(),
  episodeType: ref, objective: text, status: z.enum(["completed", "failed", "cancelled", "blocked"]), summary: text,
  facts: z.array(fact).max(200), decisions: z.array(decision).max(100), artifactIds: z.array(id("art")).max(200), evidenceIds: z.array(id("evd")).max(200), changedFiles: strings,
  unresolvedQuestions: strings, recommendedNextActions: z.array(nextAction).max(50), contextInputRefs: refs,
  trace: z.object({ firstSequence: z.number().int().positive(), lastSequence: z.number().int().positive() }).strict(),
  usage, startedAt: timestamp, completedAt: timestamp, episodeHash: sha256Digest,
}).strict().superRefine((value, context) => { if (value.trace.lastSequence < value.trace.firstSequence) context.addIssue({ code: "custom", path: ["trace", "lastSequence"], message: "lastSequence must not precede firstSequence" }); });

const session = z.object({ agentId: id("agt"), piSessionId: z.string().min(1), startedAt: timestamp, endedAt: timestamp.nullable(), endReason: z.enum(["rotated", "completed", "cancelled", "failed"]).nullable() }).strict();
export const executionThreadSchema = z.object({
  $schema: z.literal(schemaUri("execution-thread")), schemaVersion, threadId: id("thr"), projectId: id("prj"), taskId: id("tsk"), ownerScope, role, purpose: text,
  executionMode: z.enum(["background", "foreground_fork"]), state: z.enum(["open", "running", "awaiting", "awaiting_user", "paused", "completed", "cancelled", "failed"]),
  parentThreadId: id("thr").nullable(), childThreadIds: z.array(id("thr")).max(200), currentAgentId: id("agt").nullable(), currentPiSessionId: z.string().nullable(), activeInstructionId: id("ins").nullable(), sessionHistory: z.array(session).max(200),
  episodeIds: z.array(id("epi")).max(10_000), inputRefs: refs, activeSkillIds: z.array(id("skl")).max(100), capabilities: refs, taskPermissions: taskPermissionsSchema.nullable(), taskWorkspace: taskWorkspaceSchema.nullable(), budget, usage,
  nextStepNumber: z.number().int().positive(), createdAt: timestamp, updatedAt: timestamp,
}).strict();

const instructionBase = {
  $schema: z.literal(schemaUri("runtime-instruction")), schemaVersion, instructionId: id("ins"), projectId: id("prj"),
  idempotencyKey: z.string().min(16).max(256), proposedByAgentId: id("agt").nullable(), issuedAt: timestamp,
};
const openFields = {
  threadId: id("thr"), taskId: id("tsk"), initialAgentId: id("agt").nullable(), ownerScope, role, purpose: text,
  executionMode: z.enum(["background", "foreground_fork"]), parentThreadId: id("thr").nullable(), inputRefs: refs,
  skillIds: z.array(id("skl")).max(100), capabilities: refs, taskPermissions: taskPermissionsSchema.nullable().optional(),
  taskWorkspace: taskWorkspaceSchema.nullable().optional(), budget,
};
const instructionBodies = {
  THREAD_OPEN: z.object({ operation: z.literal("THREAD_OPEN"), ...openFields }).strict(),
  THREAD_STEP: z.object({ operation: z.literal("THREAD_STEP"), threadId: id("thr"), objective: text, expectedEpisodeType: ref, inputRefs: refs, skillIds: z.array(id("skl")).max(100) }).strict(),
  THREAD_FORK: z.object({ operation: z.literal("THREAD_FORK"), controllingThreadId: id("thr"), ...openFields }).strict(),
  THREAD_AWAIT: z.object({ operation: z.literal("THREAD_AWAIT"), threadIds: z.array(id("thr")).min(1).max(100), policy: z.enum(["all", "first_success", "minimum_count", "deadline"]), minimumCount: z.number().int().positive().nullable(), deadline: timestamp.nullable() }).strict(),
  THREAD_COMPOSE: z.object({ operation: z.literal("THREAD_COMPOSE"), targetThreadId: id("thr"), episodeIds: z.array(id("epi")).min(1).max(500) }).strict(),
  THREAD_PAUSE: z.object({ operation: z.literal("THREAD_PAUSE"), threadId: id("thr"), reason: text }).strict(),
  THREAD_CANCEL: z.object({ operation: z.literal("THREAD_CANCEL"), threadId: id("thr"), reason: text }).strict(),
  SKILL_APPLY: z.object({ operation: z.literal("SKILL_APPLY"), threadId: id("thr"), skillId: id("skl"), inputRefs: refs }).strict(),
  DIRECT_ACTION: z.object({ operation: z.literal("DIRECT_ACTION"), threadId: id("thr").nullable(), action: ref, payload: z.record(z.string(), jsonValueSchema) }).strict(),
  STOP: z.object({ operation: z.literal("STOP"), threadId: id("thr").nullable(), programId: id("prg").nullable(), reason: text }).strict(),
} as const;
type InstructionRefinementValue = { operation?: unknown; threadIds?: unknown[]; policy?: unknown; minimumCount?: number | null; deadline?: string | null; threadId?: string | null; programId?: string | null };
const refineInstruction = (value: InstructionRefinementValue, context: z.RefinementCtx): void => {
  if (value.operation === "THREAD_AWAIT") {
    const threadIds = value.threadIds ?? [];
    if (new Set(threadIds).size !== threadIds.length) context.addIssue({ code: "custom", path: ["threadIds"], message: "threadIds must be unique" });
    if (value.policy === "minimum_count" && (value.minimumCount === null || value.minimumCount === undefined || value.minimumCount > threadIds.length)) context.addIssue({ code: "custom", path: ["minimumCount"], message: "minimum_count requires a value no greater than threadIds.length" });
    if (value.policy === "deadline" && value.deadline === null) context.addIssue({ code: "custom", path: ["deadline"], message: "deadline policy requires deadline" });
  }
  if (value.operation === "STOP" && value.threadId === null && value.programId === null) context.addIssue({ code: "custom", path: ["threadId"], message: "STOP requires threadId or programId" });
};
export const runtimeInstructionSchema = z.discriminatedUnion("operation", [
  instructionBodies.THREAD_OPEN.extend(instructionBase), instructionBodies.THREAD_STEP.extend(instructionBase), instructionBodies.THREAD_FORK.extend(instructionBase),
  instructionBodies.THREAD_AWAIT.extend(instructionBase), instructionBodies.THREAD_COMPOSE.extend(instructionBase), instructionBodies.THREAD_PAUSE.extend(instructionBase),
  instructionBodies.THREAD_CANCEL.extend(instructionBase), instructionBodies.SKILL_APPLY.extend(instructionBase), instructionBodies.DIRECT_ACTION.extend(instructionBase),
  instructionBodies.STOP.extend(instructionBase),
]).superRefine(refineInstruction);
export const runtimeInstructionTemplateSchema = z.discriminatedUnion("operation", [
  instructionBodies.THREAD_OPEN, instructionBodies.THREAD_STEP, instructionBodies.THREAD_FORK, instructionBodies.THREAD_AWAIT, instructionBodies.THREAD_COMPOSE,
  instructionBodies.THREAD_PAUSE, instructionBodies.THREAD_CANCEL, instructionBodies.SKILL_APPLY, instructionBodies.DIRECT_ACTION, instructionBodies.STOP,
]).superRefine(refineInstruction);

export const skillManifestSchema = z.object({
  $schema: z.literal(schemaUri("skill-manifest")), schemaVersion, skillId: id("skl"), name: ref, version: z.string().regex(/^\d+\.\d+\.\d+$/), description: text,
  activation: z.object({ roles: z.array(role).min(1), requiredCapabilities: refs, episodeTypes: refs }).strict(), promptFragment: text, permittedTools: strings,
  inputEpisodeTypes: refs, outputEpisodeType: ref, executionMode: z.enum(["prompt", "program"]), programId: id("prg").nullable(), preflightChecks: refs, postflightChecks: refs,
}).strict().superRefine((value, context) => { if ((value.executionMode === "program") !== (value.programId !== null)) context.addIssue({ code: "custom", path: ["programId"], message: "program mode requires programId and prompt mode forbids it" }); });

const guard = z.object({ ref: z.string().min(1).max(128), operator: z.enum(["exists", "equals", "not_equals"]), value: jsonValueSchema.nullable() }).strict();
export const orchestrationProgramSchema = z.object({
  $schema: z.literal(schemaUri("orchestration-program")), schemaVersion, programId: id("prg"), name: ref, version: z.string().regex(/^\d+\.\d+\.\d+$/), startStepId: ref,
  maximumSteps: z.number().int().positive().max(1_000), maximumWallClockSeconds: z.number().int().positive(), maximumModelTokens: z.number().int().positive(), maximumToolCalls: z.number().int().positive(),
  state: z.object({ namespace: ref, schema: z.record(stateKey, z.enum(["string", "number", "boolean", "ref", "json"])), initial: z.record(stateKey, jsonValueSchema) }).strict(),
  steps: z.array(z.object({ stepId: ref, instruction: runtimeInstructionTemplateSchema, guard: guard.nullable(), background: z.boolean(), outputStateKey: stateKey.nullable(), nextStepId: ref.nullable(), failureStepId: ref.nullable() }).strict()).min(1).max(500),
}).strict();

export const programStateSchema = z.object({
  $schema: z.literal(schemaUri("program-state")), schemaVersion, projectId: id("prj"), programId: id("prg"), namespace: ref, fieldTypes: z.record(stateKey, z.enum(["string", "number", "boolean", "ref", "json"])), values: z.record(stateKey, jsonValueSchema),
  status: z.enum(["idle", "running", "completed", "failed", "stopped"]), currentStepId: ref.nullable(), stepsExecuted: z.number().int().nonnegative(), executionCount: z.number().int().nonnegative(), usage: usage, checkpoint: jsonValueSchema.nullable(), startedAt: timestamp.nullable(), updatedAt: timestamp,
}).strict();

export const runtimeInterventionSchema = z.object({
  $schema: z.literal(schemaUri("runtime-intervention")), schemaVersion, interventionId: id("ivn"), projectId: id("prj"), instructionId: id("ins"), operation: z.enum(["THREAD_OPEN", "THREAD_STEP", "THREAD_FORK", "THREAD_AWAIT", "THREAD_COMPOSE", "THREAD_PAUSE", "THREAD_CANCEL", "SKILL_APPLY", "DIRECT_ACTION", "STOP"]),
  proposed: z.record(z.string(), jsonValueSchema), eligibility: z.object({ eligible: z.boolean(), reasons: strings }).strict(), assignment: z.enum(["execute", "suppress", "replace"]), executed: z.boolean(), inputRefs: refs, outputRefs: refs,
  stateBefore: jsonValueSchema, stateAfter: jsonValueSchema, cost: usage, latencyMilliseconds: z.number().int().nonnegative(), downstreamEpisodeIds: z.array(id("epi")), recordedAt: timestamp,
}).strict();

export const runtimeRegistry = {
  [schemaUri("episode-draft")]: episodeDraftSchema,
  [schemaUri("episode")]: episodeSchema,
  [schemaUri("execution-thread")]: executionThreadSchema,
  [schemaUri("runtime-instruction")]: runtimeInstructionSchema,
  [schemaUri("skill-manifest")]: skillManifestSchema,
  [schemaUri("orchestration-program")]: orchestrationProgramSchema,
  [schemaUri("program-state")]: programStateSchema,
  [schemaUri("runtime-intervention")]: runtimeInterventionSchema,
} as const;

export type EpisodeDraft = z.infer<typeof episodeDraftSchema>;
export type Episode = z.infer<typeof episodeSchema>;
export type ExecutionThread = z.infer<typeof executionThreadSchema>;
export type RuntimeInstruction = z.infer<typeof runtimeInstructionSchema>;
export type RuntimeInstructionTemplate = z.infer<typeof runtimeInstructionTemplateSchema>;
export type SkillManifest = z.infer<typeof skillManifestSchema>;
export type OrchestrationProgram = z.infer<typeof orchestrationProgramSchema>;
export type ProgramState = z.infer<typeof programStateSchema>;
export type RuntimeIntervention = z.infer<typeof runtimeInterventionSchema>;
