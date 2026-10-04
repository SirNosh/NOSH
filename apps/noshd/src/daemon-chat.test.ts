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
});
