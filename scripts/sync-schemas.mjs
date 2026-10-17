#!/usr/bin/env node
// Copies the TypeScript wire JSON Schemas and golden fixtures into conformance/schemas,
// recording the source commit in conformance/schemas/SOURCE.json.
// Syncing also writes conformance/schemas/refinements.json and submission-tools.json from the built TypeScript wire package
// (packages/wire/dist): the zod refinements, defaults, and date formats that the generated JSON
// Schemas cannot express, with each refinement's location and source text.
// --check exits 1 if the vendored schemas or fixtures differ from the TypeScript tree (line endings
// ignored). It does not rebuild refinements.json; the strict paths in port-map.json cover those sources.
// The TypeScript tree is --ts-root, else NOSH_TS_ROOT, else the parent of this worktree.
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { spawnSync } from "node:child_process";

const rustRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const args = process.argv.slice(2);
const check = args.includes("--check");
const tsRoot = resolve(option("--ts-root") ?? process.env.NOSH_TS_ROOT ?? resolve(rustRoot, ".."));
const vendored = join(rustRoot, "conformance", "schemas");
const sources = [
  ["packages/wire/src/schemas", "json"],
  ["fixtures/schemas", "fixtures"],
];

if (check) {
  let differences = 0;
  for (const [from, to] of sources) {
    const theirs = listFiles(join(tsRoot, from));
    const ours = listFiles(join(vendored, to));
    if (!theirs.length) {
      console.log(`missing in TypeScript tree: ${from}`);
      differences += 1;
    }
    for (const file of new Set([...theirs, ...ours])) {
      const a = theirs.includes(file) ? text(join(tsRoot, from, file)) : null;
      const b = ours.includes(file) ? text(join(vendored, to, file)) : null;
      if (a === b) continue;
      console.log(`${a === null ? "removed upstream" : b === null ? "added upstream" : "changed"}: ${from}/${file}`);
      differences += 1;
    }
  }
  if (differences) {
    console.log(`${differences} difference(s); run node scripts/sync-schemas.mjs and port the changes`);
    process.exit(1);
  }
  console.log("vendored schemas and fixtures match the TypeScript tree");
} else {
  for (const [from, to] of sources) {
    const files = listFiles(join(tsRoot, from));
    if (!files.length) throw new Error(`no files under ${join(tsRoot, from)}`);
    rmSync(join(vendored, to), { recursive: true, force: true });
    for (const file of files) {
      const target = join(vendored, to, file);
      mkdirSync(dirname(target), { recursive: true });
      writeFileSync(target, text(join(tsRoot, from, file)));
    }
    console.log(`copied ${files.length} files from ${from}`);
  }
  const head = spawnSync("git", ["-C", tsRoot, "rev-parse", "HEAD"], { encoding: "utf8", windowsHide: true });
  if (head.status !== 0) throw new Error(head.stderr);
  writeFileSync(join(vendored, "SOURCE.json"), `${JSON.stringify({ commit: head.stdout.trim() }, null, 2)}\n`);
  const refinements = await extractRefinements();
  writeFileSync(join(vendored, "refinements.json"), `${JSON.stringify(refinements, null, 2)}\n`);
  console.log(`recorded ${refinements.refinements.length} refinement locations and ${refinements.defaults.length} defaults`);
  const { submissionToolSchemas } = await import(pathToFileURL(join(tsRoot, "packages", "wire", "dist", "submission-tools.js")).href);
  writeFileSync(join(vendored, "submission-tools.json"), `${JSON.stringify(submissionToolSchemas, null, 2)}
`);
  console.log(`recorded the schemas of ${Object.keys(submissionToolSchemas).length} submission tools`);
  const vectors = await buildVectors();
  writeFileSync(join(vendored, "vectors.json"), `${JSON.stringify(vectors, null, 2)}\n`);
  console.log(`recorded TypeScript verdicts for ${vectors.records.length} record, ${vectors.canonical.length} canonical-JSON, and ${vectors.taskTerminal.length} task-terminal cases`);
}

// Runs conformance/cases.json through the TypeScript validator and canonicalizer.
async function buildVectors() {
  const dist = join(tsRoot, "packages", "wire", "dist");
  const { validateRecord } = await import(pathToFileURL(join(dist, "registry.js")).href);
  const { canonicalJson, sha256 } = await import(pathToFileURL(join(dist, "json.js")).href);
  const { isTaskTerminalRecord } = await import(pathToFileURL(join(dist, "submissions.js")).href);
  const cases = JSON.parse(readFileSync(join(rustRoot, "conformance", "cases.json"), "utf8"));
  const records = cases.records.map((c) => {
    let record = JSON.parse(readFileSync(join(tsRoot, "fixtures", "schemas", c.schema, `${c.base ?? "full.valid"}.json`), "utf8"));
    if (c.pick) record = Object.fromEntries(c.pick.map((key) => [key, record[key]]));
    for (const pointer of c.delete ?? []) {
      const [parent, key] = locate(record, pointer);
      delete parent[key];
    }
    for (const [pointer, value] of Object.entries(c.set ?? {})) {
      const [parent, key] = locate(record, pointer);
      parent[key] = value?.$repeat ? value.$repeat[0].repeat(value.$repeat[1]) : value;
    }
    let input = JSON.stringify(record);
    if (c.rawReplace) {
      const [from, to] = c.rawReplace;
      if (input.split(from).length !== 2) throw new Error(`${c.name}: rawReplace must match exactly once`);
      input = input.replace(from, to);
    }
    const result = validateRecord(`https://nosh.dev/schemas/${c.schema}/v1`, JSON.parse(input));
    const outcome = result.ok ? "accept" : result.errors.every((issue) => issue.code === "custom") ? "custom" : "reject";
    if (outcome !== c.expect) throw new Error(`${c.name}: TypeScript says ${outcome}, case expects ${c.expect}: ${JSON.stringify(result.errors ?? null)}`);
    return { name: c.name, schema: c.schema, input, outcome, ...(result.ok ? { value: result.value } : { issues: result.errors }) };
  });
  const canonical = cases.canonical.map((input) => {
    const value = JSON.parse(input);
    return { input, canonical: canonicalJson(value), sha256: sha256(value) };
  });
  const taskTerminal = cases.taskTerminal.map((input) => ({ input, terminal: isTaskTerminalRecord(input) }));
  return { records, canonical, taskTerminal };
}

function locate(root, pointer) {
  const parts = pointer.split("/").slice(1).map((part) => part.replaceAll("~1", "/").replaceAll("~0", "~"));
  const key = parts.pop();
  let parent = root;
  for (const part of parts) {
    if (parent?.[part] === undefined) throw new Error(`no ${part} on the way to ${pointer}`);
    parent = parent[part];
  }
  return [parent, key];
}

async function extractRefinements() {
  const wire = join(tsRoot, "packages", "wire");
  const registryPath = join(wire, "dist", "registry.js");
  if (!existsSync(registryPath)) throw new Error(`${registryPath} is missing; run corepack pnpm build in the TypeScript tree`);
  const newestSource = Math.max(...listFiles(join(wire, "src")).filter((f) => f.endsWith(".ts")).map((f) => statSync(join(wire, "src", f)).mtimeMs));
  if (newestSource > statSync(registryPath).mtimeMs) throw new Error("packages/wire/dist is older than its sources; run corepack pnpm build in the TypeScript tree");
  const { schemaFor, schemaUris } = await import(pathToFileURL(registryPath).href);
  const { datetimeRegex } = await import(pathToFileURL(createRequire(join(wire, "package.json")).resolve("zod")).href);

  const out = { formats: {}, patternMessages: {}, refinements: [], defaults: [] };
  const patternMessages = new Map();
  const datetimeArgs = new Set();
  const wrappers = new Set();
  const stringChecks = new Set(["min", "max", "regex", "datetime", "date", "url"]);
  const numberChecks = new Set(["int", "min", "max", "finite"]);
  for (const uri of schemaUris()) {
    const schema = /\/schemas\/([^/]+)\/v1$/.exec(uri)[1];
    walk(schemaFor(uri), [], [], new Set());
    // Mirrors zod's parse: a refinement or default inside a discriminated-union branch applies only
    // when the discriminator selects that branch ("when").
    function walk(node, path, when, lazyParents) {
      const def = node._def;
      switch (def.typeName) {
        case "ZodEffects": {
          if (def.effect.type !== "refinement") throw new Error(`${schema} /${path.join("/")}: unsupported zod effect ${def.effect.type}`);
          const source = def.effect.refinement.toString();
          // .refine(check) hides check inside zod's generic wrapper, so its source cannot tell checks apart.
          if (source.includes("const result = check(val)")) wrappers.add(def.effect.refinement);
          out.refinements.push({ schema, path, when, rule: createHash("sha256").update(source).digest("hex"), source });
          return walk(def.schema, path, when, lazyParents);
        }
        case "ZodObject":
          for (const [key, value] of Object.entries(def.shape())) walk(value, [...path, key], when, lazyParents);
          return;
        case "ZodArray":
          return walk(def.type, [...path, "*"], when, lazyParents);
        case "ZodRecord":
          return walk(def.valueType, [...path, "*"], when, lazyParents);
        case "ZodOptional":
        case "ZodNullable":
          return walk(def.innerType, path, when, lazyParents);
        case "ZodDefault":
          out.defaults.push({ schema, path, when, value: def.defaultValue() });
          return walk(def.innerType, path, when, lazyParents);
        case "ZodDiscriminatedUnion":
          for (const option of def.options) {
            walk(option, path, [...when, { path, key: def.discriminator, equals: option._def.shape()[def.discriminator]._def.value }], lazyParents);
          }
          return;
        case "ZodUnion":
          // Branch-dependent refinements in a plain union would need zod's first-match semantics.
          for (const option of def.options) {
            const before = out.refinements.length + out.defaults.length;
            walk(option, path, when, lazyParents);
            if (out.refinements.length + out.defaults.length !== before) throw new Error(`${schema} /${path.join("/")}: refinement inside a plain union branch is not supported`);
          }
          return;
        case "ZodLazy":
          if (lazyParents.has(node)) return; // recursive JSON values hold no refinements
          return walk(def.getter(), path, when, new Set([...lazyParents, node]));
        case "ZodString":
          for (const c of def.checks) {
            if (!stringChecks.has(c.kind)) throw new Error(`${schema} /${path.join("/")}: unsupported string check ${c.kind}`);
            if (c.kind === "datetime") datetimeArgs.add(JSON.stringify({ offset: c.offset, precision: c.precision, local: c.local }));
            // A custom message replaces zod's default; only regex messages are mapped (by pattern).
            if (c.kind === "regex") {
              const message = c.message ?? null;
              if (patternMessages.has(c.regex.source) && patternMessages.get(c.regex.source) !== message) throw new Error(`${schema} /${path.join("/")}: pattern ${c.regex.source} has different messages in different places`);
              patternMessages.set(c.regex.source, message);
            } else if (c.message) {
              throw new Error(`${schema} /${path.join("/")}: custom message on a ${c.kind} check is not mapped`);
            }
          }
          return;
        case "ZodNumber":
          for (const c of def.checks) if (!numberChecks.has(c.kind)) throw new Error(`${schema} /${path.join("/")}: unsupported number check ${c.kind}`);
          return;
        case "ZodLiteral":
        case "ZodEnum":
        case "ZodBoolean":
        case "ZodNull":
          return;
        default:
          throw new Error(`${schema} /${path.join("/")}: unsupported zod type ${def.typeName}`);
      }
    }
  }
  if (wrappers.size > 1) throw new Error("more than one .refine() check exists; extend sync-schemas.mjs to tell them apart");
  if (datetimeArgs.size !== 1) throw new Error(`expected one datetime variant, found ${[...datetimeArgs].join(", ")}`);
  const datetime = datetimeRegex(JSON.parse([...datetimeArgs][0])).source;
  // zod's .date() uses ^<date>$, where <date> is the prefix of the datetime regex before "T".
  const date = `^${datetime.slice(1, datetime.indexOf("T([01]"))}$`;
  if (!new RegExp(date).test("2024-02-29") || new RegExp(date).test("2023-02-29")) throw new Error("could not derive zod's date regex");
  out.formats = { "date-time": datetime, date };
  out.patternMessages = Object.fromEntries([...patternMessages].filter(([, message]) => message !== null));
  return out;
}

function option(flag) {
  const index = args.indexOf(flag);
  return index >= 0 ? args[index + 1] : undefined;
}
function listFiles(dir, prefix = "") {
  if (!existsSync(dir)) return [];
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) =>
    entry.isDirectory()
      ? listFiles(join(dir, entry.name), `${prefix}${entry.name}/`)
      : [`${prefix}${entry.name}`],
  );
}
function text(path) {
  return readFileSync(path, "utf8").replace(/\r\n/g, "\n");
}
