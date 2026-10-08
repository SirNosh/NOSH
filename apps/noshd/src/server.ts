import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import { randomBytes } from "node:crypto";
import type { Duplex } from "node:stream";
import { isIP } from "node:net";
import { URL } from "node:url";
import { WebSocketServer, type WebSocket } from "ws";
import { modelSelectionSchema, thinkingLevelSchema, type JsonValue, type ModelSelection } from "@nosh/wire";
import type { PiSessionOptions } from "@nosh/pi-adapter";
import type { JobSpec } from "@nosh/jobs";
import type { DirectionState, GraphNode, GraphOperation, MissionState } from "@nosh/graph";
import type { AutoresearchProjection } from "./research-control.js";
import { NoshDaemon } from "./daemon.js";

type ServerOptions = { host?: "127.0.0.1" | "::1"; port: number };

export class LocalApiServer {
  private readonly http: Server;
  private readonly websocket = new WebSocketServer({ noServer: true });
  private readonly sessions = new Map<string, number>();

  constructor(private readonly daemon: NoshDaemon) {
    this.http = createServer((request, response) => void this.handle(request, response));
    this.http.on("upgrade", (request, socket, head) => this.handleUpgrade(request, socket, head));
  }

  async listen(options: ServerOptions): Promise<void> {
    await new Promise<void>((resolve, reject) => {
      this.http.once("error", reject);
      this.http.listen(options.port, options.host ?? "127.0.0.1", () => {
        this.http.off("error", reject);
        resolve();
      });
    });
  }

  async close(): Promise<void> {
    this.websocket.clients.forEach((client) => client.close());
    await new Promise<void>((resolve, reject) => this.http.close((error) => (error ? reject(error) : resolve())));
  }

  address(): string {
    const address = this.http.address();
    if (!address || typeof address === "string") throw new Error("Server is not listening");
    return `http://${address.address.includes(":") ? `[${address.address}]` : address.address}:${address.port}`;
  }

  private async handle(request: IncomingMessage, response: ServerResponse): Promise<void> {
    const url = new URL(request.url ?? "/", "http://localhost");
    const pathname = apiPath(url.pathname);
    if (request.method === "GET" && pathname === "/health") {
      this.send(response, 200, { status: "ok" });
      return;
    }
    if (request.method === "POST" && pathname === "/session") { if (!this.bootstrapAuthorized(request)) { this.send(response, 401, { error: "unauthorized" }); return; } const token = randomBytes(32).toString("base64url"); const expiresAt = Date.now() + 15 * 60_000; for (const [stale, expiry] of this.sessions) if (expiry <= Date.now()) this.sessions.delete(stale); this.sessions.set(token, expiresAt); this.send(response, 201, { token, expiresAt: new Date(expiresAt).toISOString() }); return; }
    if (!this.authorized(request)) {
      this.send(response, 401, { error: "unauthorized" });
      return;
    }

    try {
      if (request.method === "GET" && pathname === "/projects") {
        this.send(response, 200, { projects: this.daemon.projects() });
        return;
      }
      if (request.method === "POST" && /^\/projects\/[^/]+\/contract\/amend$/.test(pathname)) { const body = await readJson(request) as { contract?: unknown; idempotencyKey?: string }; requiredStrings(body, ["idempotencyKey"]); this.send(response, 200, { contract: this.daemon.amendProjectContract(pathname.split("/")[2]!, body.contract, body.idempotencyKey!) }); return; }
      if (request.method === "GET" && /^\/projects\/[^/]+\/contract$/.test(pathname)) { this.send(response, 200, { contract: this.daemon.projectContract(pathname.split("/")[2]!) }); return; }
      if (request.method === "GET" && pathname === "/models") { this.send(response, 200, { models: await this.daemon.agents.availableModels(), defaultModel: this.daemon.agents.defaultModel ?? null }); return; }
      if (request.method === "POST" && pathname === "/projects") {
        const project = await readJson(request);
        this.send(response, 201, { project: this.daemon.registerProject(project as { projectId: string; repositoryRoot: string; databasePath: string }) });
        return;
      }
      if (request.method === "POST" && pathname === "/projects/open") { const body = await readJson(request) as { path?: string; createRepository?: boolean; workingTitle?: string; githubRepositoryUrl?: string; model?: unknown; thinkingLevel?: unknown }; requiredStrings(body, ["path", "workingTitle"]); if (typeof body.createRepository !== "boolean" || body.githubRepositoryUrl !== undefined && typeof body.githubRepositoryUrl !== "string") throw new Error("repository mode or GitHub link is invalid"); const selection = parseModelSelection(body.model, body.thinkingLevel); await this.daemon.validateModelSelection(selection); const project = this.daemon.initializeProject({ path: body.path!, createRepository: body.createRepository, workingTitle: body.workingTitle!, ...(body.githubRepositoryUrl ? { githubRepositoryUrl: body.githubRepositoryUrl } : {}) }); void this.daemon.beginProjectIntake(project.projectId, selection).catch(() => undefined); this.send(response, 201, { project }); return; }
      if (request.method === "GET" && pathname === "/events") {
        const projectId = url.searchParams.get("projectId");
        const after = Number(url.searchParams.get("after") ?? "0");
        if (!projectId || !Number.isSafeInteger(after) || after < 0) throw new Error("projectId and a non-negative after cursor are required");
        const limit = Number(url.searchParams.get("limit") ?? "300");
        const recent = url.searchParams.get("recent");
        if (recent !== null && recent !== "true" && recent !== "false") throw new Error("recent must be true or false");
        this.send(response, 200, this.daemon.replayPage(projectId, after, limit, recent === "true"));
        return;
      }
      if (request.method === "POST" && pathname === "/chat") { const body = await readJson(request) as { projectId?: string; message?: string; idempotencyKey?: string; model?: unknown; thinkingLevel?: unknown }; requiredStrings(body, ["projectId", "message", "idempotencyKey"]); const selection = parseModelSelection(body.model, body.thinkingLevel); this.send(response, 202, await this.daemon.chat(body.projectId!, body.message!, body.idempotencyKey!, selection)); return; }
      if (request.method === "POST" && pathname === "/runtime/instructions") { this.send(response, 202, await this.daemon.runtime.execute(await readJson(request))); return; }
      if (request.method === "GET" && pathname === "/threads") { this.send(response, 200, { threads: this.daemon.runtime.threads(requiredQuery(url, "projectId")) }); return; }
      if (request.method === "GET" && /^\/threads\/[^/]+$/.test(pathname)) { this.send(response, 200, { thread: this.daemon.runtime.thread(requiredQuery(url, "projectId"), pathname.split("/")[2]!) }); return; }
      if (request.method === "POST" && /^\/threads\/[^/]+\/rotate$/.test(pathname)) { const body = await readJson(request) as { projectId?: string; idempotencyKey?: string }; requiredStrings(body, ["projectId", "idempotencyKey"]); this.send(response, 202, { thread: await this.daemon.runtime.rotate(body.projectId!, pathname.split("/")[2]!, body.idempotencyKey!) }); return; }
      if (request.method === "POST" && /^\/threads\/[^/]+\/messages$/.test(pathname)) { const body = await readJson(request) as { projectId?: string; message?: string; idempotencyKey?: string }; requiredStrings(body, ["projectId", "message", "idempotencyKey"]); await this.daemon.runtime.forkMessage(body.projectId!, pathname.split("/")[2]!, body.message!, body.idempotencyKey!); this.send(response, 202, { accepted: true }); return; }
      if (request.method === "GET" && pathname === "/episodes") { this.send(response, 200, { episodes: this.daemon.runtime.episodes(requiredQuery(url, "projectId")) }); return; }
      if (request.method === "GET" && /^\/episodes\/[^/]+\/trace$/.test(pathname)) { this.send(response, 200, { events: this.daemon.runtime.trace(requiredQuery(url, "projectId"), pathname.split("/")[2]!) }); return; }
      if (request.method === "POST" && pathname === "/skills") { const body = await readJson(request) as { projectId?: string; idempotencyKey?: string; manifest?: unknown }; requiredStrings(body, ["projectId", "idempotencyKey"]); this.send(response, 201, { skill: this.daemon.runtime.registerSkill(body.projectId!, body.manifest, body.idempotencyKey!) }); return; }
      if (request.method === "GET" && pathname === "/skills") { this.send(response, 200, { skills: this.daemon.runtime.skills(requiredQuery(url, "projectId")) }); return; }
      if (request.method === "POST" && pathname === "/programs") { const body = await readJson(request) as { projectId?: string; idempotencyKey?: string; program?: unknown }; requiredStrings(body, ["projectId", "idempotencyKey"]); this.send(response, 201, { program: this.daemon.runtime.registerProgram(body.projectId!, body.program, body.idempotencyKey!) }); return; }
      if (request.method === "GET" && pathname === "/programs") { this.send(response, 200, { programs: this.daemon.runtime.programRecords(requiredQuery(url, "projectId")) }); return; }
      if (request.method === "GET" && pathname === "/programs/states") { this.send(response, 200, { states: this.daemon.runtime.programStates(requiredQuery(url, "projectId")) }); return; }
      if (request.method === "POST" && /^\/programs\/[^/]+\/run$/.test(pathname)) { const body = await readJson(request) as { projectId?: string }; requiredStrings(body, ["projectId"]); this.send(response, 202, { results: await this.daemon.runtime.runProgram(body.projectId!, pathname.split("/")[2]!) }); return; }
      if (request.method === "GET" && pathname === "/missions") { const projectId = requiredQuery(url, "projectId"); this.send(response, 200, { missions: this.daemon.research.missions(projectId) }); return; }
      if (request.method === "POST" && pathname === "/missions") { const body = await readJson(request) as { projectId?: string; title?: string; objective?: string; deliverables?: string[]; successCriteria?: string[]; nonObjectives?: string[]; startingEvidence?: string[]; idempotencyKey?: string }; requiredStrings(body, ["projectId", "title", "objective", "idempotencyKey"]); if (!Array.isArray(body.deliverables) || !Array.isArray(body.successCriteria) || body.nonObjectives && !Array.isArray(body.nonObjectives) || body.startingEvidence && !Array.isArray(body.startingEvidence)) throw new Error("Mission deliverables and success criteria are required arrays"); this.send(response, 201, { mission: this.daemon.research.createMission(body.projectId!, { title: body.title!, objective: body.objective!, deliverables: body.deliverables, successCriteria: body.successCriteria, nonObjectives: body.nonObjectives ?? [], startingEvidence: body.startingEvidence ?? [], idempotencyKey: body.idempotencyKey! }) }); return; }
      if (request.method === "GET" && /^\/missions\/[^/]+\/completion$/.test(pathname)) {
        const projectId = requiredQuery(url, "projectId");
        const missionId = pathname.split("/")[2]!;
        const packet = [...this.daemon.research.submitted(projectId)].reverse().find((entry) => entry.event.scope.missionId === missionId && (entry.record as { $schema?: unknown }).$schema === "https://nosh.dev/schemas/mission-completion-packet/v1")?.record ?? null;
        this.send(response, 200, { basis: this.daemon.research.missionCompletionBasis(projectId, missionId), packet });
        return;
      }
      if (request.method === "GET" && /^\/missions\/[^/]+$/.test(pathname)) { const missionId = pathname.split("/")[2]!; this.send(response, 200, { mission: this.daemon.research.mission(requiredQuery(url, "projectId"), missionId) }); return; }
      if (request.method === "POST" && /^\/missions\/[^/]+\/steer$/.test(pathname)) { const missionId = pathname.split("/")[2]!; const body = await readJson(request) as { projectId?: string; expectedVersion?: number; message?: string; idempotencyKey?: string }; requiredStrings(body, ["projectId", "message", "idempotencyKey"]); if (!Number.isInteger(body.expectedVersion)) throw new Error("expectedVersion is required"); await this.daemon.steerMission(body.projectId!, missionId, body.expectedVersion!, body.message!, body.idempotencyKey!); this.send(response, 202, { accepted: true }); return; }
      if (request.method === "POST" && /^\/missions\/[^/]+\/control$/.test(pathname)) { const missionId = pathname.split("/")[2]!; const body = await readJson(request) as { projectId?: string; expectedVersion?: number; action?: "pause" | "resume" | "stop"; mode?: "safe" | "checkpoint" | "immediate"; idempotencyKey?: string }; requiredStrings(body, ["projectId", "action", "mode", "idempotencyKey"]); if (!Number.isInteger(body.expectedVersion) || !["pause", "resume", "stop"].includes(body.action!) || !["safe", "checkpoint", "immediate"].includes(body.mode!)) throw new Error("expectedVersion, action, and mode are required"); this.send(response, 202, { mission: await this.daemon.controlMission(body.projectId!, missionId, body.expectedVersion!, body.action!, body.mode!, body.idempotencyKey!) }); return; }
      if (request.method === "POST" && /^\/(missions|directions|autoresearch)\/[^/]+\/start$/.test(pathname)) { const [, family, entityId] = pathname.split("/") as [string, "missions" | "directions" | "autoresearch", string]; const body = await readJson(request) as { projectId?: string; expectedVersion?: number; idempotencyKey?: string }; requiredStrings(body, ["projectId", "idempotencyKey"]); if (!Number.isInteger(body.expectedVersion)) throw new Error("expectedVersion is required"); this.send(response, 202, { [family === "missions" ? "mission" : family === "directions" ? "direction" : "execution"]: this.daemon.research.start(body.projectId!, family, entityId, body.expectedVersion!, body.idempotencyKey!) }); return; }
      if (request.method === "POST" && /^\/missions\/[^/]+\/transition$/.test(pathname)) { const missionId = pathname.split("/")[2]!; const body = await readJson(request) as { projectId?: string; expectedVersion?: number; next?: MissionState; idempotencyKey?: string }; requiredStrings(body, ["projectId", "next", "idempotencyKey"]); if (!Number.isInteger(body.expectedVersion)) throw new Error("expectedVersion is required"); if (body.next === "stopping" || body.next === "pausing") throw new Error(`Use the Mission control action to ${body.next === "stopping" ? "stop" : "pause"} it (TUI: /control <missionId> <version> ${body.next === "stopping" ? "stop" : "pause"}); it also cancels Jobs and ends sessions, which a bare transition would leave running`); this.send(response, 202, { mission: this.daemon.research.transitionMission(body.projectId!, missionId, body.expectedVersion!, body.next!, body.idempotencyKey!) }); return; }
      if (request.method === "POST" && /^\/missions\/[^/]+\/graph$/.test(pathname)) { const missionId = pathname.split("/")[2]!; const body = await readJson(request) as { projectId?: string; expectedVersion?: number; baseGraphVersion?: number; operations?: GraphOperation[]; rationale?: string; evidenceIds?: string[]; idempotencyKey?: string }; requiredStrings(body, ["projectId", "rationale", "idempotencyKey"]); if (!Number.isInteger(body.expectedVersion) || !Number.isInteger(body.baseGraphVersion) || !Array.isArray(body.operations) || !Array.isArray(body.evidenceIds)) throw new Error("expectedVersion, baseGraphVersion, operations, and evidenceIds are required"); this.send(response, 202, { mission: this.daemon.research.mutateMissionGraph(body.projectId!, missionId, body.expectedVersion!, body.baseGraphVersion!, body.operations, body.rationale!, body.evidenceIds, body.idempotencyKey!) }); return; }
      if (request.method === "POST" && /^\/missions\/[^/]+\/nodes\/[^/]+\/transition$/.test(pathname)) { const parts = pathname.split("/"); const missionId = parts[2]!; const nodeId = parts[4]!; const body = await readJson(request) as { projectId?: string; expectedVersion?: number; next?: GraphNode["state"]; lease?: NonNullable<GraphNode["lease"]>; idempotencyKey?: string }; requiredStrings(body, ["projectId", "next", "idempotencyKey"]); if (!Number.isInteger(body.expectedVersion)) throw new Error("expectedVersion is required"); if (!publicGraphNodeState(body.next) || body.lease !== undefined) throw new Error("Public graph transitions allow only ready, blocked, or cancelled without a lease"); this.send(response, 202, { mission: this.daemon.research.transitionMissionNode(body.projectId!, missionId, body.expectedVersion!, nodeId, body.next!, body.idempotencyKey!, undefined, true) }); return; }
      if (request.method === "POST" && /^\/graph-proposals\/[^/]+\/approve$/.test(pathname)) { const proposalId = pathname.split("/")[2]!; const body = await readJson(request) as { projectId?: string; expectedProposalVersion?: number; idempotencyKey?: string }; requiredStrings(body, ["projectId", "idempotencyKey"]); if (!Number.isInteger(body.expectedProposalVersion)) throw new Error("expectedProposalVersion is required"); this.send(response, 202, { proposal: this.daemon.research.approveGraphProposal(body.projectId!, proposalId, body.expectedProposalVersion!, body.idempotencyKey!) }); return; }
      if (request.method === "GET" && pathname === "/graph-proposals") { this.send(response, 200, { proposals: this.daemon.research.graphProposals(requiredQuery(url, "projectId")) }); return; }
      if (request.method === "PUT" && pathname === "/default-model") { const body = await readJson(request) as { model?: unknown }; if (typeof body.model !== "string") throw new Error("model (provider/id[:level]) is required"); this.send(response, 200, { defaultModel: await this.daemon.setDefaultModel(body.model) }); return; }
      if (request.method === "GET" && pathname === "/research-map") { this.send(response, 200, this.daemon.researchMap(requiredQuery(url, "projectId"))); return; }
      if (request.method === "GET" && pathname === "/directions") { this.send(response, 200, { directions: this.daemon.research.directions(requiredQuery(url, "projectId")) }); return; }
      if (request.method === "POST" && pathname === "/directions") { const body = await readJson(request) as { projectId?: string; question?: string; decisionUse?: string; missionId?: string | null; evaluationContract?: JsonValue; idempotencyKey?: string }; requiredStrings(body, ["projectId", "question", "decisionUse", "idempotencyKey"]); this.send(response, 201, { direction: this.daemon.research.createDirection(body.projectId!, { question: body.question!, decisionUse: body.decisionUse!, missionId: body.missionId ?? null, evaluationContract: body.evaluationContract ?? { metrics: [], datasets: [], seeds: [0] }, idempotencyKey: body.idempotencyKey! }) }); return; }
      if (request.method === "POST" && /^\/directions\/[^/]+\/transition$/.test(pathname)) { const directionId = pathname.split("/")[2]!; const body = await readJson(request) as { projectId?: string; expectedVersion?: number; next?: DirectionState; idempotencyKey?: string }; requiredStrings(body, ["projectId", "next", "idempotencyKey"]); if (!Number.isInteger(body.expectedVersion)) throw new Error("expectedVersion is required"); this.send(response, 202, { direction: this.daemon.research.transitionDirection(body.projectId!, directionId, body.expectedVersion!, body.next!, body.idempotencyKey!) }); return; }
      if (request.method === "POST" && /^\/directions\/[^/]+\/baseline$/.test(pathname)) { const directionId = pathname.split("/")[2]!; const body = await readJson(request) as { projectId?: string; expectedVersion?: number; commit?: string; reviewId?: string; evaluationContractHash?: string; idempotencyKey?: string }; requiredStrings(body, ["projectId", "commit", "reviewId", "evaluationContractHash", "idempotencyKey"]); if (!Number.isInteger(body.expectedVersion)) throw new Error("expectedVersion is required"); this.send(response, 202, { direction: this.daemon.research.acceptDirectionBaseline(body.projectId!, directionId, body.expectedVersion!, { commit: body.commit!, reviewId: body.reviewId!, evaluationContractHash: body.evaluationContractHash!, idempotencyKey: body.idempotencyKey! }) }); return; }
      if (request.method === "POST" && /^\/directions\/[^/]+\/nodes\/[^/]+\/transition$/.test(pathname)) { const parts = pathname.split("/"); const directionId = parts[2]!; const nodeId = parts[4]!; const body = await readJson(request) as { projectId?: string; expectedVersion?: number; next?: GraphNode["state"]; lease?: NonNullable<GraphNode["lease"]>; idempotencyKey?: string }; requiredStrings(body, ["projectId", "next", "idempotencyKey"]); if (!Number.isInteger(body.expectedVersion)) throw new Error("expectedVersion is required"); if (!publicGraphNodeState(body.next) || body.lease !== undefined) throw new Error("Public graph transitions allow only ready, blocked, or cancelled without a lease"); this.send(response, 202, { direction: this.daemon.research.transitionDirectionNode(body.projectId!, directionId, body.expectedVersion!, nodeId, body.next!, body.idempotencyKey!, undefined, true) }); return; }
      if (request.method === "GET" && pathname === "/autoresearch") { this.send(response, 200, { executions: this.daemon.research.autoresearch(requiredQuery(url, "projectId")) }); return; }
      if (request.method === "POST" && pathname === "/autoresearch") { const body = await readJson(request) as { projectId?: string; decisionQuestion?: string; directionId?: string | null; missionId?: string | null; familyTags?: string[]; scope?: string[]; evaluationContract?: JsonValue; maximumExperiments?: number; maximumRounds?: number; maximumWallClockSeconds?: number; maximumModelTokens?: number; maximumGpuSeconds?: number; maximumDiskBytes?: number; idempotencyKey?: string }; requiredStrings(body, ["projectId", "decisionQuestion", "idempotencyKey"]); if (body.familyTags && !Array.isArray(body.familyTags) || body.scope && !Array.isArray(body.scope)) throw new Error("familyTags and scope must be arrays"); this.send(response, 201, { execution: this.daemon.research.createAutoresearch(body.projectId!, { decisionQuestion: body.decisionQuestion!, directionId: body.directionId ?? null, missionId: body.missionId ?? null, familyTags: body.familyTags ?? [], scope: body.scope ?? [], ...(body.evaluationContract === undefined ? {} : { evaluationContract: body.evaluationContract }), ...(body.maximumExperiments === undefined ? {} : { maximumExperiments: body.maximumExperiments }), ...(body.maximumRounds === undefined ? {} : { maximumRounds: body.maximumRounds }), ...(body.maximumWallClockSeconds === undefined ? {} : { maximumWallClockSeconds: body.maximumWallClockSeconds }), ...(body.maximumModelTokens === undefined ? {} : { maximumModelTokens: body.maximumModelTokens }), ...(body.maximumGpuSeconds === undefined ? {} : { maximumGpuSeconds: body.maximumGpuSeconds }), ...(body.maximumDiskBytes === undefined ? {} : { maximumDiskBytes: body.maximumDiskBytes }), idempotencyKey: body.idempotencyKey! }) }); return; }
      if (request.method === "POST" && /^\/autoresearch\/[^/]+\/transition$/.test(pathname)) { const autoresearchId = pathname.split("/")[2]!; const body = await readJson(request) as { projectId?: string; expectedVersion?: number; next?: AutoresearchProjection["state"]; idempotencyKey?: string }; requiredStrings(body, ["projectId", "next", "idempotencyKey"]); if (!Number.isInteger(body.expectedVersion)) throw new Error("expectedVersion is required"); this.send(response, 202, { execution: this.daemon.research.transitionAutoresearch(body.projectId!, autoresearchId, body.expectedVersion!, body.next!, body.idempotencyKey!) }); return; }
      if (request.method === "GET" && pathname === "/records") { const projectId = requiredQuery(url, "projectId"); this.send(response, 200, { records: this.daemon.research.records(projectId, url.searchParams.get("schema") ?? undefined) }); return; }
      if (request.method === "GET" && pathname === "/reviews") { this.send(response, 200, { reviews: this.daemon.research.reviews(requiredQuery(url, "projectId")) }); return; }
      if (request.method === "GET" && pathname === "/evidence") { this.send(response, 200, { evidence: this.daemon.research.records(requiredQuery(url, "projectId"), "evidence") }); return; }
      if (request.method === "POST" && pathname === "/evidence") { const body = await readJson(request) as { projectId?: string; evidence?: JsonValue; idempotencyKey?: string }; requiredStrings(body, ["projectId", "idempotencyKey"]); if (!body.evidence || typeof body.evidence !== "object" || Array.isArray(body.evidence)) throw new Error("evidence must be an object"); this.send(response, 201, { event: this.daemon.research.submitUserEvidence(body.projectId!, body.evidence, body.idempotencyKey!) }); return; }
      if (request.method === "GET" && pathname === "/claims") { this.send(response, 200, { claims: this.daemon.research.latestClaims(requiredQuery(url, "projectId")) }); return; }
      if (request.method === "GET" && pathname === "/notification-acks") { this.send(response, 200, { eventIds: this.daemon.notificationAcks(requiredQuery(url, "projectId")) }); return; }
      if (request.method === "POST" && pathname === "/notifications/acknowledge") { const body = await readJson(request) as { projectId?: string; eventIds?: unknown; idempotencyKey?: string }; requiredStrings(body, ["projectId", "idempotencyKey"]); if (!Array.isArray(body.eventIds) || !body.eventIds.every((id) => typeof id === "string")) throw new Error("eventIds must be an array of strings"); this.send(response, 202, { event: this.daemon.acknowledgeNotifications(body.projectId!, body.eventIds, body.idempotencyKey!) }); return; }
      if (request.method === "POST" && pathname === "/claims") { const body = await readJson(request) as { projectId?: string; claim?: JsonValue; idempotencyKey?: string }; requiredStrings(body, ["projectId", "idempotencyKey"]); if (!body.claim || typeof body.claim !== "object" || Array.isArray(body.claim)) throw new Error("claim must be an object"); this.send(response, 201, { event: this.daemon.research.submitUserClaim(body.projectId!, body.claim, body.idempotencyKey!) }); return; }
      if (request.method === "PUT" && /^\/claims\/[^/]+$/.test(pathname)) { const body = await readJson(request) as { projectId?: string; expectedClaimVersion?: number; changes?: JsonValue; idempotencyKey?: string }; requiredStrings(body, ["projectId", "idempotencyKey"]); if (!Number.isInteger(body.expectedClaimVersion) || !body.changes || typeof body.changes !== "object" || Array.isArray(body.changes)) throw new Error("expectedClaimVersion and changes object are required"); this.send(response, 202, { event: this.daemon.research.editUserClaim(body.projectId!, pathname.split("/")[2]!, body.expectedClaimVersion!, body.changes, body.idempotencyKey!) }); return; }
      if (request.method === "GET" && pathname === "/paper") { this.send(response, 200, this.daemon.research.readPaper(requiredQuery(url, "projectId"))); return; }
      if (request.method === "PUT" && pathname === "/paper") { const body = await readJson(request) as { projectId?: string; markdown?: unknown; bibliography?: unknown; expected?: { markdownHash?: string; bibliographyHash?: string; version?: string }; idempotencyKey?: string }; requiredStrings(body, ["projectId", "idempotencyKey"]); if (typeof body.markdown !== "string" || typeof body.bibliography !== "string" || !body.expected || typeof body.expected.markdownHash !== "string" || typeof body.expected.bibliographyHash !== "string" || typeof body.expected.version !== "string") throw new Error("paper content and expected paper hashes and version are required"); this.send(response, 202, { event: this.daemon.research.savePaper(body.projectId!, body.markdown, body.bibliography, body.expected as { markdownHash: string; bibliographyHash: string; version: string }, body.idempotencyKey!) }); return; }
      if (request.method === "POST" && pathname === "/paper/export") { const body = await readJson(request) as { projectId?: string }; requiredStrings(body, ["projectId"]); this.send(response, 200, this.daemon.research.exportPaper(body.projectId!)); return; }
      if (request.method === "GET" && pathname === "/backups") { this.send(response, 200, { backups: this.daemon.backups(requiredQuery(url, "projectId")) }); return; }
      if (request.method === "POST" && pathname === "/backups") { const body = await readJson(request) as { projectId?: string }; requiredStrings(body, ["projectId"]); this.send(response, 201, { backup: await this.daemon.createBackup(body.projectId!) }); return; }
      if (request.method === "POST" && /^\/backups\/[^/]+\/restore$/.test(pathname)) { const body = await readJson(request) as { projectId?: string }; requiredStrings(body, ["projectId"]); this.daemon.scheduleBackupRestore(body.projectId!, decodeURIComponent(pathname.split("/")[2]!)); this.send(response, 202, { scheduled: true, stopping: true }); setImmediate(() => this.daemon.events.emit("shutdown-request")); return; }
      if (request.method === "GET" && pathname === "/activity") { this.send(response, 200, { active: this.daemon.activity() }); return; }
      if (request.method === "POST" && pathname === "/shutdown") { if (this.daemon.backupMaintenanceInProgress()) throw new Error("Shutdown is unavailable while a backup is in progress"); this.send(response, 202, { stopping: true }); setImmediate(() => this.daemon.events.emit("shutdown-request")); return; }
      if (request.method === "GET" && pathname === "/agents") {
        const projectId = url.searchParams.get("projectId");
        this.send(response, 200, { agents: this.daemon.agents.inspect().filter((agent) => !projectId || agent.projectId === projectId) });
        return;
      }
      if (request.method === "POST" && pathname === "/agents") {
        this.send(response, 201, { agent: await this.daemon.startAgent(await readJson(request) as PiSessionOptions) });
        return;
      }
      if (request.method === "POST" && pathname.startsWith("/agents/")) {
        const parts = pathname.split("/");
        const agentId = parts[2]; const action = parts[3];
        if (!agentId || !action) throw new Error("agent and action are required");
        const agent = this.daemon.agents.inspect().find((entry) => entry.agentId === agentId);
        if (!agent) throw new Error("unknown agent");
        this.daemon.assertProjectWritable(agent.projectId);
        const body = await readJson(request) as { message?: string; instructions?: string };
        if (action === "prompt" && body.message) await this.daemon.agents.prompt(agentId, body.message);
        else if (action === "steer" && body.message) await this.daemon.agents.steer(agentId, body.message);
        else if (action === "compact") await this.daemon.agents.compact(agentId, body.instructions);
        else if (action === "abort") await this.daemon.agents.abort(agentId);
        else if (action === "stop") this.daemon.agents.stop(agentId);
        else throw new Error("unsupported agent action");
        this.send(response, 202, { accepted: true });
        return;
      }
      if (request.method === "GET" && pathname === "/jobs") {
        const projectId = url.searchParams.get("projectId");
        this.send(response, 200, { jobs: this.daemon.jobs.list().filter((job) => !projectId || job.projectId === projectId) });
        return;
      }
      if (request.method === "POST" && pathname === "/jobs") {
        this.send(response, 201, { job: this.daemon.startJob(await readJson(request) as JobSpec) });
        return;
      }
      if (request.method === "GET" && pathname.startsWith("/jobs/")) {
        const parts = pathname.split("/"); const jobId = parts[2]; const action = parts[3]; const projectId = requiredQuery(url, "projectId");
        if (!jobId) throw new Error("job is required");
        const job = this.daemon.jobs.get(jobId); if (job.projectId !== projectId) throw new Error("Job is outside the requested Project");
        if (action === "tail") {
          const stream = url.searchParams.get("stream") === "stderr" ? "stderr" : "stdout";
          this.send(response, 200, { text: this.daemon.jobs.tail(jobId, stream) });
        } else this.send(response, 200, { job, resources: await this.daemon.jobs.resourceSnapshot(jobId) });
        return;
      }
      if (request.method === "POST" && pathname.startsWith("/jobs/")) {
        const parts = pathname.split("/"); const jobId = parts[2]; const action = parts[3]; const body = await readJson(request) as { projectId?: string; idempotencyKey?: string };
        if (!jobId || !action) throw new Error("job and action are required"); requiredStrings(body, ["projectId", "idempotencyKey"]);
        if (action === "checkpoint" || action === "cancel") this.send(response, 202, { job: this.daemon.controlJob(body.projectId!, jobId, action, body.idempotencyKey!) });
        else throw new Error("unsupported job action");
        return;
      }
      this.send(response, 404, { error: "not_found" });
    } catch (error) {
      this.send(response, 400, { error: error instanceof Error ? error.message : "invalid_request" });
    }
  }

  private handleUpgrade(request: IncomingMessage, socket: Duplex, head: Buffer): void {
    const url = new URL(request.url ?? "/", "http://localhost");
    const pathname = apiPath(url.pathname);
    if (!this.authorized(request)) { socket.destroy(); return; }
    const projectId = url.searchParams.get("projectId");
    const after = Number(url.searchParams.get("after") ?? "0");
    if (pathname !== "/events" || !projectId || !Number.isSafeInteger(after) || after < 0) {
      socket.destroy();
      return;
    }

    this.websocket.handleUpgrade(request, socket, head, (client) => this.subscribe(client, projectId, after));
  }

  private subscribe(client: WebSocket, projectId: string, after: number): void {
    // Durable events stay in SQLite until sent. Only transient events enter memory.
    // A slow consumer is disconnected explicitly and must reconnect from its own
    // last received durable cursor and refresh queries (transient events are not replayable).
    let cursor = after;
    let busy = false;
    let stopped = false;
    let transientBytes = 0;
    const transient: string[] = [];
    const maximumBytes = 1024 * 1024;
    const stop = (code: number, reason: string) => {
      if (stopped) return;
      stopped = true;
      this.daemon.events.off("event", listener);
      transient.length = 0;
      client.close(code, reason);
      const timer = setTimeout(() => client.terminate(), 1000);
      timer.unref();
      client.once("close", () => clearTimeout(timer));
    };
    const pump = () => {
      if (busy || stopped || client.readyState !== client.OPEN) return;
      busy = true;
      let page: ReturnType<NoshDaemon["replayPage"]>;
      try { page = this.daemon.replayPage(projectId, cursor, 1); }
      catch { stop(1011, "Replay failed; reconnect from last received cursor"); return; }
      const event = page.events[0];
      const message = event ? JSON.stringify(event) : transient.shift();
      if (!event && message) transientBytes -= Buffer.byteLength(message);
      if (!message) { busy = false; return; }
      if (Buffer.byteLength(message) + client.bufferedAmount + transientBytes > maximumBytes) {
        stop(1013, "Slow consumer; reconnect from last received cursor; refresh queries"); return;
      }
      const timeout = setTimeout(() => stop(1013, "Slow consumer; reconnect from last received cursor; refresh queries"), 5000);
      timeout.unref();
      client.send(message, (error) => {
        clearTimeout(timeout);
        if (error) { stop(1011, "Send failed; reconnect from last received cursor"); return; }
        if (event?.sequence !== null && event?.sequence !== undefined) cursor = event.sequence;
        busy = false;
        setImmediate(pump);
      });
    };
    const listener = (event: { scope: { projectId: string }; sequence: number | null }) => {
      if (stopped || event.scope.projectId !== projectId) return;
      if (event.sequence === null) {
        const message = JSON.stringify(event);
        transientBytes += Buffer.byteLength(message);
        if (transient.length >= 300 || transientBytes + client.bufferedAmount > maximumBytes) {
          stop(1013, "Slow consumer; reconnect from last received cursor; refresh queries"); return;
        }
        transient.push(message);
      }
      pump();
    };
    this.daemon.events.on("event", listener);
    client.once("close", () => { stopped = true; transient.length = 0; this.daemon.events.off("event", listener); });
    client.on("error", () => stop(1011, "Socket failed; reconnect from last received cursor"));
    pump();
  }

  private authorized(request: IncomingMessage): boolean {
    if (automaticallyTrusted(request)) return true;
    const match = /^Bearer (.+)$/.exec(request.headers.authorization ?? "");
    const protocol = request.headers["sec-websocket-protocol"]?.split(",").map((value) => value.trim()).find((value) => value.startsWith("auth."));
    const token = match?.[1] ?? protocol?.slice(5); if (!token) return false; const expiresAt = this.sessions.get(token); if (!expiresAt) return false; if (expiresAt <= Date.now()) { this.sessions.delete(token); return false; } return true;
  }
  private bootstrapAuthorized(request: IncomingMessage): boolean { return this.daemon.authenticate(/^Bearer (.+)$/.exec(request.headers.authorization ?? "")?.[1]); }

  private send(response: ServerResponse, status: number, body: unknown): void {
    response.writeHead(status, { "content-type": "application/json; charset=utf-8", "cache-control": "no-store" });
    response.end(JSON.stringify(body));
  }
}

function apiPath(pathname: string): string {
  return pathname === "/api" ? "/" : pathname.startsWith("/api/") ? pathname.slice(4) : pathname;
}

function automaticallyTrusted(request: Pick<IncomingMessage, "headers" | "socket">): boolean {
  if (!isLoopbackAddress(request.socket.remoteAddress)) return false;
  // DNS rebinding reaches loopback under an attacker-controlled Host name.
  if (!loopbackHost(request.headers.host)) return false;
  const origin = request.headers.origin;
  if (!origin) {
    const fetchSite = request.headers["sec-fetch-site"];
    return !fetchSite || fetchSite === "same-origin" || fetchSite === "none";
  }
  const host = request.headers.host;
  if (!host) return false;
  const scheme = (request.socket as { encrypted?: boolean }).encrypted ? "https" : "http";
  try {
    return new URL(origin).origin === new URL(`${scheme}://${host}`).origin;
  } catch {
    return false;
  }
}

function loopbackHost(host: string | undefined): boolean {
  if (!host) return false;
  try {
    const name = new URL(`http://${host}`).hostname;
    return name === "localhost" || name === "[::1]" || isIP(name) === 4 && name.split(".")[0] === "127";
  } catch {
    return false;
  }
}

export function isLoopbackAddress(address: string | undefined): boolean {
  if (!address || isIP(address) === 0) return false;
  if (isIP(address) === 4) return address.split(".")[0] === "127";
  const words = ipv6Words(address);
  if (!words) return false;
  if (words.slice(0, 7).every((word) => word === 0) && words[7] === 1) return true;
  return words.slice(0, 5).every((word) => word === 0) && words[5] === 0xffff && (words[6]! >> 8) === 127;
}

function ipv6Words(address: string): number[] | undefined {
  const parts = address.toLowerCase().split("::");
  if (parts.length > 2) return undefined;
  const parse = (part: string): number[] | undefined => part ? part.split(":").flatMap((value) => {
    if (value.includes(".")) {
      const octets = value.split(".").map(Number);
      if (octets.length !== 4 || octets.some((octet) => !Number.isInteger(octet) || octet < 0 || octet > 255)) return [];
      return [(octets[0]! << 8) | octets[1]!, (octets[2]! << 8) | octets[3]!];
    }
    if (!/^[0-9a-f]{1,4}$/.test(value)) return [];
    return [Number.parseInt(value, 16)];
  }) : [];
  const left = parse(parts[0]!);
  const right = parts.length === 2 ? parse(parts[1]!) : [];
  if (!left || !right || parts.length === 1 && left.length !== 8 || parts.length === 2 && left.length + right.length >= 8) return undefined;
  return [...left, ...Array.from({ length: 8 - left.length - right.length }, () => 0), ...right];
}

async function readJson(request: IncomingMessage): Promise<unknown> {
  const chunks: Buffer[] = [];
  let size = 0;
  for await (const chunk of request) {
    const bytes = Buffer.from(chunk);
    size += bytes.length;
    if (size > 3_000_000) throw new Error("request body exceeds 3 MB");
    chunks.push(bytes);
  }
  return JSON.parse(Buffer.concat(chunks).toString("utf8"));
}

function requiredQuery(url: URL, name: string): string { const value = url.searchParams.get(name); if (!value) throw new Error(`${name} is required`); return value; }

export function parseModelSelection(value: unknown, topLevelThinkingLevel?: unknown): ModelSelection | undefined {
  if (value === undefined) {
    if (topLevelThinkingLevel !== undefined) throw new Error("thinkingLevel requires a selected Pi model");
    return undefined;
  }
  const parsed = modelSelectionSchema.safeParse(value);
  if (!parsed.success) throw new Error(`Pi model selection must include provider and id, with thinkingLevel one of ${thinkingLevelSchema.options.join(", ")}`);
  if (topLevelThinkingLevel === undefined) return parsed.data;
  const topLevel = thinkingLevelSchema.safeParse(topLevelThinkingLevel);
  if (!topLevel.success) throw new Error(`thinkingLevel must be one of ${thinkingLevelSchema.options.join(", ")}`);
  if (parsed.data.thinkingLevel !== undefined && parsed.data.thinkingLevel !== topLevel.data) throw new Error("Nested and top-level thinkingLevel selections must match");
  return { ...parsed.data, thinkingLevel: topLevel.data };
}

function requiredStrings(value: object, names: string[], allowEmptyName?: string): void { const record = value as Record<string, unknown>; for (const name of names) if (typeof record[name] !== "string" || (name !== allowEmptyName && !(record[name] as string).trim()) || (name === "idempotencyKey" && (record[name] as string).length < 16)) throw new Error(name === "idempotencyKey" && typeof record[name] === "string" && (record[name] as string).trim() ? "idempotencyKey must be at least 16 characters" : `${name} is required`); }
function publicGraphNodeState(value: unknown): value is "ready" | "blocked" | "cancelled" { return value === "ready" || value === "blocked" || value === "cancelled"; }
