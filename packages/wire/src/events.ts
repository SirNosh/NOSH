import { id, schemaUri, schemaVersion, scopeSchema, timestamp } from "./common.js";
import { jsonValueSchema } from "./json.js";
import { z } from "zod";

export const eventType = z.string().regex(/^[a-z][a-z0-9_]*(?:\.[a-z][a-z0-9_]*)+$/);
export const eventRetention = z.enum(["persistent", "ephemeral"]);

const eventBody = {
  $schema: z.literal(schemaUri("event")),
  schemaVersion,
  retention: eventRetention,
  type: eventType,
  source: z.string().min(1).max(128),
  scope: scopeSchema,
  correlationId: z.string().min(1).max(128).nullable(),
  causationId: id("evt").nullable(),
  payload: jsonValueSchema,
};

export const eventDraftSchema = z.object(eventBody).strict();

export const eventEnvelopeSchema = z
  .object({
    ...eventBody,
    eventId: id("evt"),
    sequence: z.number().int().positive().nullable(),
    timestamp,
  })
  .strict()
  .superRefine((event, context) => {
    if (event.retention === "persistent" && event.sequence === null) {
      context.addIssue({ code: z.ZodIssueCode.custom, path: ["sequence"], message: "persistent events require a sequence" });
    }
    if (event.retention === "ephemeral" && event.sequence !== null) {
      context.addIssue({ code: z.ZodIssueCode.custom, path: ["sequence"], message: "ephemeral events cannot have a sequence" });
    }
  });

export type EventDraft = z.infer<typeof eventDraftSchema>;
export type EventEnvelope = z.infer<typeof eventEnvelopeSchema>;
