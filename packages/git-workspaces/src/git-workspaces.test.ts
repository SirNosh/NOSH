import { mkdtempSync, mkdirSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { describe, expect, it } from "vitest";
import { GitWorkspaceManager, worktreeGitEnvironment } from "./index.js";

describe("GitWorkspaceManager", () => {
  it("freezes evaluated commits and contract provenance", () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-git-"));
    const repository = join(directory, "repo"); mkdirSync(repository);
    try {
      run(repository, ["init", "-b", "main"]); run(repository, ["config", "user.email", "nosh@example.invalid"]); run(repository, ["config", "user.name", "NOSH Test"]);
      writeFileSync(join(repository, "README.md"), "baseline\n"); run(repository, ["add", "README.md"]); run(repository, ["commit", "-m", "baseline"]);
      const base = run(repository, ["rev-parse", "HEAD"]);
      const manager = new GitWorkspaceManager(repository, join(directory, "worktrees"), join(directory, "state"));
      const worktree = manager.create("ar/test/exp1", base, "wt1");
      writeFileSync(join(worktree, "variant.txt"), "variant\n");
      manager.commit(worktree, ["variant.txt"], "variant");
      const frozen = manager.freeze({ experimentId: "exp_1", parentExperimentId: "exp_base", branch: "ar/test/exp1", worktree, parentCommit: base, evaluationContractHash: "sha256:contract", environmentManifest: { node: process.version }, runCommand: ["python", "train.py"] });
      expect(manager.assertFrozen("exp_1", frozen.evaluatedCommit, "sha256:contract")).toEqual(frozen);
      expect(() => manager.assertFrozen("exp_1", base, "sha256:contract")).toThrow("immutable");
    } finally { rmSync(directory, { recursive: true, force: true }); }
  });
  it("promotes with the daemon's own identity when the user has none configured", () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-git-")); const repository = join(directory, "repo"); mkdirSync(repository);
    const saved = { global: process.env.GIT_CONFIG_GLOBAL, nosystem: process.env.GIT_CONFIG_NOSYSTEM };
    try {
      run(repository, ["init", "-b", "main"]); run(repository, ["config", "user.email", "nosh@example.invalid"]); run(repository, ["config", "user.name", "NOSH Test"]);
      writeFileSync(join(repository, "README.md"), "baseline\n"); run(repository, ["add", "README.md"]); run(repository, ["commit", "-m", "baseline"]);
      const base = run(repository, ["rev-parse", "HEAD"]);
      const manager = new GitWorkspaceManager(repository, join(directory, "worktrees"), join(directory, "state"));
      const worktree = manager.create("ar/test/exp1", base, "wt1"); const integration = manager.create("ar/test/frontier", base, "frontier");
      writeFileSync(join(worktree, "variant.txt"), "variant\n"); manager.commit(worktree, ["variant.txt"], "variant");
      manager.freeze({ experimentId: "exp_1", parentExperimentId: "exp_base", branch: "ar/test/exp1", worktree, parentCommit: base, evaluationContractHash: "sha256:contract", environmentManifest: {}, runCommand: ["node", "-v"] });
      run(repository, ["config", "--unset", "user.email"]); run(repository, ["config", "--unset", "user.name"]);
      writeFileSync(join(directory, "empty.gitconfig"), ""); process.env.GIT_CONFIG_GLOBAL = join(directory, "empty.gitconfig"); process.env.GIT_CONFIG_NOSYSTEM = "1";
      const merged = manager.promote("exp_1", integration, "PASS", true);
      expect(run(integration, ["log", "-1", "--format=%an <%ae>", merged])).toBe("NOSH <noshd@nosh.invalid>");
    } finally {
      for (const [key, value] of [["GIT_CONFIG_GLOBAL", saved.global], ["GIT_CONFIG_NOSYSTEM", saved.nosystem]] as const) { if (value === undefined) delete process.env[key]; else process.env[key] = value; }
      rmSync(directory, { recursive: true, force: true });
    }
  });
});

function run(cwd: string, args: string[]): string {
  const result = spawnSync("git", ["-C", cwd, ...args], { encoding: "utf8", windowsHide: true });
  if (result.status !== 0) throw new Error(result.stderr); return result.stdout.trim();
}

describe("worktreeGitEnvironment", () => {
  it("adds scoped safe.directory trust only for a linked worktree of a trusted repository", () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-trust-")); const repository = join(directory, "repo");
    try {
      mkdirSync(repository); const git = (cwd: string, ...args: string[]) => spawnSync("git", ["-C", cwd, ...args], { encoding: "utf8" });
      git(repository, "init", "-q"); writeFileSync(join(repository, "a.txt"), "a\n"); git(repository, "add", "a.txt"); git(repository, "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "a");
      expect(worktreeGitEnvironment(repository)).toBe(process.env);
      const worktree = new GitWorkspaceManager(repository, join(repository, ".nosh", "worktrees"), join(repository, ".nosh", "git")).create("nosh/task-trust", git(repository, "rev-parse", "HEAD").stdout.trim(), "wt_trust");
      const environment = worktreeGitEnvironment(worktree);
      const values = Object.entries(environment).filter(([key]) => key.startsWith("GIT_CONFIG_VALUE_")).map(([, value]) => String(value).toLowerCase());
      expect(values).toEqual(expect.arrayContaining([worktree.replaceAll("\\", "/").toLowerCase(), realpathSync.native(repository).replaceAll("\\", "/").toLowerCase()]));
    } finally { rmSync(directory, { recursive: true, force: true }); }
  });
});

describe("worktree checkout bytes", () => {
  it("matches committed blobs even when the user's Git converts line endings", () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-eol-")); const repository = join(directory, "repo");
    try {
      mkdirSync(repository); const git = (cwd: string, ...args: string[]) => spawnSync("git", ["-C", cwd, ...args], { encoding: "utf8" });
      git(repository, "init", "-q"); git(repository, "config", "core.autocrlf", "true");
      writeFileSync(join(repository, "config.json"), "{\n  \"threshold\": 0.5\n}\n"); git(repository, "add", "config.json"); git(repository, "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "a");
      const worktree = new GitWorkspaceManager(repository, join(repository, ".nosh", "worktrees"), join(repository, ".nosh", "git")).create("nosh/task-eol", git(repository, "rev-parse", "HEAD").stdout.trim(), "wt_eol");
      expect(readFileSync(join(worktree, "config.json"), "utf8")).toBe("{\n  \"threshold\": 0.5\n}\n");
      // Plumbing without user config must see a clean tree.
      expect(spawnSync("git", ["-C", worktree, "-c", "core.autocrlf=false", "status", "--porcelain"], { encoding: "utf8" }).stdout.trim()).toBe("");
    } finally { rmSync(directory, { recursive: true, force: true }); }
  });
});
