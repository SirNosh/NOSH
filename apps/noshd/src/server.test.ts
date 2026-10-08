import { createId } from "@nosh/core";
import { EventStore, type RegisteredProject } from "@nosh/persistence";
import { schemaUri } from "@nosh/wire";
import { existsSync, mkdtempSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import WebSocket from "ws";
import { EventEmitter } from "node:events";
import { request } from "node:http";
import { missionJobMatches, NoshDaemon } from "./daemon.js";
import { isLoopbackAddress, LocalApiServer, parseModelSelection } from "./server.js";
import { ResearchControl } from "./research-control.js";
import { loadNoshPiResources } from "./pi-resources.js";

describe("noshd local API", () => {
  it("streams ordered durable pages and resumes strictly after the received cursor", async () => {
    const events = new EventEmitter();
    const history = Array.from({ length: 5 }, (_, i) => ({ scope: { projectId: "prj_test" }, sequence: i + 1 }));
    const daemon = { events, replayPage: (_project: string, after: number, limit: number) => ({ events: history.filter((event) => event.sequence > after).slice(0, limit), nextCursor: after, hasMore: false }) } as unknown as NoshDaemon;
    const server = new LocalApiServer(daemon);
    const receive = (after: number) => new Promise<number[]>((resolve) => {
      class Socket extends EventEmitter {
        OPEN = 1; readyState = 1; bufferedAmount = 0; received: number[] = [];
        send(message: string, callback: (error?: Error) => void) {
          const event = JSON.parse(message) as { sequence: number };
          this.received.push(event.sequence);
          callback();
          if (event.sequence === 5) { this.readyState = 3; this.emit("close"); resolve(this.received); }
        }
        close() { this.readyState = 3; this.emit("close"); }
        terminate() { this.close(); }
      }
      (server as unknown as { subscribe(client: WebSocket, projectId: string, after: number): void }).subscribe(new Socket() as unknown as WebSocket, "prj_test", after);
    });
    expect(await receive(0)).toEqual([1, 2, 3, 4, 5]);
    expect(await receive(3)).toEqual([4, 5]);
    expect(events.listenerCount("event")).toBe(0);
  });

  it("closes overflowing transient queues explicitly and removes the listener", () => {
    const events = new EventEmitter();
    const daemon = { events, replayPage: () => ({ events: [], nextCursor: 0, hasMore: false }) } as unknown as NoshDaemon;
    const server = new LocalApiServer(daemon);
    class Socket extends EventEmitter {
      OPEN = 1; readyState = 1; bufferedAmount = 0; code = 0; reason = "";
      send() { /* hold send callback to simulate a blocked consumer */ }
      close(code: number, reason: string) { this.code = code; this.reason = reason; this.readyState = 3; this.emit("close"); }
      terminate() { this.emit("close"); }
    }
    const socket = new Socket();
    (server as unknown as { subscribe(client: WebSocket, projectId: string, after: number): void }).subscribe(socket as unknown as WebSocket, "prj_test", 0);
    for (let i = 0; i < 305; i++) events.emit("event", { scope: { projectId: "prj_test" }, sequence: null, payload: "x" });
    expect(socket.code).toBe(1013);
    expect(socket.reason).toContain("last received cursor");
    expect(events.listenerCount("event")).toBe(0);
  });
  it("loads the NOSH Pi package through Pi's resource loader", async () => {
    const skills = await loadNoshPiResources(resolveWorkspaceRoot());
    expect(skills.some((skill) => skill.name === "nosh-control")).toBe(true);
  });

  it("auto-trusts local origins while preserving bearer fallback and event replay", async () => {
    const directory = mkdtempSync(join(tmpdir(), "noshd-"));
    const repository = join(directory, "repository");
    mkdirSync(repository);
    mkdirSync(join(repository, "docs")); writeFileSync(join(repository, "docs", "paper.md"), "# Test\n", "utf8"); writeFileSync(join(repository, "docs", "paper.bib"), "", "utf8");
    const token = "test-bootstrap-token";
    const projectId = createId("prj");
    const projectDatabasePath = join(directory, "data", "projects", projectId, "nosh.sqlite");
    const daemon = new NoshDaemon({ dataDirectory: join(directory, "data"), bootstrapToken: token });
    const server = new LocalApiServer(daemon);
    daemon.start();
    await server.listen({ port: 0 });
    const base = server.address();

    try {
      expect((await fetch(`${base}/projects`)).status).toBe(200);
      expect((await fetch(`${base}/projects`, { headers: { origin: base } })).status).toBe(200);
      expect((await fetch(`${base}/projects`, { headers: { origin: "http://attacker.invalid" } })).status).toBe(401);
      expect((await fetch(`${base}/projects`, { headers: { "sec-fetch-site": "cross-site" } })).status).toBe(401);
      // DNS rebinding: attacker Host and matching Origin arrive from loopback.
      const rebound = (headers: Record<string, string>) => new Promise<number>((resolveStatus, rejectStatus) => { const req = request(`${base}/projects`, { headers }, (res) => { res.resume(); resolveStatus(res.statusCode ?? 0); }); req.once("error", rejectStatus); req.end(); });
      const port = new URL(base).port;
      expect(await rebound({ host: `attacker.invalid:${port}`, origin: `http://attacker.invalid:${port}` })).toBe(401);
      expect(await rebound({ host: `attacker.invalid:${port}` })).toBe(401);
      expect(await rebound({ host: `localhost:${port}` })).toBe(200);
      expect((await fetch(`${base}/session`, { method: "POST" })).status).toBe(401);
      const sessionResponse = await fetch(`${base}/session`, { method: "POST", headers: { authorization: `Bearer ${token}` } }); expect(sessionResponse.status).toBe(201); const session = await sessionResponse.json() as { token: string; expiresAt: string }; expect(Date.parse(session.expiresAt)).toBeGreaterThan(Date.now()); expect((await fetch(`${base}/projects`, { headers: { origin: "http://attacker.invalid", authorization: `Bearer ${session.token}` } })).status).toBe(200); const headers = { "content-type": "application/json" };
      const createdRoot = join(directory, "created-project"); const createdResponse = await fetch(`${base}/projects/open`, { method: "POST", headers, body: JSON.stringify({ path: createdRoot, createRepository: true, workingTitle: "Created Project" }) }); expect(createdResponse.status).toBe(201); const created = (await createdResponse.json() as { project: { projectId: string } }).project; expect(existsSync(join(createdRoot, ".nosh", "contracts", "project.v1.json"))).toBe(true); expect(existsSync(join(createdRoot, "docs", "paper.md"))).toBe(true); const draftContract = await fetch(`${base}/projects/${created.projectId}/contract`, { headers }); expect((await draftContract.json() as { contract: { approvedAt: string | null; northStar: { contributionType: string }; computeEnvelope: { maximumDiskBytes: number } } }).contract).toMatchObject({ approvedAt: null, northStar: { contributionType: "contribution_pending" }, computeEnvelope: { maximumDiskBytes: 0 } });
      const registered = await fetch(`${base}/projects`, {
        method: "POST",
        headers,
        body: JSON.stringify({ projectId, repositoryRoot: repository, databasePath: projectDatabasePath }),
      });
      expect(registered.status).toBe(201);
      expect((await fetch(`${base}/terminals`, { method: "POST", headers, body: JSON.stringify({ projectId }) })).status).toBe(404);
      expect((await fetch(`${base}/remote`, { headers })).status).toBe(404);
      expect((await fetch(`${base}/directories/select`, { method: "POST", headers })).status).toBe(404);
      expect((await fetch(`${base}/`, { headers })).status).toBe(404);
      const threads = await fetch(`${base}/threads?projectId=${projectId}`, { headers }); expect((await threads.json() as { threads: unknown[] }).threads).toEqual([]); const skillId = createId("skl"); const skill = await fetch(`${base}/skills`, { method: "POST", headers, body: JSON.stringify({ projectId, idempotencyKey: "register-runtime-skill-0001", manifest: { $schema: schemaUri("skill-manifest"), schemaVersion: 1, skillId, name: "runtime_test", version: "1.0.0", description: "API integration fixture", activation: { roles: ["general_worker"], requiredCapabilities: [], episodeTypes: [] }, promptFragment: "Return a bounded result.", permittedTools: ["nosh_episode_submit"], inputEpisodeTypes: [], outputEpisodeType: "episode_test", executionMode: "prompt", programId: null, preflightChecks: [], postflightChecks: ["check_episode"] } }) }); expect(skill.status).toBe(201); const skills = await fetch(`${base}/skills?projectId=${projectId}`, { headers }); expect((await skills.json() as { skills: Array<{ entityId: string }> }).skills.map((item) => item.entityId)).toContain(skillId);

      const removedCommandResponse = await fetch(`${base}/commands`, { method: "POST", headers, body: JSON.stringify({}) });
      expect(removedCommandResponse.status).toBe(404);
      const events = await fetch(`${base}/events?projectId=${projectId}&after=0`, { headers });
      const replayed = (await events.json() as { events: Array<{ type: string }> }).events;
      expect(replayed.some((event) => event.type === "agent.started")).toBe(false);
      const pageResponse = await fetch(`${base}/events?projectId=${projectId}&after=0&limit=1`, { headers });
      const page = await pageResponse.json() as { events: Array<{ sequence: number }>; nextCursor: number; hasMore: boolean };
      expect(page.events).toHaveLength(1);
      expect(page.nextCursor).toBe(page.events[0]!.sequence);
      expect(page.hasMore).toBe(true);
      expect((await fetch(`${base}/events?projectId=${projectId}&limit=1001`, { headers })).status).toBe(400);
      const nextPage = await (await fetch(`${base}/events?projectId=${projectId}&after=${page.nextCursor}&limit=1`, { headers })).json() as { events: Array<{ sequence: number }> };
      expect(nextPage.events[0]!.sequence).toBeGreaterThan(page.nextCursor);
      const recentPage = await (await fetch(`${base}/events?projectId=${projectId}&recent=true&limit=2`, { headers })).json() as { events: Array<{ sequence: number }>; nextCursor: number; hasMore: boolean };
      const fullHistory = daemon.replay(projectId, 0);
      expect(recentPage.events.map(event => event.sequence)).toEqual(fullHistory.slice(-2).map(event => event.sequence));
      expect(recentPage.nextCursor).toBe(fullHistory.at(-1)!.sequence);
      expect(recentPage.hasMore).toBe(false);
      expect((await fetch(`${base}/events?projectId=${projectId}&recent=true&after=1`, { headers })).status).toBe(400);
      expect((await fetch(`${base}/events?projectId=${projectId}&recent=invalid`, { headers })).status).toBe(400);
      const deltaStore = new EventStore(projectDatabasePath);
      const delta = deltaStore.append({ $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "agent.started", source: "test", scope: { projectId, missionId: null, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: null }, correlationId: null, causationId: null, payload: {} });
      deltaStore.close();
      const deltaPage = await (await fetch(`${base}/events?projectId=${projectId}&after=${recentPage.nextCursor}&limit=2`, { headers })).json() as { events: Array<{ sequence: number }>; nextCursor: number };
      expect(deltaPage.events.map(event => event.sequence)).toEqual([delta.sequence]);
      expect(deltaPage.nextCursor).toBe(delta.sequence);
      const eventSocketUrl = `${base.replace(/^http/, "ws")}/events?projectId=${projectId}&after=0`;
      const crossOriginSocketRejected = await new Promise<boolean>((resolveRejected) => { const socket = new WebSocket(eventSocketUrl, { headers: { origin: "http://attacker.invalid" } }); let settled = false; const finish = (rejected: boolean) => { if (settled) return; settled = true; socket.close(); resolveRejected(rejected); }; socket.once("open", () => finish(false)); socket.once("error", () => finish(true)); socket.once("close", () => finish(true)); setTimeout(() => finish(true), 5_000); }); expect(crossOriginSocketRejected).toBe(true);

      const missionResponse = await fetch(`${base}/missions`, { method: "POST", headers, body: JSON.stringify({ projectId, title: "Test Mission", objective: "Produce a reviewed result", deliverables: ["Reviewed result"], successCriteria: ["All required nodes pass"], idempotencyKey: "create-mission-0001" }) }); expect(missionResponse.status).toBe(201); const mission = (await missionResponse.json() as { mission: { entityId: string; version: number; value: { graphVersion: number } } }).mission; expect(mission.value.graphVersion).toBe(1);
      const forbiddenLease = await fetch(`${base}/missions/${mission.entityId}/nodes/internal-lease/transition`, { method: "POST", headers, body: JSON.stringify({ projectId, expectedVersion: mission.version, next: "leased", lease: {}, idempotencyKey: "public-lease-rejected-0001" }) });
      expect(forbiddenLease.status).toBe(400);
      for (const [next, expectedVersion] of [["planning", 1], ["awaiting_approval", 2], ["running", 3]] as const) { const changed = await fetch(`${base}/missions/${mission.entityId}/transition`, { method: "POST", headers, body: JSON.stringify({ projectId, expectedVersion, next, idempotencyKey: `mission-transition-${next}-0001` }) }); expect(changed.status).toBe(202); }
      const listed = await fetch(`${base}/missions?projectId=${projectId}`, { headers }); expect((await listed.json() as { missions: Array<{ state: string }> }).missions[0]?.state).toBe("running");
      const bareStop = await fetch(`${base}/missions/${mission.entityId}/transition`, { method: "POST", headers, body: JSON.stringify({ projectId, expectedVersion: 4, next: "stopping", idempotencyKey: "mission-transition-stopping-0001" }) }); expect(bareStop.status).toBe(400); expect(await bareStop.text()).toContain("/control <missionId> <version> stop");
      const pausedResponse = await fetch(`${base}/missions/${mission.entityId}/control`, { method: "POST", headers, body: JSON.stringify({ projectId, expectedVersion: 4, action: "pause", mode: "safe", idempotencyKey: "mission-control-pause-0001" }) }); expect(pausedResponse.status).toBe(202); const paused = (await pausedResponse.json() as { mission: { state: string; version: number } }).mission; expect(paused).toMatchObject({ state: "paused", version: 6 });
      const resumedResponse = await fetch(`${base}/missions/${mission.entityId}/control`, { method: "POST", headers, body: JSON.stringify({ projectId, expectedVersion: paused.version, action: "resume", mode: "safe", idempotencyKey: "mission-control-resume-0001" }) }); expect(resumedResponse.status).toBe(202); expect((await resumedResponse.json() as { mission: { state: string; version: number } }).mission).toMatchObject({ state: "running", version: 7 });
      const directionResponse = await fetch(`${base}/directions`, { method: "POST", headers, body: JSON.stringify({ projectId, question: "Does the mechanism work?", decisionUse: "Decide the paper claim", missionId: mission.entityId, idempotencyKey: "create-direction-0001" }) }); expect(directionResponse.status).toBe(201); const direction = (await directionResponse.json() as { direction: { entityId: string; version: number } }).direction;
      for (const [next, expectedVersion] of [["proposed", 1], ["active", 2]] as const) { const changed = await fetch(`${base}/directions/${direction.entityId}/transition`, { method: "POST", headers, body: JSON.stringify({ projectId, expectedVersion, next, idempotencyKey: `direction-transition-${next}-0001` }) }); expect(changed.status).toBe(202); }
      const prematureExecution = await fetch(`${base}/autoresearch`, { method: "POST", headers, body: JSON.stringify({ projectId, directionId: direction.entityId, decisionQuestion: "Does an unreviewed baseline suffice?", idempotencyKey: "premature-ar-0001" }) }); expect(prematureExecution.status).toBe(400); expect((await prematureExecution.json() as { error: string }).error).toContain("reviewed immutable baseline");
      const originalPaper = await (await fetch(`${base}/paper?projectId=${projectId}`, { headers })).json() as { bibliography: string; markdownHash: string; bibliographyHash: string; version: string };
      const expectedPaper = { markdownHash: originalPaper.markdownHash, bibliographyHash: originalPaper.bibliographyHash, version: originalPaper.version };
      const paper = await fetch(`${base}/paper`, { method: "PUT", headers, body: JSON.stringify({ projectId, markdown: "# Revised\n", bibliography: originalPaper.bibliography, expected: expectedPaper, idempotencyKey: "paper-save-command-0001" }) }); expect(paper.status).toBe(202);
      const readPaper = await fetch(`${base}/paper?projectId=${projectId}`, { headers }); expect((await readPaper.json() as { markdown: string }).markdown).toBe("# Revised\n");
      const stalePaper = await fetch(`${base}/paper`, { method: "PUT", headers, body: JSON.stringify({ projectId, markdown: "# Stale overwrite\n", bibliography: originalPaper.bibliography, expected: expectedPaper, idempotencyKey: "paper-save-command-stale-0001" }) }); expect(stalePaper.status).toBe(400);
    } finally {
      await server.close();
      daemon.stop();
      rmSync(directory, { recursive: true, force: true });
    }
  });

  it("retains accepted runtime instructions when their durable effect fails", async () => {
    const directory = mkdtempSync(join(tmpdir(), "noshd-runtime-effect-"));
    const repository = join(directory, "repository");
    const projectId = createId("prj");
    const daemon = new NoshDaemon({ dataDirectory: join(directory, "data"), bootstrapToken: "test-bootstrap-token" });
    mkdirSync(repository);
    daemon.registerProject({ projectId, repositoryRoot: repository, databasePath: join(directory, "data", "projects", projectId, "nosh.sqlite") });
    const instructionId = createId("ins");
    try {
      const result = await daemon.submitTool("nosh_runtime_instruct", projectId, "runtime-effect-failure-0001", {
        $schema: schemaUri("runtime-instruction"), schemaVersion: 1, instructionId, projectId, idempotencyKey: "runtime-effect-failure-0001", proposedByAgentId: null, issuedAt: new Date().toISOString(),
        operation: "STOP", threadId: createId("thr"), programId: null, reason: "Exercise durable failed-effect recovery",
      });
      expect(result).toMatchObject({ accepted: true, effect: { state: "failed" } });
      expect(daemon.research.records(projectId, "runtime-instruction")).toContainEqual(expect.objectContaining({ instructionId }));
    } finally {
      daemon.stop();
      rmSync(directory, { recursive: true, force: true });
    }
  });

  it("binds graph proposal approval replays to the expected proposal version", () => {
    const directory = mkdtempSync(join(tmpdir(), "noshd-graph-approval-"));
    const projectId = createId("prj");
    const project: RegisteredProject = { projectId, repositoryRoot: directory, databasePath: join(directory, "project.sqlite"), registeredAt: new Date().toISOString() };
    const store = new EventStore(project.databasePath);
    const research = new ResearchControl(() => store, () => project, () => undefined);
    try {
      const direction = research.createDirection(projectId, { question: "Does the replay bind its intent?", decisionUse: "Exercise approval idempotency", idempotencyKey: "create-direction-approval-replay" });
      const proposalId = "proposal_replay";
      const scope = { projectId, missionId: null, directionId: direction.entityId, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: null };
      const submittedAt = new Date().toISOString();
      const record = {
        $schema: schemaUri("graph-change-proposal"), schemaVersion: 1, proposalId, scopeType: "direction", scopeId: direction.entityId,
        baseGraphVersion: direction.value.graphVersion, operations: [{ type: "add_node", node: { id: "dnode_approval_replay", type: "general_worker", title: "Verify idempotent approval binding", required: false, criterionIds: [], hardDependencies: [], softDependencies: [], state: "pending", attempt: 0, maximumAttempts: 1, priority: 1, criticalWeight: 1, createdAt: submittedAt, lease: null } }],
        rationale: "Verify that approval replays bind the expected proposal version.", evidenceIds: [], proposerRole: "user", approvalRequired: true, contractImpact: "none", reasonCode: "reason_replay", expectedEffect: "Reject a changed expected version for the same idempotency key.", budgetImpact: { modelTokensDelta: 0, gpuSecondsDelta: 0 }, submittedAt,
      };
      const pending = store.mutateProjection(`domain-graph:${proposalId}`, 0, { $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "graph.proposal_pending", source: "noshd", scope, correlationId: proposalId, causationId: null, payload: { proposalId, contractImpact: "none" } }, { entityType: "graph_proposal", entityId: proposalId, state: "pending", value: { record, scope, submittedAt } });
      research.approveGraphProposal(projectId, proposalId, pending.projection.version, "approve-graph-proposal-replay");
      expect(() => research.approveGraphProposal(projectId, proposalId, pending.projection.version + 1, "approve-graph-proposal-replay")).toThrow("different graph-proposal approval");
    } finally {
      store.close();
      rmSync(directory, { recursive: true, force: true });
    }
  });

  it("recognizes IPv4, IPv6, and IPv4-mapped loopback addresses", () => {
    expect(isLoopbackAddress("127.0.0.1")).toBe(true);
    expect(isLoopbackAddress("127.42.0.9")).toBe(true);
    expect(isLoopbackAddress("::1")).toBe(true);
    expect(isLoopbackAddress("::ffff:127.0.0.1")).toBe(true);
    expect(isLoopbackAddress("::ffff:7f00:1")).toBe(true);
    expect(isLoopbackAddress("192.168.1.10")).toBe(false);
    expect(isLoopbackAddress("::2")).toBe(false);
  });

  it("scopes Mission Job control to both Project and Mission", () => {
    const job = { projectId: "project-a", missionId: "mission-a" };
    expect(missionJobMatches(job, "project-a", "mission-a")).toBe(true);
    expect(missionJobMatches(job, "project-a", "mission-b")).toBe(false);
    expect(missionJobMatches(job, "project-b", "mission-a")).toBe(false);
  });

  it("validates model selection request shapes while preserving omitted thinking levels", () => {
    expect(parseModelSelection({ provider: "provider-a", id: "model-a" })).toEqual({ provider: "provider-a", id: "model-a" });
    expect(parseModelSelection({ provider: "provider-a", id: "model-a" }, "max")).toEqual({ provider: "provider-a", id: "model-a", thinkingLevel: "max" });
    expect(() => parseModelSelection({ provider: "provider-a", id: "model-a", thinkingLevel: "invalid" })).toThrow("thinkingLevel one of");
    expect(() => parseModelSelection(undefined, "high")).toThrow("requires a selected Pi model");
  });
});

function resolveWorkspaceRoot(): string {
  return join(import.meta.dirname, "..", "..", "..");
}
