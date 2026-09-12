import { mkdirSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { EventStore } from "@nosh/persistence";
import { createId } from "@nosh/core";
import { schemaUri, type JsonValue } from "@nosh/wire";
import type { TerminalContext } from "@nosh/pi-adapter";
import { describe, expect, it, vi } from "vitest";
import { NoshDaemon } from "./daemon.js";

const context: TerminalContext = { projectId: createId("prj"), agentId: createId("agt"), taskId: createId("tsk"), instructionId: "ins_test", threadId: "thr_test", expectedVersion: 2, turnId: "ins_test", allowedTools: ["nosh_response_submit", "nosh_episode_submit"], expectedEpisodeType: "task_execution" };
const records = [{ $schema: schemaUri("general-worker-completion"), schemaVersion: 1 }, { $schema: schemaUri("episode-draft"), schemaVersion: 1 }];
function host(store: EventStore, validate = vi.fn()) {
  const daemon = Object.create(NoshDaemon.prototype) as NoshDaemon;
  Object.assign(daemon, { terminalTurns: new Map(), agents: { terminalTurnActive: () => true }, assertProjectWritable: () => {}, storeFor: () => store, validateTerminal: validate });
  return daemon;
}

describe("durable terminal host journal", () => {
  it("fails closed after an admitted generation is interrupted before rejection", async () => {
    const store = new EventStore(":memory:");
    try {
      expect(await host(store).admitTerminal(context)).toBeUndefined();
      expect(await host(store).admitTerminal(context)).toMatchObject({ accepted: false, retryAllowed: false });
      expect(await host(store).submitTerminal(context, records)).toMatchObject({ accepted: false });
    } finally { store.close(); }
  });

  it("reports an explicit permanently closed partial acceptance when the second record is refused", async () => {
    const store = new EventStore(":memory:");
    try {
      const daemon = host(store);
      const submit = vi.spyOn(daemon, "submitTool").mockResolvedValueOnce({ accepted: true }).mockResolvedValueOnce({ accepted: false, retryAllowed: true });
      expect(await daemon.submitTerminal(context, records)).toMatchObject({ accepted: false, retryAllowed: false, status: "partial", results: [{ accepted: true }, { accepted: false }] });
      expect(await daemon.admitTerminal(context)).toMatchObject({ accepted: false, retryAllowed: false, status: "partial" });
      expect(submit).toHaveBeenCalledTimes(2);
    } finally { store.close(); }
  });

  it("checks cancellation at the serialized commit boundary", async () => {
    const store = new EventStore(":memory:");
    try {
      const daemon = host(store); let active = true;
      vi.spyOn(daemon.agents, "terminalTurnActive").mockImplementation(() => active);
      const submit = vi.spyOn(daemon, "submitTool");
      const pending = daemon.submitTerminal(context, records);
      active = false;
      expect(await pending).toMatchObject({ accepted: false, retryAllowed: false, status: "rejected" });
      expect(submit).not.toHaveBeenCalled();
    } finally { store.close(); }
  });

  it("uses real daemon validation and durable task authority, then recovers the event/journal crash window without a live agent", async () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-terminal-real-"));
    const repository = join(directory, "repository"); mkdirSync(repository);
    const databasePath = join(directory, "project.sqlite");
    const daemon = new NoshDaemon({ dataDirectory: join(directory, "data"), bootstrapToken: "test" });
    const taskContext: TerminalContext = { ...context, instructionId: null, threadId: null, expectedVersion: null, turnId: `task:${context.taskId}` };
    delete taskContext.expectedEpisodeType;
    const record = { $schema: schemaUri("general-worker-completion"), schemaVersion: 1, taskOutcome: "completed", workPerformed: [], codeChanges: { startingCommit: "a".repeat(40), endingCommit: "a".repeat(40), changedPaths: [], diffArtifactId: null, branch: "main" }, commands: [], criteria: [], scientificImpact: { claimIds: [], evidenceIds: [], interpretation: "No scientific claims." }, deviations: [], newRisks: [], unresolvedItems: [], suggestedNextActions: [], readyForDeterministicPostflight: true, readyForReview: false };
    try {
      daemon.registerProject({ projectId: context.projectId, repositoryRoot: repository, databasePath });
      // Only provider/session liveness is a fixture. Validation, research policy,
      // submission gate, event acceptance and SQLite task authority are real.
      vi.spyOn(daemon.agents, "inspect").mockReturnValue([{ agentId: context.agentId, projectId: context.projectId, taskId: context.taskId, role: "general_worker" }] as ReturnType<typeof daemon.agents.inspect>);
      vi.spyOn(daemon.agents, "terminalTurnActive").mockReturnValue(true);
      expect(await daemon.admitTerminal(taskContext)).toBeUndefined();
      expect(await daemon.submitTerminal(taskContext, [{ ...record, schemaVersion: 2 }])).toMatchObject({ accepted: false, retryAllowed: true });
      expect(daemon.replay(context.projectId, 0).filter(event => event.type === "record.submitted")).toHaveLength(0);
      expect(await daemon.admitTerminal(taskContext)).toBeUndefined();
      expect(await daemon.submitTerminal(taskContext, [record])).toMatchObject({ accepted: true, status: "completed" });
      expect(daemon.research.terminalRecord(context.projectId, context.taskId!)).toEqual(record);
      const store = new EventStore(databasePath);
      try {
        const journal = store.terminalTurn(context.projectId, taskContext.turnId, taskContext as unknown as JsonValue);
        // Simulate death after event append but before saving its journal result.
        store.saveTerminalTurn(context.projectId, taskContext.turnId, { ...journal, results: [], closed: false, status: "pending" });
      } finally { store.close(); }
      vi.mocked(daemon.agents.inspect).mockReturnValue([]);
      vi.mocked(daemon.agents.terminalTurnActive).mockReturnValue(false);
      expect(await daemon.admitTerminal(taskContext)).toMatchObject({ accepted: true, status: "completed" });
      expect(daemon.replay(context.projectId, 0).filter(event => event.type === "record.submitted")).toHaveLength(1);
    } finally { daemon.stop(); rmSync(directory, { recursive: true, force: true }); }
  });

  it("persists one correction across reopening and never revives an exhausted turn", async () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-terminal-")); const path = join(directory, "store.sqlite");
    let store = new EventStore(path);
    try {
      expect(await host(store).rejectTerminal(context, "bad JSON")).toMatchObject({ retryAllowed: true });
      store.close(); store = new EventStore(path);
      const daemon = host(store); const commit = vi.spyOn(daemon, "submitTool");
      expect(await daemon.rejectTerminal(context, "bad JSON again")).toMatchObject({ retryAllowed: false });
      expect(await daemon.submitTerminal(context, records)).toMatchObject({ accepted: false, retryAllowed: false });
      expect(commit).not.toHaveBeenCalled();
    } finally { store.close(); rmSync(directory, { recursive: true, force: true }); }
  });

  it("freezes intent before effects, resumes accepted prefix, and rejects rewritten replay", async () => {
    const store = new EventStore(":memory:");
    try {
      const daemon = host(store); let interrupted = true;
      const submit = vi.spyOn(daemon, "submitTool").mockImplementation(async (_tool, _project, attempt) => {
        expect(store.terminalTurn(context.projectId, context.turnId, context as unknown as JsonValue).records).toEqual(records);
        if (attempt === `instruction:${context.instructionId}` && interrupted) { interrupted = false; throw new Error("interrupted"); }
        return { accepted: true, effect: { state: "failed", error: "effect failed after acceptance" } };
      });
      expect(await daemon.submitTerminal(context, records)).toMatchObject({ accepted: false, status: "partial", retryAllowed: false });
      const replacement = records.map(record => ({ ...record, schemaVersion: 2 }));
      expect(await daemon.submitTerminal(context, replacement)).toMatchObject({ accepted: false, error: "Terminal envelope replay conflict" });
      expect(await daemon.submitTerminal(context, records)).toMatchObject({ accepted: true, status: "completed", effect: { state: "failed" } });
      expect(submit).toHaveBeenCalledTimes(3);
      expect(await daemon.submitTerminal(context, records)).toMatchObject({ accepted: true });
      expect(submit).toHaveBeenCalledTimes(3);
      await expect(daemon.submitTerminal({ ...context, agentId: "agt_other" }, records)).rejects.toThrow("context replay conflict");
    } finally { store.close(); }
  });

  it("prevalidates the whole batch before any acceptance and serializes identical concurrent submissions", async () => {
    const store = new EventStore(":memory:");
    try {
      const validate = vi.fn().mockImplementationOnce(() => { throw new Error("Invalid terminal record: invalid second record"); });
      const daemon = host(store, validate); const submit = vi.spyOn(daemon, "submitTool").mockResolvedValue({ accepted: true });
      expect(await daemon.submitTerminal(context, records)).toMatchObject({ accepted: false, retryAllowed: true });
      expect(submit).not.toHaveBeenCalled();
      const receipts = await Promise.all([daemon.submitTerminal(context, records), daemon.submitTerminal(context, records)]);
      expect(receipts.every(receipt => receipt.accepted)).toBe(true);
      expect(submit).toHaveBeenCalledTimes(2);
    } finally { store.close(); }
  });

  it("admits at most two provider generations across host replacement", async () => {
    const store = new EventStore(":memory:");
    try {
      expect(await host(store).admitTerminal(context)).toBeUndefined();
      expect(await host(store).rejectTerminal(context, "bad JSON")).toMatchObject({ retryAllowed: true });
      expect(await host(store).admitTerminal(context)).toBeUndefined();
      expect(await host(store).admitTerminal(context)).toMatchObject({ accepted: false, retryAllowed: false, status: "rejected" });
      expect(await host(store).submitTerminal(context, records)).toMatchObject({ accepted: false });
    } finally { store.close(); }
  });

  it("provider failure permanently closes a turn before accepting any apparent JSON", async () => {
    const store = new EventStore(":memory:");
    try {
      const daemon = host(store); const submit = vi.spyOn(daemon, "submitTool");
      expect(await daemon.rejectTerminal(context, "Non-successful provider stop: aborted")).toMatchObject({ retryAllowed: false });
      expect(await daemon.submitTerminal(context, records)).toMatchObject({ accepted: false });
      expect(submit).not.toHaveBeenCalled();
    } finally { store.close(); }
  });
});
