import { id, observedVersionsSchema, schemaUri, schemaVersion, timestamp } from "./common.js";
import { eventDraftSchema } from "./events.js";
import { jsonValueSchema } from "./json.js";
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
