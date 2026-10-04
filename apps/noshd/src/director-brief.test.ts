import { createId } from "@nosh/core";
import { episodeTypeForRole, schemaUri, validateRecord } from "@nosh/wire";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { NoshDaemon } from "./daemon.js";
import { librarianNetworkAllowed } from "./research-control.js";
import { directionBrief, missionBrief } from "./director-brief.js";

describe("director briefs", () => {
  it("give the Research Director its scope state and daemon-allocated cycle identity", () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-brief-"));
    const daemon = new NoshDaemon({ dataDirectory: join(directory, "data"), bootstrapToken: "test" });
    try {
      const project = daemon.initializeProject({ path: join(directory, "repository"), createRepository: true, workingTitle: "Brief fixture" });
      const direction = daemon.research.createDirection(project.projectId, { question: "Does a bounded threshold improve score?", decisionUse: "Adopt one configuration or not.", evaluationContract: { primaryMetric: { name: "score", objective: "maximize", minimumEffect: 0.01 }, execution: { runner: "native", command: ["node", "evaluate.mjs"], resultPath: "metrics.json", timeoutSeconds: 60 } }, idempotencyKey: "brief-direction-0001" });
      const node = direction.value.nodes[0]!; const agentId = createId("agt");
      const { prompt, ids } = directionBrief({ research: daemon.research, project, direction, node, agentId });

      for (const expected of ["Does a bounded threshold improve score?", "Adopt one configuration or not.", direction.value.evaluationContractHash, `Selected by the deterministic scheduler to run next: ${node.id}`, "Failure ledger:", "Event digest", ids.cycleId, "research-director-cycle.v1.schema.json", episodeTypeForRole("research_director")]) expect(prompt).toContain(expected);
      for (const item of direction.value.nodes) expect(prompt).toContain(item.id);
      expect(ids.consumedEventRange.toSequence).toBeGreaterThanOrEqual(ids.consumedEventRange.fromSequence);
      expect(prompt).not.toContain("nosh_response_submit");

      // The templates a director copies must already validate exactly as the host will check them.
      const lines = prompt.split("\n");
      const template = (label: string) => JSON.parse(lines[lines.findIndex((line) => line.startsWith(label)) + 1]!) as Record<string, unknown>;
      const cycle = template("Record 1 ("), episode = template("Record 2 (");
      expect(cycle).toMatchObject({ cycleId: ids.cycleId, directionId: direction.entityId, directorAgentId: agentId, consumedEventRange: ids.consumedEventRange, submittedAt: ids.submittedAt });
      expect(validateRecord(schemaUri("research-director-cycle"), cycle)).toMatchObject({ ok: true });
      expect(validateRecord(schemaUri("episode-draft"), episode)).toMatchObject({ ok: true });
      expect(episode.episodeType).toBe(episodeTypeForRole("research_director"));
    } finally { daemon.stop(); rmSync(directory, { recursive: true, force: true }); }
  });

  it("record user steering durably and show it to the next Mission Director cycle", async () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-steer-"));
    const daemon = new NoshDaemon({ dataDirectory: join(directory, "data"), bootstrapToken: "test" });
    try {
      const project = daemon.initializeProject({ path: join(directory, "repository"), createRepository: true, workingTitle: "Steer fixture" });
      let mission = daemon.research.createMission(project.projectId, { title: "Steer", objective: "Steer durably", deliverables: ["Note"], successCriteria: ["Recorded"], idempotencyKey: "steer-mission" });
      for (const state of ["planning", "awaiting_approval", "running"] as const) mission = daemon.research.transitionMission(project.projectId, mission.entityId, mission.version, state, `steer-${state}`);
      // No Director session is live: the steer must still be accepted and recorded.
      await daemon.steerMission(project.projectId, mission.entityId, mission.version, "Prioritize the baseline check.", "steer-key-0001");
      const { prompt } = missionBrief({ research: daemon.research, project, mission: daemon.research.mission(project.projectId, mission.entityId), node: mission.value.nodes[0]!, agentId: createId("agt") });
      expect(prompt).toContain("User steering");
      expect(prompt).toContain("Prioritize the baseline check.");
      // Node churn after the inspected version does not invalidate a steer; a state change does.
      const inspected = daemon.research.mission(project.projectId, mission.entityId).version;
      const churned = daemon.research.transitionMissionNode(project.projectId, mission.entityId, inspected, mission.value.nodes[0]!.id, "leased", "steer-churn", { leaseId: createId("tsk"), ownerId: createId("agt"), version: inspected, expiresAt: new Date(Date.now() + 60_000).toISOString() });
      expect(churned.version).toBeGreaterThan(inspected);
      await daemon.steerMission(project.projectId, mission.entityId, inspected, "Still in scope.", "steer-key-0002");
      daemon.research.transitionMission(project.projectId, mission.entityId, churned.version, "pausing", "steer-pausing");
      await expect(daemon.steerMission(project.projectId, mission.entityId, inspected, "Too late.", "steer-key-0003")).rejects.toThrow("state changed after the inspected version");
    } finally { daemon.stop(); rmSync(directory, { recursive: true, force: true }); }
  });
});

describe("librarian network authority", () => {
  it("is denied under the default Project policy (network_user.approved)", () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-network-"));
    const daemon = new NoshDaemon({ dataDirectory: join(directory, "data"), bootstrapToken: "test" });
    try {
      const project = daemon.initializeProject({ path: join(directory, "repository"), createRepository: true, workingTitle: "Network fixture" });
      expect(librarianNetworkAllowed(project)).toBe(false);
      expect(librarianNetworkAllowed({ ...project, repositoryRoot: join(directory, "missing") })).toBe(false);
    } finally { daemon.stop(); rmSync(directory, { recursive: true, force: true }); }
  });
});

describe("librarian template", () => {
  it("cites files by path, is invalid until the daemon resolves every citation, then schema-valid", async () => {
    const { librarianInstructions, REGISTER_ARTIFACT } = await import("./task-templates.js");
    const taskId = createId("tsk"); const text = librarianInstructions(taskId, false);
    const envelope = JSON.parse(text.split("\n").at(-1)!) as { records: Array<Record<string, unknown>> };
    const record = envelope.records[0]!;
    expect(validateRecord(schemaUri("librarian-completion"), record as never).ok).toBe(false);
    // Report and bibliography paths are daemon-known and prefilled; only sources are the librarian's to name.
    expect(record).toMatchObject({ reportArtifactId: `artifact:research/${taskId}/report.md`, bibliographyArtifactId: `artifact:research/${taskId}/bibliography.bib` });
    expect(JSON.stringify(record)).toContain(REGISTER_ARTIFACT);
    const filled = JSON.parse(JSON.stringify(record).replace(/"artifact:[^"]+"/g, () => JSON.stringify(createId("art")))) as Record<string, unknown>;
    expect(validateRecord(schemaUri("librarian-completion"), filled as never)).toMatchObject({ ok: true });
    expect(validateRecord(schemaUri("episode-draft"), envelope.records[1] as never)).toMatchObject({ ok: true });
    expect(text).toContain("network is disabled");
  });
});

describe("worker templates", () => {
  it("are schema-valid as issued, so a worker only edits judgment", async () => {
    const { generalWorkerCompletionTemplate } = await import("./task-templates.js");
    const packet = { taskId: createId("tsk"), attempt: 1, assignedAgentId: createId("agt"), requiredOutputs: [{ outputId: "output_x" }], acceptanceCriteria: [{ criterionId: "dnode_x", validatorIds: ["validator_direction.postflight"], required: true }], lease: { leaseId: "lease_x" }, workspace: { startingCommit: "a".repeat(40), branch: "nosh/task-x" } };
    const evaluation = { jobId: createId("job"), state: "completed", exitCode: 0, displayCommand: "node evaluate.mjs", metrics: { score: 0.84 }, metricArtifactId: createId("art"), failure: null };
    for (const template of [generalWorkerCompletionTemplate(packet, null), generalWorkerCompletionTemplate(packet, evaluation)]) expect(validateRecord(schemaUri("general-worker-completion"), template)).toMatchObject({ ok: true });
    const { completionInstructions } = await import("./task-templates.js");
    const envelope = JSON.parse(completionInstructions(generalWorkerCompletionTemplate(packet, evaluation)).split("\n").at(-1)!) as { $schema: string; records: Array<Record<string, unknown>> };
    expect(envelope.$schema).toBe(schemaUri("terminal-output"));
    expect(validateRecord(schemaUri("general-worker-completion"), envelope.records[0])).toMatchObject({ ok: true });
    expect(validateRecord(schemaUri("episode-draft"), envelope.records[1])).toMatchObject({ ok: true });
    expect(envelope.records[1]!.episodeType).toBe(episodeTypeForRole("general_worker"));

    const { reviewerInstructions, UNDECIDED_STATUS, UNDECIDED_VERDICT } = await import("./task-templates.js");
    const request = { reviewId: createId("rev"), reviewRequestId: "request_x", reviewType: "task", target: { targetType: "graph_node", targetId: "dnode_x", targetVersion: 1 }, reviewerAgentId: createId("agt"), requiredArtifactIds: [evaluation.metricArtifactId], requiredEvidenceIds: [], criteria: [{ criterionId: "dnode_x" }] };
    const review = JSON.parse(reviewerInstructions(request).split("\n").at(-1)!) as { records: Array<Record<string, unknown>> };
    // As issued the verdict is undecided and must not validate; a reviewer has to choose.
    expect(validateRecord(schemaUri("review-verdict"), review.records[0])).toMatchObject({ ok: false });
    const decided = JSON.parse(JSON.stringify(review.records[0]).replaceAll(UNDECIDED_VERDICT, "PASS").replaceAll(UNDECIDED_STATUS, "PASS")) as Record<string, unknown>;
    expect(validateRecord(schemaUri("review-verdict"), decided)).toMatchObject({ ok: true });
    expect(validateRecord(schemaUri("episode-draft"), review.records[1])).toMatchObject({ ok: true });
  });
});
