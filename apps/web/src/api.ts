import { remoteActive, remoteApi, subscribeRemote } from "./remote.js";

export type Project = { projectId: string; repositoryRoot: string; registeredAt: string };
export type ProjectContract = { projectId: string; contractVersion: number; workingTitle: string; northStar: { question: string; contributionType: string; decisionUse: string }; computeEnvelope: { maximumGpuHours: number; maximumDiskBytes: number; allowedHardwareClasses: string[] }; approvedAt: string | null };
export type Agent = { agentId: string; projectId: string; missionId: string | null; directionId: string | null; autoresearchId: string | null; experimentId: string | null; runId: string | null; jobId: string | null; role: string; status: string; currentTool: string | null; startedAt: string; lastEventAt: string; taskId: string | null; modelProvider: string | null; modelId: string | null; modelName: string | null; thinkingLevel: string; contextTokens: number | null; contextWindow: number | null; contextPercent: number | null };
export type Job = { jobId: string; experimentId: string; state: string; startedAt: string | null; command: string[] };
export type NoshEvent = { eventId: string; sequence: number | null; timestamp: string; type: string; source: string; payload: unknown; scope: { projectId: string; missionId: string | null; directionId: string | null; autoresearchId: string | null; experimentId: string | null; runId: string | null; agentId: string | null; jobId: string | null } };
export type GraphNodeRecord = { id: string; type: string; title: string; required: boolean; hardDependencies: string[]; state: string; attempt: number; maximumAttempts: number };
export type Stored<T> = { entityId: string; version: number; state: string; updatedAt: string; value: T };
export type MissionRecord = { missionId: string; projectId: string; title: string; objective: string; deliverables: string[]; successCriteria: string[]; approvedGraphVersion: number | null; state: string; graphVersion: number; nodes: GraphNodeRecord[]; createdAt: string; updatedAt: string };
export type DirectionRecord = { directionId: string; projectId: string; missionId: string | null; question: string; decisionUse: string; state: string; evaluationContractHash: string; acceptedBaseline: { commit: string; reviewId: string } | null; graphVersion: number; nodes: GraphNodeRecord[]; createdAt: string; updatedAt: string };
export type AutoresearchRecord = { autoresearchId: string; projectId: string; directionId: string | null; decisionQuestion: string; familyTags: string[]; scope: string[]; state: string; evaluationContractHash: string; currentRound: number; maximumExperiments: number; maximumRounds: number; maximumWallClockSeconds: number; maximumModelTokens: number; maximumGpuSeconds: number; maximumDiskBytes: number };
export type ThreadRecord = { threadId: string; projectId: string; purpose: string; executionMode: "background" | "foreground_fork"; state: string; parentThreadId: string | null; childThreadIds: string[]; episodeIds: string[]; role: string; currentAgentId: string | null; sessionHistory: Array<{ agentId: string; piSessionId: string; startedAt: string; endedAt: string | null; endReason: string | null }>; usage: { toolCalls: number; modelTokens: number; wallClockSeconds: number }; budget: { maximumToolCalls: number; maximumModelTokens: number; maximumWallClockSeconds: number }; ownerScope: { missionId: string | null; directionId: string | null; autoresearchId: string | null; experimentId: string | null; graphNodeId: string | null }; updatedAt: string };
export type EpisodeRecord = { episodeId: string; threadId: string; instructionId: string; stepNumber: number; episodeType: string; objective: string; status: string; summary: string; facts: Array<{ statement: string; evidenceRefs: string[]; confidence: string }>; decisions: Array<{ statement: string; rationale: string; evidenceRefs: string[] }>; artifactIds: string[]; evidenceIds: string[]; changedFiles: string[]; unresolvedQuestions: string[]; trace: { firstSequence: number; lastSequence: number }; usage: { toolCalls: number; modelTokens: number; wallClockSeconds: number }; episodeHash: string; completedAt: string };

export function token(): string { return sessionStorage.getItem("nosh.sessionToken") ?? localStorage.getItem("nosh.bootstrapToken") ?? ""; }

let exchange: Promise<string> | null = null;
export async function connectLocal(rawBootstrap: string): Promise<string> {
  const bootstrap = rawBootstrap.trim();
  if (!bootstrap) throw new Error("Enter the loopback bootstrap token");
  let response: Response;
  try { response = await fetch("/api/session", { method: "POST", headers: { authorization: `Bearer ${bootstrap}` } }); }
  catch { throw new Error("noshd is not reachable. Run `nosh start`, then try again."); }
  if (!response.ok) throw new Error(response.status === 401 ? "The bootstrap token was rejected" : `Local connection failed (${response.status})`);
  const value = await response.json() as { token: string; expiresAt: string };
  sessionStorage.setItem("nosh.sessionToken", value.token);
  sessionStorage.setItem("nosh.sessionExpiresAt", value.expiresAt);
  localStorage.removeItem("nosh.bootstrapToken");
  return value.token;
}

async function localToken(): Promise<string> { const session = sessionStorage.getItem("nosh.sessionToken"); const expiresAt = Date.parse(sessionStorage.getItem("nosh.sessionExpiresAt") ?? ""); if (session && expiresAt > Date.now() + 5_000) return session; if (exchange) return exchange; const bootstrap = localStorage.getItem("nosh.bootstrapToken"); if (!bootstrap) throw new Error("Enter the loopback bootstrap token in Settings"); exchange = connectLocal(bootstrap).finally(() => { exchange = null; }); return exchange; }

export async function api<T>(path: string, init?: RequestInit): Promise<T> {
  if (remoteActive()) return remoteApi<T>(path, init);
  const response = await fetch(`/api${path}`, { ...init, headers: { authorization: `Bearer ${await localToken()}`, "content-type": "application/json", ...init?.headers } });
  if (!response.ok) throw new Error((await response.json() as { error?: string }).error ?? `Request failed (${response.status})`);
  return response.json() as Promise<T>;
}

export function subscribe(projectId: string, after: number, receive: (event: NoshEvent) => void): () => void {
  if (remoteActive()) return subscribeRemote((event) => { if (event.scope.projectId === projectId && (event.sequence === null || event.sequence > after)) receive(event); });
  if (!projectId || !token()) return () => undefined; let socket: WebSocket | undefined; let closed = false; void localToken().then((session) => { if (closed) return; const url = new URL(`/api/events?projectId=${encodeURIComponent(projectId)}&after=${after}`, window.location.href); url.protocol = url.protocol === "https:" ? "wss:" : "ws:"; socket = new WebSocket(url, ["nosh", `auth.${session}`]); socket.onmessage = (message) => receive(JSON.parse(String(message.data)) as NoshEvent); }).catch(() => undefined); return () => { closed = true; socket?.close(); };
}
