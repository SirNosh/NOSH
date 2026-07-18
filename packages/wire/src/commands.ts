import { id, observedVersionsSchema, schemaUri, schemaVersion, timestamp } from "./common.js";
import { eventDraftSchema } from "./events.js";
import { jsonValueSchema, type JsonValue } from "./json.js";
import { z } from "zod";

export const commandSchema = z
  .object({
    $schema: z.literal(schemaUri("command")),
    schemaVersion,
    commandId: id("cmd"),
    idempotencyKey: z.string().min(16).max(256),
    projectId: id("prj"),
    targetType: z.string().regex(/^[a-z][a-z0-9_]*$/),
    targetId: z.string().min(1).max(128).nullable(),
    expectedVersion: z.number().int().nonnegative().nullable(),
    type: z.string().regex(/^[a-z][a-z0-9_]*(?:\.[a-z][a-z0-9_]*)+$/),
    observedVersions: observedVersionsSchema,
    payload: jsonValueSchema,
    issuedAt: timestamp,
  })
  .strict();

export const appendEventCommandSchema = commandSchema.extend({
  type: z.literal("event.append"),
  payload: eventDraftSchema,
});

export type Command = z.infer<typeof commandSchema>;
export type AppendEventCommand = z.infer<typeof appendEventCommandSchema>;

export const remoteCommandTypeSchema = z.enum([
  "mission.pause", "mission.resume", "mission.stop", "mission.steer", "agent.message", "agent.safe_pause", "agent.cancel",
  "job.checkpoint", "job.cancel", "review.respond", "artifact.preview.request", "thread.message", "thread.stop",
]);

export const remoteCommandEnvelopeSchema = z.object({
  protocolVersion: z.literal(1),
  accountId: z.string().regex(/^acc_[a-z0-9]{16,64}$/),
  commandId: id("cmd"),
  idempotencyKey: z.string().min(16).max(256),
  deviceId: z.string().regex(/^dev_[a-z0-9]{16,64}$/),
  projectId: id("prj"),
  targetType: z.enum(["mission", "agent", "job", "review", "artifact", "thread"]),
  targetId: z.string().min(1).max(128),
  expectedVersion: z.number().int().nonnegative(),
  type: remoteCommandTypeSchema,
  requiredPermission: z.string().regex(/^[a-z][a-z0-9_.]*$/),
  keyVersion: z.number().int().positive(),
  issuedAt: timestamp,
  expiresAt: timestamp,
  nonce: z.string().regex(/^[A-Za-z0-9_-]+$/),
  ciphertext: z.string().regex(/^[A-Za-z0-9_-]+$/),
  signature: z.string().regex(/^[A-Za-z0-9_-]+$/),
}).strict();

export type RemoteCommandEnvelope = z.infer<typeof remoteCommandEnvelopeSchema>;
export type RemoteCommandType = z.infer<typeof remoteCommandTypeSchema>;

export const remoteCommandPolicy: Record<RemoteCommandType, { targetType: RemoteCommandEnvelope["targetType"]; permission: string; payload: z.ZodTypeAny }> = {
  "mission.pause": { targetType: "mission", permission: "mission.pause", payload: z.object({ mode: z.enum(["safe", "checkpoint", "immediate"]).default("safe") }).strict() },
  "mission.resume": { targetType: "mission", permission: "mission.resume", payload: z.object({}).strict() },
  "mission.stop": { targetType: "mission", permission: "mission.stop", payload: z.object({ mode: z.enum(["safe", "immediate"]).default("safe") }).strict() },
  "mission.steer": { targetType: "mission", permission: "mission.steer", payload: z.object({ message: z.string().min(1).max(8_000) }).strict() },
  "agent.message": { targetType: "agent", permission: "agent.message", payload: z.object({ message: z.string().min(1).max(8_000) }).strict() },
  "agent.safe_pause": { targetType: "agent", permission: "agent.safe_pause", payload: z.object({}).strict() },
  "agent.cancel": { targetType: "agent", permission: "agent.cancel", payload: z.object({}).strict() },
  "job.checkpoint": { targetType: "job", permission: "job.checkpoint", payload: z.object({}).strict() },
  "job.cancel": { targetType: "job", permission: "job.cancel", payload: z.object({}).strict() },
  "review.respond": { targetType: "review", permission: "review.respond", payload: z.object({ verdict: z.enum(["approve", "reject"]), comment: z.string().max(8_000).default("") }).strict() },
  "artifact.preview.request": { targetType: "artifact", permission: "artifact.preview.request", payload: z.object({ maximumBytes: z.number().int().positive().max(500_000).default(100_000) }).strict() },
  "thread.message": { targetType: "thread", permission: "thread.message", payload: z.object({ message: z.string().min(1).max(8_000) }).strict() },
  "thread.stop": { targetType: "thread", permission: "thread.stop", payload: z.object({}).strict() },
};

export function parseRemoteCommandPayload(type: RemoteCommandType, payload: unknown): JsonValue {
  return remoteCommandPolicy[type].payload.parse(payload) as JsonValue;
}
