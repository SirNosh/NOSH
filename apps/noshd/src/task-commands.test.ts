import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createId } from "@nosh/core";
import { approveProjectContract, readProjectContract } from "@nosh/evidence";
import { schemaUri, validateRecord, type JsonValue } from "@nosh/wire";
import { describe, expect, it, vi } from "vitest";
import { NoshDaemon } from "./daemon.js";
import { citedRunIssues, writeUpOnly } from "./task-postflight.js";
import { createTaskWorktree } from "./task-worktree.js";

describe("nosh_run task commands", () => {
  it("runs only contract-declared commands, commits worker edits itself, and fills completion facts without model tool calls", async () => {
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
      const packet: Record<string, JsonValue> & { permissions: Record<string, JsonValue> } = { taskId, permissions: { subprocess: "allowlisted", allowedToolIds: ["tool_nosh.run"] }, workspace: { worktreeId: tree.worktreeId } };
      const records = daemon.research.records.bind(daemon.research);
      vi.spyOn(daemon.research, "records").mockImplementation((projectId, schema) => schema === "task-packet" ? [packet] : records(projectId, schema));
      expect(await daemon.submitTool("nosh_run", project.projectId, `task:${taskId}`, { commandId: "command_check" }, agentId)).toMatchObject({ accepted: false, error: expect.stringContaining("declares: none") });

      daemon.amendProjectContract(project.projectId, { ...approved, contractVersion: 3, execution: { runner: "native", commands: [
        { commandId: "command_check", description: "Exit with the code in exit.txt", argv: [process.execPath, "-e", "const c=Number(require('fs').readFileSync('exit.txt','utf8'));console.log('code '+c);process.exit(c)"], timeoutSeconds: 30 },
      ] } }, "amend-0001");
      // An edit-test iteration: the first run fails, the worker fixes it. Only the final state counts as the outcome.
      writeFileSync(join(tree.path, "exit.txt"), "1");
      expect(await daemon.submitTool("nosh_run", project.projectId, `task:${taskId}`, { commandId: "command_check" }, agentId)).toMatchObject({ accepted: true, exitCode: 1 });
      writeFileSync(join(tree.path, "exit.txt"), "0");
      const dirty = await daemon.submitTool("nosh_run", project.projectId, `task:${taskId}`, { commandId: "command_check" }, agentId) as Record<string, unknown>;
      expect(dirty).toMatchObject({ accepted: true, exitCode: 0, dirty: true, citable: false, commit: head });
      expect(String(dirty.stdoutTail)).toContain("code 0");

      // With commit authority, the daemon commits the worker's edits before each run, so every run is tied to an exact clean commit.
      Object.assign(packet, { attempt: 1, permissions: { network: "disabled", subprocess: "allowlisted", gitCommit: true, gitPush: false, delegation: "request_only", networkAllowlist: [], allowedToolIds: ["tool_nosh.run", "tool_nosh.git.commit", "tool_pi.read", "tool_pi.write"] }, workspace: { worktreeId: tree.worktreeId, branch: tree.branch, startingCommit: head, writeScopes: ["**"], protectedScopes: [".nosh/contracts/**"] } });
      const clean = await daemon.submitTool("nosh_run", project.projectId, `task:${taskId}`, { commandId: "command_check" }, agentId) as Record<string, unknown>;
      const worktreeGit = (args: string[]) => spawnSync("git", ["-C", tree.path, ...args], { encoding: "utf8", windowsHide: true }).stdout.trim();
      const ending = worktreeGit(["rev-parse", "HEAD"]);
      expect(ending).not.toBe(head);
      expect(clean).toMatchObject({ accepted: true, exitCode: 0, dirty: false, citable: true, commit: ending });
      expect(worktreeGit(["log", "-1", "--format=%an|%s"])).toBe(`NOSH general_worker|nosh: ${taskId} edits before command_check`);
      // The same command on the same clean commit answers from the record instead of launching another Job.
      const started = Date.now();
      const rerun = await daemon.submitTool("nosh_run", project.projectId, `task:${taskId}`, { commandId: "command_check" }, agentId) as Record<string, unknown>;
      expect(rerun).toMatchObject({ accepted: true, cached: true, jobId: clean.jobId, exitCode: 0, citable: true, commit: ending });
      expect(Date.now() - started).toBeLessThan(1_000);

      const runs = daemon.replay(project.projectId, 0).filter((event) => event.type === "task.command_run").map((event) => event.payload as Record<string, unknown>);
      expect(runs).toHaveLength(3);
      expect(citedRunIssues(runs, taskId, ending, [{ criterionId: "criterion_1", validatorRunIds: [String(clean.jobId)] }])).toEqual([]);
      expect(citedRunIssues(runs, taskId, ending, [{ criterionId: "criterion_1", validatorRunIds: [String(dirty.jobId)] }])[0]).toContain("clean ending commit");
      expect(citedRunIssues(runs, createId("tsk"), ending, [{ criterionId: "criterion_1", validatorRunIds: [String(clean.jobId)] }])[0]).toContain("not a nosh_run of this task");
      // A Job the daemon itself ran for the task (a Direction baseline evaluation) is an authoritative validator.
      expect(citedRunIssues(runs, taskId, ending, [{ criterionId: "criterion_1", validatorRunIds: ["job_daemonbaseline"] }], ["job_daemonbaseline"])).toEqual([]);
      expect(citedRunIssues(runs, taskId, ending, [{ criterionId: "criterion_1", validatorRunIds: ["job_daemonbaseline"] }])[0]).toContain("not a nosh_run of this task");

      // commands[] is daemon-owned: whatever shape the model wrote, the completion carries each command's latest run.
      const normalized = (daemon as unknown as { withDaemonCommands(context: unknown, record: Record<string, unknown>): Record<string, unknown> }).withDaemonCommands({ projectId: project.projectId, taskId }, { $schema: schemaUri("general-worker-completion"), commands: [{ commandId: "command_check", jobId: clean.jobId }] });
      expect(normalized.commands).toEqual([{ commandId: "command_check", displayCommand: expect.stringContaining("exit.txt"), exitCode: 0, resultArtifactId: null }]);

      // Final answer: the daemon commits the remaining edits, fills codeChanges and changedFiles, and snapshots "artifact:<path>" citations.
      writeFileSync(join(tree.path, "report.md"), "# report\n"); writeFileSync(join(tree.path, "notes.md"), "notes\n");
      const withFacts = (records: Array<Record<string, unknown>>) => (daemon as unknown as { withDaemonFacts(context: unknown, records: Array<Record<string, unknown>>): Array<Record<string, unknown>> }).withDaemonFacts({ projectId: project.projectId, taskId, agentId }, records);
      const submitted = [
        { $schema: schemaUri("general-worker-completion"), workPerformed: [{ subject: "artifact: prose with spaces is untouched", artifactIds: ["artifact:report.md"] }], codeChanges: { startingCommit: head, endingCommit: head, changedPaths: [], diffArtifactId: null, branch: tree.branch } },
        { $schema: schemaUri("episode-draft"), artifactIds: ["artifact:report.md"], changedFiles: [] },
      ];
      const [completion, episode] = withFacts(submitted) as [{ workPerformed: Array<{ subject: string; artifactIds: string[] }>; codeChanges: Record<string, unknown> }, { artifactIds: string[]; changedFiles: string[] }];
      const final = worktreeGit(["rev-parse", "HEAD"]);
      expect(final).not.toBe(ending);
      expect(worktreeGit(["status", "--porcelain"])).toBe("");
      expect(completion.codeChanges).toEqual({ startingCommit: head, endingCommit: final, changedPaths: expect.arrayContaining(["exit.txt", "notes.md", "report.md"]), diffArtifactId: expect.stringMatching(/^art_[0-9a-f]{32}$/), branch: tree.branch });
      // Each nosh_run is a daemon-produced Artifact cited by commands[] and by the criteria that name its Job.
      const withRuns = withFacts([{ $schema: schemaUri("general-worker-completion"), commands: [], criteria: [{ criterionId: "criterion_1", validatorRunIds: [String(clean.jobId)], artifactIds: [] }] }])[0] as { commands: Array<{ resultArtifactId: string }>; criteria: Array<{ artifactIds: string[] }> };
      const cleanRunArtifact = withRuns.commands[0]!.resultArtifactId;
      expect(withRuns.criteria[0]!.artifactIds).toEqual([cleanRunArtifact]);
      expect(await daemon.submitTool("nosh_artifact_read", project.projectId, `task:${taskId}`, { artifactId: cleanRunArtifact }, agentId)).toMatchObject({ accepted: true, kind: "artifact_run-result", content: expect.stringContaining(String(clean.jobId)) });
      // The exact change is a daemon-produced Artifact, so reviewers judge the code itself.
      expect(await daemon.submitTool("nosh_artifact_read", project.projectId, `task:${taskId}`, { artifactId: completion.codeChanges.diffArtifactId }, agentId)).toMatchObject({ accepted: true, kind: "artifact_diff", content: expect.stringContaining("+notes") });
      expect(completion.codeChanges.changedPaths).toHaveLength(3);
      expect(episode.changedFiles).toEqual(completion.codeChanges.changedPaths);
      const artifactId = completion.workPerformed[0]!.artifactIds[0]!;
      expect(artifactId).toMatch(/^art_[0-9a-f]{32}$/);
      expect(episode.artifactIds).toEqual([artifactId]);
      expect(completion.workPerformed[0]!.subject).toBe("artifact: prose with spaces is untouched");
      expect(daemon.research.records(project.projectId, "artifact").find((entry) => entry.artifactId === artifactId)).toMatchObject({ kind: "artifact_file", producer: { type: "agent", agentId, taskId, path: "report.md" } });
      expect(await daemon.submitTool("nosh_artifact_read", project.projectId, `task:${taskId}`, { artifactId }, agentId)).toMatchObject({ accepted: true, kind: "artifact_file", content: "# report\n", remainingCharacters: 0 });
      expect(await daemon.submitTool("nosh_artifact_read", project.projectId, `task:${taskId}`, { artifactId: "art_missing" }, agentId)).toMatchObject({ accepted: false });
      // Review Requests cite Evidence by evd_ id; the reader returns the record so a reviewer can follow it to its Artifacts.
      const evidence = daemon.research.submitUserEvidence(project.projectId, { evidenceType: "evidence_observation", statement: "The report exists.", polarity: "supports", sourceRefs: [{ refType: "ref_artifact", refId: artifactId, locator: "report.md" }], evaluationContractHash: null, scopeLimitations: [], quality: { status: "unreviewed", reviewId: null, confidence: "medium" } }, "evidence-read").payload as { evidenceId: string };
      expect(await daemon.submitTool("nosh_artifact_read", project.projectId, `task:${taskId}`, { artifactId: evidence.evidenceId }, agentId)).toMatchObject({ accepted: true, kind: "evidence", content: expect.stringContaining(artifactId) });
      expect(await daemon.submitTool("nosh_artifact_read", project.projectId, `task:${taskId}`, { artifactId: "evd_0123abcd" }, agentId)).toMatchObject({ accepted: false });
      // Candidate Evidence frozen for an open Review is readable until the Review ends.
      daemon.research.offerCandidateEvidence(project.projectId, { evidenceId: "evd_0123abcd", statement: "candidate" });
      expect(await daemon.submitTool("nosh_artifact_read", project.projectId, `task:${taskId}`, { artifactId: "evd_0123abcd" }, agentId)).toMatchObject({ accepted: true, kind: "evidence_candidate", content: expect.stringContaining("candidate") });
      daemon.research.withdrawCandidateEvidence(project.projectId, "evd_0123abcd");
      expect(await daemon.submitTool("nosh_artifact_read", project.projectId, `task:${taskId}`, { artifactId: "evd_0123abcd" }, agentId)).toMatchObject({ accepted: false });
      // A corrected or replayed answer is idempotent: same Artifact, no new commit.
      const [again] = withFacts(submitted) as [{ workPerformed: Array<{ artifactIds: string[] }>; codeChanges: { endingCommit: string } }];
      expect(again.workPerformed[0]!.artifactIds).toEqual([artifactId]);
      expect(again.codeChanges.endingCommit).toBe(final);
      // One file cited under different fields (report field and episode) is one Artifact with the most specific kind.
      const [librarian, librarianEpisode] = withFacts([{ $schema: schemaUri("librarian-completion"), reportArtifactId: "artifact:report.md" }, { $schema: schemaUri("episode-draft"), artifactIds: ["artifact:report.md"] }]) as [{ reportArtifactId: string }, { artifactIds: string[] }];
      expect(librarianEpisode.artifactIds).toEqual([librarian.reportArtifactId]);
      expect(daemon.research.records(project.projectId, "artifact").find((entry) => entry.artifactId === librarian.reportArtifactId)).toMatchObject({ kind: "artifact_report" });
      // A source's canonical URL is filled when derivable: a cited worktree file, or a DOI.
      const [sourced] = withFacts([{ $schema: schemaUri("librarian-completion"), sources: [{ sourceId: "source_1", artifactId: "artifact:report.md" }, { sourceId: "source_2", persistentId: "doi:10.1109/ICPR.2016.7900006" }, { sourceId: "source_3", canonicalUrl: "https://example.org/kept" }] }]) as [{ sources: Array<{ canonicalUrl?: string }> }];
      expect(sourced.sources.map((source) => source.canonicalUrl)).toEqual(["file:///report.md", "https://doi.org/10.1109/ICPR.2016.7900006", "https://example.org/kept"]);
      // A source may cite recorded Evidence: it becomes a readable snapshot Artifact of that record.
      const [fromEvidence] = withFacts([{ $schema: schemaUri("librarian-completion"), sources: [{ sourceId: "source_4", artifactId: evidence.evidenceId }] }]) as [{ sources: Array<{ artifactId: string; canonicalUrl: string }> }];
      expect(fromEvidence.sources[0]!.artifactId).toMatch(/^art_[0-9a-f]{32}$/);
      expect(fromEvidence.sources[0]!.canonicalUrl).toBe(`nosh://evidence/${evidence.evidenceId}`);
      expect(await daemon.submitTool("nosh_artifact_read", project.projectId, `task:${taskId}`, { artifactId: fromEvidence.sources[0]!.artifactId }, agentId)).toMatchObject({ accepted: true, content: expect.stringContaining(evidence.evidenceId) });
      expect(() => withFacts([{ $schema: schemaUri("librarian-completion"), sources: [{ sourceId: "source_5", artifactId: "evd_0123abcd" }] }])).toThrow("not recorded Evidence");
      // Citations must name files inside the worktree; the reason is correctable by the model.
      for (const [path, reason] of [["missing.md", "names no file"], ["../outside.md", "inside your worktree"], [".git/HEAD", "inside your worktree"]] as const) expect(() => withFacts([{ $schema: schemaUri("general-worker-completion"), workPerformed: [{ artifactIds: [`artifact:${path}`] }] }])).toThrow(reason);

      // Progress is a text line the daemon turns into a schema-valid progress-update (no model tool call).
      const recorded: unknown[] = [];
      vi.spyOn(daemon, "submitRecord").mockImplementation((tool, _projectId, attemptKey, record) => { recorded.push({ tool, attemptKey, record }); return { accepted: true } as ReturnType<typeof daemon.submitRecord>; });
      expect(await daemon.submitTool("nosh_progress_note", project.projectId, `task:${taskId}`, { note: "tests pass -> write the report" }, agentId)).toMatchObject({ accepted: true });
      const note = recorded[0] as { tool: string; attemptKey: string; record: Record<string, unknown> };
      expect(note).toMatchObject({ tool: "nosh_progress_emit", attemptKey: `task:${taskId}`, record: { taskId, agentId, attempt: 1, summary: "tests pass", nextOperation: "write the report" } });
      expect(validateRecord(schemaUri("progress-update"), note.record as never)).toMatchObject({ ok: true });
      expect(await daemon.submitTool("nosh_progress_note", project.projectId, `task:${createId("tsk")}`, { note: "spoofed" }, agentId)).toMatchObject({ accepted: false });

      // A session whose packet lacks subprocess authority cannot run anything.
      packet.permissions = { ...packet.permissions, subprocess: "disabled" };
      expect(await daemon.submitTool("nosh_run", project.projectId, `task:${taskId}`, { commandId: "command_check" }, agentId)).toMatchObject({ accepted: false, error: "This task is not authorized to run commands" });
    } finally { daemon.stop(); rmSync(directory, { recursive: true, force: true }); }
  }, 60_000);
});

describe("paper write-up after a cited run", () => {
  it("keeps a run valid when only docs/paper.md or docs/paper.bib changed since it, and not after any other change", () => {
    const repository = mkdtempSync(join(tmpdir(), "nosh-writeup-"));
    const git = (...args: string[]) => spawnSync("git", ["-C", repository, "-c", "user.name=t", "-c", "user.email=t@t", ...args], { encoding: "utf8" }).stdout.trim();
    try {
      git("init", "-q"); writeFileSync(join(repository, "a.txt"), "a"); git("add", "."); git("commit", "-qm", "base");
      const runCommit = git("rev-parse", "HEAD");
      mkdirSync(join(repository, "docs")); writeFileSync(join(repository, "docs", "paper.md"), "# results"); git("add", "."); git("commit", "-qm", "paper");
      const paper = git("rev-parse", "HEAD");
      writeFileSync(join(repository, "a.txt"), "changed"); git("commit", "-qam", "code");
      const code = git("rev-parse", "HEAD");
      const runs = [{ taskId: "tsk_1", jobId: "job_1", commit: runCommit, dirty: false, state: "completed", exitCode: 0 }];
      const cited = [{ criterionId: "criterion_1", validatorRunIds: ["job_1"] }];
      expect(citedRunIssues(runs, "tsk_1", paper, cited, [], writeUpOnly(repository))).toEqual([]);
      expect(citedRunIssues(runs, "tsk_1", code, cited, [], writeUpOnly(repository))[0]).toContain("only the paper write-up");
      expect(citedRunIssues(runs, "tsk_1", paper, cited)[0]).toContain("did not run on the clean ending commit");
    } finally { rmSync(repository, { recursive: true, force: true }); }
  });
});
