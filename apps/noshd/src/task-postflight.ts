import { worktreeGitEnvironment } from "@nosh/git-workspaces";
import { spawnSync } from "node:child_process";

export type GitCompletion = { startingCommit: string; endingCommit: string; changedPaths: string[]; branch: string };
export type GitTaskWorkspace = { startingCommit: string; branch: string; writeScopes: string[]; protectedScopes: string[] };

export function gitChangedPaths(cwd: string, startingCommit: string, endingCommit: string): string[] {
  const result = spawnSync("git", ["-C", cwd, "diff", "--name-status", "-z", "--find-renames", startingCommit, endingCommit], { encoding: "utf8", windowsHide: true, env: worktreeGitEnvironment(cwd) });
  if (result.status !== 0) throw new Error(result.stderr.trim() || `Git postflight diff failed: status ${result.status ?? "spawn-error"}`);
  const fields = result.stdout.split("\0");
  const paths: string[] = [];
  for (let index = 0; index < fields.length;) {
    const status = fields[index++];
    if (!status) continue;
    const count = ["R", "C"].includes(status[0] ?? "") ? 2 : 1;
    for (let pathIndex = 0; pathIndex < count; pathIndex += 1) {
      const path = fields[index++];
      if (!path) throw new Error("Git postflight diff has an incomplete path record");
      paths.push(path);
    }
  }
  return [...new Set(paths)];
}

/** HEAD of a task worktree and whether it has uncommitted or untracked (non-ignored) changes. */
export function gitWorktreeState(cwd: string): { head: string; dirty: boolean } {
  const git = (args: string[]): string => { const result = spawnSync("git", ["-C", cwd, ...args], { encoding: "utf8", windowsHide: true, env: worktreeGitEnvironment(cwd) }); if (result.status !== 0) throw new Error(result.stderr.trim() || `git ${args[0]} failed`); return result.stdout; };
  return { head: git(["rev-parse", "HEAD"]).trim(), dirty: git(["status", "--porcelain", "--untracked-files=all"]).trim().length > 0 };
}

/** A cited job_ validator must be this task's own nosh_run that passed on the ending commit with a clean worktree. */
export function citedRunIssues(runs: Array<{ taskId?: unknown; jobId?: unknown; commit?: unknown; dirty?: unknown; state?: unknown; exitCode?: unknown }>, taskId: string, endingCommit: string, criteria: Array<{ criterionId: string; validatorRunIds: string[] }>): string[] {
  const issues: string[] = [];
  for (const criterion of criteria) for (const id of criterion.validatorRunIds.filter((value) => value.startsWith("job_"))) {
    const run = runs.find((entry) => entry.jobId === id && entry.taskId === taskId);
    if (!run) issues.push(`criterion ${criterion.criterionId} cites ${id}, which is not a nosh_run of this task`);
    else if (run.state !== "completed" || run.exitCode !== 0) issues.push(`criterion ${criterion.criterionId} cites ${id}, which did not pass (exit ${String(run.exitCode)})`);
    else if (run.dirty !== false || run.commit !== endingCommit) issues.push(`criterion ${criterion.criterionId} cites ${id}, which did not run on the clean ending commit ${endingCommit}`);
  }
  return issues;
}

/** citedRunIssues for a general-worker completion record; other records cite no runs. */
export function completionRunIssues(runs: Parameters<typeof citedRunIssues>[0], taskId: string, completion: unknown): string[] {
  const value = completion as { codeChanges?: { endingCommit?: unknown }; criteria?: Array<{ criterionId?: unknown; validatorRunIds?: unknown }> } | null;
  if (!value || typeof value.codeChanges?.endingCommit !== "string" || !Array.isArray(value.criteria)) return [];
  return citedRunIssues(runs, taskId, value.codeChanges.endingCommit, value.criteria.map((criterion) => ({ criterionId: String(criterion.criterionId), validatorRunIds: Array.isArray(criterion.validatorRunIds) ? criterion.validatorRunIds.map(String) : [] })));
}

export function validateGitCompletion(cwd: string, workspace: GitTaskWorkspace, codeChanges: GitCompletion): string[] {
  const issues: string[] = []; const add = (issue: string) => { if (!issues.includes(issue)) issues.push(issue); };
  if (codeChanges.startingCommit !== workspace.startingCommit) add("starting commit differs from Task Packet");
  if (codeChanges.branch !== workspace.branch) add("branch differs from Task Packet");
  const git = (args: string[]): string | null => { const result = spawnSync("git", ["-C", cwd, ...args], { encoding: "utf8", windowsHide: true, env: worktreeGitEnvironment(cwd) }); if (result.status !== 0) { const detail = result.stderr.trim() || `status ${result.status ?? "spawn-error"}`; add(`Git ${args.join(" ")} failed: ${detail}`); return null; } return result.stdout.trim(); };
  const actualBranch = git(["symbolic-ref", "--quiet", "--short", "HEAD"]); if (actualBranch !== null && actualBranch !== workspace.branch) add("checked-out branch differs from Task Packet");
  const ending = git(["rev-parse", `${codeChanges.endingCommit}^{commit}`]); const head = git(["rev-parse", "HEAD"]); if (ending === null || head === null || ending !== codeChanges.endingCommit || head !== codeChanges.endingCommit) add("ending commit is not the checked-out immutable HEAD");
  const ancestry = spawnSync("git", ["-C", cwd, "merge-base", "--is-ancestor", workspace.startingCommit, codeChanges.endingCommit], { encoding: "utf8", windowsHide: true, env: worktreeGitEnvironment(cwd) }); if (ancestry.status === 1) add("starting commit is not an ancestor of ending commit"); else if (ancestry.status !== 0) add(`Git merge-base --is-ancestor failed: ${ancestry.stderr.trim() || `status ${ancestry.status ?? "spawn-error"}`}`);
  const status = git(["status", "--porcelain"]); if (status !== null && status) add("workspace is dirty after completion");
  if (new Set(codeChanges.changedPaths).size !== codeChanges.changedPaths.length) add("completion changedPaths contains duplicates");
  try {
    const actualPaths = gitChangedPaths(cwd, workspace.startingCommit, codeChanges.endingCommit); const claimed = [...codeChanges.changedPaths].sort(); const actual = [...actualPaths].sort(); if (claimed.length !== actual.length || claimed.some((path, index) => path !== actual[index])) add("completion changed path set differs from Git");
    if (actualPaths.some((path) => workspace.protectedScopes.some((scope) => matchesScope(path, scope)))) add("completion changed a protected path");
    if (actualPaths.some((path) => !workspace.writeScopes.some((scope) => matchesScope(path, scope)))) add("completion changed a path outside its Task Packet write scope");
  } catch (error) { add(error instanceof Error && error.message ? error.message : "Git changed-path derivation failed"); }
  return issues;
}

function matchesScope(path: string, scope: string): boolean { if (scope === "**") return true; const prefix = scope.endsWith("/**") ? scope.slice(0, -3) : scope; return path === prefix || path.startsWith(`${prefix}/`); }
