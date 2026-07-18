import { id, schemaUri, schemaVersion, templateVersion, timestamp } from "./common.js";
import { jsonValueSchema } from "./json.js";
import { z } from "zod";

export const taskAcknowledgementSchema = z
  .object({
    $schema: z.literal(schemaUri("task-acknowledgement")),
    schemaVersion,
    taskId: id("tsk"),
    attempt: z.number().int().positive(),
    agentId: id("agt"),
    decision: z.enum(["accepted", "rejected_conflict", "rejected_missing_input", "rejected_permission", "clarification_required"]),
    understoodObjective: z.string().max(2000),
    understoodOutputIds: z.array(z.string()).max(100),
    understoodCriterionIds: z.array(z.string()).max(100),
    observedLeaseId: z.string().min(1).max(128),
    observedStartingCommit: z.string().min(7).max(128),
    conflicts: z.array(z.string().max(1000)).max(50),
    clarificationRequest: z.string().max(2000).nullable(),
    submittedAt: timestamp,
  })
  .strict();

export const progressUpdateSchema = z
  .object({
    $schema: z.literal(schemaUri("progress-update")),
    schemaVersion,
    progressId: z.string().min(1).max(128),
    taskId: id("tsk"),
    attempt: z.number().int().positive(),
    agentId: id("agt"),
    kind: z.enum(["milestone", "anomaly", "budget_warning", "review_ready", "heartbeat_recovery"]),
    summary: z.string().min(1).max(2000),
    goalStackIds: z.array(z.string().min(1).max(128)).min(1).max(16),
    durableDelta: z
      .object({
        commitIds: z.array(z.string()),
        artifactIds: z.array(id("art")),
        evidenceIds: z.array(id("evd")),
        graphNodeStateChanges: z.array(jsonValueSchema),
        closedDefectIds: z.array(z.string()),
      })
      .strict(),
    validation: z.array(jsonValueSchema),
    currentOperation: z.string().max(1000),
    nextOperation: z.string().max(1000),
    estimatedRemainingSeconds: z.number().int().nonnegative().nullable(),
    newRisk: z.string().max(2000).nullable(),
    blockerId: id("blk").nullable(),
    attemptFingerprint: z.string().regex(/^sha256:[0-9a-f]{64}$/),
    emittedAt: timestamp,
  })
  .strict();

const responseType = z.enum([
  "nosh.chat_turn",
  "mission_director.cycle",
  "research_director.cycle",
  "worker.completion",
  "librarian.research_completion",
  "review.verdict",
  "task.blocked",
  "task.failure",
]);

export const responseEnvelopeSchema = z
  .object({
    $schema: z.literal(schemaUri("response-envelope")),
    schemaVersion,
    templateVersion,
    responseId: id("rsp"),
    responseType,
    taskId: id("tsk").nullable(),
    role: z.enum(["nosh", "mission_director", "research_director", "librarian_researcher", "general_worker", "reviewer"]),
    status: z.enum(["completed", "partial", "blocked", "failed", "cancelled", "superseded"]),
    summary: z.string().max(2000),
    recordIds: z.array(z.string().min(1).max(128)).max(100),
    payload: jsonValueSchema,
    submittedAt: timestamp,
  })
  .strict();

export const templateRegistry = {
  [schemaUri("task-acknowledgement")]: taskAcknowledgementSchema,
  [schemaUri("progress-update")]: progressUpdateSchema,
  [schemaUri("response-envelope")]: responseEnvelopeSchema,
} as const;
