import { canonicalJson, sha256, type JsonValue } from "@nosh/wire";
import { existsSync, mkdirSync, readFileSync, realpathSync, renameSync, writeFileSync } from "node:fs";
import { dirname, relative, resolve } from "node:path";
import { spawnSync } from "node:child_process";

export type FrozenExperiment = {
  experimentId: string; parentExperimentId: string | null; branch: string; worktree: string; parentCommit: string;
  evaluatedCommit: string; evaluationContractHash: string; environmentManifestHash: string; runCommand: string[]; frozenAt: string;
};

export class GitWorkspaceManager {
  private readonly registryPath: string;

  constructor(private readonly repositoryRoot: string, private readonly worktreesRoot: string, stateDirectory: string) {
    this.repositoryRoot = realpathSync(resolve(repositoryRoot));
    this.worktreesRoot = resolve(worktreesRoot);
    mkdirSync(this.worktreesRoot, { recursive: true }); mkdirSync(stateDirectory, { recursive: true });
    this.registryPath = resolve(stateDirectory, "evaluated-experiments.json");
  }

  create(branch: string, baseCommit: string, worktreeId: string): string {
    git(this.repositoryRoot, ["check-ref-format", "--branch", branch]);
    const path = resolve(this.worktreesRoot, worktreeId);
    if (relative(this.worktreesRoot, path).startsWith("..")) throw new Error("Worktree path escapes configured root");
    if (existsSync(path)) { const worktree = realpathSync(path); if (git(worktree, ["branch", "--show-current"]) !== branch || !succeeds(worktree, ["merge-base", "--is-ancestor", baseCommit, "HEAD"])) throw new Error(`Existing worktree ${path} does not match its durable intent`); return worktree; }
    // Check out exact blob bytes: NOSH's hardened Git plumbing ignores user config, so an autocrlf
    // checkout would make every file look modified and break exact-path commits and workspace edits.
    const exact = ["-c", "core.autocrlf=false", "-c", "core.eol=lf"];
    if (succeeds(this.repositoryRoot, ["show-ref", "--verify", `refs/heads/${branch}`])) git(this.repositoryRoot, [...exact, "worktree", "add", path, branch]); else git(this.repositoryRoot, [...exact, "worktree", "add", "-b", branch, path, baseCommit]);
    return realpathSync(path);
  }

  commit(worktree: string, paths: string[], message: string): string {
    if (!paths.length) throw new Error("Explicit commit paths are required");
    git(worktree, ["add", "--", ...paths]);
    git(worktree, ["commit", "-m", message]);
    return git(worktree, ["rev-parse", "HEAD"]);
  }

  freeze(input: Omit<FrozenExperiment, "evaluatedCommit" | "environmentManifestHash" | "frozenAt"> & { environmentManifest: JsonValue }): FrozenExperiment {
    if (git(input.worktree, ["status", "--porcelain"])) throw new Error("Cannot evaluate a dirty worktree");
    const evaluatedCommit = git(input.worktree, ["rev-parse", "HEAD"]);
    const branch = git(input.worktree, ["branch", "--show-current"]);
    if (branch !== input.branch) throw new Error(`Expected branch ${input.branch}, found ${branch}`);
    const records = this.records(); const prior = records.find((record) => record.experimentId === input.experimentId);
    if (prior) { if (prior.evaluatedCommit !== evaluatedCommit || prior.branch !== branch || prior.parentCommit !== input.parentCommit || prior.evaluationContractHash !== input.evaluationContractHash || prior.environmentManifestHash !== sha256(input.environmentManifest) || canonicalJson(prior.runCommand as unknown as JsonValue) !== canonicalJson(input.runCommand as unknown as JsonValue)) throw new Error(`Experiment ${input.experimentId} is already frozen with different provenance`); return prior; }
    const record: FrozenExperiment = {
      experimentId: input.experimentId, parentExperimentId: input.parentExperimentId, branch, worktree: realpathSync(input.worktree), parentCommit: input.parentCommit,
      evaluatedCommit, evaluationContractHash: input.evaluationContractHash, environmentManifestHash: sha256(input.environmentManifest), runCommand: [...input.runCommand], frozenAt: new Date().toISOString(),
    };
    records.push(record); this.save(records); return record;
  }

  assertFrozen(experimentId: string, commit: string, evaluationContractHash: string): FrozenExperiment {
    const record = this.records().find((candidate) => candidate.experimentId === experimentId);
    if (!record) throw new Error(`Experiment ${experimentId} is not frozen`);
    if (record.evaluatedCommit !== commit) throw new Error("Evaluated commit is immutable");
    if (record.evaluationContractHash !== evaluationContractHash) throw new Error("Evaluation contract hash mismatch");
    return record;
  }

  promote(experimentId: string, integrationWorktree: string, reviewVerdict: string, guardrailsPass: boolean): string {
    const record = this.records().find((candidate) => candidate.experimentId === experimentId);
    if (!record) throw new Error(`Experiment ${experimentId} is not frozen`);
    if (reviewVerdict !== "PASS" || !guardrailsPass) throw new Error("Promotion requires a passing independent review and guardrails");
    if (!succeeds(integrationWorktree, ["merge-base", "--is-ancestor", record.evaluatedCommit, "HEAD"])) git(integrationWorktree, ["merge", "--no-ff", "--no-edit", record.evaluatedCommit]);
    return git(integrationWorktree, ["rev-parse", "HEAD"]);
  }

  mergeProtected(sourceCommit: string, protectedWorktree: string, userApproved: boolean): string {
    if (!userApproved) throw new Error("Protected-branch merge requires explicit user approval");
    git(protectedWorktree, ["merge", "--no-ff", "--no-edit", sourceCommit]);
    return git(protectedWorktree, ["rev-parse", "HEAD"]);
  }

  records(): FrozenExperiment[] {
    return existsSync(this.registryPath) ? JSON.parse(readFileSync(this.registryPath, "utf8")) as FrozenExperiment[] : [];
  }

  private save(records: FrozenExperiment[]): void {
    mkdirSync(dirname(this.registryPath), { recursive: true }); const temporary = `${this.registryPath}.tmp`; writeFileSync(temporary, `${canonicalJson(records)}\n`, "utf8"); renameSync(temporary, this.registryPath);
  }
}

function git(cwd: string, args: string[]): string {
  const result = spawnSync("git", ["-C", cwd, ...args], { encoding: "utf8", windowsHide: true, env: worktreeGitEnvironment(cwd) });
  if (result.status !== 0) throw new Error(result.stderr.trim() || `git ${args[0]} failed`);
  return result.stdout.trim();
}
function succeeds(cwd: string, args: string[]): boolean { return spawnSync("git", ["-C", cwd, ...args], { windowsHide: true, env: worktreeGitEnvironment(cwd) }).status === 0; }
const trustCache = new Map<string, NodeJS.ProcessEnv>();
/**
 * Environment for Git inside a NOSH-created linked worktree. Filesystems without ownership
 * (FAT/exFAT, some network drives) need explicit safe.directory entries, and a user's global
 * list names the repository, not NOSH's per-task worktrees. Trust the worktree only when Git
 * already trusts its owning repository under the user's own configuration; never wider.
 */
export function worktreeGitEnvironment(cwd: string): NodeJS.ProcessEnv {
  const key = resolve(cwd); const cached = trustCache.get(key); if (cached) return cached;
  let environment: NodeJS.ProcessEnv = process.env;
  try {
    const gitDir = /^gitdir:\s*(.+)$/m.exec(readFileSync(resolve(key, ".git"), "utf8"))?.[1]?.trim();
    const owner = gitDir ? dirname(dirname(dirname(resolve(key, gitDir)))) : null;
    if (owner && spawnSync("git", ["-C", owner, "rev-parse", "--git-dir"], { windowsHide: true }).status === 0) {
      const count = Number(process.env.GIT_CONFIG_COUNT ?? 0) || 0;
      environment = { ...process.env, GIT_CONFIG_COUNT: String(count + 2), [`GIT_CONFIG_KEY_${count}`]: "safe.directory", [`GIT_CONFIG_VALUE_${count}`]: key.replaceAll("\\", "/"), [`GIT_CONFIG_KEY_${count + 1}`]: "safe.directory", [`GIT_CONFIG_VALUE_${count + 1}`]: owner.replaceAll("\\", "/") };
    }
  } catch { /* Not a linked worktree: use the caller's environment unchanged. */ }
  trustCache.set(key, environment); return environment;
}

