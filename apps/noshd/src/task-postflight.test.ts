import { mkdirSync, mkdtempSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { describe, expect, it } from "vitest";
import { gitChangedPaths } from "./task-postflight.js";

describe("gitChangedPaths", () => {
  it("includes both sides of renames and deleted protected paths", () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-postflight-"));
    try {
      git(directory, ["init"]);
      git(directory, ["config", "user.email", "fixture@nosh.dev"]);
      git(directory, ["config", "user.name", "NOSH Fixture"]);
      mkdirSync(join(directory, ".nosh", "contracts"), { recursive: true });
      mkdirSync(join(directory, "src"), { recursive: true });
      writeFileSync(join(directory, ".nosh", "contracts", "retired.json"), "{}\n");
      writeFileSync(join(directory, "src", "old-name.ts"), "export const oldName = true;\n");
      git(directory, ["add", "-f", ".nosh/contracts/retired.json"]);
      git(directory, ["add", "src/old-name.ts"]);
      git(directory, ["commit", "-m", "baseline"]);
      const startingCommit = git(directory, ["rev-parse", "HEAD"]);

      mkdirSync(join(directory, "lib"));
      renameSync(join(directory, "src", "old-name.ts"), join(directory, "lib", "new-name.ts"));
      rmSync(join(directory, ".nosh", "contracts", "retired.json"));
      git(directory, ["add", "-A"]);
      git(directory, ["commit", "-m", "rename and delete"]);
      const endingCommit = git(directory, ["rev-parse", "HEAD"]);

      expect(gitChangedPaths(directory, startingCommit, endingCommit)).toEqual(expect.arrayContaining([
        "src/old-name.ts",
        "lib/new-name.ts",
        ".nosh/contracts/retired.json",
      ]));
    } finally {
      rmSync(directory, { recursive: true, force: true });
    }
  });
});

function git(cwd: string, args: string[]): string {
  const result = spawnSync("git", ["-C", cwd, ...args], { encoding: "utf8", windowsHide: true });
  if (result.status !== 0) throw new Error(result.stderr.trim() || `git ${args[0]} failed`);
  return result.stdout.trim();
}
