import { spawnSync } from "node:child_process";

export type GitCompletion = { startingCommit: string; endingCommit: string; changedPaths: string[]; branch: string };
export type GitTaskWorkspace = { startingCommit: string; branch: string; writeScopes: string[]; protectedScopes: string[] };

export function gitChangedPaths(cwd: string, startingCommit: string, endingCommit: string): string[] {
  const result = spawnSync("git", ["-C", cwd, "diff", "--name-status", "-z", "--find-renames", startingCommit, endingCommit], { encoding: "utf8", windowsHide: true });
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

export function validateGitCompletion(cwd: string, workspace: GitTaskWorkspace, codeChanges: GitCompletion): string[] {
  const issues: string[] = []; const add = (issue: string) => { if (!issues.includes(issue)) issues.push(issue); };
  if (codeChanges.startingCommit !== workspace.startingCommit) add("starting commit differs from Task Packet");
  if (codeChanges.branch !== workspace.branch) add("branch differs from Task Packet");
  const git = (args: string[]): string | null => { const result = spawnSync("git", ["-C", cwd, ...args], { encoding: "utf8", windowsHide: true }); if (result.status !== 0) { const detail = result.stderr.trim() || `status ${result.status ?? "spawn-error"}`; add(`Git ${args.join(" ")} failed: ${detail}`); return null; } return result.stdout.trim(); };
  const actualBranch = git(["symbolic-ref", "--quiet", "--short", "HEAD"]); if (actualBranch !== null && actualBranch !== workspace.branch) add("checked-out branch differs from Task Packet");
  const ending = git(["rev-parse", `${codeChanges.endingCommit}^{commit}`]); const head = git(["rev-parse", "HEAD"]); if (ending === null || head === null || ending !== codeChanges.endingCommit || head !== codeChanges.endingCommit) add("ending commit is not the checked-out immutable HEAD");
  const ancestry = spawnSync("git", ["-C", cwd, "merge-base", "--is-ancestor", workspace.startingCommit, codeChanges.endingCommit], { encoding: "utf8", windowsHide: true }); if (ancestry.status === 1) add("starting commit is not an ancestor of ending commit"); else if (ancestry.status !== 0) add(`Git merge-base --is-ancestor failed: ${ancestry.stderr.trim() || `status ${ancestry.status ?? "spawn-error"}`}`);
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
