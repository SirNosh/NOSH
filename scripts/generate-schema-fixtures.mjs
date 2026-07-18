import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { zodToJsonSchema } from "zod-to-json-schema";
import { canonicalJson, schemaFor, schemaUris, sha256 } from "../packages/wire/dist/index.js";

for (const uri of schemaUris()) {
  const name = /\/schemas\/([^/]+)\/v1$/.exec(uri)?.[1]; const schema = schemaFor(uri); if (!name || !schema) throw new Error(`Unresolvable schema ${uri}`);
  const minimal = special(name, sample(schema, false, name)); const full = special(name, sample(schema, true, name));
  for (const [kind, value] of [["minimal", minimal], ["full", full]]) { const result = schema.safeParse(value); if (!result.success) throw new Error(`${name} ${kind}: ${result.error.message}`); }
  const directory = join("fixtures", "schemas", name); mkdirSync(directory, { recursive: true });
  write(directory, "minimal.valid.json", minimal); write(directory, "full.valid.json", full); const missing = structuredClone(minimal); delete missing[Object.keys(missing).find((key) => key !== "$schema")]; write(directory, "missing.invalid.json", missing); write(directory, "unknown.invalid.json", { ...minimal, unknownField: true }); writeFileSync(join(directory, "full.sha256.txt"), `${sha256(full)}\n`, "utf8");
  const document = zodToJsonSchema(schema, { name, target: "jsonSchema7", $refStrategy: "none" }); document.$schema = "https://json-schema.org/draft/2020-12/schema"; document.$id = uri; const output = join("packages", "wire", "src", "schemas"); mkdirSync(output, { recursive: true }); writeFileSync(join(output, `${name}.v1.schema.json`), `${JSON.stringify(document, null, 2)}\n`, "utf8");
}

function sample(schema, full, key) {
  const type = schema?._def?.typeName;
  if (type === "ZodEffects") return sample(schema._def.schema, full, key);
  if (type === "ZodLazy") return full ? sample(schema._def.getter(), false, key) : null;
  if (type === "ZodDefault") return full ? sample(schema._def.innerType, full, key) : schema._def.defaultValue();
  if (type === "ZodOptional") return full ? sample(schema._def.innerType, full, key) : undefined;
  if (type === "ZodNullable") return full ? sample(schema._def.innerType, full, key) : null;
  if (type === "ZodLiteral") return schema._def.value;
  if (type === "ZodEnum") return schema._def.values[0];
  if (type === "ZodNativeEnum") return Object.values(schema._def.values).find((value) => typeof value === "string");
  if (type === "ZodBoolean") return true;
  if (type === "ZodNull") return null;
  if (type === "ZodNumber") { const minimum = schema._def.checks?.find((check) => check.kind === "min")?.value ?? 0; return Number.isInteger(minimum) ? Math.max(minimum, 1) : minimum; }
  if (type === "ZodString") return string(schema, key);
  if (type === "ZodArray") { const minimum = schema._def.minLength?.value ?? 0; return Array.from({ length: Math.max(minimum, full ? 1 : 0) }, () => sample(schema._def.type, full, singular(key))); }
  if (type === "ZodRecord") return full ? { item: sample(schema._def.valueType, full, "item") } : {};
  if (type === "ZodObject") { const value = {}; for (const [property, child] of Object.entries(schema._def.shape())) { const generated = sample(child, full, property); if (generated !== undefined) value[property] = generated; } return value; }
  if (type === "ZodDiscriminatedUnion") return sample([...schema._def.options.values()][0], full, key);
  if (type === "ZodUnion") return sample(schema._def.options[0], full, key);
  if (type === "ZodIntersection") return { ...sample(schema._def.left, full, key), ...sample(schema._def.right, full, key) };
  if (["ZodAny", "ZodUnknown"].includes(type)) return full ? { value: "sample" } : null;
  throw new Error(`No fixture sampler for ${type} at ${key}`);
}

function string(schema, key) {
  if (key === "$schema") throw new Error("Schema URI must be a literal"); if (schema._def.checks?.some((check) => check.kind === "url")) return "https://example.com/resource"; if (schema._def.checks?.some((check) => check.kind === "date")) return "2026-07-17"; if (/At$|Timestamp$|timestamp/i.test(key) || schema._def.checks?.some((check) => check.kind === "datetime")) return "2026-07-17T00:00:00.000Z";
  if (/Version$/i.test(key)) return "1.0.0";
  if (/Hash$|Digest$|checksum/i.test(key)) return `sha256:${"0".repeat(64)}`; const prefix = idPrefix(key); if (prefix) return `${prefix}_${"0".repeat(32)}`;
  const regex = schema._def.checks?.find((check) => check.kind === "regex")?.regex?.source ?? ""; const id = /^\^([a-z]+)_/.exec(regex)?.[1]; if (id) return `${id}_${"0".repeat(32)}`; if (regex.includes("sha256:")) return `sha256:${"0".repeat(64)}`; if (regex.includes("(?:\\.[a-z]")) return "event.valid"; if (regex.includes("[a-z0-9.:-]")) return "ref_1"; if (regex.includes("[A-Za-z0-9_-]")) return "opaque_AA";
  const minimum = schema._def.checks?.find((check) => check.kind === "min")?.value ?? 1; return "sample".padEnd(minimum, "x");
}
function idPrefix(key) { const values = [["project", "prj"], ["mission", "mis"], ["direction", "dir"], ["autoresearch", "ar"], ["experiment", "exp"], ["run", "run"], ["job", "job"], ["task", "tsk"], ["review", "rev"], ["artifact", "art"], ["evidence", "evd"], ["claim", "clm"], ["agent", "agt"], ["handoff", "hnd"], ["blocker", "blk"], ["response", "rsp"], ["event", "evt"], ["command", "cmd"], ["snapshot", "snap"]]; const lower = key.toLowerCase(); if (!lower.endsWith("id")) return undefined; return values.map(([name, prefix]) => ({ prefix, position: lower.lastIndexOf(name) })).filter((item) => item.position >= 0).sort((a, b) => b.position - a.position)[0]?.prefix; }
function singular(key) { return key.endsWith("Ids") ? key.slice(0, -1) : key.endsWith("s") ? key.slice(0, -1) : key; }
function special(name, value) { if (name === "claim" && !value.limitations?.length) value.limitations = ["No evidence yet"]; if (name === "event" && value.retention === "persistent") value.sequence = 1; if (name === "review-request") value.reviewerAgentId = `agt_${"1".repeat(32)}`; if (name === "review-verdict" && value.defects?.length) value.verdict = "REVISE"; if (name === "skill-manifest" && value.executionMode === "prompt") value.programId = null; if ("openDefectIds" in value) value.openDefectIds = []; if (value.claimAudit) value.claimAudit.unresolvedClaimIds = []; return value; }
function write(directory, name, value) { writeFileSync(join(directory, name), `${canonicalJson(value)}\n`, "utf8"); }
