import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createId } from "@nosh/core";
import { approveProjectContract, readProjectContract } from "@nosh/evidence";
import { schemaUri } from "@nosh/wire";
import { describe, expect, it, vi } from "vitest";
import { NoshDaemon } from "./daemon.js";
import { citedRunIssues } from "./task-postflight.js";
import { createTaskWorktree } from "./task-worktree.js";

describe("nosh_run task commands", () => {
  it("runs only contract-declared commands in the task worktree and makes only clean final-commit passes citable", async () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-run-"));
    const daemon = new NoshDaemon({ dataDirectory: join(directory, "data"), bootstrapToken: "test" });
    try {
      const project = daemon.initializeProject({ path: join(directory, "repository"), createRepository: true, workingTitle: "Run fixture" });
      const git = (args: string[]) => spawnSync("git", ["-C", project.repositoryRoot, ...args], { encoding: "utf8", windowsHide: true }).stdout.trim();
      const draft = readProjectContract(project.repositoryRoot);
      const approved = approveProjectContract(project.repositoryRoot, { ...draft, contractVersion: 2 });
      // Before amendment no command is declared, so nothing can run.
      const taskId = createId("tsk"); const agentId = createId("agt");
      writeFileSync(join(project.repositoryRoot, "README.md"), "# fixture\n"); git(["add", "README.md"]); git(["-c", "user.name=T", "-c", "user.email=t@example.invalid", "commit", "-m", "initial"]);
      const head = git(["rev-parse", "HEAD"]);
      const tree = createTaskWorktree(daemon.research, project, taskId, head);
      vi.spyOn(daemon.agents, "inspect").mockReturnValue([{ agentId, projectId: project.projectId, taskId, role: "general_worker", missionId: null, directionId: null, autoresearchId: null, experimentId: null }] as unknown as ReturnType<typeof daemon.agents.inspect>);
      const packet = { taskId, permissions: { subprocess: "allowlisted", allowedToolIds: ["tool_nosh.run"] }, workspace: { worktreeId: tree.worktreeId } };
      const records = daemon.research.records.bind(daemon.research);
      vi.spyOn(daemon.research, "records").mockImplementation((projectId, schema) => schema === "task-packet" ? [packet] : records(projectId, schema));
      expect(await daemon.submitTool("nosh_run", project.projectId, `task:${taskId}`, { commandId: "command_check" }, agentId)).toMatchObject({ accepted: false, error: expect.stringContaining("declares: none") });

      daemon.amendProjectContract(project.projectId, { ...approved, contractVersion: 3, execution: { runner: "native", commands: [
        { commandId: "command_check", description: "Exit with the code in exit.txt", argv: [process.execPath, "-e", "const c=Number(require('fs').readFileSync('exit.txt','utf8'));console.log('code',c);process.exit(c)"], timeoutSeconds: 30 },
      ] } }, "amend-0001");
      writeFileSync(join(tree.path, "exit.txt"), "0");
      const dirty = await daemon.submitTool("nosh_run", project.projectId, `task:${taskId}`, { commandId: "command_check" }, agentId) as Record<string, unknown>;
      expect(dirty).toMatchObject({ accepted: true, exitCode: 0, dirty: true, citable: false, commit: head });
      expect(String(dirty.stdoutTail)).toContain("code 0");

      spawnSync("git", ["-C", tree.path, "add", "exit.txt"], { windowsHide: true });
      spawnSync("git", ["-C", tree.path, "-c", "user.name=T", "-c", "user.email=t@example.invalid", "commit", "-m", "exit"], { windowsHide: true });
      const ending = spawnSync("git", ["-C", tree.path, "rev-parse", "HEAD"], { encoding: "utf8", windowsHide: true }).stdout.trim();
      const clean = await daemon.submitTool("nosh_run", project.projectId, `task:${taskId}`, { commandId: "command_check" }, agentId) as Record<string, unknown>;
      expect(clean).toMatchObject({ accepted: true, exitCode: 0, dirty: false, citable: true, commit: ending });

      const runs = daemon.replay(project.projectId, 0).filter((event) => event.type === "task.command_run").map((event) => event.payload as Record<string, unknown>);
      expect(runs).toHaveLength(2);
      expect(citedRunIssues(runs, taskId, ending, [{ criterionId: "criterion_1", validatorRunIds: [String(clean.jobId)] }])).toEqual([]);
      expect(citedRunIssues(runs, taskId, ending, [{ criterionId: "criterion_1", validatorRunIds: [String(dirty.jobId)] }])[0]).toContain("clean ending commit");
      expect(citedRunIssues(runs, createId("tsk"), ending, [{ criterionId: "criterion_1", validatorRunIds: [String(clean.jobId)] }])[0]).toContain("not a nosh_run of this task");

      // commands[] is daemon-owned: whatever shape the model wrote, the completion carries exactly this task's runs.
      const normalized = (daemon as unknown as { withDaemonCommands(context: unknown, record: Record<string, unknown>): Record<string, unknown> }).withDaemonCommands({ projectId: project.projectId, taskId }, { $schema: schemaUri("general-worker-completion"), commands: [{ commandId: "command_check", jobId: clean.jobId }] });
      expect(normalized.commands).toEqual([{ commandId: "command_check", displayCommand: expect.stringContaining("exit.txt"), exitCode: 0, resultArtifactId: null }, { commandId: "command_check", displayCommand: expect.any(String), exitCode: 0, resultArtifactId: null }]);

      // Any task with a workspace can snapshot its own files as Artifacts, nothing outside the worktree.
      writeFileSync(join(tree.path, "report.md"), "# report\n");
      const registered = await daemon.submitTool("nosh_artifact_register", project.projectId, `task:${taskId}`, { path: "report.md", kind: "artifact_report" }, agentId) as Record<string, unknown>;
      expect(registered).toMatchObject({ accepted: true, path: "report.md", kind: "artifact_report", artifactId: expect.stringMatching(/^art_/) });
      expect(daemon.research.records(project.projectId, "artifact").some((entry) => entry.artifactId === registered.artifactId)).toBe(true);
      expect(await daemon.submitTool("nosh_artifact_read", project.projectId, `task:${taskId}`, { artifactId: registered.artifactId }, agentId)).toMatchObject({ accepted: true, kind: "artifact_report", content: "# report\n", remainingCharacters: 0 });
      expect(await daemon.submitTool("nosh_artifact_read", project.projectId, `task:${taskId}`, { artifactId: "art_missing" }, agentId)).toMatchObject({ accepted: false });
      for (const [path, kind] of [["../outside.md", "artifact_report"], [".git/HEAD", "artifact_report"], ["report.md", "report"]]) expect(await daemon.submitTool("nosh_artifact_register", project.projectId, `task:${taskId}`, { path, kind }, agentId)).toMatchObject({ accepted: false });

      // A session whose packet lacks subprocess authority cannot run anything.
      packet.permissions.subprocess = "disabled";
      expect(await daemon.submitTool("nosh_run", project.projectId, `task:${taskId}`, { commandId: "command_check" }, agentId)).toMatchObject({ accepted: false, error: "This task is not authorized to run commands" });
    } finally { daemon.stop(); rmSync(directory, { recursive: true, force: true }); }
  }, 60_000);
});
