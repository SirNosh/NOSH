#!/usr/bin/env node
// Calls the built TypeScript pi-adapter (packages/pi-adapter/dist) for nosh-agent's differential
// tests. Reads a JSON array of ops on stdin and prints one { ok } or { error, syntax } per op:
//   { fn, args, env? }          an exported function; env sets process.env for the call
//   { terminal: [text, ctx] }   parseTerminalOutput, plus { excerpt } = withJsonExcerpt on failure
//   { piError: message }        userFacingPiError(new Error(message)).message
//   { value: name }             an exported constant (a Set becomes a sorted array)
//   { tools: options, env? }    createSessionTools definitions: [{ name, label, description, parameters }]
//   { exec: { options, name, callId, params, receipts, grace?, wake? }, env? }
//                               one tool execute against a scripted daemon; ok = { result | error,
//                               submits, events, wake? } (wake: the next async message, when asked)
//   { turn: { context, admits?, prompts, submits, rejects } }
//                               runTerminalTurn with a scripted host and provider; ok = { result | error, trace }
//   { commit: { options, heads, message, paths } }   commitWorkspaceFiles; ok = { commit, paths }
//   { commitAll: { options, message } }              commitAllWorkspaceChanges; ok = { commit, paths } or null
//   { git: { cwd, args } }                           test setup: git with a hermetic config and fixed identity and dates
//   { validate: { options, calls } }               Pi's validateToolArguments verdicts ({ ok: args } | { error }) for calls
//                                                    [{ name, arguments }] against the session's TypeBox tools; ok =
//                                                    { verdicts, parameters: { name: JSON text } }
//   { writeFile: [path, content] } / { removeFile: path } / { mkdir: path } / { link: [existing, path] }   test setup
// A scripted reply is { value }, { throw: message }, or { delay: ms, value }; a missing one is { accepted: true }.
// The TypeScript tree is NOSH_TS_ROOT, else the parent of this worktree.
import { linkSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const tsRoot = resolve(process.env.NOSH_TS_ROOT ?? resolve(dirname(fileURLToPath(import.meta.url)), "..", ".."));
const adapter = await import(pathToFileURL(join(tsRoot, "packages/pi-adapter/dist/index.js")).href);
const ops = JSON.parse(readFileSync(0, "utf8"));

async function withEnv(env, work) {
  const previous = Object.fromEntries(Object.keys(env ?? {}).map((key) => [key, process.env[key]]));
  for (const [key, value] of Object.entries(env ?? {})) { if (value === null) delete process.env[key]; else process.env[key] = value; }
  try { return await work(); } finally { for (const [key, value] of Object.entries(previous)) { if (value === undefined) delete process.env[key]; else process.env[key] = value; } }
}

function reply(script) {
  const next = script.shift() ?? { value: { accepted: true } };
  if (next.throw !== undefined) throw new Error(next.throw);
  if (next.delay !== undefined) return new Promise((resolve) => setTimeout(() => resolve(next.value), next.delay));
  return next.value;
}

const hermeticGit = { GIT_CONFIG_GLOBAL: process.platform === "win32" ? "NUL" : "/dev/null", GIT_CONFIG_NOSYSTEM: "1", GIT_AUTHOR_NAME: "Test", GIT_AUTHOR_EMAIL: "test@example.invalid", GIT_COMMITTER_NAME: "Test", GIT_COMMITTER_EMAIL: "test@example.invalid", GIT_AUTHOR_DATE: "2026-01-01T00:00:00Z", GIT_COMMITTER_DATE: "2026-01-01T00:00:00Z" };
function git({ cwd, args }) {
  const result = spawnSync("git", args, { cwd, encoding: "utf8", windowsHide: true, env: { ...process.env, ...hermeticGit } });
  if (result.status !== 0) throw new Error(`git ${args.join(" ")}: ${result.stderr}`);
  return result.stdout.trim();
}

const errorText = (error) => error instanceof Error ? error.message : String(error);

async function exec({ options, name, callId, params, receipts, grace, wake }) {
  const submits = []; const events = []; const script = [...(receipts ?? [])];
  const calls = new adapter.AsyncToolCalls(grace ?? 2000);
  const tools = adapter.createSessionTools(options, (...values) => { submits.push(values); return reply(script); }, calls, (event) => events.push(event));
  const tool = tools.find((candidate) => candidate.name === name);
  const outcome = {};
  try { const result = await tool.execute(callId, params, undefined, undefined, {}); outcome.result = { content: result.content, details: result.details ?? null }; }
  catch (error) { outcome.error = errorText(error); }
  if (wake) outcome.wake = (await calls.next(10_000)) ?? null;
  return { ...outcome, submits, events };
}

async function turn({ context, admits, prompts, submits, rejects }) {
  const trace = []; const promptScript = [...prompts]; const submitScript = [...submits]; const rejectScript = [...rejects]; const admitScript = [...(admits ?? [])];
  const host = {
    submit: async (_context, records) => { trace.push(["submit", records]); return reply(submitScript); },
    reject: async (_context, reason) => { trace.push(["reject", reason]); return reply(rejectScript); },
    ...(admits ? { admit: async () => { trace.push(["admit"]); const next = admitScript.shift(); return next === null || next === undefined ? undefined : reply([next]); } } : {}),
  };
  const prompt = async (correction) => { trace.push(["prompt", correction ?? null]); return reply(promptScript); };
  try { return { result: await adapter.runTerminalTurn(context, host, prompt), trace }; }
  catch (error) { return { error: errorText(error), trace }; }
}

async function validate({ options, calls }) {
  const { validateToolArguments } = await import(pathToFileURL(join(tsRoot, "packages/pi-adapter/node_modules/@earendil-works/pi-ai/dist/index.js")).href);
  const tools = new Map(adapter.createSessionTools(options, () => ({ accepted: true })).map((tool) => [tool.name, tool]));
  const verdicts = calls.map((call) => { try { return { ok: validateToolArguments(tools.get(call.name), call) }; } catch (error) { return { error: errorText(error) }; } });
  return { verdicts, parameters: Object.fromEntries([...tools].map(([name, tool]) => [name, JSON.stringify(tool.parameters)])) };
}

const output = [];
for (const op of ops) {
  try {
    if (op.terminal) {
      const [text, context] = op.terminal;
      try { output.push({ ok: adapter.parseTerminalOutput(text, context) }); }
      catch (error) { output.push({ error: error.message, syntax: error instanceof SyntaxError, excerpt: adapter.withJsonExcerpt(error, text) }); }
      continue;
    }
    if (op.piError !== undefined) { output.push({ ok: adapter.userFacingPiError(new Error(op.piError)).message }); continue; }
    if (op.value) { const value = adapter[op.value]; output.push({ ok: value instanceof Set ? [...value].sort() : value }); continue; }
    if (op.tools) { output.push({ ok: await withEnv(op.env, () => adapter.createSessionTools(op.tools, () => ({ accepted: true })).map(({ name, label, description, parameters }) => JSON.parse(JSON.stringify({ name, label, description, parameters })))) }); continue; }
    if (op.exec) { output.push({ ok: await withEnv(op.env, () => exec(op.exec)) }); continue; }
    if (op.commit) { output.push({ ok: adapter.commitWorkspaceFiles(op.commit.options, { heads: new Set(op.commit.heads ?? []) }, op.commit.message, op.commit.paths) }); continue; }
    if (op.commitAll) { output.push({ ok: adapter.commitAllWorkspaceChanges(op.commitAll.options, op.commitAll.message) }); continue; }
    if (op.git) { output.push({ ok: git(op.git) }); continue; }
    if (op.writeFile) { writeFileSync(op.writeFile[0], op.writeFile[1]); output.push({ ok: null }); continue; }
    if (op.mkdir) { mkdirSync(op.mkdir, { recursive: true }); output.push({ ok: null }); continue; }
    if (op.link) { linkSync(op.link[0], op.link[1]); output.push({ ok: null }); continue; }
    if (op.removeFile) { rmSync(op.removeFile, { recursive: true, force: true }); output.push({ ok: null }); continue; }
    if (op.validate) { output.push({ ok: await validate(op.validate) }); continue; }
    if (op.turn) { output.push({ ok: await turn(op.turn) }); continue; }
    output.push({ ok: (await withEnv(op.env, () => adapter[op.fn](...(op.args ?? [])))) ?? null });
  } catch (error) {
    output.push({ error: error instanceof Error ? error.message : String(error), syntax: error instanceof SyntaxError });
  }
}
// Lone surrogates (a UTF-16 slice through a pair) become U+FFFD, as Rust strings hold them.
process.stdout.write(JSON.stringify(output, (_key, value) => typeof value === "string" ? value.toWellFormed() : value));
