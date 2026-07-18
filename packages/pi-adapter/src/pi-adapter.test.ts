import { createId } from "@nosh/core";
import { describe, expect, it } from "vitest";
import { createSessionTools, mapPiEvent } from "./index.js";

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

  it("binds typed submissions to daemon-issued session scope", async () => { const calls: unknown[][] = []; const tools = createSessionTools({ ...scope, role: "general_worker", cwd: ".", packagePath: "." }, (...values) => { calls.push(values); return { accepted: true }; }); const tool = tools.find((candidate) => candidate.name === "nosh_response_submit")!; await tool.execute("call_1", { record: { answer: 1 } }, undefined, undefined, {} as never); expect(calls).toEqual([["nosh_response_submit", scope.projectId, `task:${scope.taskId}`, { answer: 1 }, scope.agentId]]); });
});
