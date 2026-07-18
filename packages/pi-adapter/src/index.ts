import {
  AgentSession,
  DefaultResourceLoader,
  ModelRuntime,
  SettingsManager,
  createAgentSession,
  defineTool,
  type AgentSessionEvent,
  type ToolDefinition,
} from "@earendil-works/pi-coding-agent";
import { Type } from "@earendil-works/pi-ai";
import { schemaUri, type EventDraft, type JsonValue } from "@nosh/wire";
import { resolve } from "node:path";

export type PiSessionScope = {
  projectId: string;
  missionId: string | null;
  directionId: string | null;
  autoresearchId: string | null;
  experimentId: string | null;
  runId: string | null;
  jobId: string | null;
  taskId: string | null;
  agentId: string;
};

export type PiSessionOptions = PiSessionScope & {
  role: "nosh" | "mission_director" | "research_director" | "librarian_researcher" | "general_worker" | "reviewer";
  cwd: string;
  packagePath: string;
  tools?: string[];
  thinkingLevel?: "off" | "minimal" | "low" | "medium" | "high" | "xhigh";
  model?: { provider: string; id: string };
};

export type AgentInspection = PiSessionScope & {
  role: PiSessionOptions["role"];
  piSessionId: string;
  status: "idle" | "running" | "compacting" | "aborting";
  currentTool: string | null;
  startedAt: string;
  lastEventAt: string;
  modelProvider: string | null;
  modelId: string | null;
  modelName: string | null;
  thinkingLevel: string;
  contextTokens: number | null;
  contextWindow: number | null;
  contextPercent: number | null;
};

type Managed = { session: AgentSession; unsubscribe: () => void; inspection: AgentInspection; options: PiSessionOptions };

export class PiAdapter {
  private readonly sessions = new Map<string, Managed>();
  private readonly modelTokens = new Map<string, number>();
  private modelRuntime: ModelRuntime | undefined;

  constructor(private readonly emit: (event: EventDraft) => void, private readonly submit?: (tool: string, projectId: string, attemptKey: string, record: unknown, agentId: string) => unknown | Promise<unknown>) {}

  async availableModels(): Promise<Array<{ provider: string; id: string; name: string }>> {
    const runtime = await this.runtime();
    const models = await runtime.getAvailable();
    return models.map((model) => ({ provider: model.provider, id: model.id, name: model.name }));
  }

  async start(options: PiSessionOptions): Promise<AgentInspection> {
    const existing = this.sessions.get(options.agentId); if (existing) { if (JSON.stringify(existing.options) !== JSON.stringify(options)) throw new Error(`Agent ${options.agentId} already has a different Pi session scope`); return { ...existing.inspection }; }
    const loader = new DefaultResourceLoader({
      cwd: options.cwd,
      agentDir: resolve(options.cwd, ".nosh", "pi"),
      settingsManager: SettingsManager.inMemory({ packages: [resolve(options.packagePath)] }),
      noExtensions: true,
    });
    await loader.reload();
    const modelRuntime = await this.runtime(); const model = options.model ? modelRuntime.getModel(options.model.provider, options.model.id) : undefined; if (options.model && !model) throw new Error(`Unavailable Pi model ${options.model.provider}/${options.model.id}`); const { session } = await createAgentSession({
      cwd: options.cwd,
      modelRuntime,
      resourceLoader: loader,
      customTools: this.submit ? createSessionTools(options, this.submit) : [],
      ...(model ? { model } : {}),
      ...(options.tools ? { tools: options.tools } : {}),
      ...(options.thinkingLevel ? { thinkingLevel: options.thinkingLevel } : {}),
    });
    const timestamp = new Date().toISOString();
    const inspection: AgentInspection = {
      projectId: options.projectId, missionId: options.missionId, directionId: options.directionId, autoresearchId: options.autoresearchId,
      experimentId: options.experimentId, runId: options.runId, jobId: options.jobId, taskId: options.taskId, agentId: options.agentId,
      role: options.role, piSessionId: session.sessionId, status: "idle", currentTool: null, startedAt: timestamp, lastEventAt: timestamp, modelProvider: session.model?.provider ?? null, modelId: session.model?.id ?? null, modelName: session.model?.name ?? null, thinkingLevel: session.thinkingLevel, contextTokens: null, contextWindow: session.model?.contextWindow ?? null, contextPercent: null,
    };
    const unsubscribe = session.subscribe((event) => this.onEvent(inspection, event));
    this.sessions.set(options.agentId, { session, unsubscribe, inspection, options: structuredClone(options) });
    return { ...inspection };
  }

  async prompt(agentId: string, prompt: string): Promise<void> {
    const managed = this.required(agentId);
    managed.inspection.status = "running";
    await managed.session.prompt(prompt);
  }

  async steer(agentId: string, message: string): Promise<void> {
    await this.required(agentId).session.steer(message);
  }

  async followUp(agentId: string, message: string): Promise<void> {
    await this.required(agentId).session.followUp(message);
  }

  async compact(agentId: string, instructions?: string): Promise<void> {
    const managed = this.required(agentId);
    managed.inspection.status = "compacting";
    await managed.session.compact(instructions);
    managed.inspection.status = "idle";
  }

  async abort(agentId: string): Promise<void> {
    const managed = this.required(agentId);
    managed.inspection.status = "aborting";
    await managed.session.abort();
  }

  stop(agentId: string): void {
    const managed = this.sessions.get(agentId); if (!managed) return;
    managed.unsubscribe();
    managed.session.dispose();
    this.sessions.delete(agentId);
    this.modelTokens.delete(agentId);
  }

  inspect(): AgentInspection[] {
    return [...this.sessions.values()].map(({ inspection, session }) => { const usage = session.getSessionStats().contextUsage; return { ...inspection, modelProvider: session.model?.provider ?? null, modelId: session.model?.id ?? null, modelName: session.model?.name ?? null, thinkingLevel: session.thinkingLevel, contextTokens: usage?.tokens ?? null, contextWindow: usage?.contextWindow ?? session.model?.contextWindow ?? null, contextPercent: usage?.percent ?? null }; });
  }

  private required(agentId: string): Managed {
    const managed = this.sessions.get(agentId);
    if (!managed) throw new Error(`Unknown Pi agent ${agentId}`);
    return managed;
  }

  private async runtime(): Promise<ModelRuntime> {
    this.modelRuntime ??= await ModelRuntime.create();
    return this.modelRuntime;
  }

  private onEvent(inspection: AgentInspection, event: AgentSessionEvent): void {
    inspection.lastEventAt = new Date().toISOString();
    const mapped = mapPiEvent(inspection, event, this.modelTokens.get(inspection.agentId) ?? 0); if (event.type === "agent_end") this.modelTokens.set(inspection.agentId, tokenUsage(event.messages));
    if (!mapped) return;
    if (event.type === "agent_start") inspection.status = "running";
    if (event.type === "agent_end") { inspection.status = "idle"; inspection.currentTool = null; }
    if (event.type === "tool_execution_start") inspection.currentTool = event.toolName;
    if (event.type === "tool_execution_end") inspection.currentTool = null;
    this.emit(mapped);
  }
}

const submissionTools = ["nosh_task_acknowledge", "nosh_progress_emit", "nosh_response_submit", "nosh_review_submit", "nosh_blocker_submit", "nosh_graph_change_propose", "nosh_delegation_request", "nosh_handoff_create", "nosh_handoff_teachback", "nosh_experiment_propose", "nosh_evidence_submit", "nosh_episode_submit", "nosh_runtime_instruct"] as const;
export function createSessionTools(options: PiSessionOptions, submit: (tool: string, projectId: string, attemptKey: string, record: unknown, agentId: string) => unknown | Promise<unknown>): ToolDefinition[] { const attemptKey = options.taskId ? `task:${options.taskId}` : `agent:${options.agentId}`; return submissionTools.map((name) => defineTool({ name, label: name, description: "Submit one authoritative NOSH typed record to the daemon-bound Project and attempt.", parameters: Type.Object({ record: Type.Unknown({ description: "Complete NOSH schema record" }) }), async execute(_toolCallId, parameters) { const result = await submit(name, options.projectId, attemptKey, parameters.record, options.agentId); const accepted = Boolean((result as { accepted?: boolean }).accepted); return { content: [{ type: "text", text: accepted ? `NOSH accepted the typed record: ${JSON.stringify(result)}` : `NOSH rejected the typed record: ${JSON.stringify(result)}` }], details: result, isError: !accepted }; } })); }

export function mapPiEvent(scope: PiSessionScope, event: AgentSessionEvent, previousModelTokens = 0): EventDraft | undefined {
  const base = {
    $schema: schemaUri("event"), schemaVersion: 1 as const, source: "pi", correlationId: scope.taskId, causationId: null,
    scope: { projectId: scope.projectId, missionId: scope.missionId, directionId: scope.directionId, autoresearchId: scope.autoresearchId, experimentId: scope.experimentId, runId: scope.runId, jobId: scope.jobId, agentId: scope.agentId },
  };
  switch (event.type) {
    case "agent_start": return { ...base, retention: "persistent", type: "agent.started", payload: {} };
    case "agent_end": { const total = tokenUsage(event.messages); return { ...base, retention: "persistent", type: "agent.completed", payload: { message: assistantText(event.messages), modelTokens: total >= previousModelTokens ? total - previousModelTokens : total, willRetry: event.willRetry } }; }
    case "turn_start": return { ...base, retention: "persistent", type: "agent.turn_started", payload: {} };
    case "turn_end": return { ...base, retention: "persistent", type: "agent.turn_completed", payload: { toolResultCount: event.toolResults.length } };
    case "tool_execution_start": return { ...base, retention: "persistent", type: "agent.tool_started", payload: { toolCallId: event.toolCallId, toolName: event.toolName, args: json(event.args) } };
    case "tool_execution_update": return { ...base, retention: "ephemeral", type: "agent.tool_update", payload: { toolCallId: event.toolCallId, toolName: event.toolName, partialResult: json(event.partialResult) } };
    case "tool_execution_end": return { ...base, retention: "persistent", type: "agent.tool_completed", payload: { toolCallId: event.toolCallId, toolName: event.toolName, isError: event.isError, result: json(event.result) } };
    case "message_update":
      if (event.assistantMessageEvent.type !== "text_delta") return undefined;
      return { ...base, retention: "ephemeral", type: "agent.text_delta", payload: { delta: event.assistantMessageEvent.delta } };
    case "compaction_start": return { ...base, retention: "persistent", type: "agent.compaction_started", payload: { reason: event.reason } };
    case "compaction_end": return { ...base, retention: "persistent", type: "agent.compaction_completed", payload: { reason: event.reason, aborted: event.aborted, willRetry: event.willRetry, errorMessage: event.errorMessage ?? null } };
    default: return undefined;
  }
}

function assistantText(messages: unknown[]): string {
  const message = [...messages].reverse().find((entry) => entry && typeof entry === "object" && (entry as { role?: string }).role === "assistant") as { content?: unknown[] } | undefined;
  return message?.content?.filter((content): content is { type: "text"; text: string } => Boolean(content && typeof content === "object" && (content as { type?: string }).type === "text" && typeof (content as { text?: unknown }).text === "string")).map((content) => content.text).join("\n") ?? "";
}

function tokenUsage(messages: unknown[]): number { return messages.reduce<number>((total, message) => { if (!message || typeof message !== "object" || (message as { role?: string }).role !== "assistant") return total; const tokens = (message as { usage?: { totalTokens?: unknown } }).usage?.totalTokens; return total + (typeof tokens === "number" && Number.isFinite(tokens) && tokens > 0 ? tokens : 0); }, 0); }

function json(value: unknown): JsonValue {
  try {
    const encoded = JSON.stringify(value);
    return encoded === undefined ? null : JSON.parse(encoded) as JsonValue;
  } catch {
    return { unavailable: "non_serializable" };
  }
}
