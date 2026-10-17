import { createId } from "@nosh/core";
import { createEpisode } from "@nosh/episodes";
import type { Episode, ExecutionThread } from "@nosh/wire";
import { describe, expect, it } from "vitest";
import { renderThreadContext } from "./index.js";

describe("compact thread context", () => {
  it("keeps selected truth addressable under a hard episode projection budget", () => {
    const thread = fixtureThread(); const episodes = Array.from({ length: 20 }, (_, index) => fixtureEpisode(thread, index));
    thread.inputRefs = episodes.map((episode) => episode.episodeId);
    const context = renderThreadContext(thread, episodes, [], []);
    expect(context.length).toBeLessThan(18_000);
    expect(context).toContain(episodes.at(-1)!.episodeId);
    expect(context).toContain("fetch by ID if needed");
  });

  it("never reports negative remaining budgets", () => {
    const thread = fixtureThread(); thread.usage = { toolCalls: 99, modelTokens: 99_000, wallClockSeconds: 9_999 };
    expect(renderThreadContext(thread, [], [], [])).toContain("0 tool calls, 0 model tokens, 0 seconds");
  });
});

function fixtureThread(): ExecutionThread {
  const now = new Date().toISOString();
  return {
    $schema: "https://nosh.dev/schemas/execution-thread/v1",
    schemaVersion: 1,
    threadId: createId("thr"),
    projectId: createId("prj"),
    taskId: createId("tsk"),
    ownerScope: { missionId: null, directionId: null, autoresearchId: null, experimentId: null, graphNodeId: null },
    role: "general_worker",
    purpose: "test",
    executionMode: "background",
    state: "open",
    parentThreadId: null,
    childThreadIds: [],
    currentAgentId: null,
    currentPiSessionId: null,
    activeInstructionId: null,
    sessionHistory: [],
    episodeIds: [],
    inputRefs: [],
    activeSkillIds: [],
    capabilities: [],
    taskPermissions: null,
    taskWorkspace: null,
    budget: { maximumToolCalls: 20, maximumModelTokens: 1_000, maximumWallClockSeconds: 120 },
    usage: { toolCalls: 0, modelTokens: 0, wallClockSeconds: 0 },
    nextStepNumber: 1,
    createdAt: now,
    updatedAt: now,
  };
}

function fixtureEpisode(thread: ExecutionThread, index: number): Episode {
  const statement = `${index} ${"verified context ".repeat(80)}`;
  return createEpisode({ projectId: thread.projectId, threadId: thread.threadId, instructionId: createId("ins"), stepNumber: index + 1, episodeType: "episode_test", objective: statement, status: "completed", summary: statement, facts: Array.from({ length: 20 }, () => ({ statement, evidenceRefs: [], confidence: "high" as const })), decisions: Array.from({ length: 20 }, () => ({ statement, rationale: statement, evidenceRefs: [] })), artifactIds: [], evidenceIds: [], changedFiles: [], unresolvedQuestions: Array.from({ length: 10 }, () => statement), recommendedNextActions: [], contextInputRefs: [], trace: { firstSequence: index + 1, lastSequence: index + 1 }, usage: { toolCalls: 1, modelTokens: 1, wallClockSeconds: 1 }, startedAt: new Date().toISOString(), completedAt: new Date().toISOString() });
}
