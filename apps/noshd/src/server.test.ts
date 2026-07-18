import { createId } from "@nosh/core";
import { schemaUri } from "@nosh/wire";
import { mkdtempSync, mkdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
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
    const token = "test-bootstrap-token";
    const projectId = createId("prj");
    const daemon = new NoshDaemon({ dataDirectory: join(directory, "data"), bootstrapToken: token });
    const server = new LocalApiServer(daemon);
    daemon.start();
    await server.listen({ port: 0 });
    const base = server.address();
    const headers = { authorization: `Bearer ${token}`, "content-type": "application/json" };

    try {
      expect((await fetch(`${base}/projects`)).status).toBe(401);
      const registered = await fetch(`${base}/projects`, {
        method: "POST",
        headers,
        body: JSON.stringify({ projectId, repositoryRoot: repository, databasePath: join(directory, "data", "projects", projectId, "nosh.sqlite") }),
      });
      expect(registered.status).toBe(201);

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
      expect((await events.json() as { events: unknown[] }).events).toHaveLength(1);
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
