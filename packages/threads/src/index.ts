import { nowUtc } from "@nosh/core";
import { renderEpisode } from "@nosh/episodes";
import { executionThreadSchema, type Episode, type ExecutionThread, type RuntimeInstruction } from "@nosh/wire";

type OpenInstruction = Extract<RuntimeInstruction, { operation: "THREAD_OPEN" }> | Extract<RuntimeInstruction, { operation: "THREAD_FORK" }>;

export function createThread(instruction: OpenInstruction): ExecutionThread {
  const now = nowUtc();
  return executionThreadSchema.parse({
    $schema: "https://nosh.dev/schemas/execution-thread/v1", schemaVersion: 1, threadId: instruction.threadId, projectId: instruction.projectId, taskId: instruction.taskId, ownerScope: instruction.ownerScope,
    role: instruction.role, purpose: instruction.purpose, executionMode: instruction.executionMode, state: instruction.executionMode === "foreground_fork" ? "awaiting_user" : "open",
    parentThreadId: instruction.operation === "THREAD_FORK" ? instruction.controllingThreadId : instruction.parentThreadId, childThreadIds: [], currentAgentId: null, currentPiSessionId: null, activeInstructionId: null, sessionHistory: [],
    episodeIds: [], inputRefs: instruction.inputRefs, activeSkillIds: instruction.skillIds, capabilities: instruction.capabilities, taskPermissions: instruction.taskPermissions ?? null, taskWorkspace: instruction.taskWorkspace ?? null, budget: instruction.budget, usage: { toolCalls: 0, modelTokens: 0, wallClockSeconds: 0 }, nextStepNumber: 1, createdAt: now, updatedAt: now,
  });
}

export function ensureCanStep(thread: ExecutionThread): void {
  if (!["open", "awaiting", "awaiting_user", "paused"].includes(thread.state)) throw new Error(`Thread ${thread.threadId} cannot step from ${thread.state}`);
  if (thread.activeInstructionId) throw new Error(`Thread ${thread.threadId} still has an active instruction`);
  if (thread.usage.toolCalls >= thread.budget.maximumToolCalls) throw new Error(`Thread ${thread.threadId} exhausted its tool-call budget`);
  if (thread.usage.modelTokens >= thread.budget.maximumModelTokens) throw new Error(`Thread ${thread.threadId} exhausted its model-token budget`);
  if (thread.usage.wallClockSeconds >= thread.budget.maximumWallClockSeconds) throw new Error(`Thread ${thread.threadId} exhausted its wall-clock budget`);
}

export function renderThreadContext(thread: ExecutionThread, episodes: Episode[], extraRefs: string[], skillPrompts: string[]): string {
  const refs = [...new Set([...thread.inputRefs, ...extraRefs])]; const candidates = refs.flatMap((ref) => episodes.find((episode) => episode.episodeId === ref) ?? []).slice(-20); const rendered: string[] = []; const included = new Set<string>(); let remaining = 16_000;
  for (const episode of [...candidates].reverse()) { const value = renderEpisode(episode); if (value.length > remaining) continue; rendered.unshift(value); included.add(episode.episodeId); remaining -= value.length + 2; }
  const omitted = candidates.filter((episode) => !included.has(episode.episodeId)).map((episode) => episode.episodeId); const episodeIds = new Set(candidates.map((episode) => episode.episodeId)); const other = refs.filter((ref) => !episodeIds.has(ref)).slice(-50);
  return [skillPrompts.length ? `Active skills:\n${skillPrompts.join("\n\n")}` : "", rendered.length ? `Selected prior episodes:\n\n${rendered.join("\n\n")}` : "", omitted.length ? `Older selected episodes (fetch by ID if needed): ${omitted.join(", ")}` : "", other.length ? `Selected authoritative references: ${other.join(", ")}` : "", `Thread budget remaining: ${Math.max(0, thread.budget.maximumToolCalls - thread.usage.toolCalls)} tool calls, ${Math.max(0, thread.budget.maximumModelTokens - thread.usage.modelTokens)} model tokens, ${Math.max(0, thread.budget.maximumWallClockSeconds - thread.usage.wallClockSeconds)} seconds.`].filter(Boolean).join("\n\n");
}
