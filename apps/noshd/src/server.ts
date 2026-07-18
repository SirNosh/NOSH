import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import { randomBytes } from "node:crypto";
import type { Duplex } from "node:stream";
import { URL } from "node:url";
import { existsSync, readFileSync, statSync } from "node:fs";
import { extname, relative, resolve } from "node:path";
import { WebSocketServer, type WebSocket } from "ws";
import { appendEventCommandSchema } from "@nosh/wire";
import type { PiSessionOptions } from "@nosh/pi-adapter";
import type { JobSpec } from "@nosh/jobs";
import { NoshDaemon } from "./daemon.js";
import type { RemoteControl } from "./remote-control.js";
import { TerminalSessions } from "./terminal.js";

type ServerOptions = { host?: "127.0.0.1" | "::1"; port: number };

export class LocalApiServer {
  private readonly http: Server;
  private readonly websocket = new WebSocketServer({ noServer: true });
  private readonly sessions = new Map<string, number>();
  private readonly terminals = new TerminalSessions();

  constructor(private readonly daemon: NoshDaemon, private readonly publicDirectory?: string, private readonly remote?: RemoteControl) {
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
    this.terminals.closeAll();
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
    if (request.method === "GET" && !url.pathname.startsWith("/api/") && this.serveStatic(url.pathname, response)) return;
    if (request.method === "POST" && pathname === "/session") { if (!this.bootstrapAuthorized(request)) { this.send(response, 401, { error: "unauthorized" }); return; } const token = randomBytes(32).toString("base64url"); const expiresAt = Date.now() + 15 * 60_000; this.sessions.set(token, expiresAt); this.send(response, 201, { token, expiresAt: new Date(expiresAt).toISOString() }); return; }
    if (!this.authorized(request)) {
      this.send(response, 401, { error: "unauthorized" });
      return;
    }

    try {
      if (request.method === "GET" && pathname === "/projects") {
        this.send(response, 200, { projects: this.daemon.projects() });
        return;
      }
      if (request.method === "GET" && pathname === "/models") { this.send(response, 200, { models: await this.daemon.agents.availableModels() }); return; }
      if (request.method === "POST" && pathname === "/terminals") { const body = await readJson(request) as { projectId?: string; profile?: "powershell" | "wsl" }; requiredStrings(body, ["projectId"]); if (body.profile && !["powershell", "wsl"].includes(body.profile)) throw new Error("profile must be powershell or wsl"); const project = this.daemon.projects().find((item) => item.projectId === body.projectId); if (!project) throw new Error("Project is not registered on this host"); this.send(response, 201, { terminal: this.terminals.create(project.projectId, project.repositoryRoot, body.profile ?? "powershell") }); return; }
      if (request.method === "GET" && /^\/terminals\/[^/]+$/.test(pathname)) { this.send(response, 200, { terminal: this.terminals.get(pathname.split("/")[2]!) }); return; }
      if (request.method === "DELETE" && /^\/terminals\/[^/]+$/.test(pathname)) { this.terminals.close(pathname.split("/")[2]!); this.send(response, 202, { closed: true }); return; }
      if (request.method === "POST" && pathname === "/projects") {
        const project = await readJson(request);
        this.send(response, 201, { project: this.daemon.registerProject(project as { projectId: string; repositoryRoot: string; databasePath: string }) });
        return;
      }
      if (request.method === "POST" && pathname === "/projects/open") { const body = await readJson(request) as { path?: string; createRepository?: boolean; workingTitle?: string; northStarQuestion?: string; contributionType?: string; maximumGpuHours?: number; maximumDiskBytes?: number }; requiredStrings(body, ["path", "workingTitle", "northStarQuestion", "contributionType"]); if (typeof body.createRepository !== "boolean" || typeof body.maximumGpuHours !== "number" || body.maximumGpuHours < 0 || typeof body.maximumDiskBytes !== "number" || body.maximumDiskBytes < 1) throw new Error("repository mode and compute envelope are required"); this.send(response, 201, { project: this.daemon.initializeProject({ path: body.path!, createRepository: body.createRepository, workingTitle: body.workingTitle!, northStarQuestion: body.northStarQuestion!, contributionType: body.contributionType!, maximumGpuHours: body.maximumGpuHours, maximumDiskBytes: body.maximumDiskBytes }) }); return; }
      if (request.method === "GET" && pathname === "/events") {
        const projectId = url.searchParams.get("projectId");
        const after = Number(url.searchParams.get("after") ?? "0");
        if (!projectId || !Number.isSafeInteger(after) || after < 0) throw new Error("projectId and a non-negative after cursor are required");
        this.send(response, 200, { events: this.daemon.replay(projectId, after) });
        return;
      }
      if (request.method === "POST" && pathname === "/commands") {
        const command = appendEventCommandSchema.parse(await readJson(request));
        this.send(response, 202, this.daemon.appendCommand(command));
        return;
      }
      if (request.method === "POST" && pathname === "/chat") { const body = await readJson(request) as { projectId?: string; message?: string; idempotencyKey?: string; model?: { provider?: string; id?: string } }; requiredStrings(body, ["projectId", "message", "idempotencyKey"]); if (body.model && (typeof body.model.provider !== "string" || typeof body.model.id !== "string")) throw new Error("model provider and id are required"); this.send(response, 202, await this.daemon.chat(body.projectId!, body.message!, body.idempotencyKey!, body.model as { provider: string; id: string } | undefined)); return; }
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
      if (request.method === "GET" && /^\/missions\/[^/]+$/.test(pathname)) { const missionId = pathname.split("/")[2]!; this.send(response, 200, { mission: this.daemon.research.mission(requiredQuery(url, "projectId"), missionId) }); return; }
      if (request.method === "POST" && /^\/missions\/[^/]+\/steer$/.test(pathname)) { const missionId = pathname.split("/")[2]!; const body = await readJson(request) as { projectId?: string; expectedVersion?: number; message?: string; idempotencyKey?: string }; requiredStrings(body, ["projectId", "message", "idempotencyKey"]); if (!Number.isInteger(body.expectedVersion)) throw new Error("expectedVersion is required"); await this.daemon.steerMission(body.projectId!, missionId, body.expectedVersion!, body.message!, body.idempotencyKey!); this.send(response, 202, { accepted: true }); return; }
      if (request.method === "POST" && /^\/missions\/[^/]+\/control$/.test(pathname)) { const missionId = pathname.split("/")[2]!; const body = await readJson(request) as { projectId?: string; expectedVersion?: number; action?: "pause" | "resume" | "stop"; mode?: "safe" | "checkpoint" | "immediate"; idempotencyKey?: string }; requiredStrings(body, ["projectId", "action", "mode", "idempotencyKey"]); if (!Number.isInteger(body.expectedVersion) || !["pause", "resume", "stop"].includes(body.action!) || !["safe", "checkpoint", "immediate"].includes(body.mode!)) throw new Error("expectedVersion, action, and mode are required"); this.send(response, 202, { mission: await this.daemon.controlMission(body.projectId!, missionId, body.expectedVersion!, body.action!, body.mode!, body.idempotencyKey!) }); return; }
      if (request.method === "POST" && /^\/missions\/[^/]+\/transition$/.test(pathname)) { const missionId = pathname.split("/")[2]!; const body = await readJson(request) as { projectId?: string; expectedVersion?: number; next?: import("@nosh/graph").MissionState; idempotencyKey?: string }; requiredStrings(body, ["projectId", "next", "idempotencyKey"]); if (!Number.isInteger(body.expectedVersion)) throw new Error("expectedVersion is required"); this.send(response, 202, { mission: this.daemon.research.transitionMission(body.projectId!, missionId, body.expectedVersion!, body.next!, body.idempotencyKey!) }); return; }
      if (request.method === "POST" && /^\/missions\/[^/]+\/graph$/.test(pathname)) { const missionId = pathname.split("/")[2]!; const body = await readJson(request) as { projectId?: string; expectedVersion?: number; baseGraphVersion?: number; operations?: import("@nosh/graph").GraphOperation[]; rationale?: string; evidenceIds?: string[]; idempotencyKey?: string }; requiredStrings(body, ["projectId", "rationale", "idempotencyKey"]); if (!Number.isInteger(body.expectedVersion) || !Number.isInteger(body.baseGraphVersion) || !Array.isArray(body.operations) || !Array.isArray(body.evidenceIds)) throw new Error("expectedVersion, baseGraphVersion, operations, and evidenceIds are required"); this.send(response, 202, { mission: this.daemon.research.mutateMissionGraph(body.projectId!, missionId, body.expectedVersion!, body.baseGraphVersion!, body.operations, body.rationale!, body.evidenceIds, body.idempotencyKey!) }); return; }
      if (request.method === "POST" && /^\/missions\/[^/]+\/nodes\/[^/]+\/transition$/.test(pathname)) { const parts = pathname.split("/"); const missionId = parts[2]!; const nodeId = parts[4]!; const body = await readJson(request) as { projectId?: string; expectedVersion?: number; next?: import("@nosh/graph").GraphNode["state"]; lease?: NonNullable<import("@nosh/graph").GraphNode["lease"]>; idempotencyKey?: string }; requiredStrings(body, ["projectId", "next", "idempotencyKey"]); if (!Number.isInteger(body.expectedVersion)) throw new Error("expectedVersion is required"); this.send(response, 202, { mission: this.daemon.research.transitionMissionNode(body.projectId!, missionId, body.expectedVersion!, nodeId, body.next!, body.idempotencyKey!, body.lease) }); return; }
      if (request.method === "GET" && pathname === "/directions") { this.send(response, 200, { directions: this.daemon.research.directions(requiredQuery(url, "projectId")) }); return; }
      if (request.method === "POST" && pathname === "/directions") { const body = await readJson(request) as { projectId?: string; question?: string; decisionUse?: string; missionId?: string | null; evaluationContract?: import("@nosh/wire").JsonValue; idempotencyKey?: string }; requiredStrings(body, ["projectId", "question", "decisionUse", "idempotencyKey"]); this.send(response, 201, { direction: this.daemon.research.createDirection(body.projectId!, { question: body.question!, decisionUse: body.decisionUse!, missionId: body.missionId ?? null, evaluationContract: body.evaluationContract ?? { metrics: [], datasets: [], seeds: [0] }, idempotencyKey: body.idempotencyKey! }) }); return; }
      if (request.method === "POST" && /^\/directions\/[^/]+\/transition$/.test(pathname)) { const directionId = pathname.split("/")[2]!; const body = await readJson(request) as { projectId?: string; expectedVersion?: number; next?: import("@nosh/graph").DirectionState; idempotencyKey?: string }; requiredStrings(body, ["projectId", "next", "idempotencyKey"]); if (!Number.isInteger(body.expectedVersion)) throw new Error("expectedVersion is required"); this.send(response, 202, { direction: this.daemon.research.transitionDirection(body.projectId!, directionId, body.expectedVersion!, body.next!, body.idempotencyKey!) }); return; }
      if (request.method === "POST" && /^\/directions\/[^/]+\/baseline$/.test(pathname)) { const directionId = pathname.split("/")[2]!; const body = await readJson(request) as { projectId?: string; expectedVersion?: number; commit?: string; reviewId?: string; evaluationContractHash?: string; idempotencyKey?: string }; requiredStrings(body, ["projectId", "commit", "reviewId", "evaluationContractHash", "idempotencyKey"]); if (!Number.isInteger(body.expectedVersion)) throw new Error("expectedVersion is required"); this.send(response, 202, { direction: this.daemon.research.acceptDirectionBaseline(body.projectId!, directionId, body.expectedVersion!, { commit: body.commit!, reviewId: body.reviewId!, evaluationContractHash: body.evaluationContractHash!, idempotencyKey: body.idempotencyKey! }) }); return; }
      if (request.method === "POST" && /^\/directions\/[^/]+\/nodes\/[^/]+\/transition$/.test(pathname)) { const parts = pathname.split("/"); const directionId = parts[2]!; const nodeId = parts[4]!; const body = await readJson(request) as { projectId?: string; expectedVersion?: number; next?: import("@nosh/graph").GraphNode["state"]; lease?: NonNullable<import("@nosh/graph").GraphNode["lease"]>; idempotencyKey?: string }; requiredStrings(body, ["projectId", "next", "idempotencyKey"]); if (!Number.isInteger(body.expectedVersion)) throw new Error("expectedVersion is required"); this.send(response, 202, { direction: this.daemon.research.transitionDirectionNode(body.projectId!, directionId, body.expectedVersion!, nodeId, body.next!, body.idempotencyKey!, body.lease) }); return; }
      if (request.method === "GET" && pathname === "/autoresearch") { this.send(response, 200, { executions: this.daemon.research.autoresearch(requiredQuery(url, "projectId")) }); return; }
      if (request.method === "POST" && pathname === "/autoresearch") { const body = await readJson(request) as { projectId?: string; decisionQuestion?: string; directionId?: string | null; missionId?: string | null; familyTags?: string[]; scope?: string[]; evaluationContract?: import("@nosh/wire").JsonValue; maximumExperiments?: number; maximumRounds?: number; maximumWallClockSeconds?: number; maximumModelTokens?: number; maximumGpuSeconds?: number; maximumDiskBytes?: number; idempotencyKey?: string }; requiredStrings(body, ["projectId", "decisionQuestion", "idempotencyKey"]); if (body.familyTags && !Array.isArray(body.familyTags) || body.scope && !Array.isArray(body.scope)) throw new Error("familyTags and scope must be arrays"); this.send(response, 201, { execution: this.daemon.research.createAutoresearch(body.projectId!, { decisionQuestion: body.decisionQuestion!, directionId: body.directionId ?? null, missionId: body.missionId ?? null, familyTags: body.familyTags ?? [], scope: body.scope ?? [], ...(body.evaluationContract === undefined ? {} : { evaluationContract: body.evaluationContract }), ...(body.maximumExperiments === undefined ? {} : { maximumExperiments: body.maximumExperiments }), ...(body.maximumRounds === undefined ? {} : { maximumRounds: body.maximumRounds }), ...(body.maximumWallClockSeconds === undefined ? {} : { maximumWallClockSeconds: body.maximumWallClockSeconds }), ...(body.maximumModelTokens === undefined ? {} : { maximumModelTokens: body.maximumModelTokens }), ...(body.maximumGpuSeconds === undefined ? {} : { maximumGpuSeconds: body.maximumGpuSeconds }), ...(body.maximumDiskBytes === undefined ? {} : { maximumDiskBytes: body.maximumDiskBytes }), idempotencyKey: body.idempotencyKey! }) }); return; }
      if (request.method === "POST" && /^\/autoresearch\/[^/]+\/transition$/.test(pathname)) { const autoresearchId = pathname.split("/")[2]!; const body = await readJson(request) as { projectId?: string; expectedVersion?: number; next?: import("./research-control.js").AutoresearchProjection["state"]; idempotencyKey?: string }; requiredStrings(body, ["projectId", "next", "idempotencyKey"]); if (!Number.isInteger(body.expectedVersion)) throw new Error("expectedVersion is required"); this.send(response, 202, { execution: this.daemon.research.transitionAutoresearch(body.projectId!, autoresearchId, body.expectedVersion!, body.next!, body.idempotencyKey!) }); return; }
      if (request.method === "GET" && pathname === "/records") { const projectId = requiredQuery(url, "projectId"); this.send(response, 200, { records: this.daemon.research.records(projectId, url.searchParams.get("schema") ?? undefined) }); return; }
      if (request.method === "GET" && pathname === "/paper") { this.send(response, 200, this.daemon.research.readPaper(requiredQuery(url, "projectId"))); return; }
      if (request.method === "PUT" && pathname === "/paper") { const body = await readJson(request) as { projectId?: string; markdown?: string; idempotencyKey?: string }; requiredStrings(body, ["projectId", "markdown", "idempotencyKey"], "markdown"); this.send(response, 202, { event: this.daemon.research.savePaper(body.projectId!, body.markdown!, body.idempotencyKey!) }); return; }
      if (request.method === "POST" && pathname === "/paper/export") { const body = await readJson(request) as { projectId?: string }; requiredStrings(body, ["projectId"]); this.send(response, 200, this.daemon.research.exportPaper(body.projectId!)); return; }
      if (request.method === "POST" && pathname === "/shutdown") { this.send(response, 202, { stopping: true }); setImmediate(() => this.daemon.events.emit("shutdown-request")); return; }
      if (request.method === "POST" && pathname === "/records") {
        const body = await readJson(request) as { tool?: unknown; projectId?: unknown; attemptKey?: unknown; record?: unknown };
        if (typeof body.tool !== "string" || typeof body.projectId !== "string" || typeof body.attemptKey !== "string") throw new Error("tool, projectId, and attemptKey are required");
        const result = await this.daemon.submitTool(body.tool, body.projectId, body.attemptKey, body.record) as { accepted: boolean };
        this.send(response, result.accepted ? 202 : 422, result);
        return;
      }
      if (request.method === "GET" && pathname === "/agents") {
        this.send(response, 200, { agents: this.daemon.agents.inspect() });
        return;
      }
      if (request.method === "POST" && pathname === "/agents") {
        this.send(response, 201, { agent: await this.daemon.startAgent(await readJson(request) as PiSessionOptions) });
        return;
      }
      if (request.method === "POST" && pathname.startsWith("/agents/")) {
        const parts = pathname.split("/");
        const agentId = parts[2];
        const action = parts[3];
        if (!agentId || !action) throw new Error("agent and action are required");
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
        this.send(response, 200, { jobs: this.daemon.jobs.list() });
        return;
      }
      if (request.method === "GET" && pathname === "/remote") {
        this.send(response, 200, this.remote ? { configured: true, ...this.remote.status() } : { configured: false });
        return;
      }
      if (request.method === "GET" && pathname === "/remote/commands/unresolved") {
        this.send(response, 200, { commands: this.daemon.unresolvedRemoteCommands(requiredQuery(url, "projectId")) }); return;
      }
      if (request.method === "POST" && /^\/remote\/commands\/[^/]+\/resolve$/.test(pathname)) {
        const commandId = pathname.split("/")[3]!; const body = await readJson(request) as { projectId?: string; outcome?: "applied" | "not_applied"; note?: string; idempotencyKey?: string }; requiredStrings(body, ["projectId", "outcome", "note", "idempotencyKey"]); if (!["applied", "not_applied"].includes(body.outcome!)) throw new Error("outcome must be applied or not_applied"); this.send(response, 202, { event: this.daemon.resolveRemoteCommand(body.projectId!, commandId, body.outcome!, body.note!, body.idempotencyKey!) }); return;
      }
      if (request.method === "POST" && pathname === "/remote/pairings") {
        if (!this.remote) throw new Error("Remote control is not configured"); this.send(response, 201, await this.remote.pairing.create()); return;
      }
      if (request.method === "POST" && pathname === "/remote/pairings/approve") {
        if (!this.remote) throw new Error("Remote control is not configured"); const body = await readJson(request) as { capability?: string; verificationCode?: string; permissions?: string[] }; if (!body.capability || !body.verificationCode || !Array.isArray(body.permissions)) throw new Error("capability, verificationCode, and permissions are required"); this.send(response, 202, await this.remote.pairing.approve(body.capability, body.verificationCode, body.permissions)); return;
      }
      if (request.method === "POST" && pathname === "/remote/revoke") {
        if (!this.remote) throw new Error("Remote control is not configured"); const body = await readJson(request) as { deviceId?: string }; if (!body.deviceId) throw new Error("deviceId is required"); this.send(response, 202, { revoked: true, ...await this.remote.revoke(body.deviceId) }); return;
      }
      if (request.method === "POST" && pathname === "/jobs") {
        this.send(response, 201, { job: this.daemon.startJob(await readJson(request) as JobSpec) });
        return;
      }
      if (request.method === "GET" && pathname.startsWith("/jobs/")) {
        const parts = pathname.split("/"); const jobId = parts[2]; const action = parts[3];
        if (!jobId) throw new Error("job is required");
        if (action === "tail") {
          const stream = url.searchParams.get("stream") === "stderr" ? "stderr" : "stdout";
          this.send(response, 200, { text: this.daemon.jobs.tail(jobId, stream) });
        } else this.send(response, 200, { job: this.daemon.jobs.get(jobId), resources: this.daemon.jobs.resourceSnapshot(jobId) });
        return;
      }
      if (request.method === "POST" && pathname.startsWith("/jobs/")) {
        const parts = pathname.split("/"); const jobId = parts[2]; const action = parts[3];
        if (!jobId || !action) throw new Error("job and action are required");
        if (action === "checkpoint") this.send(response, 202, { job: this.daemon.jobs.checkpoint(jobId) });
        else if (action === "cancel") this.send(response, 202, { job: this.daemon.jobs.cancel(jobId) });
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
    if (/^\/terminals\/[^/]+\/stream$/.test(pathname)) { const terminalId = pathname.split("/")[2]!; try { this.terminals.get(terminalId); } catch { socket.destroy(); return; } this.websocket.handleUpgrade(request, socket, head, (client) => this.attachTerminal(client, terminalId)); return; }
    const projectId = url.searchParams.get("projectId");
    const after = Number(url.searchParams.get("after") ?? "0");
    if (pathname !== "/events" || !projectId || !Number.isSafeInteger(after) || after < 0) {
      socket.destroy();
      return;
    }

    this.websocket.handleUpgrade(request, socket, head, (client) => this.subscribe(client, projectId, after));
  }

  private attachTerminal(client: WebSocket, terminalId: string): void {
    const detach = this.terminals.attach(terminalId, (message) => { if (client.readyState === client.OPEN) client.send(JSON.stringify(message)); });
    client.on("message", (raw) => { try { const message = JSON.parse(raw.toString()) as { type?: string; data?: string; cols?: number; rows?: number }; if (message.type === "input" && typeof message.data === "string") this.terminals.write(terminalId, message.data); else if (message.type === "resize") this.terminals.resize(terminalId, message.cols!, message.rows!); } catch (error) { if (client.readyState === client.OPEN) client.send(JSON.stringify({ type: "error", message: error instanceof Error ? error.message : "Invalid terminal command" })); } });
    client.once("close", detach);
  }

  private subscribe(client: WebSocket, projectId: string, after: number): void {
    let lastSequence = after;
    for (const event of this.daemon.replay(projectId, after)) {
      client.send(JSON.stringify(event));
      lastSequence = event.sequence ?? lastSequence;
    }
    const listener = (event: { scope: { projectId: string }; sequence: number | null }) => {
      if (event.scope.projectId !== projectId || (event.sequence !== null && event.sequence <= lastSequence)) return;
      if (event.sequence !== null) lastSequence = event.sequence;
      if (client.readyState === client.OPEN) client.send(JSON.stringify(event));
    };
    this.daemon.events.on("event", listener);
    client.once("close", () => this.daemon.events.off("event", listener));
  }

  private authorized(request: IncomingMessage): boolean {
    const match = /^Bearer (.+)$/.exec(request.headers.authorization ?? "");
    const protocol = request.headers["sec-websocket-protocol"]?.split(",").map((value) => value.trim()).find((value) => value.startsWith("auth."));
    const token = match?.[1] ?? protocol?.slice(5); if (!token) return false; const expiresAt = this.sessions.get(token); if (!expiresAt) return false; if (expiresAt <= Date.now()) { this.sessions.delete(token); return false; } return true;
  }
  private bootstrapAuthorized(request: IncomingMessage): boolean { return this.daemon.authenticate(/^Bearer (.+)$/.exec(request.headers.authorization ?? "")?.[1]); }

  private serveStatic(pathname: string, response: ServerResponse): boolean {
    if (!this.publicDirectory || !existsSync(this.publicDirectory)) return false;
    const root = resolve(this.publicDirectory);
    let path = resolve(root, `.${pathname}`);
    if (relative(root, path).startsWith("..")) return false;
    if (!existsSync(path) || statSync(path).isDirectory()) path = resolve(root, "index.html");
    if (!existsSync(path) || !statSync(path).isFile()) return false;
    const types: Record<string, string> = { ".html": "text/html; charset=utf-8", ".js": "text/javascript; charset=utf-8", ".css": "text/css; charset=utf-8", ".json": "application/json", ".webmanifest": "application/manifest+json", ".png": "image/png", ".svg": "image/svg+xml", ".woff2": "font/woff2" };
    response.writeHead(200, {
      "content-type": types[extname(path)] ?? "application/octet-stream",
      "cache-control": pathname.startsWith("/assets/") ? "public, max-age=31536000, immutable" : "no-cache",
      "content-security-policy": "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self' ws: wss:; img-src 'self' data: blob:; font-src 'self'; worker-src 'self'; object-src 'none'; base-uri 'self'; frame-ancestors 'none'",
      "x-content-type-options": "nosniff",
      "referrer-policy": "no-referrer",
    });
    response.end(readFileSync(path));
    return true;
  }

  private send(response: ServerResponse, status: number, body: unknown): void {
    response.writeHead(status, { "content-type": "application/json; charset=utf-8", "cache-control": "no-store" });
    response.end(JSON.stringify(body));
  }
}

function apiPath(pathname: string): string {
  return pathname === "/api" ? "/" : pathname.startsWith("/api/") ? pathname.slice(4) : pathname;
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
function requiredStrings(value: object, names: string[], allowEmptyName?: string): void { const record = value as Record<string, unknown>; for (const name of names) if (typeof record[name] !== "string" || (name !== allowEmptyName && !(record[name] as string).trim()) || (name === "idempotencyKey" && (record[name] as string).length < 16)) throw new Error(`${name} is required`); }
