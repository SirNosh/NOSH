import { createId } from "@nosh/core";
import { ModelRuntime } from "@earendil-works/pi-coding-agent";
import { fauxProvider, fauxAssistantMessage } from "@earendil-works/pi-ai";
import type { Api, Model } from "@earendil-works/pi-ai";
import type { EventDraft } from "@nosh/wire";
import { linkSync, mkdirSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { describe, expect, it } from "vitest";
import { PiAdapter, commitWorkspaceFiles, createSessionTools, mapPiEvent, noshPromptCacheKey, noshSystemPrompt, projectAvailableModel, readAllowedUrl, resolveWorkspacePath, sessionToolIds } from "./index.js";

const scope = { projectId: createId("prj"), missionId: null, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, taskId: createId("tsk"), agentId: createId("agt") };

const reasoningModel = {
  provider: "provider-a", id: "reasoning-model", name: "Reasoning Model", api: "openai-responses", baseUrl: "https://provider.invalid/v1", reasoning: true,
  thinkingLevelMap: { xhigh: "xhigh", max: "max" }, input: ["text"], cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }, contextWindow: 10_000, maxTokens: 1_000,
} as Model<Api>;
const plainModel = { ...reasoningModel, id: "plain-model", name: "Plain Model", reasoning: false } as Model<Api>;

function testRuntime(): ModelRuntime {
  return {
    hasConfiguredAuth: () => true, getAuth: async () => ({ auth: { apiKey: "fake" }, env: {} }), getAvailable: async () => [reasoningModel, plainModel],
    getProviders: () => [{ id: "provider-a", name: "Provider A" }],
  } as unknown as ModelRuntime;
}

describe("Pi event mapping", () => {
  it("streams text before completion as ephemeral events", () => {
    const mapped = mapPiEvent(scope, { type: "message_update", message: {} as never, assistantMessageEvent: { type: "text_delta", delta: "hello", contentIndex: 0, partial: {} as never } });
    expect(mapped).toMatchObject({ retention: "ephemeral", type: "agent.text_delta", payload: { delta: "hello" } });
  });

  it("records tool lifecycle with correlation", () => {
    const mapped = mapPiEvent(scope, { type: "tool_execution_start", toolCallId: "call_1", toolName: "read", args: { path: "README.md" } });
    expect(mapped).toMatchObject({ retention: "persistent", type: "agent.tool_started", correlationId: scope.taskId });
  });

  it("persists exact Pi token usage without assistant reasoning", () => { const mapped = mapPiEvent(scope, { type: "agent_end", willRetry: false, messages: [{ role: "assistant", stopReason: "stop", content: [{ type: "thinking", thinking: "hidden" }, { type: "text", text: "result" }], usage: { totalTokens: 42 } }] as never }); expect(mapped).toMatchObject({ type: "agent.completed", payload: { message: "result", modelTokens: 42 } }); expect(JSON.stringify(mapped)).not.toContain("hidden"); });

  it("records retryable provider failures as activity before the successful result", () => { const retry = mapPiEvent(scope, { type: "agent_end", willRetry: true, messages: [{ role: "assistant", stopReason: "error", errorMessage: "Temporary provider rejection", content: [{ type: "thinking", thinking: "hidden" }], usage: { totalTokens: 12 } }] as never }); const success = mapPiEvent(scope, { type: "agent_end", willRetry: false, messages: [{ role: "assistant", stopReason: "stop", content: [{ type: "text", text: "recovered result" }], usage: { totalTokens: 30 } }] as never }, 12); expect(retry).toMatchObject({ type: "agent.retrying", payload: { reason: "provider_error", willRetry: true, modelTokens: 12 } }); expect(retry).not.toMatchObject({ type: "agent.failed" }); expect(success).toMatchObject({ type: "agent.completed", payload: { message: "recovered result", modelTokens: 18 } }); expect(JSON.stringify(retry)).not.toContain("Temporary provider rejection"); expect(JSON.stringify(retry)).not.toContain("hidden"); });

  it("maps provider credit failures to safe actionable failures with usage", () => { const mapped = mapPiEvent(scope, { type: "agent_end", willRetry: false, messages: [{ role: "assistant", stopReason: "error", errorMessage: "CreditsError: Insufficient balance", content: [{ type: "thinking", thinking: "hidden" }], usage: { totalTokens: 42, input: 20, output: 10, cacheRead: 8, cacheWrite: 4 } }] as never }); expect(mapped).toMatchObject({ type: "agent.failed", payload: { reason: "provider_credits", message: "Provider credits are unavailable. Check the provider account balance or choose another model/provider, then retry.", modelTokens: 35, providerTotalTokens: 42, inputTokens: 20, outputTokens: 10, cacheReadTokens: 8, cacheWriteTokens: 4, willRetry: false } }); expect(JSON.stringify(mapped)).not.toContain("CreditsError"); expect(JSON.stringify(mapped)).not.toContain("hidden"); });

  it("maps aborted Pi turns to distinguishable cancellation failures", () => { const mapped = mapPiEvent(scope, { type: "agent_end", willRetry: false, messages: [{ role: "assistant", stopReason: "aborted", errorMessage: "Request aborted by user", content: [], usage: { totalTokens: 7 } }] as never }); expect(mapped).toMatchObject({ type: "agent.failed", payload: { reason: "cancelled", message: "This Pi turn was cancelled. Send the message again when ready.", modelTokens: 7, willRetry: false } }); expect(JSON.stringify(mapped)).not.toContain("Request aborted by user"); });

  it("attributes only the current bounded turn after session reuse or compaction", () => { const event = { type: "agent_end", willRetry: false, messages: [{ role: "assistant", content: [{ type: "text", text: "result" }], usage: { totalTokens: 42 } }] }; expect(mapPiEvent(scope, event as never, 30)).toMatchObject({ payload: { modelTokens: 12 } }); expect(mapPiEvent(scope, { ...event, messages: [{ role: "assistant", content: [], usage: { totalTokens: 10 } }] } as never, 42)).toMatchObject({ payload: { modelTokens: 10 } }); });

  it("records cache reads and writes for cost diagnostics", () => { const event = { type: "agent_end", willRetry: false, messages: [{ role: "assistant", content: [], usage: { totalTokens: 100, input: 20, output: 10, cacheRead: 60, cacheWrite: 10 } }] }; expect(mapPiEvent(scope, event as never)).toMatchObject({ payload: { modelTokens: 46, providerTotalTokens: 100, inputTokens: 20, outputTokens: 10, cacheReadTokens: 60, cacheWriteTokens: 10 } }); });

  it("uses stable cache affinity without volatile task or agent IDs", () => {
    const options = { ...scope, role: "general_worker" as const, tools: ["read", "bash"] };
    const rotated = { ...options, taskId: createId("tsk"), agentId: createId("agt"), tools: ["bash", "read"] };
    expect(noshPromptCacheKey(options)).toBe(noshPromptCacheKey(rotated));
    expect(noshPromptCacheKey(options)).not.toBe(noshPromptCacheKey({ ...options, role: "reviewer" }));
    expect(noshSystemPrompt("general_worker")).not.toContain(scope.taskId);
  });

  it("binds typed submissions to daemon-issued session scope with a compact receipt", async () => { const calls: unknown[][] = []; const tools = createSessionTools({ ...scope, role: "general_worker", cwd: ".", packagePath: "." }, (...values) => { calls.push(values); return { accepted: true, veryLargeProjection: "x".repeat(10_000) }; }); const tool = tools.find((candidate) => candidate.name === "nosh_task_acknowledge")!; const result = await tool.execute("call_1", { record: { answer: 1 } }, undefined, undefined, {} as never); expect(calls).toEqual([["nosh_task_acknowledge", scope.projectId, `task:${scope.taskId}`, { answer: 1 }, scope.agentId]]); expect(JSON.stringify(result.content).length).toBeLessThan(4200); });

  it("activates only Task Packet-authorized tools in a real Pi session", async () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-pi-adapter-"));
    const adapter = new PiAdapter(() => undefined, () => ({ accepted: true }), testRuntime());
    const taskPermissions = { network: "disabled" as const, subprocess: "disabled" as const, gitCommit: true, gitPush: false, delegation: "request_only" as const, networkAllowlist: [], allowedToolIds: ["tool_pi.read", "tool_nosh.git.commit", "tool_nosh.response.submit"] };
    const taskWorkspace = { worktreeId: "wt_adapter", branch: "main", startingCommit: "a".repeat(40), writeScopes: ["src/**"], protectedScopes: [".nosh/**"] };
    const options = { ...scope, role: "general_worker" as const, cwd: directory, packagePath: resolve(import.meta.dirname, "..", "..", "..", "pi-package"), model: { provider: "provider-a", id: "reasoning-model" }, taskPermissions, taskWorkspace };
    try {
      const started = await adapter.start(options);
      expect(started.activeToolIds).toEqual(expect.arrayContaining(["nosh_workspace_read", "nosh_git_commit"]));
      expect(started.activeToolIds).not.toEqual(expect.arrayContaining(["bash", "read", "edit", "write", "nosh_subprocess"]));
      expect(sessionToolIds({ taskId: options.taskId, taskPermissions, taskWorkspace })).toEqual(["nosh_artifact_read", "nosh_artifact_register", "nosh_git_commit", "nosh_response_submit", "nosh_workspace_read"]);
      await expect(adapter.start({ ...options, agentId: createId("agt"), taskPermissions: { ...taskPermissions, subprocess: "allowlisted" } })).rejects.toThrow("subprocess authority");
      await expect(adapter.start({ ...options, agentId: createId("agt"), taskPermissions: { ...taskPermissions, gitCommit: false } })).rejects.toThrow("gitCommit authority");
      expect(() => resolveWorkspacePath(options, "src/allowed.ts", true)).not.toThrow();
      expect(() => resolveWorkspacePath(options, ".nosh/protected.ts", true)).toThrow("protected");
      expect(() => resolveWorkspacePath(options, ".git/HEAD", true)).toThrow("Git metadata");
      if (process.platform === "win32") expect(() => resolveWorkspacePath(options, ".GIT/HEAD", true)).toThrow("Git metadata");
      if (process.platform === "win32") expect(() => resolveWorkspacePath(options, ".NOSH/protected.ts", true)).toThrow("protected");
    } finally {
      adapter.stop(options.agentId);
      rmSync(directory, { recursive: true, force: true });
    }
  });
  it("uses the NOSH default model and thinking level when a session selects none", async () => {
    const adapter = new PiAdapter(() => undefined, () => ({ accepted: true }), testRuntime());
    adapter.defaultModel = { provider: "provider-a", id: "reasoning-model", thinkingLevel: "low" };
    const agentId = createId("agt");
    try {
      const started = await adapter.start({ ...scope, agentId, role: "general_worker", cwd: ".", packagePath: resolve(import.meta.dirname, "..", "..", "..", "pi-package") });
      expect(started).toMatchObject({ modelProvider: "provider-a", modelId: "reasoning-model", thinkingLevel: "low" });
    } finally { adapter.stop(agentId); }
  });
  it("offers nosh_run only for allowlisted subprocess authority and delegates execution to the daemon", async () => {
    const taskWorkspace = { worktreeId: "wt_run", branch: "main", startingCommit: "a".repeat(40), writeScopes: ["src/**"], protectedScopes: [".nosh/**"] };
    const taskPermissions = { network: "disabled" as const, subprocess: "allowlisted" as const, gitCommit: false, gitPush: false, delegation: "request_only" as const, networkAllowlist: [], allowedToolIds: ["tool_pi.read", "tool_nosh.run"] };
    expect(sessionToolIds({ taskId: scope.taskId, taskPermissions, taskWorkspace })).toEqual(["nosh_artifact_read", "nosh_artifact_register", "nosh_run", "nosh_workspace_read"]);
    expect(sessionToolIds({ taskId: scope.taskId, taskPermissions: { ...taskPermissions, subprocess: "disabled" }, taskWorkspace })).toEqual(["nosh_artifact_read", "nosh_artifact_register", "nosh_workspace_read"]);
    const calls: unknown[][] = [];
    const run = createSessionTools({ ...scope, role: "general_worker", cwd: ".", packagePath: ".", taskPermissions, taskWorkspace }, (...values) => { calls.push(values); return { accepted: true, exitCode: 0 }; }).find((tool) => tool.name === "nosh_run")!;
    await run.execute("call_run", { commandId: "command_test" }, undefined, undefined, {} as never);
    expect(calls).toEqual([["nosh_run", scope.projectId, `task:${scope.taskId}`, { commandId: "command_test" }, scope.agentId]]);
    await expect(new PiAdapter(() => undefined, () => ({ accepted: true }), testRuntime()).start({ ...scope, role: "general_worker", cwd: ".", packagePath: ".", taskPermissions: { ...taskPermissions, subprocess: "enabled" }, taskWorkspace })).rejects.toThrow("subprocess authority=enabled");
  });
  it("rejects generic subprocess authority and linked workspace ancestry", async () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-pi-boundary-")); const root = join(directory, "root"); const linked = join(directory, "linked"); mkdirSync(root);
    const taskPermissions = { network: "disabled" as const, subprocess: "allowlisted" as const, gitCommit: false, gitPush: false, delegation: "request_only" as const, networkAllowlist: [], allowedToolIds: ["tool_nosh.subprocess"] };
    const options = { ...scope, role: "general_worker" as const, cwd: root, packagePath: ".", taskPermissions, taskWorkspace: { worktreeId: "wt_boundary", branch: "main", startingCommit: "a".repeat(40), writeScopes: ["src/**"], protectedScopes: [".nosh/**"] } };
    try {
      await expect(new PiAdapter(() => undefined, () => ({ accepted: true }), testRuntime()).start(options)).rejects.toThrow("subprocess authority");
      expect(sessionToolIds({ taskId: options.taskId, taskPermissions, taskWorkspace: options.taskWorkspace })).toEqual([]);
      symlinkSync(root, linked, process.platform === "win32" ? "junction" : "dir");
      expect(() => resolveWorkspacePath({ ...options, cwd: linked }, "src/file.ts", true)).toThrow("root");
    } finally { rmSync(directory, { recursive: true, force: true }); }
  });
  it("edits CRLF files with LF text from the model and keeps CRLF endings", async () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-pi-crlf-")); const file = join(directory, "config.json");
    const taskPermissions = { network: "disabled" as const, subprocess: "disabled" as const, gitCommit: false, gitPush: false, delegation: "request_only" as const, networkAllowlist: [], allowedToolIds: ["tool_pi.read", "tool_pi.edit"] };
    const options = { ...scope, role: "general_worker" as const, cwd: directory, packagePath: ".", taskPermissions, taskWorkspace: { worktreeId: "wt_crlf", branch: "main", startingCommit: "a".repeat(40), writeScopes: ["**"], protectedScopes: [".nosh/**"] } };
    try {
      writeFileSync(file, "{\r\n  \"threshold\": 0.5\r\n}\r\n", "utf8");
      const edit = createSessionTools(options, () => ({ accepted: true })).find((tool) => tool.name === "nosh_workspace_edit")!;
      await edit.execute("edit_crlf", { path: "config.json", oldText: "  \"threshold\": 0.5\n}", newText: "  \"threshold\": 0.6\n}" }, undefined, undefined, {} as never);
      expect(readFileSync(file, "utf8")).toBe("{\r\n  \"threshold\": 0.6\r\n}\r\n");
    } finally { rmSync(directory, { recursive: true, force: true }); }
  });
  it("rejects hardlinked files before workspace reads or edits", async () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-pi-hardlink-")); const outside = join(directory, "outside.txt"); const linked = join(directory, "linked.txt");
    const taskPermissions = { network: "disabled" as const, subprocess: "disabled" as const, gitCommit: false, gitPush: false, delegation: "request_only" as const, networkAllowlist: [], allowedToolIds: ["tool_pi.read", "tool_pi.edit"] };
    const options = { ...scope, role: "general_worker" as const, cwd: directory, packagePath: ".", taskPermissions, taskWorkspace: { worktreeId: "wt_hardlink", branch: "main", startingCommit: "a".repeat(40), writeScopes: ["**"], protectedScopes: [".nosh/**"] } };
    try {
      writeFileSync(outside, "original", "utf8"); linkSync(outside, linked);
      const tools = createSessionTools(options, () => ({ accepted: true }));
      const read = tools.find((tool) => tool.name === "nosh_workspace_read")!; const edit = tools.find((tool) => tool.name === "nosh_workspace_edit")!;
      await expect(read.execute("read_hardlink", { path: "linked.txt" }, undefined, undefined, {} as never)).rejects.toThrow("unlinked regular files");
      await expect(edit.execute("edit_hardlink", { path: "linked.txt", oldText: "original", newText: "changed" }, undefined, undefined, {} as never)).rejects.toThrow("unlinked regular files");
    } finally { rmSync(directory, { recursive: true, force: true }); }
  });
  it("exposes the bounded network reader only with an explicit allowlist and rejects unsafe targets", async () => {
    const taskPermissions = { network: "allowlisted" as const, subprocess: "disabled" as const, gitCommit: false, gitPush: false, delegation: "request_only" as const, networkAllowlist: ["api.crossref.org"], allowedToolIds: ["tool_nosh.network.read"] };
    expect(sessionToolIds({ taskId: scope.taskId, taskPermissions })).toEqual(["nosh_network_read"]);
    expect(sessionToolIds({ taskId: scope.taskId, taskPermissions: { ...taskPermissions, networkAllowlist: [] } })).toEqual([]);
    await expect(readAllowedUrl("http://api.crossref.org/works", taskPermissions.networkAllowlist)).rejects.toThrow("HTTPS");
    await expect(readAllowedUrl("https://example.invalid/works", taskPermissions.networkAllowlist)).rejects.toThrow("not allowlisted");
    await expect(readAllowedUrl("https://api.crossref.org:444/works", taskPermissions.networkAllowlist)).rejects.toThrow("Custom ports");
  });
  it("builds commits in an isolated index and preserves the real index on rejection", () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-pi-commit-"));
    const git = (args: string[]): string => { const result = spawnSync("git", ["-C", directory, ...args], { encoding: "utf8", windowsHide: true }); if (result.status !== 0) throw new Error(String(result.stderr)); return String(result.stdout).trim(); };
    try {
      git(["init"]);
      mkdirSync(join(directory, "src")); writeFileSync(join(directory, "src", "a.txt"), "a\n"); writeFileSync(join(directory, "src", "b.txt"), "b\n");
      git(["add", "src/a.txt", "src/b.txt"]); git(["-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "-m", "initial"]);
      const head = git(["rev-parse", "HEAD"]); const branch = git(["branch", "--show-current"]);
      const options = { ...scope, role: "general_worker" as const, cwd: directory, packagePath: ".", taskPermissions: { network: "disabled" as const, subprocess: "disabled" as const, gitCommit: true, gitPush: false, delegation: "request_only" as const, networkAllowlist: [], allowedToolIds: ["tool_nosh.git.commit"] }, taskWorkspace: { worktreeId: "wt_commit", branch, startingCommit: head, writeScopes: ["src/**"], protectedScopes: [".nosh/**"] } };
      writeFileSync(join(directory, "src", "a.txt"), "changed\n"); writeFileSync(join(directory, "src", "b.txt"), "also changed\n");
      git(["add", "src/a.txt"]);
      expect(() => commitWorkspaceFiles(options, { heads: new Set<string>() }, "change a", ["src/a.txt"])).toThrow("pre-staged");
      expect(git(["diff", "--cached", "--name-only"])).toBe("src/a.txt");
      git(["reset", "--", "src/a.txt"]);
      expect(() => commitWorkspaceFiles(options, { heads: new Set<string>() }, "change a", ["src/a.txt"])).toThrow("outside the explicit");
      expect(readFileSync(join(directory, "src", "b.txt"), "utf8")).toBe("also changed\n");
      writeFileSync(join(directory, "src", "b.txt"), "b\n");
      const result = commitWorkspaceFiles(options, { heads: new Set<string>() }, "change a", ["src/a.txt"]);
      expect(result.paths).toEqual(["src/a.txt"]);
      expect(git(["rev-parse", "HEAD"])).toBe(result.commit);
      // The repository has no identity configured; the agent authors its own commits.
      expect(git(["log", "-1", "--format=%an <%ae>"])).toBe(`NOSH general_worker <${scope.agentId}@agents.nosh.invalid>`);
      expect(git(["diff", "--cached", "--name-only"])).toBe("");
      expect(git(["diff", "--name-only"])).toBe("");
    } finally { rmSync(directory, { recursive: true, force: true }); }
  });
});

describe("Pi model selection", () => {
  it("projects provider identity and model-specific thinking levels", () => {
    expect(projectAvailableModel(reasoningModel, "Provider A")).toMatchObject({ provider: "provider-a", providerName: "Provider A", id: "reasoning-model", name: "Reasoning Model", reasoning: true, thinkingLevels: ["off", "minimal", "low", "medium", "high", "xhigh", "max"] });
    expect(projectAvailableModel(plainModel, "Provider A")).toMatchObject({ reasoning: false, thinkingLevels: ["off"] });
  });

  it("rejects invalid and model-unsupported thinking levels without provider access", async () => {
    const adapter = new PiAdapter(() => undefined, undefined, testRuntime());
    await expect(adapter.validateModelSelection({ provider: "provider-a", id: "reasoning-model", thinkingLevel: "invalid" as never })).rejects.toThrow("Pi model provider, id, and thinking level are invalid");
    await expect(adapter.validateModelSelection({ provider: "provider-a", id: "plain-model", thinkingLevel: "high" })).rejects.toThrow("supported levels: off");
  });

  it("propagates the selected level into a new session and changes it in place when idle", async () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-pi-adapter-"));
    const adapter = new PiAdapter(() => undefined, undefined, testRuntime());
    const options = { ...scope, role: "nosh" as const, cwd: directory, packagePath: resolve(import.meta.dirname, "..", "..", "..", "pi-package"), model: { provider: "provider-a", id: "reasoning-model" }, thinkingLevel: "max" as const };
    try {
      const started = await adapter.start(options);
      const changed = adapter.setThinkingLevel(options.agentId, "medium");
      expect(started).toMatchObject({ modelProvider: "provider-a", modelId: "reasoning-model", thinkingLevel: "max" });
      expect(changed).toMatchObject({ piSessionId: started.piSessionId, thinkingLevel: "medium" });
      expect(adapter.inspect()[0]).toMatchObject({ piSessionId: started.piSessionId, thinkingLevel: "medium" });
    } finally {
      adapter.stop(options.agentId);
      rmSync(directory, { recursive: true, force: true });
    }
  });
});


describe("Pi provider-backed lifecycle", () => {
  it("starts a real AgentSession, streams a turn, reuses scope, and returns idle", async () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-pi-provider-"));
    const faux = fauxProvider({ provider: "test-provider", api: "test-api", models: [{ id: "deterministic", name: "Deterministic", reasoning: false }] });
    faux.setResponses([fauxAssistantMessage("deterministic result")]);
    const model = faux.models[0];
    const runtime = { hasConfiguredAuth: () => true, getAuth: async () => ({ auth: { apiKey: "fake" }, env: {} }), getAvailable: async () => [model], getProviders: () => [faux.provider], streamSimple: (selected: Model<Api>, context: never, options: never) => faux.provider.stream(selected, context, options) } as unknown as ModelRuntime;
    const events: EventDraft[] = [];
    const adapter = new PiAdapter((event) => events.push(event), undefined, runtime);
    const options = { ...scope, taskId: null, role: "general_worker" as const, cwd: directory, packagePath: resolve(import.meta.dirname, "..", "..", "..", "pi-package"), model: { provider: "test-provider", id: "deterministic" } };
    try {
      const started = await adapter.start(options);
      expect(await adapter.start(options)).toMatchObject({ piSessionId: started.piSessionId, status: "idle" });
      await adapter.prompt(options.agentId, "Say one deterministic sentence.");
      expect(events.map((event) => event.type)).toEqual(expect.arrayContaining(["agent.started", "agent.text_delta", "agent.completed"]));
      expect(events.some((event) => event.type === "agent.completed" && (event.payload as { message?: string }).message === "deterministic result")).toBe(true);
      expect(adapter.inspect()[0]).toMatchObject({ agentId: options.agentId, status: "idle" });
    } finally { adapter.stop(options.agentId); rmSync(directory, { recursive: true, force: true }); }
  });

  it("maps faux provider rejection to a safe failure and idle status", async () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-pi-reject-"));
    const faux = fauxProvider({ provider: "reject-provider", api: "reject-api", models: [{ id: "rejecting", reasoning: false }] });
    faux.setResponses([fauxAssistantMessage("", { stopReason: "error", errorMessage: "provider secret: rejected" })]);
    const model = faux.models[0];
    const events: EventDraft[] = [];
    const runtime = { hasConfiguredAuth: () => true, getAuth: async () => ({ auth: { apiKey: "fake" }, env: {} }), getAvailable: async () => [model], getProviders: () => [faux.provider], streamSimple: (selected: Model<Api>, context: never, options: never) => faux.provider.stream(selected, context, options) } as unknown as ModelRuntime;
    const adapter = new PiAdapter((event) => events.push(event), undefined, runtime);
    const options = { ...scope, taskId: null, agentId: createId("agt"), role: "general_worker" as const, cwd: directory, packagePath: resolve(import.meta.dirname, "..", "..", "..", "pi-package"), model: { provider: "reject-provider", id: "rejecting" } };
    try {
      await adapter.start(options);
      await adapter.prompt(options.agentId, "reject");
      const failure = events.find((event) => event.type === "agent.failed");
      expect(failure).toMatchObject({ type: "agent.failed", payload: { reason: "provider_error" } });
      expect(JSON.stringify(failure)).not.toContain("provider secret");
      expect(adapter.inspect()[0]).toMatchObject({ status: "idle" });
    } finally { adapter.stop(options.agentId); rmSync(directory, { recursive: true, force: true }); }
  });
});


describe("Pi queued follow-up and abort lifecycle", () => {
  it("aborts a controlled streaming turn without post-stop event leakage", async () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-pi-stop-"));
    let release!: (message: ReturnType<typeof fauxAssistantMessage>) => void;
    const response = new Promise<ReturnType<typeof fauxAssistantMessage>>((resolve) => { release = resolve; });
    const faux = fauxProvider({ provider: "stop-provider", api: "stop-api", models: [{ id: "controlled", reasoning: false }], tokensPerSecond: 1 });
    faux.setResponses([async () => response]);
    const model = faux.models[0];
    const events: EventDraft[] = [];
    const runtime = { hasConfiguredAuth: () => true, getAuth: async () => ({ auth: { apiKey: "fake" }, env: {} }), getAvailable: async () => [model], getProviders: () => [faux.provider], streamSimple: (selected: Model<Api>, context: never, options: never) => faux.provider.stream(selected, context, options) } as unknown as ModelRuntime;
    const adapter = new PiAdapter((event) => events.push(event), undefined, runtime);
    const options = { ...scope, taskId: null, agentId: createId("agt"), role: "general_worker" as const, cwd: directory, packagePath: resolve(import.meta.dirname, "..", "..", "..", "pi-package"), model: { provider: "stop-provider", id: "controlled" } };
    try {
      await adapter.start(options);
      const turn = adapter.prompt(options.agentId, "hold");
      for (let attempt = 0; attempt < 20 && !events.some((event) => event.type === "agent.started"); attempt += 1) await new Promise((resolve) => setTimeout(resolve, 0));
      expect(events.some((event) => event.type === "agent.started")).toBe(true);
      await adapter.followUp(options.agentId, "queued");
      const aborting = adapter.abort(options.agentId);
      // The controlled response promise does not observe AbortSignal. Release it
      // after requesting abort so the SDK can finish cancellation.
      release(fauxAssistantMessage("released after abort"));
      await aborting;
      await turn.catch(() => undefined);
      expect(adapter.inspect()[0]).toMatchObject({ status: "idle" });
      const count = events.length;
      adapter.stop(options.agentId);
      await new Promise((resolve) => setTimeout(resolve, 0));
      expect(events).toHaveLength(count);
      expect(adapter.inspect()).toEqual([]);
    } finally { adapter.stop(options.agentId); rmSync(directory, { recursive: true, force: true }); }
  });
});

describe("typed submission tool descriptions", () => {
  it("name each accepted schema and point at its non-empty JSON Schema", () => {
    const tools = createSessionTools({ projectId: createId("prj"), agentId: createId("agt"), cwd: tmpdir() } as Parameters<typeof createSessionTools>[0], () => ({}));
    const submissions = tools.filter((tool) => tool.description.startsWith("Submit one typed NOSH record."));
    expect(submissions.length).toBeGreaterThan(10);
    for (const tool of submissions) {
      const paths = tool.description.match(/\S+\.v1\.schema\.json/g) ?? [];
      expect(paths.length, tool.name).toBeGreaterThan(0);
      for (const path of paths) {
        const document = JSON.parse(readFileSync(path.replace(/[.,]$/, ""), "utf8")) as { definitions: Record<string, object> };
        expect(JSON.stringify(Object.values(document.definitions)[0]).length, path).toBeGreaterThan(10);
      }
    }
  });
});

describe("default terminal tool without a Task Packet", () => {
  it("follows the session role so reviewers are told to return a review-verdict", () => {
    const taskId = createId("tsk");
    expect(sessionToolIds({ taskId, role: "reviewer" })).toEqual(["nosh_artifact_read", "nosh_review_submit"]);
    for (const role of ["general_worker", "research_director", "mission_director"] as const) expect(sessionToolIds({ taskId, role })).toEqual(["nosh_response_submit"]);
  });
});
