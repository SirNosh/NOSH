import { type AgentInspection } from "@nosh/pi-adapter";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it, vi } from "vitest";
import { NoshDaemon } from "./daemon.js";
import { LocalApiServer } from "./server.js";

function fixture() {
  const directory = mkdtempSync(join(tmpdir(), "nosh-chat-contract-"));
  const daemon = new NoshDaemon({ dataDirectory: join(directory, "data"), bootstrapToken: "test" });
  const project = daemon.initializeProject({ path: join(directory, "repository"), createRepository: true, workingTitle: "Chat fixture" });
  const metadataPath = join(project.repositoryRoot, ".nosh", "project.json");
  const metadata = JSON.parse(readFileSync(metadataPath, "utf8"));
  const draftPath = join(project.repositoryRoot, ".nosh", "contracts", "project.v1.json");
  const draftBytes = readFileSync(draftPath, "utf8");
  const contract = { ...JSON.parse(draftBytes), contractVersion: 2, approvedAt: new Date().toISOString() };
  const activePath = join(project.repositoryRoot, ".nosh", "contracts", "project.v2.json");
  writeFileSync(activePath, JSON.stringify(contract));
  writeFileSync(metadataPath, JSON.stringify({ ...metadata, activeProjectContractVersion: 2, activeProjectContractPath: ".nosh/contracts/project.v2.json" }));
  const validate = vi.spyOn(daemon, "validateModelSelection").mockResolvedValue();
  const inspect = vi.spyOn(daemon.agents, "inspect").mockReturnValue([]);
  const start = vi.spyOn(daemon.agents, "start").mockImplementation(async options => inspection(project.projectId, options.agentId));
  const stop = vi.spyOn(daemon.agents, "stop").mockImplementation(() => undefined);
  const thinking = vi.spyOn(daemon.agents, "setThinkingLevel").mockImplementation(() => inspection(project.projectId));
  const prompt = vi.spyOn(daemon.agents, "prompt").mockResolvedValue();
  const followUp = vi.spyOn(daemon.agents, "followUp").mockResolvedValue();
  const emitted = vi.fn(); daemon.events.on("event", emitted);
  const history = daemon.replay(project.projectId, 0);
  return {
    daemon, project, metadataPath, draftPath, draftBytes, contract, activePath, validate, inspect, start, stop, thinking, prompt, followUp, emitted, history,
    corrupt() {
      const invalid = { ...contract, northStar: { ...contract.northStar, contributionType: "invalid" }, baseline: {} };
      delete invalid.templateVersion; delete invalid.paper;
      const bytes = JSON.stringify(invalid); writeFileSync(activePath, bytes); return bytes;
    },
    assertNoEffects() {
      expect(start).not.toHaveBeenCalled(); expect(stop).not.toHaveBeenCalled(); expect(thinking).not.toHaveBeenCalled();
      expect(prompt).not.toHaveBeenCalled(); expect(followUp).not.toHaveBeenCalled(); expect(emitted).not.toHaveBeenCalled();
      expect(daemon.replay(project.projectId, 0)).toEqual(history);
    },
    close() { daemon.stop(); vi.restoreAllMocks(); rmSync(directory, { recursive: true, force: true }); },
  };
}

function inspection(projectId: string, agentId = "agt_00000000000000000000000000"): AgentInspection {
  return {
    projectId, agentId, missionId: null, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, taskId: null,
    role: "nosh", piSessionId: "test", status: "idle", currentTool: null, activeToolIds: [], startedAt: new Date().toISOString(), lastEventAt: new Date().toISOString(),
    modelProvider: "test", modelId: "old", modelName: "Old", thinkingLevel: "off", contextTokens: null, contextWindow: null, contextPercent: null,
  };
}

describe("chat active contract preflight", () => {
  it("returns an actionable HTTP error without ghost messages and permits the same key after explicit repair", async () => {
    const f = fixture(); const server = new LocalApiServer(f.daemon);
    await server.listen({ port: 0 });
    try {
      const invalidBytes = f.corrupt();
      const metadataBytes = readFileSync(f.metadataPath, "utf8");
      const send = (key: string) => fetch(`${server.address()}/chat`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ projectId: f.project.projectId, message: "Hello", idempotencyKey: key }) });
      for (const key of ["chat-contract-retry-0001", "chat-contract-retry-0001", "chat-contract-retry-0002"]) {
        const response = await send(key); expect(response.status).toBe(400);
        const { error } = await response.json() as { error: string };
        expect(error).toContain(f.activePath); expect(error).toContain("templateVersion: Required");
        expect(error).toContain("northStar.contributionType:"); expect(error).toContain("paper: Required");
        expect(error).toContain("+1 more validation issues"); expect(error).toContain("Repair this file");
        expect(error).not.toContain('"code"');
      }
      f.assertNoEffects();
      expect(readFileSync(f.activePath, "utf8")).toBe(invalidBytes);
      expect(readFileSync(f.metadataPath, "utf8")).toBe(metadataBytes);
      expect(readFileSync(f.draftPath, "utf8")).toBe(f.draftBytes);
      // Repair only this disposable fixture, without selecting the valid older draft.
      writeFileSync(f.activePath, JSON.stringify(f.contract));
      const accepted = await send("chat-contract-retry-0001"); expect(accepted.status).toBe(202);
      expect(await accepted.json()).toMatchObject({ replayed: false });
      expect(f.start).toHaveBeenCalledTimes(1); expect(f.prompt).toHaveBeenCalledTimes(1);
      expect(f.prompt.mock.calls[0]![1]).toBe("Hello");
      expect(f.daemon.replay(f.project.projectId, 0).filter(event => event.type === "chat.user_message")).toHaveLength(1);
      f.corrupt();
      const replay = await send("chat-contract-retry-0001"); expect(replay.status).toBe(202);
      expect(await replay.json()).toMatchObject({ replayed: true }); expect(f.prompt).toHaveBeenCalledTimes(1);
    } finally { await server.close(); f.close(); }
  });

  it.each([
    { provider: "test", id: "new", thinkingLevel: "high" as const },
    { provider: "test", id: "old", thinkingLevel: "high" as const },
  ])("does not change an existing session when its contract is invalid: $id", async selection => {
    const f = fixture();
    try {
      f.inspect.mockReturnValue([inspection(f.project.projectId)]); f.corrupt();
      await expect(f.daemon.chat(f.project.projectId, "Hello", "chat-contract-session-0001", selection)).rejects.toThrow(f.activePath);
      expect(f.validate).not.toHaveBeenCalled(); f.assertNoEffects();
    } finally { f.close(); }
  });

  it.each(["malformed contract", "missing contract", "malformed metadata", "invalid metadata"])("rejects %s before any chat side effect", async failure => {
    const f = fixture();
    try {
      if (failure === "malformed contract") writeFileSync(f.activePath, "{broken");
      if (failure === "missing contract") rmSync(f.activePath);
      if (failure === "malformed metadata") writeFileSync(f.metadataPath, "{broken");
      if (failure === "invalid metadata") writeFileSync(f.metadataPath, "{}");
      await expect(f.daemon.chat(f.project.projectId, "Hello", "chat-contract-unreadable-0001")).rejects.toThrow(failure.includes("metadata") ? f.metadataPath : f.activePath);
      expect(f.validate).not.toHaveBeenCalled(); f.assertNoEffects();
    } finally { f.close(); }
  });

  it("restores the recent conversation into a fresh session, so a daemon restart does not erase discovery", async () => {
    const f = fixture();
    try {
      const first = await f.daemon.chat(f.project.projectId, "We study early exit.", "chat-restore-first-0001");
      expect(f.prompt.mock.calls[0]![1]).toBe("We study early exit.");
      (f.daemon as unknown as { appendDraft(draft: unknown): void }).appendDraft({ $schema: "https://nosh.dev/schemas/event/v1", schemaVersion: 1, retention: "persistent", type: "agent.completed", source: "pi", scope: { projectId: f.project.projectId, missionId: null, directionId: null, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: first.agentId }, correlationId: null, causationId: null, payload: { message: "Which decision should it inform?" } });
      await f.daemon.chat(f.project.projectId, "Whether to ship it.", "chat-restore-second-0001");
      const restored = String(f.prompt.mock.calls[1]![1]);
      expect(restored).toContain("Conversation so far in this Project (restored after a session restart");
      expect(restored).toContain("User: We study early exit.\nNOSH: Which decision should it inform?");
      expect(restored.endsWith("Whether to ship it.")).toBe(true);
      expect(restored).not.toContain("User: Whether to ship it.");
    } finally { f.close(); }
  });
});

describe("discovery contract successor template", () => {
  it("is schema-valid once its EDIT placeholders are filled, keeps daemon facts, and prefills detected commands", async () => {
    const { contractSuccessorTemplate } = await import("./daemon.js");
    const { readProjectContract } = await import("@nosh/evidence");
    const { schemaUri, validateRecord } = await import("@nosh/wire");
    const directory = mkdtempSync(join(tmpdir(), "nosh-successor-"));
    const daemon = new NoshDaemon({ dataDirectory: join(directory, "data"), bootstrapToken: "test" });
    try {
      const project = daemon.initializeProject({ path: join(directory, "repository"), createRepository: true, workingTitle: "Successor fixture" });
      const draft = readProjectContract(project.repositoryRoot);
      const commands = [{ commandId: "command_test", description: "Unit tests", argv: ["node", "--test"], timeoutSeconds: 120 }];
      const template = contractSuccessorTemplate(draft, commands) as Record<string, unknown>;
      expect(template).toMatchObject({ projectId: draft.projectId, contractVersion: draft.contractVersion + 1, approvedAt: null, createdAt: draft.createdAt, execution: { runner: "native", commands } });
      expect(JSON.stringify(template)).toContain('"EDIT:');
      const filled = JSON.parse(JSON.stringify(template).replace(/"EDIT: a ref such as [^"]*"/g, '"contribution_empirical.evaluation"').replace(/"EDIT: [^"]*"/g, '"filled by discovery"'));
      expect(validateRecord(schemaUri("project-contract"), filled)).toMatchObject({ ok: true });
    } finally { daemon.stop(); rmSync(directory, { recursive: true, force: true }); }
  });
});

describe("one-step start", () => {
  it("walks a draft Mission to running under one approval, idempotently, and refuses a stale version", async () => {
    const f = fixture();
    try {
      const mission = f.daemon.research.createMission(f.project.projectId, { title: "Start fixture", objective: "Exercise start", deliverables: ["a result"], successCriteria: ["it runs"], idempotencyKey: "start-fixture-create-0001" });
      expect(() => f.daemon.research.start(f.project.projectId, "missions", mission.entityId, mission.version + 1, "start-fixture-stale-0001")).toThrow("Stale missions version");
      const started = f.daemon.research.start(f.project.projectId, "missions", mission.entityId, mission.version, "start-fixture-run-0001");
      expect(started).toMatchObject({ state: "running", version: mission.version + 3 });
      expect(f.daemon.research.start(f.project.projectId, "missions", mission.entityId, started.version, "start-fixture-run-0002")).toMatchObject({ state: "running", version: started.version });
      // A start refused by the one-active-Mission rule leaves the second Mission where it was, not half-walked.
      const second = f.daemon.research.createMission(f.project.projectId, { title: "Second", objective: "Wait its turn", deliverables: ["a result"], successCriteria: ["it runs"], idempotencyKey: "start-fixture-create-0002" });
      expect(() => f.daemon.research.start(f.project.projectId, "missions", second.entityId, second.version, "start-fixture-run-0003")).toThrow("one active Mission");
      expect(f.daemon.research.mission(f.project.projectId, second.entityId)).toMatchObject({ state: "draft", version: second.version });
    } finally { f.close(); }
  });
});
