import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createId } from "@nosh/core";
import { approveProjectContract, readProjectContract } from "@nosh/evidence";
import type { EventStore, RegisteredProject } from "@nosh/persistence";
import type { AgentInspection, PiAdapter, PiSessionOptions } from "@nosh/pi-adapter";
import { schemaUri, type EventDraft, type JsonValue } from "@nosh/wire";
import { describe, expect, it, vi } from "vitest";
import { NoshDaemon } from "./daemon.js";
import { DirectionSupervisor } from "./direction-supervisor.js";
import { MissionSupervisor } from "./mission-supervisor.js";

/** Directors that answer from the brief: Mission Directors fill spawn when the brief asks for a research child. */
class Directors {
  readonly sessions = new Map<string, PiSessionOptions>(); readonly prompts: string[] = [];
  constructor(private readonly submit: (session: PiSessionOptions, record: JsonValue) => void, private readonly spec: (nodeId: string, prompt: string) => JsonValue | undefined) {}
  inspect(): AgentInspection[] { return [...this.sessions.values()].map((session) => ({ ...session, piSessionId: `pi_${session.agentId}`, status: "running" })) as unknown as AgentInspection[]; }
  async start(options: PiSessionOptions): Promise<AgentInspection> { this.sessions.set(options.agentId, options); return this.inspect().find((agent) => agent.agentId === options.agentId)!; }
  stop(agentId: string): void { this.sessions.delete(agentId); }
  async prompt(agentId: string, prompt: string): Promise<void> {
    const session = this.sessions.get(agentId)!; this.prompts.push(prompt);
    const common = { cycleId: /"cycleId":"(cycle_[0-9a-f]+)"/.exec(prompt)?.[1], directorAgentId: agentId, observedGraphVersion: Number(/graph version (\d+)/.exec(prompt)?.[1]) };
    const missionNode = /Research child to specify: node (mnode_[0-9a-f]+)/.exec(prompt)?.[1]; const directionNode = /Autoresearch to specify for node ([a-z]+_[0-9a-z]+)/.exec(prompt)?.[1];
    const spawn = missionNode ? this.spec(missionNode, prompt) : directionNode ? this.spec(directionNode, prompt) : undefined;
    const record = session.role === "mission_director"
      ? { $schema: schemaUri("mission-director-cycle"), ...common, missionId: session.missionId, northStarCheck: { currentWorkContributes: true }, ...(spawn ? { spawn } : {}) }
      : { $schema: schemaUri("research-director-cycle"), ...common, directionId: session.directionId, evaluationContractHash: /sha256:[0-9a-f]{64}/.exec(prompt)?.[0] ?? "", questionCheck: { currentWorkContributes: true }, ...(spawn ? { spawn } : {}) };
    this.submit(session, record as JsonValue);
  }
}

function fixture(prefix: string) {
  const directory = mkdtempSync(join(tmpdir(), prefix));
  const daemon = new NoshDaemon({ dataDirectory: join(directory, "data"), bootstrapToken: "test" });
  const project = daemon.initializeProject({ path: join(directory, "repository"), createRepository: true, workingTitle: "Spawn fixture" });
  const git = (args: string[]) => spawnSync("git", ["-C", project.repositoryRoot, "-c", "user.name=T", "-c", "user.email=t@example.invalid", ...args], { encoding: "utf8", windowsHide: true }).stdout.trim();
  writeFileSync(join(project.repositoryRoot, "README.md"), "# fixture\n"); git(["add", "README.md"]); git(["commit", "-m", "initial"]);
  const approved = approveProjectContract(project.repositoryRoot, { ...readProjectContract(project.repositoryRoot), contractVersion: 2 });
  daemon.amendProjectContract(project.projectId, { ...approved, contractVersion: 3, execution: { runner: "native", commands: [
    { commandId: "command_evaluate", description: "Write the evaluation metrics", argv: [process.execPath, "-e", "require('fs').writeFileSync('metrics.json', JSON.stringify({ score: 0.5, f1: 0.9 }))"], timeoutSeconds: 60 },
  ] } }, "amend-commands-0001");
  const store = (daemon as unknown as { storeFor(projectId: string): EventStore }).storeFor(project.projectId);
  const emit = (draft: EventDraft) => { (daemon as unknown as { appendDraft(draft: EventDraft): void }).appendDraft(draft); };
  const submit = (session: PiSessionOptions, record: JsonValue) => { const draft: EventDraft = { $schema: schemaUri("event"), schemaVersion: 1, retention: "persistent", type: "record.submitted", source: "pi", scope: { projectId: session.projectId, missionId: session.missionId, directionId: session.directionId, autoresearchId: null, experimentId: null, runId: null, jobId: null, agentId: session.agentId }, correlationId: `task:${session.taskId}`, causationId: null, payload: record }; store.appendTerminalSubmission(session.taskId!, "fixture", (record as { $schema: string }).$schema, record, draft); };
  const jobs = { start: (spec: Parameters<NoshDaemon["startJob"]>[0]) => daemon.startJob(spec), get: (jobId: string) => daemon.jobs.get(jobId) };
  const running = (mission: ReturnType<NoshDaemon["research"]["createMission"]>) => { for (const next of ["planning", "awaiting_approval", "running"] as const) mission = daemon.research.transitionMission(project.projectId, mission.entityId, mission.version, next, `${mission.entityId}-${next}`); return mission; };
  const cleanup = () => { daemon.stop(); rmSync(directory, { recursive: true, force: true }); };
  return { daemon, project: project as RegisteredProject, emit, submit, jobs, running, cleanup };
}
const spec = (kind: string) => (nodeId: string): JsonValue => ({ nodeId, kind, question: `Does change ${nodeId} lower the score?`, decisionUse: kind === "research_direction" ? "Choose what the Mission builds on." : null, evaluation: { commandId: "command_evaluate", resultPath: "metrics.json", primaryMetric: { name: "score", objective: "minimize", minimumEffect: 0.01 }, guardrails: [{ name: "f1", direction: "maximize", threshold: -0.05 }], target: null, maximumNoProgressRounds: 1 }, familyTags: [`family-${nodeId.slice(-6)}`], scope: ["src/**"] });

describe("Mission research spawning", { timeout: 120_000 }, () => {
  it("spawns its Research Directions and its own Autoresearch from Director specs, with a measured baseline", async () => {
    const { daemon, project, emit, submit, jobs, running, cleanup } = fixture("nosh-spawn-");
    try {
      const mission = running(daemon.research.createMission(project.projectId, { title: "Research plan", objective: "Answer two questions and improve a metric", deliverables: ["Result"], successCriteria: ["Reviewed"], researchPlan: { directions: 2, missionAutoresearch: true }, idempotencyKey: "plan-create" }));
      expect(mission.value.budgets.maximumModelTokens).toBe(4_000_000);
      const spawnNodes = mission.value.nodes.filter((node) => ["research_direction", "autoresearch"].includes(node.type));
      expect(spawnNodes.map((node) => node.type).sort()).toEqual(["autoresearch", "research_direction", "research_direction"]);
      expect(mission.value.nodes.find((node) => node.type === "implementation")!.hardDependencies.sort()).toEqual(spawnNodes.map((node) => node.id).sort());
      const directors = new Directors(submit, (nodeId) => spec(spawnNodes.find((node) => node.id === nodeId)!.type)(nodeId));
      const supervisor = new MissionSupervisor(daemon.research, directors as unknown as PiAdapter, () => [project], ".", emit, undefined, jobs);
      for (const deadline = Date.now() + 90_000; Date.now() < deadline && daemon.research.mission(project.projectId, mission.entityId).value.nodes.filter((node) => node.state === "working").length < 3;) { await supervisor.tick(); await new Promise((resolve) => setTimeout(resolve, 50)); }
      const directions = daemon.research.directions(project.projectId); const runs = daemon.research.autoresearch(project.projectId);
      expect(directions).toHaveLength(2); expect(runs).toHaveLength(1);
      for (const direction of directions) expect(direction.value).toMatchObject({ createdBy: "mission_director", missionId: mission.entityId, origin: { scopeType: "mission", scopeId: mission.entityId, attempt: 1 } });
      expect(directions.every((direction) => direction.state === "active")).toBe(true);
      expect(runs[0]!.value).toMatchObject({ directionId: null, missionId: mission.entityId, invokedByRole: "mission_director", state: "running" });
      // The baseline is what the declared command measured, never a number from the Director.
      expect((runs[0]!.value.evaluationContract as { baselineMetrics: Record<string, number> }).baselineMetrics).toEqual({ score: 0.5, f1: 0.9 });
      // A spawn guardrail is a change from that baseline: "f1 may drop 0.05" freezes as f1 >= 0.85.
      expect((runs[0]!.value.evaluationContract as { guardrails: unknown[] }).guardrails).toEqual([{ name: "f1", direction: "maximize", threshold: 0.85 }]);
      expect(directors.prompts.some((prompt) => prompt.includes("command_evaluate:"))).toBe(true);
      const nodes = daemon.research.mission(project.projectId, mission.entityId).value.nodes.filter((node) => node.state === "working");
      expect(nodes.map((node) => node.lease?.ownerId).sort()).toEqual([...directions.map((direction) => direction.entityId), runs[0]!.entityId].sort());
      // A fresh supervisor (a restart) leaves child-owned nodes alone and spawns nothing twice.
      await new MissionSupervisor(daemon.research, directors as unknown as PiAdapter, () => [project], ".", emit, undefined, jobs).tick();
      expect(daemon.research.directions(project.projectId)).toHaveLength(2);
      expect(daemon.research.mission(project.projectId, mission.entityId).value.nodes.filter((node) => node.state === "working")).toHaveLength(3);
    } finally { cleanup(); }
  });

  it("feeds a rejected spec back to the Director and refuses a spawn the budget cannot carry", async () => {
    const { daemon, project, emit, submit, jobs, running, cleanup } = fixture("nosh-spawn-reject-");
    try {
      const mission = running(daemon.research.createMission(project.projectId, { title: "Reject", objective: "Specify correctly", deliverables: ["Result"], successCriteria: ["Reviewed"], researchPlan: { directions: 1, missionAutoresearch: false }, idempotencyKey: "reject-create" }));
      const directors = new Directors(submit, (nodeId) => ({ ...(spec("research_direction")(nodeId) as Record<string, JsonValue>), evaluation: { ...((spec("research_direction")(nodeId) as { evaluation: Record<string, JsonValue> }).evaluation), commandId: "command_missing" } }));
      const supervisor = new MissionSupervisor(daemon.research, directors as unknown as PiAdapter, () => [project], ".", emit, undefined, jobs);
      await supervisor.tick(); await new Promise((resolve) => setTimeout(resolve, 300)); await supervisor.tick(); await new Promise((resolve) => setTimeout(resolve, 300));
      const rejected = daemon.research.scopedEvents(project.projectId, mission.entityId, "research.spawn_rejected");
      expect(rejected.length).toBeGreaterThanOrEqual(1);
      expect(JSON.stringify(rejected[0]!.payload)).toContain("command_missing is not a command declared in the Project contract");
      expect(directors.prompts.at(-1)).toContain("command_missing is not a command declared");
      expect(directors.prompts.at(-1)).toContain("change from the measured baseline");
      const { spawnIssues } = await import("./research-spawner.js");
      const nodeId = mission.value.nodes.find((node) => node.type === "research_direction")!.id;
      const demanding = { ...(spec("research_direction")(nodeId) as Record<string, JsonValue>), evaluation: { ...((spec("research_direction")(nodeId) as { evaluation: Record<string, JsonValue> }).evaluation), guardrails: [{ name: "f1", direction: "maximize", threshold: 0.005 }] } };
      expect(spawnIssues(project, demanding as never, nodeId, "research_direction").join(" ")).toContain("guardrail f1");
      expect(daemon.research.directions(project.projectId)).toHaveLength(0);

      // One active Mission per Project: finish the first before the tight one.
      const stopping = daemon.research.transitionMission(project.projectId, mission.entityId, daemon.research.mission(project.projectId, mission.entityId).version, "stopping", "reject-stop");
      daemon.research.transitionMission(project.projectId, mission.entityId, stopping.version, "stopped", "reject-stopped");
      const tight = running(daemon.research.createMission(project.projectId, { title: "Tight", objective: "Too little budget", deliverables: ["Result"], successCriteria: ["Reviewed"], researchPlan: { directions: 2, missionAutoresearch: true }, budgets: { maximumModelTokens: 600_000 }, idempotencyKey: "tight-create" }));
      await new MissionSupervisor(daemon.research, directors as unknown as PiAdapter, () => [project], ".", emit, undefined, jobs).tick(); await new Promise((resolve) => setTimeout(resolve, 300));
      expect(JSON.stringify(daemon.research.scopedEvents(project.projectId, tight.entityId, "research.spawn_rejected")[0]?.payload)).toContain("budget_insufficient");
      expect(daemon.research.mission(project.projectId, tight.entityId).value.nodes.some((node) => node.state === "blocked")).toBe(true);
    } finally { cleanup(); }
  });

  it("tells the Director which JSON file the command wrote and which metric keys it holds, even when the file is gitignored", async () => {
    const { daemon, project, emit, submit, jobs, running, cleanup } = fixture("nosh-spawn-hint-");
    try {
      writeFileSync(join(project.repositoryRoot, ".gitignore"), "metrics.json\n");
      spawnSync("git", ["-C", project.repositoryRoot, "-c", "user.name=T", "-c", "user.email=t@example.invalid", "add", ".gitignore"], { windowsHide: true });
      spawnSync("git", ["-C", project.repositoryRoot, "-c", "user.name=T", "-c", "user.email=t@example.invalid", "commit", "-qm", "ignore metrics"], { windowsHide: true });
      const mission = running(daemon.research.createMission(project.projectId, { title: "Hint", objective: "Find the metrics file", deliverables: ["Result"], successCriteria: ["Reviewed"], researchPlan: { directions: 1, missionAutoresearch: false }, idempotencyKey: "hint-create" }));
      const wrong = [{ resultPath: "results/dev.json", metric: "score" }, { resultPath: "metrics.json", metric: "ece" }];
      let call = 0;
      const directors = new Directors(submit, (nodeId) => { const base = spec("research_direction")(nodeId) as { evaluation: Record<string, JsonValue> }; const pick = wrong[Math.min(call++, wrong.length - 1)]!; return { ...base, evaluation: { ...base.evaluation, resultPath: pick.resultPath, primaryMetric: { name: pick.metric, objective: "minimize", minimumEffect: 0.01 }, guardrails: [] } } as JsonValue; });
      const supervisor = new MissionSupervisor(daemon.research, directors as unknown as PiAdapter, () => [project], ".", emit, undefined, jobs);
      for (const deadline = Date.now() + 60_000; Date.now() < deadline && daemon.research.scopedEvents(project.projectId, mission.entityId, "research.spawn_rejected").length < 2;) { await supervisor.tick(); await new Promise((resolve) => setTimeout(resolve, 50)); }
      const [absent, missingKey] = daemon.research.scopedEvents(project.projectId, mission.entityId, "research.spawn_rejected").map((entry) => JSON.stringify(entry.payload));
      expect(absent).toContain("results/dev.json"); expect(absent).toContain("metrics.json");
      // Files the checkout itself brought in are not outputs of the command.
      expect(absent).not.toContain(".nosh/project.json");
      expect(missingKey).toContain("score"); expect(missingKey).toContain("f1");
      expect(directors.prompts.at(-1)).toContain("metrics.json");
    } finally { cleanup(); }
  });

  it("re-runs an interrupted task session under the same task ID, as resuming after a daemon restart does", async () => {
    const { daemon, project, running, cleanup } = fixture("nosh-session-resume-");
    try {
      const mission = running(daemon.research.createMission(project.projectId, { title: "Resume", objective: "Resume a task session", deliverables: ["Result"], successCriteria: ["Reviewed"], idempotencyKey: "resume-create" }));
      vi.spyOn(daemon.agents, "start").mockImplementation(async (options) => ({ ...options, piSessionId: `pi_${options.agentId}`, status: "running" }) as unknown as AgentInspection);
      vi.spyOn(daemon.agents, "prompt").mockResolvedValue(); vi.spyOn(daemon.agents, "stop").mockImplementation(() => undefined);
      const owner = { missionId: mission.entityId, directionId: null, autoresearchId: null };
      const direction = { entityId: createId("dir"), value: { missionId: mission.entityId } };
      type Session = { runSession(...args: unknown[]): Promise<void>; session(...args: unknown[]): Promise<void> };
      const missions = daemon.missions as unknown as Session; const directions = daemon.directions as unknown as Session; const experiments = daemon.experiments as unknown as Session;
      const sessions = [
        (taskId: string, agentId: string) => missions.runSession(project, { agentId, taskId, missionId: mission.entityId, role: "general_worker" }, "Do the task"),
        (taskId: string, agentId: string) => directions.session(project, direction, agentId, taskId, "general_worker", "Do the task"),
        (taskId: string, agentId: string) => experiments.session(project, owner, { agentId, taskId, experimentId: null, role: "general_worker", cwd: project.repositoryRoot }, "Do the task"),
      ];
      for (const run of sessions) {
        // The first session is cut short (here: no record submitted); the resumed one reuses its task and agent IDs.
        const taskId = createId("tsk"); const agentId = createId("agt");
        for (const attempt of [1, 2]) { const error = await run(taskId, agentId).then(() => null, (failure: unknown) => failure as Error); expect({ attempt, message: error?.message ?? "" }).not.toMatchObject({ message: expect.stringContaining("idempotency key was reused") }); }
      }
    } finally { vi.restoreAllMocks(); cleanup(); }
  });

  it("accepts a spawn node when its child closes and retries it when the child stops", async () => {
    const { daemon, project, emit, submit, jobs, running, cleanup } = fixture("nosh-spawn-resolve-");
    try {
      const mission = running(daemon.research.createMission(project.projectId, { title: "Resolve", objective: "Map children onto nodes", deliverables: ["Result"], successCriteria: ["Reviewed"], researchPlan: { directions: 2, missionAutoresearch: false }, idempotencyKey: "resolve-create" }));
      const directors = new Directors(submit, (nodeId) => spec("research_direction")(nodeId));
      const supervisor = new MissionSupervisor(daemon.research, directors as unknown as PiAdapter, () => [project], ".", emit, undefined, jobs);
      for (const deadline = Date.now() + 90_000; Date.now() < deadline && daemon.research.directions(project.projectId).length < 2;) { await supervisor.tick(); await new Promise((resolve) => setTimeout(resolve, 50)); }
      const [closed, stopped] = daemon.research.directions(project.projectId);
      // Closure needs a reviewed closure packet; the mapping only reads the child's terminal state.
      const direction = daemon.research.direction.bind(daemon.research);
      vi.spyOn(daemon.research, "direction").mockImplementation((projectId, id) => { const value = direction(projectId, id); return id === closed!.entityId ? { ...value, state: "closed" } : value; });
      daemon.research.transitionDirection(project.projectId, stopped!.entityId, stopped!.version, "stopped", "child-stopped");
      await supervisor.tick();
      const nodes = daemon.research.mission(project.projectId, mission.entityId).value.nodes;
      expect(nodes.find((node) => node.id === closed!.value.origin!.nodeId)?.state).toBe("accepted");
      const retried = nodes.find((node) => node.id === stopped!.value.origin!.nodeId)!;
      expect(retried).toMatchObject({ state: "ready", attempt: 1 });
      expect(daemon.research.scopedEvents(project.projectId, mission.entityId, "mission.child_resolved").map((entry) => (entry.payload as { outcome: string }).outcome).sort()).toEqual(["accepted", "failed"]);
      // The stopped child's unused share returns to the pool, so the retried node can spawn a replacement.
      for (const deadline = Date.now() + 60_000; Date.now() < deadline && daemon.research.directions(project.projectId).length < 3 && !daemon.research.scopedEvents(project.projectId, mission.entityId, "research.spawn_rejected").length;) { await supervisor.tick(); await new Promise((resolve) => setTimeout(resolve, 50)); }
      expect(daemon.research.scopedEvents(project.projectId, mission.entityId, "research.spawn_rejected").map((entry) => entry.payload)).toEqual([]);
      expect(daemon.research.directions(project.projectId)).toHaveLength(3);
    } finally { vi.restoreAllMocks(); cleanup(); }
  });

  it("lets a Mission-spawned Direction start its own Autoresearch, and stops its children when the Mission stops", async () => {
    const { daemon, project, emit, submit, jobs, running, cleanup } = fixture("nosh-spawn-direction-");
    try {
      const mission = running(daemon.research.createMission(project.projectId, { title: "Direction child", objective: "Let the Direction search", deliverables: ["Result"], successCriteria: ["Reviewed"], researchPlan: { directions: 1, missionAutoresearch: false }, idempotencyKey: "direction-child-create" }));
      let directionSpecs = 0;
      const directors = new Directors(submit, (nodeId) => nodeId.startsWith("dnode_") ? { nodeId, kind: "autoresearch", question: `Distinct question ${++directionSpecs}`, familyTags: [`family-${directionSpecs}`], scope: ["src/**"] } : spec("research_direction")(nodeId));
      const supervisor = new MissionSupervisor(daemon.research, directors as unknown as PiAdapter, () => [project], ".", emit, undefined, jobs);
      for (const deadline = Date.now() + 90_000; Date.now() < deadline && !daemon.research.directions(project.projectId).length;) { await supervisor.tick(); await new Promise((resolve) => setTimeout(resolve, 50)); }
      let child = daemon.research.directions(project.projectId)[0]!;
      // Stand in for the reviewed baseline, then let the Direction reach its hypothesis node.
      const direction = daemon.research.direction.bind(daemon.research);
      vi.spyOn(daemon.research, "direction").mockImplementation((projectId, id) => { const value = direction(projectId, id); return id === child.entityId ? { ...value, value: { ...value.value, acceptedBaseline: { commit: value.value.baseCommit!, reviewId: "rev_fixture" } } } : value; });
      const baseline = child.value.nodes.find((node) => node.title === "Reproduce accepted baseline")!;
      const path = ["pending", "ready", "leased", "working", "postflight", "reviewing", "accepted"] as const;
      for (let step = path.indexOf(baseline.state as typeof path[number]) + 1; step < path.length; step += 1) { child = daemon.research.direction(project.projectId, child.entityId); child = daemon.research.transitionDirectionNode(project.projectId, child.entityId, child.version, baseline.id, path[step]!, `fixture-baseline-${path[step]}`, path[step] === "leased" ? { leaseId: "tsk_fixture", ownerId: "agt_fixture", version: child.version, expiresAt: new Date(Date.now() + 60_000).toISOString() } : undefined); }
      const directions = new DirectionSupervisor(daemon.research, directors as unknown as PiAdapter, () => [project], ".", emit, undefined, jobs);
      for (const deadline = Date.now() + 60_000; Date.now() < deadline && !daemon.research.autoresearch(project.projectId).length;) { await directions.tick(); await new Promise((resolve) => setTimeout(resolve, 50)); }
      const runs = daemon.research.autoresearch(project.projectId);
      expect(runs).toHaveLength(1);
      expect(runs[0]!.value).toMatchObject({ directionId: child.entityId, invokedByRole: "research_director", decisionQuestion: child.value.question, origin: { scopeType: "direction", scopeId: child.entityId } });
      // An Autoresearch left blocked with nobody working on it must not hold the Direction forever.
      const blocked = daemon.research.transitionAutoresearch(project.projectId, runs[0]!.entityId, daemon.research.autoresearchExecution(project.projectId, runs[0]!.entityId).version, "blocked", "fixture-ar-blocked");
      const now = Date.now(); vi.spyOn(Date, "now").mockReturnValue(now + 11 * 60_000);
      await directions.tick();
      expect(daemon.research.autoresearchExecution(project.projectId, blocked.entityId).state).toBe("stopped");
      expect(daemon.research.scopedEvents(project.projectId, mission.entityId, "direction.autoresearch_stopped").map((entry) => (entry.payload as { autoresearchId: string }).autoresearchId)).toEqual([blocked.entityId]);
      // Runs stopped before evaluating anything did no research, so they do not use up the Direction's Autoresearch cap.
      const nextRun = async (count: number) => { for (let tick = 0; tick < 40 && daemon.research.autoresearch(project.projectId).length < count; tick += 1) { await directions.tick(); await new Promise((resolve) => setTimeout(resolve, 50)); } };
      await nextRun(2);
      const second = daemon.research.autoresearch(project.projectId).find((run) => run.entityId !== blocked.entityId)!;
      expect(second).toBeDefined();
      daemon.research.transitionAutoresearch(project.projectId, second.entityId, second.version, "blocked", "fixture-ar-blocked-2");
      await nextRun(3);
      expect(daemon.research.autoresearch(project.projectId)).toHaveLength(3);
      vi.restoreAllMocks();
      await daemon.controlMission(project.projectId, mission.entityId, daemon.research.mission(project.projectId, mission.entityId).version, "stop", "immediate", "stop-mission-0001");
      expect(daemon.research.direction(project.projectId, child.entityId).state).toBe("stopped");
      expect(daemon.research.autoresearchExecution(project.projectId, runs[0]!.entityId).state).toBe("stopped");
    } finally { vi.restoreAllMocks(); cleanup(); }
  });
});
