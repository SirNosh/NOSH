import { schemaUri, type JsonValue } from "@nosh/wire";

export const terminalOutputSchema = "https://nosh.dev/schemas/terminal-output/v1";
export const terminalOutputMaxBytes = 131_072;
export const terminalSchemaTools: Readonly<Record<string, string>> = Object.freeze(Object.fromEntries([
  ["general-worker-completion", "nosh_response_submit"], ["librarian-completion", "nosh_response_submit"],
  ["task-failure", "nosh_response_submit"], ["mission-director-cycle", "nosh_response_submit"],
  ["research-director-cycle", "nosh_response_submit"], ["review-verdict", "nosh_review_submit"],
  ["episode-draft", "nosh_episode_submit"], ["experiment-proposal", "nosh_experiment_propose"],
].map(([schema, tool]) => [schemaUri(schema!), tool!])));
export const terminalToolNames = new Set(Object.values(terminalSchemaTools));
export type TerminalOutput = { $schema: typeof terminalOutputSchema; schemaVersion: 1; records: Record<string, JsonValue>[] };
export type TerminalContext = {
  projectId: string; agentId: string; taskId: string | null; instructionId: string | null;
  threadId: string | null; expectedVersion: number | null; turnId: string; allowedTools: string[];
  expectedEpisodeType?: string;
};
export type TerminalReceipt = { accepted: boolean; retryAllowed: boolean; effect?: unknown; [key: string]: unknown };
export type TerminalHost = {
  admit?(context: TerminalContext): Promise<TerminalReceipt | undefined>;
  submit(context: TerminalContext, records: Record<string, JsonValue>[]): Promise<TerminalReceipt>;
  reject(context: TerminalContext, reason: string): Promise<TerminalReceipt>;
};
export type TerminalStep = Pick<TerminalContext, "instructionId" | "threadId" | "expectedVersion"> & { expectedEpisodeType: string };

/** Reject duplicate keys rather than accepting JSON.parse's last-key-wins ambiguity. */
function assertUniqueKeys(text: string): void {
  const tokens = text.match(/"(?:[^"\\]|\\.)*"|[{}\[\]:,]|[^\s{}\[\]:,]+/g) ?? [];
  let index = 0;
  const value = (): void => {
    const token = tokens[index++];
    if (token === "{") {
      const keys = new Set<string>();
      while (tokens[index] !== "}") {
        const key = JSON.parse(tokens[index++]! ) as string;
        if (keys.has(key)) throw new Error("Duplicate JSON object key"); keys.add(key);
        if (tokens[index++] !== ":") throw new Error("Invalid JSON object");
        value(); if (tokens[index] !== ",") break; index++;
      }
      if (tokens[index++] !== "}") throw new Error("Invalid JSON object");
    } else if (token === "[") {
      while (tokens[index] !== "]") { value(); if (tokens[index] !== ",") break; index++; }
      if (tokens[index++] !== "]") throw new Error("Invalid JSON array");
    }
  };
  value(); if (index !== tokens.length) throw new Error("Multiple JSON values are not allowed");
}
/**
 * Models hand-copying long envelopes often append stray closing brackets after a complete object.
 * Accept the first complete top-level object only when everything after it is `]`, `}`, or
 * whitespace; any other trailing content (prose, a second object) still fails JSON parsing.
 */
function withoutStrayClosers(text: string): string {
  const start = text.indexOf("{"); let depth = 0; let inString = false; let escaped = false;
  for (let index = start; index < text.length; index += 1) {
    const character = text[index]!;
    if (inString) { if (escaped) escaped = false; else if (character === "\\") escaped = true; else if (character === "\"") inString = false; continue; }
    if (character === "\"") inString = true;
    else if (character === "{" || character === "[") depth += 1;
    else if (character === "}" || character === "]") { depth -= 1; if (depth === 0) { const rest = text.slice(index + 1); return /^[\s\]}]*$/.test(rest) ? text.slice(0, index + 1) : text; } }
  }
  return text;
}
export function parseTerminalOutput(text: string, context: TerminalContext): TerminalOutput {
  if (Buffer.byteLength(text, "utf8") > terminalOutputMaxBytes) throw new Error("Terminal output exceeds 131072 UTF-8 bytes");
  if (!text.trim().startsWith("{")) throw new Error("Return one unfenced JSON object, without prose");
  const envelope = withoutStrayClosers(text);
  const parsed: unknown = JSON.parse(envelope); assertUniqueKeys(envelope);
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) throw new Error("Terminal envelope must be an object");
  const object = parsed as Record<string, unknown>;
  if (Object.keys(object).sort().join(",") !== "$schema,records,schemaVersion" || object.$schema !== terminalOutputSchema || object.schemaVersion !== 1 || !Array.isArray(object.records) || object.records.length < 1 || object.records.length > 2) throw new Error("Invalid terminal envelope schema or record count (1..2)");
  const seen = new Set<string>();
  for (const record of object.records) {
    if (!record || typeof record !== "object" || Array.isArray(record)) throw new Error("Terminal record must be an object");
    const tool = terminalSchemaTools[String(record.$schema)];
    if (!tool || !context.allowedTools.includes(tool)) throw new Error("Terminal record schema is not permitted for this turn");
    if (seen.has(tool) || tool !== "nosh_episode_submit" && [...seen].some((name) => name !== "nosh_episode_submit")) throw new Error("Only one terminal outcome and one episode draft are allowed");
    seen.add(tool);
    if (tool === "nosh_episode_submit" && context.expectedEpisodeType !== record.episodeType) throw new Error("Episode type does not match the host instruction");
  }
  if (context.taskId && ![...seen].some((tool) => tool !== "nosh_episode_submit")) throw new Error("Task turn requires one terminal outcome");
  if (context.expectedEpisodeType && !seen.has("nosh_episode_submit")) throw new Error("Runtime turn requires one episode draft");
  return parsed as TerminalOutput;
}
export function terminalPrompt(context: TerminalContext): string {
  const schemas = Object.entries(terminalSchemaTools).filter(([, tool]) => context.allowedTools.includes(tool)).map(([schema]) => schema);
  return `TERMINAL OUTPUT CONTRACT (overrides older completion-tool instructions): Keep using execution tools and immediate acknowledgement/progress/effect commands. Do not call response, review, or episode submission tools. Your last assistant message must be exactly one unfenced JSON object, no prose: {"$schema":"${terminalOutputSchema}","schemaVersion":1,"records":[COMPLETE_TYPED_RECORDS]}. Maximum ${terminalOutputMaxBytes} UTF-8 bytes. Allowed schemas: ${schemas.join(", ")}. At most one terminal outcome plus one episode draft.${context.taskId ? " Include exactly one task terminal outcome." : ""}${context.expectedEpisodeType ? ` Include exactly one episode-draft of type ${context.expectedEpisodeType}.` : ""} The host binds all routing and validates records; output is a proposal, not proof of acceptance or successful effects.`;
}
/** Successful final provider stop only. Tool-loop intermediates and deltas are never proposals. */
/** A bare character offset is useless to a model; show where the JSON broke so the correction can fix it. */
export function withJsonExcerpt(error: Error, text: string): string {
  const position = error instanceof SyntaxError ? /position (\d+)/.exec(error.message)?.[1] : undefined;
  if (position === undefined) return error.message;
  const at = Number(position);
  return `${error.message}. JSON near the error: …${text.slice(Math.max(0, at - 80), at)}⟪HERE⟫${text.slice(at, at + 40)}…`;
}
export async function runTerminalTurn(context: TerminalContext, host: TerminalHost, prompt: (correction?: string) => Promise<{ text: string; stopReason: string; cancelled?: boolean }>): Promise<TerminalReceipt> {
  let lastReceipt: TerminalReceipt | undefined;
  for (let attempt = 0; attempt < 2; attempt++) {
    const prior = await host.admit?.(context);
    if (prior) { if (prior.accepted) return prior; throw new Error(`Terminal turn closed: ${JSON.stringify(prior).slice(0, 4000)}`); }
    let result: { text: string; stopReason: string; cancelled?: boolean };
    try { result = await prompt(attempt ? "The host rejected the final JSON. Correct the envelope/record validation errors below. Do not repeat execution tools. " + JSON.stringify(lastReceipt).slice(0, 4000) : undefined); }
    catch (error) { await host.reject(context, "Non-successful provider stop: prompt failed"); throw error; }
    if (result.cancelled || result.stopReason !== "stop") { await host.reject(context, `Non-successful provider stop: ${result.cancelled ? "cancelled" : result.stopReason}`); throw new Error("Terminal output discarded: cancelled, failed, or truncated provider turn"); }
    let receipt: TerminalReceipt;
    try { const envelope = parseTerminalOutput(result.text, context); receipt = await host.submit(context, envelope.records); }
    catch (error) { receipt = await host.reject(context, error instanceof Error ? withJsonExcerpt(error, result.text) : "Invalid terminal output"); }
    if (receipt.accepted) return receipt;
    if (!receipt.retryAllowed || attempt === 1) throw new Error(`Terminal output rejected: ${JSON.stringify(receipt).slice(0, 4000)}`);
    lastReceipt = receipt;
  }
  throw new Error("Terminal correction budget exhausted");
}
