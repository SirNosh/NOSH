import { schemaUri, type EventEnvelope } from "@nosh/wire";
import type { AgentInspection } from "@nosh/pi-adapter";
import type { JobRecord } from "@nosh/jobs";
import { describe, expect, it } from "vitest";
import { buildResearchMap, type MapNode } from "./research-map.js";

const node = (id: string, state: string, extra: Record<string, unknown> = {}) => ({ id, title: `Node ${id}`, type: "implementation", state, attempt: 1, maximumAttempts: 3, lease: null, ...extra });
const event = (type: string, payload: Record<string, unknown>) => ({ type, payload }) as unknown as EventEnvelope;
const submitted = (record: Record<string, unknown>) => ({ event: event("record.submitted", record), record });

describe("research map", () => {
  it("shows Missions, Directions with nested Autoresearch experiments, lease-holding workers, and running Jobs", () => {
    const worker = { agentId: "agt_w", role: "general_worker", status: "running", currentTool: "nosh_run", modelId: "gpt-6-luna", thinkingLevel: "low", contextPercent: 23.4, experimentId: null } as unknown as AgentInspection;
    const implementer = { agentId: "agt_i", role: "general_worker", status: "running", currentTool: null, modelId: null, thinkingLevel: "low", contextPercent: null, experimentId: "exp_c" } as unknown as AgentInspection;
    const job = { jobId: "job_1", command: ["node", "evaluate.mjs"], state: "running", runner: "native", missionId: null, directionId: "dir_1", autoresearchId: "ar_1" } as unknown as JobRecord;
    const map = buildResearchMap({
      missions: [
        { entityId: "mis_done", version: 9, state: "completed", value: { title: "Old mission", nodes: [node("m1", "accepted")], updatedAt: "2026-10-01T00:00:00.000Z" } },
        { entityId: "mis_1", version: 4, state: "running", value: { title: "Boundary tests", nodes: [node("m1", "accepted"), node("m2", "working", { lease: { ownerId: "agt_w" } }), node("m3", "failed", { attempt: 2 })], updatedAt: "2026-10-03T00:00:00.000Z" } },
      ],
      directions: [{ entityId: "dir_1", version: 3, state: "awaiting_autoresearch", value: { question: "Does early exit help?", nodes: [node("d1", "accepted")], updatedAt: "2026-10-02T00:00:00.000Z" } }],
      autoresearch: [{ entityId: "ar_1", version: 5, state: "running", value: { directionId: "dir_1", decisionQuestion: "Tune threshold", currentRound: 2, maximumRounds: 3, maximumExperiments: 6 } }],
      submitted: [
        ...["exp_a", "exp_b", "exp_c", "exp_d", "exp_e"].map((experimentId, index) => submitted({ $schema: schemaUri("experiment-proposal"), experimentId, autoresearchId: "ar_1", round: index < 2 ? 1 : 2, hypothesis: `Hypothesis ${experimentId}` })),
        submitted({ $schema: schemaUri("experiment-result"), experimentId: "exp_a", promotionDecision: "promoted", comparison: { candidateScore: 0.86, improvement: 0.02 }, guardrails: [{ passed: true }] }),
        submitted({ $schema: schemaUri("experiment-result"), experimentId: "exp_b", promotionDecision: "rejected", comparison: { candidateScore: 0.9, improvement: 0.06 }, guardrails: [{ passed: false }] }),
        submitted({ $schema: schemaUri("run-manifest"), experimentId: "exp_d" }),
      ],
      events: [event("autoresearch.experiment_failed", { experimentId: "exp_e", phase: "implementation" })],
      agents: [worker, implementer],
      jobs: [job, { ...job, jobId: "job_done", state: "completed" }],
    });
    // Live work first; finished work after it.
    expect(map.roots.map((root) => root.id)).toEqual(["mis_1", "dir_1", "mis_done"]);
    const mission = map.roots[0]!;
    expect(mission).toMatchObject({ kind: "mission", title: "Boundary tests", state: "running", progress: { done: 1, total: 3 } });
    expect(mission.children[1]).toMatchObject({ kind: "node", state: "working", children: [{ kind: "worker", id: "agt_w", state: "running nosh_run", detail: "gpt-6-luna:low · ctx 23%" }] });
    expect(mission.children[2]).toMatchObject({ state: "failed", detail: "implementation · attempt 2/3" });
    const autoresearch = map.roots[1]!.children.find((child) => child.kind === "autoresearch")!;
    expect(autoresearch).toMatchObject({ id: "ar_1", detail: "round 2/3 · 5/6 experiments · best 0.86" });
    expect(Object.fromEntries(autoresearch.children.map((child) => [child.id, child.state]))).toEqual({ exp_a: "promoted", exp_b: "rejected", exp_c: "implementing", exp_d: "evaluating", exp_e: "failed", job_1: "running" });
    expect(autoresearch.children.find((child) => child.id === "exp_b")!.detail).toBe("round 1 · score 0.9 (+0.06) · guardrail failed");
    expect(autoresearch.children.find((child) => child.id === "exp_e")!.detail).toBe("round 2 · failed in implementation");
    // A Direction's Autoresearch is nested, never repeated as a root; finished Jobs are not shown.
    expect(map.roots.filter((root) => root.kind === "autoresearch")).toEqual([]);
    expect(JSON.stringify(map)).not.toContain("job_done");
    expect(map.workers.map((entry) => entry.id)).toEqual(["agt_w", "agt_i"]);
  });

  it("nests Mission-spawned Directions and Autoresearch under their spawning nodes, each exactly once", () => {
    const origin = (scopeType: "mission" | "direction", scopeId: string, nodeId: string) => ({ scopeType, scopeId, nodeId, attempt: 1 });
    const job = { jobId: "job_d", command: ["node", "evaluate.mjs"], state: "running", runner: "native", missionId: "mis_2", directionId: "dir_a", autoresearchId: "ar_d" } as unknown as JobRecord;
    const map = buildResearchMap({
      missions: [{ entityId: "mis_2", version: 7, state: "running", value: { title: "Early exit", updatedAt: "2026-10-05T00:00:00.000Z", nodes: [
        node("n_spawn1", "working", { type: "research_direction", lease: { ownerId: "dir_a" } }),
        node("n_spawn2", "accepted", { type: "research_direction" }),
        node("n_ar", "working", { type: "autoresearch", lease: { ownerId: "ar_m" } }),
        node("n_spawn3", "working", { type: "research_direction", lease: { ownerId: "dir_lease" } }),
      ] } }],
      directions: [
        { entityId: "dir_a", version: 2, state: "awaiting_autoresearch", value: { question: "Does early exit help?", missionId: "mis_2", origin: origin("mission", "mis_2", "n_spawn1"), nodes: [node("h1", "working", { type: "hypothesis" }), node("h2", "ready", { type: "hypothesis" })] } },
        { entityId: "dir_b", version: 1, state: "planning", value: { question: "Is the threshold stable?", missionId: "mis_2", origin: origin("mission", "mis_2", "n_spawn2"), nodes: [] } },
        { entityId: "dir_c", version: 1, state: "planning", value: { question: "User-created under the Mission", missionId: "mis_2", nodes: [] } },
        { entityId: "dir_lease", version: 1, state: "planning", value: { question: "Spawned before origin existed", missionId: "mis_2", nodes: [] } },
        { entityId: "dir_user", version: 1, state: "planning", value: { question: "Unrelated question", nodes: [], updatedAt: "2026-10-04T00:00:00.000Z" } },
      ],
      autoresearch: [
        { entityId: "ar_d", version: 1, state: "running", value: { directionId: "dir_a", missionId: "mis_2", origin: origin("direction", "dir_a", "h1"), decisionQuestion: "Tune exit threshold" } },
        { entityId: "ar_m", version: 1, state: "running", value: { directionId: null, missionId: "mis_2", origin: origin("mission", "mis_2", "n_ar"), decisionQuestion: "Mission-level sweep" } },
      ],
      submitted: [submitted({ $schema: schemaUri("experiment-proposal"), experimentId: "exp_d", autoresearchId: "ar_d", round: 1, hypothesis: "Raise threshold" })],
      events: [], agents: [], jobs: [job],
    });
    const paths = (entry: MapNode, prefix = ""): string[] => { const path = `${prefix}${entry.id}`; return [path, ...entry.children.flatMap((child) => paths(child, `${path}/`))]; };
    const all = map.roots.flatMap((root) => paths(root));
    expect(map.roots.map((root) => root.id)).toEqual(["mis_2", "dir_user"]);
    expect(all).toEqual(expect.arrayContaining([
      "mis_2/n_spawn1/dir_a/h1/ar_d/exp_d", "mis_2/n_spawn1/dir_a/h1/ar_d/job_d", "mis_2/n_spawn2/dir_b",
      "mis_2/n_ar/ar_m", "mis_2/n_spawn3/dir_lease", "mis_2/dir_c",
    ]));
    // Every Direction and Autoresearch appears exactly once in the whole map.
    for (const id of ["dir_a", "dir_b", "dir_c", "dir_lease", "dir_user", "ar_d", "ar_m"]) expect(all.filter((path) => path.split("/").at(-1) === id)).toHaveLength(1);
    const spawned = map.roots[0]!.children[0]!.children.find((child) => child.id === "dir_a")!;
    expect(spawned).toMatchObject({ kind: "direction", title: "Does early exit help?", detail: "v2", progress: { done: 0, total: 2 } });
    expect(spawned.children.map((child) => child.id)).toEqual(["h1", "h2"]);
  });
});
