import { GitWorkspaceManager } from "@nosh/git-workspaces";
import type { RegisteredProject } from "@nosh/persistence";
import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import type { ResearchControl } from "./research-control.js";

/** Root of every NOSH task/experiment worktree; the runtime starts workspace sessions here. */
export function taskWorktreePath(repositoryRoot: string, worktreeId: string): string {
  return join(repositoryRoot, ".nosh", "worktrees", worktreeId);
}

/**
 * Give one Mission/Direction task its own Git worktree and branch from the current checkout,
 * so workers never edit the user's main working tree. Creation is a durable external
 * operation, replayed idempotently after a restart.
 */
export function createTaskWorktree(research: ResearchControl, project: RegisteredProject, taskId: string, startingCommit: string): { worktreeId: string; branch: string; path: string } {
  const worktreeId = `wt_${taskId.slice(4)}`; const branch = `nosh/task-${taskId.slice(4, 16)}`;
  const worktrees = join(project.repositoryRoot, ".nosh", "worktrees"); const state = join(project.repositoryRoot, ".nosh", "git");
  // Self-ignoring directories keep the main checkout clean even without Project .gitignore entries.
  for (const directory of [worktrees, state]) { mkdirSync(directory, { recursive: true }); if (!existsSync(join(directory, ".gitignore"))) writeFileSync(join(directory, ".gitignore"), "*\n", "utf8"); }
  const intent = research.beginExternalOperation(project.projectId, "git.worktree.create", `git-task-worktree:${taskId}`, { branch, baseCommit: startingCommit, worktreeId });
  const path = new GitWorkspaceManager(project.repositoryRoot, worktrees, state).create(branch, startingCommit, worktreeId);
  research.completeExternalOperation(project.projectId, intent.intentId, { branch, worktree: path });
  return { worktreeId, branch, path };
}
