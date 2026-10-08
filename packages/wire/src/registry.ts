import { z } from "zod";
import { eventEnvelopeSchema } from "./events.js";
import { templateRegistry } from "./templates.js";
import { submissionRegistry } from "./submissions.js";
import { domainRegistry } from "./domain-records.js";
import { runtimeRegistry } from "./runtime.js";

const entries: Array<[string, z.ZodTypeAny]> = [
  ["https://nosh.dev/schemas/event/v1", eventEnvelopeSchema],
  ...Object.entries(templateRegistry),
  ...Object.entries(submissionRegistry),
  ...Object.entries(domainRegistry),
  ...Object.entries(runtimeRegistry),
];

const schemas: ReadonlyMap<string, z.ZodTypeAny> = new Map(entries);

export function schemaFor(uri: string): z.ZodTypeAny | undefined {
  return schemas.get(uri);
}

export function schemaUris(): string[] {
  return [...schemas.keys()].sort();
}

export function validateRecord(uri: string, value: unknown):
  | { ok: true; value: unknown }
  | { ok: false; errors: Array<{ pointer: string; code: string; message: string }> } {
  const schema = schemaFor(uri);
  if (!schema) {
    return { ok: false, errors: [{ pointer: "/$schema", code: "unknown_schema", message: `Unsupported schema: ${uri}` }] };
  }

  const result = schema.safeParse(value);
  if (result.success) return { ok: true, value: result.data };

  const errors: Array<{ pointer: string; code: string; message: string }> = [];
  for (const issue of result.error.issues) {
    if (issue.code === "unrecognized_keys") {
      for (const key of issue.keys) errors.push({ pointer: pointerFor([key]), code: issue.code, message: `Unrecognized key: ${key}` });
    } else {
      // Corrections need the allowed shape, not just "Required": name what was expected and what arrived.
      const detail = issue.code === "invalid_type" && issue.message === "Required" ? `: missing; expected ${issue.expected}` : "";
      errors.push({ pointer: pointerFor(issue.path), code: issue.code, message: `${issue.message}${detail}` });
    }
  }
  return { ok: false, errors };
}

function pointerFor(path: PropertyKey[]): string {
  return path.length === 0 ? "" : `/${path.map((part) => String(part).replaceAll("~", "~0").replaceAll("/", "~1")).join("/")}`;
}
