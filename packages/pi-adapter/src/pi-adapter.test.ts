import { createId } from "@nosh/core";
import { describe, expect, it } from "vitest";
import { createSessionTools, mapPiEvent, noshPromptCacheKey, noshSystemPrompt } from "./index.js";

const scope = { projectId: createId("prj"), missionId: null, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, taskId: createId("tsk"), agentId: createId("agt") };

describe("Pi event mapping", () => {
  it("streams text before completion as ephemeral events", () => {
    const mapped = mapPiEvent(scope, { type: "message_update", message: {} as never, assistantMessageEvent: { type: "text_delta", delta: "hello", contentIndex: 0, partial: {} as never } });
    expect(mapped).toMatchObject({ retention: "ephemeral", type: "agent.text_delta", payload: { delta: "hello" } });
  });

  it("records tool lifecycle with correlation", () => {
    const mapped = mapPiEvent(scope, { type: "tool_execution_start", toolCallId: "call_1", toolName: "read", args: { path: "README.md" } });
    expect(mapped).toMatchObject({ retention: "persistent", type: "agent.tool_started", correlationId: scope.taskId });
  });

  it("persists exact Pi token usage without assistant reasoning", () => { const mapped = mapPiEvent(scope, { type: "agent_end", willRetry: false, messages: [{ role: "assistant", content: [{ type: "thinking", thinking: "hidden" }, { type: "text", text: "result" }], usage: { totalTokens: 42 } }] as never }); expect(mapped).toMatchObject({ type: "agent.completed", payload: { message: "result", modelTokens: 42 } }); expect(JSON.stringify(mapped)).not.toContain("hidden"); });

  it("attributes only the current bounded turn after session reuse or compaction", () => { const event = { type: "agent_end", willRetry: false, messages: [{ role: "assistant", content: [{ type: "text", text: "result" }], usage: { totalTokens: 42 } }] }; expect(mapPiEvent(scope, event as never, 30)).toMatchObject({ payload: { modelTokens: 12 } }); expect(mapPiEvent(scope, { ...event, messages: [{ role: "assistant", content: [], usage: { totalTokens: 10 } }] } as never, 42)).toMatchObject({ payload: { modelTokens: 10 } }); });

  it("records cache reads and writes for cost diagnostics", () => { const event = { type: "agent_end", willRetry: false, messages: [{ role: "assistant", content: [], usage: { totalTokens: 100, input: 20, output: 10, cacheRead: 60, cacheWrite: 10 } }] }; expect(mapPiEvent(scope, event as never)).toMatchObject({ payload: { inputTokens: 20, outputTokens: 10, cacheReadTokens: 60, cacheWriteTokens: 10 } }); });

  it("uses stable cache affinity without volatile task or agent IDs", () => {
    const options = { ...scope, role: "general_worker" as const, tools: ["read", "bash"] };
    const rotated = { ...options, taskId: createId("tsk"), agentId: createId("agt"), tools: ["bash", "read"] };
    expect(noshPromptCacheKey(options)).toBe(noshPromptCacheKey(rotated));
    expect(noshPromptCacheKey(options)).not.toBe(noshPromptCacheKey({ ...options, role: "reviewer" }));
    expect(noshSystemPrompt("general_worker")).not.toContain(scope.taskId);
  });

  it("binds typed submissions to daemon-issued session scope with a compact receipt", async () => { const calls: unknown[][] = []; const tools = createSessionTools({ ...scope, role: "general_worker", cwd: ".", packagePath: "." }, (...values) => { calls.push(values); return { accepted: true, veryLargeProjection: "x".repeat(10_000) }; }); const tool = tools.find((candidate) => candidate.name === "nosh_response_submit")!; const result = await tool.execute("call_1", { record: { answer: 1 } }, undefined, undefined, {} as never); expect(calls).toEqual([["nosh_response_submit", scope.projectId, `task:${scope.taskId}`, { answer: 1 }, scope.agentId]]); expect(JSON.stringify(result.content).length).toBeLessThan(100); });
});
