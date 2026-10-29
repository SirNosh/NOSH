#!/usr/bin/env node
// Hosts Pi agent sessions for the Rust daemon (crates/nosh-agent/src/pi.rs), as
// packages/pi-adapter/src/index.ts and auth.ts do in TypeScript. It owns everything that touches a
// Pi object or Pi's message shapes; Rust owns prompts, tools, permissions and NOSH events.
//
// Protocol: one JSON object per line on stdin and stdout. Requests are { id, method, params } and
// are answered with { id, result } or { id, error: { message } }; notifications have no id. Both
// sides send requests: Rust calls the methods below, and the host calls Rust's `tool` and
// `auth.prompt`. Events are the notification { method: "event", params: { agentId, kind, ... } },
// written before the reply of the call that caused them. A sign-in prompt Pi abandons (its
// `signal` aborts) is the notification { method: "auth.cancel", params: { id } }.

// Pi and its dependencies may print; stdout carries only protocol lines.
console.log = console.info = console.debug = console.error;

import { existsSync, readFileSync, statSync } from "node:fs";
import { homedir } from "node:os";
import { basename, dirname, join, resolve } from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath, pathToFileURL } from "node:url";

const PI_VERSION = "0.80.10";
const here = dirname(fileURLToPath(import.meta.url));

// Pi's own install: the source tree's (npm ci here), else the nosh-runtime package's dependencies as npm installs
// them: under the package (npm install -g) or beside it (npm install --prefix, as the nosh-harness launcher does).
const packageRoot = resolve(here, "..", "..");
const modules = [join(here, "node_modules"), join(packageRoot, "node_modules"), ...(basename(dirname(packageRoot)) === "node_modules" ? [dirname(packageRoot)] : [])]
  .find((directory) => existsSync(join(directory, "@earendil-works", "pi-coding-agent", "package.json"))) ?? join(here, "node_modules");

/**
 * Loads a Pi package from that install, never by bare specifier, so the host cannot resolve another
 * project's copy. pi-coding-agent ships an npm shrinkwrap, which installs its own pi-ai under it;
 * that copy is the one its runtime uses, so the host uses it too.
 */
async function piPackage(name) {
  const agent = join(modules, "@earendil-works", "pi-coding-agent");
  const directory = name === "pi-coding-agent" ? agent : [join(agent, "node_modules", "@earendil-works", name), join(modules, "@earendil-works", name)].find((candidate) => existsSync(candidate));
  if (!directory || !existsSync(join(directory, "package.json"))) throw new Error(`@earendil-works/${name} is not installed; run npm ci in ${here}`);
  const manifest = JSON.parse(readFileSync(join(directory, "package.json"), "utf8"));
  if (manifest.version !== PI_VERSION) throw new Error(`@earendil-works/${name} ${manifest.version} is installed; the NOSH Pi host needs ${PI_VERSION} (run npm ci in ${here})`);
  return import(pathToFileURL(join(directory, manifest.exports["."].import)).href);
}

// A missing install ends the host with one line on stderr, which Rust reports.
const loaded = await Promise.all([piPackage("pi-coding-agent"), piPackage("pi-ai")]).catch((error) => { console.error(error.message); process.exitCode = 1; });
const [{ DefaultResourceLoader, ModelRuntime, SettingsManager, createAgentSession, defineTool, getAgentDir }, { Type, fauxAssistantMessage, fauxProvider, fauxToolCall, getSupportedThinkingLevels, validateToolArguments }] = loaded ?? [{}, {}];

// ---- Transport

let nextId = 0;
const waiting = new Map();
function send(message) { process.stdout.write(`${JSON.stringify(message)}\n`); }
function notify(method, params) { send({ method, params }); }
/** An AbortSignal cannot cross the protocol: its abort is sent as `auth.cancel` with the request's id. */
function request(method, params, signal) {
  const id = `h${++nextId}`;
  signal?.addEventListener("abort", () => notify("auth.cancel", { id }), { once: true });
  return new Promise((resolve, reject) => { waiting.set(id, { resolve, reject }); send({ id, method, params }); });
}

if (loaded) createInterface({ input: process.stdin, crlfDelay: Infinity })
  .on("line", (line) => {
    if (!line.trim()) return;
    const message = JSON.parse(line);
    if (message.method) { void handle(message); return; }
    const entry = waiting.get(message.id); if (!entry) return;
    waiting.delete(message.id);
    if (message.error) entry.reject(new Error(message.error.message)); else entry.resolve(message.result);
  })
  // The daemon is gone; its sessions go with it.
  .on("close", () => process.exit(0));

async function handle({ id, method, params }) {
  try {
    const run = methods[method];
    if (!run) throw new Error(`Unknown Pi host method ${method}`);
    const result = await run(params ?? {});
    if (id !== undefined) send({ id, result: result ?? null });
  } catch (error) {
    if (id !== undefined) send({ id, error: { message: error instanceof Error ? error.message : String(error) } });
    else console.error(error);
  }
}

// ---- Model runtime

/** A test runtime replaces Pi's; like an injected TypeScript runtime it is never rebuilt. */
let injectedRuntime;
let ownRuntime;
let credentialStamp = "";

async function runtime() {
  if (injectedRuntime) return injectedRuntime;
  if (!ownRuntime) { credentialStamp = piCredentialStamp(); ownRuntime = await ModelRuntime.create(); }
  return ownRuntime;
}

/**
 * An account connected or removed with `nosh login`/`logout` after the daemon started changes Pi's credential file,
 * not the cached runtime: rebuild the runtime when that file changed, or when it still offers no model.
 */
async function runtimeWithModels() {
  const current = await runtime();
  if (injectedRuntime) return current;
  const stamp = piCredentialStamp();
  if (stamp === credentialStamp && (await current.getAvailable()).length) return current;
  credentialStamp = stamp; ownRuntime = await ModelRuntime.create();
  return ownRuntime;
}

/** Identity of Pi's credential file (shared with the `pi` CLI and `nosh login`); changes on every login, logout, and token refresh. */
function piCredentialStamp() {
  try { const stat = statSync(join(getAgentDir(), "auth.json")); return `${stat.mtimeMs}:${stat.size}`; } catch { return "none"; }
}

/** Sessions use in-memory settings, so Pi's global defaultThinkingLevel (~/.pi/agent/settings.json) is applied explicitly; the home cwd keeps repository .pi files out. */
function globalDefaultThinkingLevel() { try { return SettingsManager.create(homedir()).getDefaultThinkingLevel(); } catch { return undefined; } }

/** `selection` was checked against modelSelectionSchema (and trimmed) by Rust. */
async function resolveModelSelection(selection) {
  const model = (await (await runtime()).getAvailable()).find((candidate) => candidate.provider === selection.provider && candidate.id === selection.id);
  if (!model) throw new Error(`Unavailable Pi model ${selection.provider}/${selection.id}`);
  const available = getSupportedThinkingLevels(model);
  if (selection.thinkingLevel !== undefined && !available.includes(selection.thinkingLevel)) throw unsupportedThinkingLevel(model, selection.thinkingLevel, available);
  return { model, ...(selection.thinkingLevel === undefined ? {} : { thinkingLevel: selection.thinkingLevel }) };
}

function unsupportedThinkingLevel(model, level, available) {
  const identity = model ? `${model.provider}/${model.id}` : "the selected Pi model";
  return new Error(`Thinking level "${level}" is not supported by ${identity}; supported levels: ${available.join(", ")}`);
}

function projectAvailableModel(model, providerName) {
  return { provider: model.provider, providerName, id: model.id, name: model.name, reasoning: model.reasoning, thinkingLevels: getSupportedThinkingLevels(model), contextWindow: model.contextWindow, maxTokens: model.maxTokens };
}

// ---- Sessions

const sessions = new Map();

function promptCacheAffinity(key) {
  return (pi) => pi.on("before_provider_request", ({ payload }) => isRecord(payload) && "prompt_cache_key" in payload ? { ...payload, prompt_cache_key: key } : undefined);
}

/**
 * Rebuilds a tool's plain JSON Schema parameters as TypeBox. Pi's validator converts arguments
 * through TypeBox's hidden ~kind/~optional metadata (null becomes "null", a missing required string
 * ""), which JSON drops; plain JSON Schema would get different verdicts. Only the shapes NOSH tools
 * use are accepted, so a new shape fails here instead of validating differently.
 */
function revive(schema) {
  if (!isRecord(schema)) throw new Error("Unsupported tool parameter schema");
  const { type, properties, required, ...options } = schema;
  if (type === "object") {
    const names = Object.keys(properties ?? {});
    const mandatory = required ?? [];
    if (!Array.isArray(mandatory) || mandatory.some((name) => !names.includes(name))) throw new Error("Unsupported tool parameter schema");
    return Type.Object(Object.fromEntries(names.map((name) => { const value = revive(properties[name]); return [name, mandatory.includes(name) ? value : Type.Optional(value)]; })), options);
  }
  if (properties !== undefined || required !== undefined) throw new Error("Unsupported tool parameter schema");
  if (type === "string") return Type.String(options);
  if (type === "integer") return Type.Integer(options);
  if (type === undefined) return Type.Unknown(options);
  throw new Error(`Unsupported tool parameter type ${JSON.stringify(type)}`);
}

/**
 * A NOSH tool whose execution happens in Rust; a failure there is thrown, which Pi reports as an error result.
 * `parameters` is JSON text, so the schema keeps the key order Rust gave it.
 */
function sessionTool(agentId, { name, label, description, parameters }) {
  return defineTool({ name, label, description, parameters: revive(JSON.parse(parameters)), execute: (toolCallId, params) => request("tool", { agentId, name, toolCallId, params }) });
}

function state(session) {
  const usage = session.getSessionStats().contextUsage;
  return { activeToolIds: session.getActiveToolNames(), modelProvider: session.model?.provider ?? null, modelId: session.model?.id ?? null, modelName: session.model?.name ?? null, thinkingLevel: session.thinkingLevel, contextTokens: usage?.tokens ?? null, contextWindow: usage?.contextWindow ?? session.model?.contextWindow ?? null, contextPercent: usage?.percent ?? null };
}

function required(agentId) {
  const managed = sessions.get(agentId);
  if (!managed) throw new Error(`Unknown Pi agent ${agentId}`);
  return managed;
}

/** Forwards one Pi event; persistent kinds carry the session state, deltas only their content. */
function forward(agentId, managed, event) {
  if (event.type === "agent_end") managed.final = finalAnswer(event.willRetry ? undefined : latestAssistant(event.messages));
  const summary = summarize(event);
  notify("event", { agentId, ...summary, ...(STATELESS.has(summary.kind) ? {} : { state: state(managed.session) }) });
}

const STATELESS = new Set(["tool_update", "text_delta", "other"]);

/** What NOSH records of one Pi event (Rust builds the event draft from it). */
function summarize(event) {
  switch (event.type) {
    case "agent_start": return { kind: "agent_start" };
    case "agent_end": {
      const assistant = latestAssistant(event.messages);
      return { kind: "agent_end", willRetry: event.willRetry, usage: modelUsage(event.messages), failure: assistantFailure(assistant) ?? null, message: answerText(assistant?.content) };
    }
    case "turn_start": return { kind: "turn_start" };
    case "turn_end": return { kind: "turn_end", toolResultCount: event.toolResults.length, allText: allText(event.message) };
    case "tool_execution_start": return { kind: "tool_start", toolCallId: event.toolCallId, toolName: event.toolName, args: json(event.args) };
    case "tool_execution_update": return { kind: "tool_update", toolCallId: event.toolCallId, toolName: event.toolName, partialResult: json(event.partialResult) };
    case "tool_execution_end": return { kind: "tool_end", toolCallId: event.toolCallId, toolName: event.toolName, isError: event.isError, result: json(event.result) };
    case "message_update": return event.assistantMessageEvent.type === "text_delta" ? { kind: "text_delta", delta: event.assistantMessageEvent.delta } : { kind: "other" };
    case "compaction_start": return { kind: "compaction_start", reason: event.reason };
    case "compaction_end": return { kind: "compaction_end", reason: event.reason, aborted: event.aborted, willRetry: event.willRetry, errorMessage: event.errorMessage ?? null };
    // Still activity: it moves the inspection's lastEventAt.
    default: return { kind: "other" };
  }
}

/** What a terminal turn reads from the run's last assistant message. */
function finalAnswer(message) {
  const toolCall = message?.content?.some((block) => !!block && typeof block === "object" && block.type === "toolCall");
  return { text: answerText(message?.content), stopReason: toolCall ? "toolCall" : message?.stopReason ?? "missing" };
}

const methods = {
  async availableModels() {
    const current = await runtimeWithModels();
    const models = await current.getAvailable();
    const providerNames = new Map(current.getProviders().map((provider) => [provider.id, provider.name]));
    return models.map((model) => projectAvailableModel(model, providerNames.get(model.provider) ?? model.provider));
  },

  async resolveModel({ selection }) {
    const { thinkingLevel } = await resolveModelSelection(selection);
    return { thinkingLevel: thinkingLevel ?? null };
  },

  async start({ agentId, cwd, packagePath, systemPrompt, cacheKey, tools, activeToolIds, selection, thinkingLevel: requestedLevel }) {
    const loader = new DefaultResourceLoader({
      cwd,
      agentDir: resolve(cwd, ".nosh", "pi"),
      settingsManager: SettingsManager.inMemory({ packages: [resolve(packagePath)] }),
      noExtensions: true,
      systemPrompt,
      extensionFactories: [promptCacheAffinity(cacheKey)],
    });
    await loader.reload();
    const modelRuntime = await runtimeWithModels();
    const resolved = selection ? await resolveModelSelection(selection) : undefined;
    const model = resolved?.model;
    const thinkingLevel = resolved?.thinkingLevel ?? requestedLevel ?? globalDefaultThinkingLevel();
    const { session } = await createAgentSession({
      cwd, modelRuntime, resourceLoader: loader, customTools: tools.map((tool) => sessionTool(agentId, tool)),
      ...(model ? { model } : {}), ...(activeToolIds ? { tools: activeToolIds } : {}), ...(thinkingLevel ? { thinkingLevel } : {}),
    });
    const managed = { session };
    managed.unsubscribe = session.subscribe((event) => forward(agentId, managed, event));
    sessions.set(agentId, managed);
    return { piSessionId: session.sessionId, contextWindow: session.model?.contextWindow ?? null, state: state(session) };
  },

  /** One session.prompt; `final` is present when the prompt ended an agent run. */
  async prompt({ agentId, text }) {
    const managed = required(agentId);
    managed.final = undefined;
    await managed.session.prompt(text);
    return { final: managed.final, state: state(managed.session) };
  },

  /** Replaces the active tools; returns the previous ones and the new state. */
  setActiveTools({ agentId, names }) {
    const { session } = required(agentId);
    const previous = session.getActiveToolNames();
    session.setActiveToolsByName(names);
    return { previous, state: state(session) };
  },

  /** An async result that lands mid-run is steered in before the model's next call; otherwise it waits for the wake loop. */
  steerIfStreaming({ agentId, text }) {
    const managed = sessions.get(agentId);
    if (!managed?.session.isStreaming) return false;
    void managed.session.steer(text);
    return true;
  },

  async steer({ agentId, text }) { await required(agentId).session.steer(text); },
  async followUp({ agentId, text }) { await required(agentId).session.followUp(text); },
  clearQueue({ agentId }) { return required(agentId).session.clearQueue(); },
  async abort({ agentId }) { const { session } = required(agentId); await session.abort(); return state(session); },
  async compact({ agentId, instructions }) { const { session } = required(agentId); await session.compact(instructions ?? undefined); return state(session); },

  setThinkingLevel({ agentId, level, running }) {
    const { session } = required(agentId);
    const available = session.getAvailableThinkingLevels();
    if (!available.includes(level)) throw unsupportedThinkingLevel(session.model, level, available);
    if (level !== session.thinkingLevel && (session.isStreaming || running)) throw new Error("Stop the current Nosh turn before changing its Pi thinking level");
    session.setThinkingLevel(level);
    return state(session);
  },

  stop({ agentId }) {
    const managed = sessions.get(agentId); if (!managed) return;
    managed.unsubscribe();
    managed.session.dispose();
    sessions.delete(agentId);
  },

  // Model-provider sign-in through Pi's own runtime: subscriptions (OAuth) and API keys are stored
  // where Pi stores them (shared with the `pi` CLI). NOSH never reads or prints credential values.
  async authProviders() {
    const current = await ModelRuntime.create();
    return current.getProviders().map((provider) => {
      const auth = provider.auth ?? {};
      return { id: provider.id, name: provider.name ?? provider.id, subscription: auth.oauth ? auth.oauth.name ?? provider.name ?? provider.id : null, apiKey: auth.apiKey?.login ? auth.apiKey.name ?? `${provider.name ?? provider.id} API key` : null, connected: current.hasConfiguredAuth(provider.id) };
    });
  },

  /** Runs the provider's own sign-in flow; prompts go to Rust as `auth.prompt` requests, progress as `auth.notify`. */
  async login({ providerId, type }) {
    const current = injectedRuntime ?? await ModelRuntime.create();
    await current.login(providerId, type, { prompt: (prompt) => request("auth.prompt", { prompt }, prompt.signal), notify: (event) => notify("auth.notify", { event: json(event) }) });
  },

  async logout({ providerId }) {
    const current = await ModelRuntime.create();
    await current.logout(providerId);
  },
};

// ---- Test hooks (NOSH_PI_HOST_TEST=1): the runtimes and faux providers of pi-adapter.test.ts.

if (process.env.NOSH_PI_HOST_TEST === "1") {
  let faux;
  // A held response and its release meet in one slot, whichever arrives first.
  const holds = new Map();
  const hold = (name) => {
    if (!holds.has(name)) { let reached; holds.set(name, { reached: new Promise((resolve) => { reached = resolve; }), reach: (settle) => reached(settle) }); }
    return holds.get(name);
  };
  const captured = [];
  const fakeAuth = {
    hasConfiguredAuth: () => true, getAuth: async () => ({ auth: { apiKey: "fake" }, env: {} }),
    /** An OAuth flow's shape: a manual-code prompt raced by the browser callback, which wins after 100 ms; the abandoned prompt is aborted, then awaited so its answer reaches the test. */
    async login(providerId, type, interaction) {
      const manual = new AbortController();
      const answer = interaction.prompt({ type: "manual_code", message: "Paste the code", signal: manual.signal }).catch(() => undefined);
      try { await new Promise((resolve) => setTimeout(resolve, 100)); } finally { manual.abort(); }
      await answer;
    },
  };
  const message = ({ text = "", toolCalls, stopReason, errorMessage }) => fauxAssistantMessage(toolCalls ? toolCalls.map((call) => fauxToolCall(call.name, call.arguments, call.id ? { id: call.id } : {})) : text, { ...(stopReason ? { stopReason } : {}), ...(errorMessage ? { errorMessage } : {}) });
  Object.assign(methods, {
    /** Static models (any Model fields) for sessions that never prompt, or a faux provider that streams. */
    "test.runtime"({ models, providers, faux: options }) {
      if (options) {
        faux = fauxProvider(options);
        injectedRuntime = { ...fakeAuth, getAvailable: async () => faux.models, getProviders: () => [faux.provider], streamSimple: (model, context, streamOptions) => faux.provider.stream(model, context, streamOptions) };
        return faux.models;
      }
      injectedRuntime = { ...fakeAuth, getAvailable: async () => models, getProviders: () => providers };
      return models;
    },
    /** Queues responses: { text?, toolCalls?, stopReason?, errorMessage? }, plus { hold } (waits for test.release) or { capture } (records the system prompt, tool names and messages the provider received, as JSON). */
    "test.respond"({ steps }) {
      faux.setResponses(steps.map((step) => {
        if (step.hold) return () => new Promise((settle) => hold(step.hold).reach(settle));
        if (step.capture) return (context) => { captured.push(JSON.stringify({ systemPrompt: context.systemPrompt, tools: (context.tools ?? []).map((tool) => tool.name), messages: context.messages })); return message(step); };
        return message(step);
      }));
    },
    async "test.release"({ hold: name, ...step }) { const settle = await hold(name).reached; holds.delete(name); settle(message(step)); },
    "test.captured"() { return captured; },
    "test.summarize"({ event }) { return summarize(event); },
    /** Pi's verdicts for tool calls against revived NOSH tool parameters (JSON text), and the revived schemas as JSON. */
    "test.validate"({ tools, calls }) {
      const revived = new Map(tools.map((tool) => [tool.name, revive(JSON.parse(tool.parameters))]));
      const verdicts = calls.map((call) => { try { return { ok: validateToolArguments({ name: call.name, parameters: revived.get(call.name) }, call) }; } catch (error) { return { error: error.message }; } });
      return { verdicts, parameters: Object.fromEntries([...revived].map(([name, schema]) => [name, JSON.stringify(schema)])) };
    },
  });
}

// ---- Pi message helpers (as index.ts)

function latestAssistant(messages) {
  return [...messages].reverse().find((entry) => Boolean(entry && typeof entry === "object" && entry.role === "assistant"));
}

/** Every text block of one message, commentary included (where PROGRESS lines accompany tool calls). */
function allText(message) {
  const content = message?.content ?? [];
  return content.filter((block) => Boolean(block && typeof block === "object" && block.type === "text" && typeof block.text === "string")).map((block) => block.text).join(String.fromCharCode(10));
}

/**
 * The reply's answer text. Codex-style models send a "commentary" message before the "final_answer" (the phase is in
 * Pi's text signature); showing or parsing both duplicates the reply and puts prose in front of a terminal envelope.
 */
function answerText(content) {
  const texts = (content ?? []).filter((block) => Boolean(block && typeof block === "object" && block.type === "text" && typeof block.text === "string"));
  const phase = (block) => { try { return JSON.parse(block.textSignature ?? "").phase; } catch { return undefined; } };
  const finals = texts.filter((block) => phase(block) === "final_answer"); const spoken = texts.filter((block) => phase(block) !== "commentary");
  return (finals.length ? finals : spoken.length ? spoken : texts).map((block) => block.text).join("\n");
}

function assistantFailure(message) {
  if (message?.stopReason !== "error" && message?.stopReason !== "aborted") return undefined;
  if (message.stopReason === "aborted") return { reason: "cancelled", message: "This Pi turn was cancelled. Send the message again when ready." };
  const error = message.errorMessage?.toLowerCase() ?? "";
  if (/\bcredits?error\b|\bcredits?\b|\binsufficient\s+balance\b|\bquota\b|\bbilling\b/.test(error)) return { reason: "provider_credits", message: "Provider credits are unavailable. Check the provider account balance or choose another model/provider, then retry." };
  if (/401|403|invalid[\s_-]*(api[\s_-]*)?key|incorrect api key|unauthori[sz]ed|authentication|expired token|invalid[_ ]token/.test(error)) return { reason: "provider_auth", message: "Your model provider rejected the sign-in or API key. Run `nosh login` in a terminal to update it (or press F2 to pick another model), then send your message again." };
  return { reason: "provider_error", message: "Your model provider rejected this turn. Check the account's access to this model, or press F2 to pick another model, then send your message again." };
}

function modelUsage(messages) {
  return messages.reduce((total, message) => {
    if (!message || typeof message !== "object" || message.role !== "assistant") return total;
    const usage = message.usage; if (!usage) return total;
    total.totalTokens += positive(usage.totalTokens); total.inputTokens += positive(usage.input); total.outputTokens += positive(usage.output); total.cacheReadTokens += positive(usage.cacheRead); total.cacheWriteTokens += positive(usage.cacheWrite); return total;
  }, { totalTokens: 0, inputTokens: 0, outputTokens: 0, cacheReadTokens: 0, cacheWriteTokens: 0 });
}

function isRecord(value) { return typeof value === "object" && value !== null && !Array.isArray(value); }
function positive(value) { return typeof value === "number" && Number.isFinite(value) && value > 0 ? value : 0; }

function json(value) {
  try {
    const encoded = JSON.stringify(value);
    return encoded === undefined ? null : JSON.parse(encoded);
  } catch {
    return { unavailable: "non_serializable" };
  }
}
