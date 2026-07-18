import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { describe, expect, it } from "vitest";
import { GitWorkspaceManager } from "./index.js";

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
});

function run(cwd: string, args: string[]): string {
  const result = spawnSync("git", ["-C", cwd, ...args], { encoding: "utf8", windowsHide: true });
  if (result.status !== 0) throw new Error(result.stderr); return result.stdout.trim();
}
