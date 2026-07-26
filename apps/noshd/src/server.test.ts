import { createId } from "@nosh/core";
import { schemaUri } from "@nosh/wire";
import { existsSync, mkdtempSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import WebSocket from "ws";
import { NoshDaemon } from "./daemon.js";
import { LocalApiServer } from "./server.js";
import { loadNoshPiResources } from "./pi-resources.js";

describe("noshd local API", () => {
  it("loads the NOSH Pi package through Pi's resource loader", async () => {
    const skills = await loadNoshPiResources(resolveWorkspaceRoot());
    expect(skills.some((skill) => skill.name === "nosh-control")).toBe(true);
  });

  it("enforces loopback bootstrap authentication and idempotent event commands", async () => {
    const directory = mkdtempSync(join(tmpdir(), "noshd-"));
    const repository = join(directory, "repository");
    mkdirSync(repository);
    mkdirSync(join(repository, "docs")); writeFileSync(join(repository, "docs", "paper.md"), "# Test\n", "utf8"); writeFileSync(join(repository, "docs", "paper.bib"), "", "utf8");
    const token = "test-bootstrap-token";
    const projectId = createId("prj");
    const daemon = new NoshDaemon({ dataDirectory: join(directory, "data"), bootstrapToken: token });
    const server = new LocalApiServer(daemon);
    daemon.start();
    await server.listen({ port: 0 });
    const base = server.address();

    try {
      expect((await fetch(`${base}/projects`)).status).toBe(401);
      expect((await fetch(`${base}/projects`, { headers: { authorization: `Bearer ${token}` } })).status).toBe(401);
      const sessionResponse = await fetch(`${base}/session`, { method: "POST", headers: { authorization: `Bearer ${token}` } }); expect(sessionResponse.status).toBe(201); const session = await sessionResponse.json() as { token: string; expiresAt: string }; expect(Date.parse(session.expiresAt)).toBeGreaterThan(Date.now()); const headers = { authorization: `Bearer ${session.token}`, "content-type": "application/json" };
      const createdRoot = join(directory, "created-project"); const createdResponse = await fetch(`${base}/projects/open`, { method: "POST", headers, body: JSON.stringify({ path: createdRoot, createRepository: true, workingTitle: "Created Project" }) }); expect(createdResponse.status).toBe(201); const created = (await createdResponse.json() as { project: { projectId: string } }).project; expect(existsSync(join(createdRoot, ".nosh", "contracts", "project.v1.json"))).toBe(true); expect(existsSync(join(createdRoot, "docs", "paper.md"))).toBe(true); const draftContract = await fetch(`${base}/projects/${created.projectId}/contract`, { headers }); expect((await draftContract.json() as { contract: { approvedAt: string | null; northStar: { contributionType: string }; computeEnvelope: { maximumDiskBytes: number } } }).contract).toMatchObject({ approvedAt: null, northStar: { contributionType: "contribution_pending" }, computeEnvelope: { maximumDiskBytes: 0 } });
      const registered = await fetch(`${base}/projects`, {
        method: "POST",
        headers,
        body: JSON.stringify({ projectId, repositoryRoot: repository, databasePath: join(directory, "data", "projects", projectId, "nosh.sqlite") }),
      });
      expect(registered.status).toBe(201);
      const terminalResponse = await fetch(`${base}/terminals`, { method: "POST", headers, body: JSON.stringify({ projectId, profile: "powershell" }) }); expect(terminalResponse.status).toBe(201); const terminal = (await terminalResponse.json() as { terminal: { terminalId: string; projectId: string; cwd: string } }).terminal; expect(terminal).toMatchObject({ projectId, cwd: repository }); expect((await fetch(`${base}/terminals/${terminal.terminalId}`, { headers })).status).toBe(200);
      const terminalOutput = await new Promise<string>((resolveOutput, rejectOutput) => { const socket = new WebSocket(`${base.replace("http", "ws")}/terminals/${terminal.terminalId}/stream`, ["nosh", `auth.${session.token}`]); let output = ""; const timeout = setTimeout(() => { socket.close(); rejectOutput(new Error("terminal output timed out")); }, 5_000); socket.on("open", () => socket.send(JSON.stringify({ type: "input", data: "Write-Output NOSH_PTY_OK\r" }))); socket.on("message", (raw) => { const message = JSON.parse(raw.toString()) as { type?: string; data?: string }; if (message.type !== "output") return; output += message.data ?? ""; if (!output.includes("NOSH_PTY_OK")) return; clearTimeout(timeout); socket.close(); resolveOutput(output); }); socket.on("error", rejectOutput); }); expect(terminalOutput).toContain("NOSH_PTY_OK"); await new Promise((resolve) => setTimeout(resolve, 100)); expect((await fetch(`${base}/terminals/${terminal.terminalId}`, { method: "DELETE", headers })).status).toBe(202);
      const threads = await fetch(`${base}/threads?projectId=${projectId}`, { headers }); expect((await threads.json() as { threads: unknown[] }).threads).toEqual([]); const skillId = createId("skl"); const skill = await fetch(`${base}/skills`, { method: "POST", headers, body: JSON.stringify({ projectId, idempotencyKey: "register-runtime-skill-0001", manifest: { $schema: schemaUri("skill-manifest"), schemaVersion: 1, skillId, name: "runtime_test", version: "1.0.0", description: "API integration fixture", activation: { roles: ["general_worker"], requiredCapabilities: [], episodeTypes: [] }, promptFragment: "Return a bounded result.", permittedTools: ["nosh_episode_submit"], inputEpisodeTypes: [], outputEpisodeType: "episode_test", executionMode: "prompt", programId: null, preflightChecks: [], postflightChecks: ["check_episode"] } }) }); expect(skill.status).toBe(201); const skills = await fetch(`${base}/skills?projectId=${projectId}`, { headers }); expect((await skills.json() as { skills: Array<{ entityId: string }> }).skills.map((item) => item.entityId)).toContain(skillId);

      const command = {
        $schema: schemaUri("command"),
        schemaVersion: 1,
        commandId: createId("cmd"),
        idempotencyKey: "event-command-key-0001",
        projectId,
        targetType: "project",
        targetId: null,
        expectedVersion: null,
        type: "event.append",
        observedVersions: {},
        payload: {
          $schema: schemaUri("event"),
          schemaVersion: 1,
          retention: "persistent",
          type: "agent.started",
          source: "test",
          scope: { projectId, missionId: null, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: null },
          correlationId: null,
          causationId: null,
          payload: { task: "test" },
        },
        issuedAt: "2026-07-17T20:00:00.000Z",
      };
      const first = await fetch(`${base}/commands`, { method: "POST", headers, body: JSON.stringify(command) });
      const duplicate = await fetch(`${base}/commands`, { method: "POST", headers, body: JSON.stringify(command) });
      expect((await first.json() as { replayed: boolean }).replayed).toBe(false);
      expect((await duplicate.json() as { replayed: boolean }).replayed).toBe(true);
      const events = await fetch(`${base}/events?projectId=${projectId}&after=0`, { headers });
      const replayed = (await events.json() as { events: Array<{ type: string }> }).events; expect(replayed.filter((event) => event.type === "agent.started")).toHaveLength(1); expect(replayed.some((event) => event.type === "recovery.report")).toBe(true);

      const missionResponse = await fetch(`${base}/missions`, { method: "POST", headers, body: JSON.stringify({ projectId, title: "Test Mission", objective: "Produce a reviewed result", deliverables: ["Reviewed result"], successCriteria: ["All required nodes pass"], idempotencyKey: "create-mission-0001" }) }); expect(missionResponse.status).toBe(201); const mission = (await missionResponse.json() as { mission: { entityId: string; version: number; value: { graphVersion: number } } }).mission; expect(mission.value.graphVersion).toBe(1);
      for (const [next, expectedVersion] of [["planning", 1], ["awaiting_approval", 2], ["running", 3]] as const) { const changed = await fetch(`${base}/missions/${mission.entityId}/transition`, { method: "POST", headers, body: JSON.stringify({ projectId, expectedVersion, next, idempotencyKey: `mission-transition-${next}-0001` }) }); expect(changed.status).toBe(202); }
      const listed = await fetch(`${base}/missions?projectId=${projectId}`, { headers }); expect((await listed.json() as { missions: Array<{ state: string }> }).missions[0]?.state).toBe("running");
      const pausedResponse = await fetch(`${base}/missions/${mission.entityId}/control`, { method: "POST", headers, body: JSON.stringify({ projectId, expectedVersion: 4, action: "pause", mode: "safe", idempotencyKey: "mission-control-pause-0001" }) }); expect(pausedResponse.status).toBe(202); const paused = (await pausedResponse.json() as { mission: { state: string; version: number } }).mission; expect(paused).toMatchObject({ state: "paused", version: 6 });
      const resumedResponse = await fetch(`${base}/missions/${mission.entityId}/control`, { method: "POST", headers, body: JSON.stringify({ projectId, expectedVersion: paused.version, action: "resume", mode: "safe", idempotencyKey: "mission-control-resume-0001" }) }); expect(resumedResponse.status).toBe(202); expect((await resumedResponse.json() as { mission: { state: string; version: number } }).mission).toMatchObject({ state: "running", version: 7 });
      const directionResponse = await fetch(`${base}/directions`, { method: "POST", headers, body: JSON.stringify({ projectId, question: "Does the mechanism work?", decisionUse: "Decide the paper claim", missionId: mission.entityId, idempotencyKey: "create-direction-0001" }) }); expect(directionResponse.status).toBe(201); const direction = (await directionResponse.json() as { direction: { entityId: string; version: number } }).direction;
      for (const [next, expectedVersion] of [["proposed", 1], ["active", 2]] as const) { const changed = await fetch(`${base}/directions/${direction.entityId}/transition`, { method: "POST", headers, body: JSON.stringify({ projectId, expectedVersion, next, idempotencyKey: `direction-transition-${next}-0001` }) }); expect(changed.status).toBe(202); }
      const prematureExecution = await fetch(`${base}/autoresearch`, { method: "POST", headers, body: JSON.stringify({ projectId, directionId: direction.entityId, decisionQuestion: "Does an unreviewed baseline suffice?", idempotencyKey: "premature-ar-0001" }) }); expect(prematureExecution.status).toBe(400); expect((await prematureExecution.json() as { error: string }).error).toContain("reviewed immutable baseline");
      const paper = await fetch(`${base}/paper`, { method: "PUT", headers, body: JSON.stringify({ projectId, markdown: "# Revised\n", idempotencyKey: "paper-save-command-0001" }) }); expect(paper.status).toBe(202); const readPaper = await fetch(`${base}/paper?projectId=${projectId}`, { headers }); expect((await readPaper.json() as { markdown: string }).markdown).toBe("# Revised\n");
      const interruptedCommandId = createId("cmd"); const acceptedCommand = { ...command, commandId: createId("cmd"), idempotencyKey: "interrupted-command-event-0001", payload: { ...command.payload, type: "remote.command_accepted", correlationId: interruptedCommandId, payload: { commandId: interruptedCommandId, type: "job.cancel", targetType: "job", targetId: createId("job"), expectedVersion: 9 } } }; expect((await fetch(`${base}/commands`, { method: "POST", headers, body: JSON.stringify(acceptedCommand) })).status).toBe(202); const unresolved = await fetch(`${base}/remote/commands/unresolved?projectId=${projectId}`, { headers }); expect((await unresolved.json() as { commands: Array<{ commandId: string }> }).commands.map((item) => item.commandId)).toContain(interruptedCommandId); const resolution = await fetch(`${base}/remote/commands/${interruptedCommandId}/resolve`, { method: "POST", headers, body: JSON.stringify({ projectId, outcome: "not_applied", note: "Verified that the target Job remained unchanged after restart", idempotencyKey: "resolve-remote-command-0001" }) }); expect(resolution.status).toBe(202); const resolved = await fetch(`${base}/remote/commands/unresolved?projectId=${projectId}`, { headers }); expect((await resolved.json() as { commands: unknown[] }).commands).toHaveLength(0);
    } finally {
      await server.close();
      daemon.stop();
      rmSync(directory, { recursive: true, force: true });
    }
  });
});

function resolveWorkspaceRoot(): string {
  return join(import.meta.dirname, "..", "..", "..");
}
