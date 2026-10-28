import { GitWorkspaceManager, worktreeGitEnvironment } from "@nosh/git-workspaces";
import type { RegisteredProject } from "@nosh/persistence";
import type { EventEnvelope } from "@nosh/wire";
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { resumableAttempt, type ResearchControl } from "./research-control.js";

/** Root of every NOSH task/experiment worktree; the runtime starts workspace sessions here. */
export function taskWorktreePath(repositoryRoot: string, worktreeId: string): string {
  return join(repositoryRoot, ".nosh", "worktrees", worktreeId);
}

/**
 * Give one Mission/Direction task its own Git worktree and branch from the current checkout,
 * so workers never edit the user's main working tree. Creation is a durable external
 * operation, replayed idempotently after a restart. A resumed retry is checked out at its
 * earlier attempt's commit while its Task Packet keeps `startingCommit` as the diff base.
 */
export function createTaskWorktree(research: ResearchControl, project: RegisteredProject, taskId: string, startingCommit: string, checkoutCommit = startingCommit): { worktreeId: string; branch: string; path: string } {
  const worktreeId = `wt_${taskId.slice(4)}`; const branch = `nosh/task-${taskId.slice(4, 16)}`;
  const worktrees = join(project.repositoryRoot, ".nosh", "worktrees"); const state = join(project.repositoryRoot, ".nosh", "git");
  // Self-ignoring directories keep the main checkout clean even without Project .gitignore entries.
  for (const directory of [worktrees, state]) { mkdirSync(directory, { recursive: true }); if (!existsSync(join(directory, ".gitignore"))) writeFileSync(join(directory, ".gitignore"), "*\n", "utf8"); }
  const intent = research.beginExternalOperation(project.projectId, "git.worktree.create", `git-task-worktree:${taskId}`, { branch, baseCommit: startingCommit, worktreeId, ...(checkoutCommit !== startingCommit ? { checkoutCommit } : {}) });
  const path = new GitWorkspaceManager(project.repositoryRoot, worktrees, state).create(branch, checkoutCommit, worktreeId);
  research.completeExternalOperation(project.projectId, intent.intentId, { branch, worktree: path });
  return { worktreeId, branch, path };
}

/**
 * Where a retried worker resumes: the ending commit of the node's latest postflight-passed attempt, when that commit
 * still contains the head (every integrated change). When such an attempt exists but cannot be resumed, why not.
 */
export function resumePoint(research: ResearchControl, project: RegisteredProject, events: EventEnvelope[], nodeId: string, head: string): { taskId: string; commit: string } | { skipped: { taskId: string; commit: string | null; reason: "missing_commit" | "head_moved" } } | null {
  const attempt = resumableAttempt(events, nodeId); if (!attempt) return null;
  const completion = research.terminalRecord(project.projectId, attempt.taskId, "general-worker-completion") as { codeChanges?: { endingCommit?: string } } | undefined;
  const commit = completion?.codeChanges?.endingCommit ?? null; if (commit === head) return null;
  const git = (args: string[]) => spawnSync("git", ["-C", project.repositoryRoot, ...args], { encoding: "utf8", windowsHide: true, env: worktreeGitEnvironment(project.repositoryRoot) }).status === 0;
  if (!commit || !git(["cat-file", "-e", `${commit}^{commit}`])) return { skipped: { taskId: attempt.taskId, commit, reason: "missing_commit" } };
  if (!git(["merge-base", "--is-ancestor", head, commit])) return { skipped: { taskId: attempt.taskId, commit, reason: "head_moved" } };
  return { taskId: attempt.taskId, commit };
}

/** Tells a resumed worker that its worktree already holds the earlier attempt's work, and that its runs must be its own. */
export function resumeInstruction(commit: string): string {
  return `Your worktree already contains an earlier attempt's work at this node (commit ${commit.slice(0, 12)}), which passed postflight. Keep what is correct and fix what the failures above name; do not start over. Re-run the declared commands and cite those runs: an earlier attempt's runs do not count for this one.`;
}
