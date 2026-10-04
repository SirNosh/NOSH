import { createId } from "@nosh/core";
import { ModelRuntime } from "@earendil-works/pi-coding-agent";
import { fauxProvider, fauxAssistantMessage, type Api, type Model } from "@earendil-works/pi-ai";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { describe, expect, it } from "vitest";
import { PiAdapter, noshSystemPrompt, type PiSessionOptions } from "./index.js";

const roles: PiSessionOptions["role"][] = ["nosh", "mission_director", "research_director", "librarian_researcher", "general_worker", "reviewer"];
const packagePath = resolve(import.meta.dirname, "..", "..", "..", "pi-package");

describe("NOSH system prompt contract", () => {
  it.each(roles)("retains protected-state, approval, and scientific boundaries for %s", (role) => {
    const prompt = noshSystemPrompt(role);
    for (const rule of [
      "Tool availability is not permission", ".nosh/contracts/**", ".nosh/project.json", "active-contract pointers",
      "shell commands, scripts, or indirect delegation", "authorized source, paper, or experiment file edits",
      "Use the current canonical schema exactly", "do not silently discard them or invent extension keys",
      "User approval must refer to the concrete proposed content and version", "Material changes require renewed approval",
      "Never invent approvals", "references to existing records", "Generate new proposal IDs only",
      "Normal chat remains natural prose", "reported field/path and reason",
      "never retry blindly", "at most one schema-only correction", "must not repeat execution tools or side effects",
      "datasets and licenses", "evaluation and falsification criteria", "seed requirements", "immutable evaluated commits",
      "raw logs", "evidence-linked claims",
    ]) expect(prompt, `${role}: ${rule}`).toContain(rule);
  });

  it("gives each session only the guidance its tools need", () => {
    const session = { projectId: "prj_x", agentId: "agt_y", taskId: null };
    expect(noshSystemPrompt("nosh", session)).toContain("Project Nosh role only: use nosh_project_contract_submit");
    for (const role of roles.filter((entry) => entry !== "nosh")) expect(noshSystemPrompt(role, session)).not.toContain("nosh_project_contract_submit");
    const task = noshSystemPrompt("general_worker", { ...session, taskId: "tsk_z" });
    expect(task).not.toContain("nosh_runtime_instruct");
    expect(task).toContain("nosh_delegation_request");
    expect(noshSystemPrompt("mission_director", session)).toContain("Use nosh_runtime_instruct only when exposed and authorized");
  });

  it("describes the actual host, turn, and recovery boundaries without promising unavailable controls", () => {
    const prompt = noshSystemPrompt("nosh");
    for (const rule of [
      "noshd owns authorization", "SQLite records", "Pi supplies a replaceable model session",
      "Task Packet", "nosh_task_acknowledge", "exactly one episode-draft", "host-supplied terminal contract",
      "THREAD_OPEN/THREAD_STEP", "they do not create or approve a Mission or Direction",
      "Do not assume DIRECT_ACTION has a configured handler", "nosh_runtime_instruct", "nosh_delegation_request",
      "stopping a program does not cancel independent threads", "compact, partial context", "tool allowlists intersect",
      "daemon-managed Jobs", "not accepted Evidence", "not independent review or Mission acceptance",
      "same idempotency identity for an exact replay", "inspect authoritative status", "Do not relaunch work",
      "eligible parent Task Packet and lease", "Commit permission is not push permission",
      "deterministic postflight and independent review", "Link claims to accepted evidence",
      "cancellation request is not verified Job process exit",
    ]) expect(prompt, rule).toContain(rule);
  });

  it("gives each role its own bounded responsibilities", () => {
    expect(noshSystemPrompt("nosh")).toContain("Ask one focused question at a time");
    expect(noshSystemPrompt("nosh")).toContain("Read the current contract and canonical project-contract schema");
    expect(noshSystemPrompt("nosh")).toContain("An unapproved draft is not authorization to launch research");
    expect(noshSystemPrompt("mission_director")).toContain("Reconcile the stored Mission objective");
    expect(noshSystemPrompt("research_director")).toContain("do not change acceptance criteria after seeing results");
    expect(noshSystemPrompt("librarian_researcher")).toContain("distinguish source claims from established findings");
    expect(noshSystemPrompt("general_worker")).toContain("Request delegation rather than spawning agents");
    expect(noshSystemPrompt("reviewer")).toContain("Never return PASS with missing evidence");
    expect(noshSystemPrompt("reviewer")).toContain("without requiring a positive research outcome");
    expect(noshSystemPrompt("mission_director")).toContain("do not self-lease nodes");
    expect(noshSystemPrompt("research_director")).toContain("Do not self-lease or accept nodes");
  });

  it("keeps the control skill consistent with chat, runtime, and validation rules", () => {
    const skill = readFileSync(join(packagePath, "skills", "nosh-control", "SKILL.md"), "utf8");
    for (const rule of ["Normal chat remains prose", ".nosh/contracts/**", ".nosh/project.json", "concrete proposed content", "reported field/path", "do not add an Episode unless the host requests it", "Do not repeat execution tools", "Generate new proposal IDs only"]) expect(skill).toContain(rule);
  });

  it.each(["nosh", "mission_director", "general_worker"] as const)("passes the actual %s base prompt through the resource loader to Pi", async (role) => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-system-prompt-"));
    const faux = fauxProvider({ provider: "prompt-provider", api: "prompt-api", models: [{ id: "deterministic", reasoning: false }] });
    faux.setResponses([fauxAssistantMessage("What research question should we study?")]);
    let suppliedPrompt: string | undefined;
    const runtime = {
      hasConfiguredAuth: () => true, getAuth: async () => ({ auth: { apiKey: "fake" }, env: {} }),
      getAvailable: async () => faux.models, getProviders: () => [faux.provider],
      streamSimple: (model: Model<Api>, context: never, options: never) => {
        suppliedPrompt = (context as { systemPrompt?: string }).systemPrompt;
        return faux.provider.stream(model, context, options);
      },
    } as unknown as ModelRuntime;
    const adapter = new PiAdapter(() => undefined, undefined, runtime);
    const options: PiSessionOptions = {
      projectId: createId("prj"), agentId: createId("agt"), missionId: null, directionId: null,
      autoresearchId: null, experimentId: null, runId: null, jobId: null, taskId: null,
      role, cwd: directory, packagePath, model: { provider: "prompt-provider", id: "deterministic" },
    };
    try {
      await adapter.start(options);
      await adapter.prompt(options.agentId, "Ask a short question in normal chat.");
      expect(suppliedPrompt).toContain(noshSystemPrompt(role, options));
      expect(suppliedPrompt).toContain(`Session facts: projectId ${options.projectId}; your agentId ${options.agentId}`);
      expect(suppliedPrompt).toContain("nosh-control");
      expect(suppliedPrompt).not.toContain("TERMINAL OUTPUT CONTRACT");
    } finally {
      adapter.stop(options.agentId);
      rmSync(directory, { recursive: true, force: true });
    }
  });
});
