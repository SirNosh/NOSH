import { idPrefixes, type IdPrefix } from "@nosh/core";
import { z } from "zod";

export const schemaVersion = z.literal(1);
export const templateVersion = z.string().regex(/^\d+\.\d+\.\d+$/);
export const timestamp = z.string().datetime({ offset: true, precision: 3 });
export const sha256Digest = z.string().regex(/^sha256:[0-9a-f]{64}$/);
export const opaqueId = z.string().regex(new RegExp(`^(${idPrefixes.join("|")})_[0-9a-f]{32}$`));

export function id(prefix: IdPrefix) {
  return z.string().regex(new RegExp(`^${prefix}_[0-9a-f]{32}$`));
}

export function schemaUri(record: string): string {
  return `https://nosh.dev/schemas/${record}/v1`;
}

export const scopeSchema = z
  .object({
    projectId: id("prj"),
    missionId: id("mis").nullable(),
    directionId: id("dir").nullable(),
    autoresearchId: id("ar").nullable(),
    experimentId: id("exp").nullable(),
    runId: id("run").nullable(),
    jobId: id("job").nullable(),
    agentId: id("agt").nullable(),
  })
  .strict();

export const observedVersionsSchema = z.record(z.string(), z.union([z.number().int().nonnegative(), z.string()]));
