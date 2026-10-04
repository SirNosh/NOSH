import { ModelRuntime } from "@earendil-works/pi-coding-agent";
import { fauxProvider, fauxAssistantMessage, type Api, type Model } from "@earendil-works/pi-ai";
import { createId } from "@nosh/core";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { describe, expect, it } from "vitest";
import { PiAdapter, parseTerminalOutput, runTerminalTurn, terminalOutputSchema, type TerminalContext, type TerminalHost } from "./index.js";

const record = { $schema: "https://nosh.dev/schemas/task-failure/v1", schemaVersion: 1 };
const text = JSON.stringify({ $schema: terminalOutputSchema, schemaVersion: 1, records: [record] });
const context: TerminalContext = { projectId: createId("prj"), agentId: createId("agt"), taskId: createId("tsk"), instructionId: null, threadId: null, expectedVersion: null, turnId: "test-turn", allowedTools: ["nosh_response_submit"] };
describe("terminal JSON transport", () => {
  it("rejects fences, extra objects, duplicate keys, oversized bytes, and disallowed schemas", () => {
    for (const invalid of ["```json\n" + text + "\n```", text + text, text.replace('"schemaVersion":1', '"schemaVersion":1,"schemaVersion":1'), text.replace("task-failure", "runtime-instruction"), " ".repeat(131072) + text]) expect(() => parseTerminalOutput(invalid, context)).toThrow();
    expect(parseTerminalOutput(text, context).records).toEqual([record]);
  });
  it("accepts a complete envelope followed only by stray closing brackets, nothing else", () => {
    for (const trailing of ["]}", "}]}\n", " ] } "]) expect(parseTerminalOutput(text + trailing, context).records).toEqual([record]);
    for (const trailing of [" done", "]} ok", text, "{}", "[1]"]) expect(() => parseTerminalOutput(text + trailing, context)).toThrow();
  });
  it("never submits apparent JSON from cancelled/error/truncated turns", async () => {
    for (const stopReason of ["error", "aborted", "length", "toolUse"]) {
      let submitted = 0; let prompted = 0;
      const host: TerminalHost = { submit: async () => { submitted++; return { accepted: true, retryAllowed: false }; }, reject: async () => ({ accepted: false, retryAllowed: false }) };
      await expect(runTerminalTurn(context, host, async () => { prompted++; return { text, stopReason }; })).rejects.toThrow("discarded");
      expect(submitted).toBe(0); expect(prompted).toBe(1);
    }
  });
  it("uses at most one correction even if a host keeps offering retries", async () => {
    let prompted = 0;
    const host: TerminalHost = { submit: async () => ({ accepted: false, retryAllowed: true }), reject: async () => ({ accepted: false, retryAllowed: true }) };
    await expect(runTerminalTurn(context, host, async () => { prompted++; return { text: "bad", stopReason: "stop" }; })).rejects.toThrow("rejected");
    expect(prompted).toBe(2);
  });
  it("runs real Pi inner loop and corrects final JSON before active-session cleanup", async () => {
    const cwd = mkdtempSync(join(tmpdir(), "nosh-terminal-proof-"));
    const faux = fauxProvider({ provider: "terminal-proof", api: "terminal-proof-api", models: [{ id: "deterministic", reasoning: false }] });
    faux.setResponses([fauxAssistantMessage("```json\n" + text + "\n```"), fauxAssistantMessage(text)]);
    const runtime = { hasConfiguredAuth: () => true, getAuth: async () => ({ auth: { apiKey: "fake" }, env: {} }), getAvailable: async () => faux.models, getProviders: () => [faux.provider], streamSimple: (model: Model<Api>, ctx: never, options: never) => faux.provider.stream(model, ctx, options) } as unknown as ModelRuntime;
    const trace: string[] = []; let adapter: PiAdapter;
    const host: TerminalHost = {
      reject: async (_context, reason) => { trace.push("rejected:" + reason); return { accepted: false, retryAllowed: true }; },
      submit: async (bound, records) => { expect(bound.taskId).toBe(context.taskId); expect(records).toEqual([record]); expect(adapter.inspect()[0]?.status).toBe("running"); trace.push("accepted-while-active"); return { accepted: true, retryAllowed: false, effect: { state: "completed" } }; },
    };
    adapter = new PiAdapter((event) => { if (event.type === "agent.terminal_receipt") trace.push("receipt"); }, async () => ({ accepted: true }), runtime, host);
    try {
      await adapter.start({ ...context, missionId: null, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, role: "general_worker", cwd, packagePath: resolve(import.meta.dirname, "../../../pi-package"), model: { provider: "terminal-proof", id: "deterministic" } });
      expect(adapter.inspect()[0]?.activeToolIds).not.toContain("nosh_response_submit");
      await adapter.prompt(context.agentId, "Return a bounded task failure record.");
      expect(trace).toEqual([expect.stringContaining("rejected:"), "accepted-while-active", "receipt"]);
      expect(adapter.inspect()[0]?.status).toBe("idle");
      console.log("TERMINAL_FAUX_PROOF", JSON.stringify(trace));
    } finally { adapter.stop(context.agentId); rmSync(cwd, { recursive: true, force: true }); }
  });
});

describe("terminal JSON correction feedback", () => {
  it("shows the excerpt where JSON broke instead of a bare offset", async () => {
    const { withJsonExcerpt } = await import("./terminal-output.js");
    const broken = '{"a":["x","y"]},"b":1}';
    let error: Error | undefined; try { JSON.parse(broken); } catch (caught) { error = caught as Error; }
    const message = withJsonExcerpt(error!, broken);
    expect(message).toContain("⟪HERE⟫");
    expect(message).toContain('"y"]}');
    expect(withJsonExcerpt(new Error("Episode type does not match"), broken)).toBe("Episode type does not match");
  });
});
