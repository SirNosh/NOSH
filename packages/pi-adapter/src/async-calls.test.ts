import { ModelRuntime } from "@earendil-works/pi-coding-agent";
import { fauxAssistantMessage, fauxProvider, fauxToolCall, type Api, type Model } from "@earendil-works/pi-ai";
import { createId } from "@nosh/core";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { describe, expect, it } from "vitest";
import { AsyncToolCalls } from "./async-calls.js";
import { PiAdapter, terminalOutputSchema, type TerminalContext, type TerminalHost } from "./index.js";

describe("asynchronous tool calls", () => {
  it("returns a call that settles within the grace period as an ordinary result", async () => {
    const calls = new AsyncToolCalls(200);
    expect(await calls.start("call_1", "nosh_run command_test", Promise.resolve({ exitCode: 0 }), String)).toEqual({ settled: true, value: { exitCode: 0 } });
    expect(calls.idle).toBe(true);
    expect(await calls.next()).toBeUndefined();
  });

  it("answers a slow call with a placeholder, then wakes once with every result that landed together", async () => {
    const calls = new AsyncToolCalls(10); let finishRun!: (value: unknown) => void; let finishRead!: (value: unknown) => void;
    const run = await calls.start("call_run", "nosh_run command_test", new Promise((resolve) => { finishRun = resolve; }), (value) => JSON.stringify(value));
    const read = await calls.start("call_read", "nosh_network_read https://api.crossref.org/x", new Promise((resolve) => { finishRead = resolve; }), (value) => JSON.stringify(value));
    expect(run).toMatchObject({ settled: false, placeholder: expect.stringContaining("nosh_run command_test is still running") });
    expect(read.settled).toBe(false);
    expect(calls.worktreeBusy).toBe(true);
    expect(calls.running).toEqual([expect.stringContaining("nosh_run command_test (call call_run"), expect.stringContaining("nosh_network_read")]);
    const woken = calls.next(60_000);
    finishRun({ exitCode: 0 }); finishRead({ status: 200 });
    const message = await woken;
    expect(message).toContain("Async result of nosh_run command_test (call call_run):\n{\"exitCode\":0}");
    expect(message).toContain("Async result of nosh_network_read");
    expect(calls.worktreeBusy).toBe(false); expect(calls.idle).toBe(true);
  });

  it("steers a result into a live run, requeues one that missed the run, and reports a heartbeat", async () => {
    const calls = new AsyncToolCalls(5); const steered: string[] = []; let live = true; calls.deliver = (text) => { if (!live) return false; steered.push(text); return true; };
    let finish!: (value: unknown) => void; await calls.start("call_a", "nosh_run command_test", new Promise((resolve) => { finish = resolve; }), String);
    expect(await calls.next(20)).toContain("Heartbeat: waited 0 seconds. Still running: nosh_run command_test (call call_a");
    finish("done"); await new Promise((resolve) => setTimeout(resolve, 10));
    expect(steered).toEqual(["Async result of nosh_run command_test (call call_a):\ndone"]);
    live = false; calls.requeue(steered);
    expect(await calls.next()).toBe(steered[0]);
    // A failed call is a result too.
    expect(await calls.start("call_b", "nosh_run command_test", Promise.reject(new Error("boom")), (value) => JSON.stringify(value))).toEqual({ settled: true, value: { accepted: false, error: "boom" } });
  });

  it("runs the real Pi loop: placeholder, edits refused while running, sleep, wake with the result, then the final answer", async () => {
    const cwd = mkdtempSync(join(tmpdir(), "nosh-async-")); writeFileSync(join(cwd, "a.txt"), "x\n");
    const record = { $schema: "https://nosh.dev/schemas/task-failure/v1", schemaVersion: 1 };
    const final = JSON.stringify({ $schema: terminalOutputSchema, schemaVersion: 1, records: [record] });
    const context: TerminalContext = { projectId: createId("prj"), agentId: createId("agt"), taskId: createId("tsk"), instructionId: null, threadId: null, expectedVersion: null, turnId: "async-turn", allowedTools: ["nosh_response_submit"] };
    const seen: string[] = []; let finishRun!: (value: unknown) => void;
    const faux = fauxProvider({ provider: "async-proof", api: "async-proof-api", models: [{ id: "deterministic", reasoning: false }] });
    faux.setResponses([
      fauxAssistantMessage([fauxToolCall("nosh_run", { commandId: "command_test" }, { id: "call_run" })]),
      (ctx) => { seen.push(JSON.stringify(ctx.messages)); return fauxAssistantMessage([fauxToolCall("nosh_workspace_edit", { path: "a.txt", oldText: "x", newText: "y" }, { id: "call_edit" })]); },
      (ctx) => { seen.push(JSON.stringify(ctx.messages)); setTimeout(() => finishRun({ accepted: true, exitCode: 0, jobId: "job_1" }), 50); return fauxAssistantMessage("Waiting for the test run."); },
      (ctx) => { seen.push(JSON.stringify(ctx.messages)); return fauxAssistantMessage(final); },
    ]);
    const runtime = { hasConfiguredAuth: () => true, getAuth: async () => ({ auth: { apiKey: "fake" }, env: {} }), getAvailable: async () => faux.models, getProviders: () => [faux.provider], streamSimple: (model: Model<Api>, ctx: never, options: never) => faux.provider.stream(model, ctx, options) } as unknown as ModelRuntime;
    const accepted: unknown[] = [];
    const host: TerminalHost = { reject: async (_context, reason) => { throw new Error(`unexpected rejection: ${reason}`); }, submit: async (_bound, records) => { accepted.push(records); return { accepted: true, retryAllowed: false, effect: { state: "completed" } }; } };
    const submit = (tool: string) => tool === "nosh_run" ? new Promise((resolve) => { finishRun = resolve; }) : { accepted: true };
    const adapter = new PiAdapter(() => undefined, submit, runtime, host);
    try {
      await adapter.start({ ...context, missionId: null, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, role: "general_worker", cwd, packagePath: resolve(import.meta.dirname, "../../../pi-package"), model: { provider: "async-proof", id: "deterministic" },
        taskPermissions: { network: "disabled", subprocess: "allowlisted", gitCommit: false, gitPush: false, delegation: "request_only", networkAllowlist: [], allowedToolIds: ["tool_pi.read", "tool_pi.edit", "tool_nosh.run", "tool_nosh.response.submit"] },
        taskWorkspace: { worktreeId: "wt_async", branch: "main", startingCommit: "a".repeat(40), writeScopes: ["**"], protectedScopes: [".nosh/**"] } });
      await adapter.prompt(context.agentId, "Run the tests, then report.");
      expect(seen[0]).toContain("nosh_run command_test is still running in the background");
      expect(seen[1]).toContain("A nosh_run is running in this worktree, so edits are refused");
      expect(seen[2]).toContain("Async result of nosh_run command_test (call call_run)");
      expect(seen[2]).toContain("\\\"exitCode\\\":0");
      expect(accepted).toEqual([[record]]);
      expect(adapter.inspect()[0]?.status).toBe("idle");
    } finally { adapter.stop(context.agentId); rmSync(cwd, { recursive: true, force: true }); }
  }, 30_000);
});
