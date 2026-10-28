import { createId } from "@nosh/core";
import { schemaUri, type EventDraft, type EventEnvelope } from "@nosh/wire";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { NoshDaemon } from "./daemon.js";

describe("nosh_code inner-call events", () => {
  it("are stored and replayed with their parentToolCallId, like the events Pi emits for direct calls", () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-code-events-"));
    const daemon = new NoshDaemon({ dataDirectory: join(directory, "data"), bootstrapToken: "test" });
    try {
      const project = daemon.initializeProject({ path: join(directory, "repository"), createRepository: true, workingTitle: "Code events" });
      const scope = { projectId: project.projectId, missionId: null, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: createId("agt") };
      const inner = (type: string, payload: EventDraft["payload"]): EventDraft => ({ $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type, source: "pi", correlationId: createId("tsk"), causationId: null, scope, payload });
      // The adapter's event sink is the daemon's appendDraft: the same path every Pi tool event takes.
      const append = (draft: EventDraft) => (daemon as unknown as { appendDraft(draft: EventDraft): EventEnvelope }).appendDraft(draft);
      append(inner("agent.tool_started", { toolCallId: "call_code:0", toolName: "nosh_workspace_read", args: ["src/a.ts"], parentToolCallId: "call_code" }));
      append(inner("agent.tool_completed", { toolCallId: "call_code:0", toolName: "nosh_workspace_read", isError: false, result: "export const a = 1;\n", parentToolCallId: "call_code" }));
      const replayed = daemon.replay(project.projectId, 0).filter((event) => event.type.startsWith("agent.tool_"));
      expect(replayed.map((event) => [event.type, (event.payload as Record<string, unknown>).parentToolCallId])).toEqual([["agent.tool_started", "call_code"], ["agent.tool_completed", "call_code"]]);
    } finally { daemon.stop(); rmSync(directory, { recursive: true, force: true }); }
  });
});
