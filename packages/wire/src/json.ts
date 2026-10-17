import { canonicalize } from "json-canonicalize";
import { sha256 as hashSha256 } from "@noble/hashes/sha2";
import { bytesToHex } from "@noble/hashes/utils";
import { z } from "zod";

export type JsonObject = { [key: string]: JsonValue | undefined };
export type JsonValue = null | boolean | number | string | JsonValue[] | JsonObject;

export const jsonValueSchema: z.ZodType<JsonValue> = z.lazy(() =>
  z.union([
    z.null(),
    z.boolean(),
    z.number().finite(),
    z.string(),
    z.array(jsonValueSchema),
    z.record(jsonValueSchema),
  ]),
);

export function canonicalJson(value: JsonValue): string {
  return canonicalize(jsonValueSchema.parse(value));
}

export function sha256(value: JsonValue): string {
  return `sha256:${bytesToHex(hashSha256(new TextEncoder().encode(canonicalJson(value))))}`;
}
