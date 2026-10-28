import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { describe, expect, it } from "vitest";

const cli = resolve(import.meta.dirname, "..", "dist", "main.js");
function nosh(state: string, ...args: string[]) {
  const result = spawnSync(process.execPath, [cli, ...args], { encoding: "utf8", windowsHide: true, env: { ...process.env, LOCALAPPDATA: state, APPDATA: state } });
  return { code: result.status, out: result.stdout, err: result.stderr };
}

// The npm launcher relies on these exit codes: 0 stopped, 2 active work, 3 not running.
describe("nosh stop --if-idle and nosh update", () => {
  it("reports 3 when no daemon runs, before and after setup, and explains how to update this copy", () => {
    const state = mkdtempSync(join(tmpdir(), "nosh-cli-stop-"));
    try {
      expect(nosh(state, "stop", "--if-idle").code).toBe(3);
      const root = join(state, "NOSH"); mkdirSync(root, { recursive: true });
      // Port 1 is never a NOSH daemon.
      writeFileSync(join(root, "config.json"), JSON.stringify({ dataDirectory: join(root, "data"), port: 1, bootstrapToken: "test", currentProjectId: null }));
      expect(nosh(state, "stop", "--if-idle").code).toBe(3);
      expect(nosh(state, "stop").code).toBe(0);
      const update = nosh(state, "update");
      expect(update.code).toBe(0);
      expect(update.out).toContain("nosh-harness@latest");
    } finally { rmSync(state, { recursive: true, force: true }); }
  }, 60_000);
});
