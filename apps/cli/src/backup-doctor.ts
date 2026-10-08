export type CommandResult = { ok: boolean; stdout: string };
export type DoctorCheck = [name: string, ok: boolean, detail: string];

export function gitDoctorChecks(repositoryRoot: string | undefined, run: (command: string, args: string[]) => CommandResult): DoctorCheck[] {
  const version = run("git", ["--version"]);
  if (!version.ok) return [["Git", false, "not available"], ["Git worktrees", false, "Git unavailable"]];
  if (!repositoryRoot) return [["Git", true, "available"], ["Git worktrees", true, "not checked; no current Project"]];
  const worktrees = run("git", ["-C", repositoryRoot, "worktree", "list"]);
  const count = worktrees.stdout.split(/\r?\n/).filter((line) => line.trim()).length;
  return [["Git", true, "available"], ["Git worktrees", worktrees.ok, worktrees.ok ? `${count} available` : "unavailable"]];
}
