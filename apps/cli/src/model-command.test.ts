import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { describe, expect, it } from "vitest";

const cli = resolve(import.meta.dirname, "..", "dist", "main.js");
function nosh(state: string, ...args: string[]) {
  const result = spawnSync(process.execPath, [cli, ...args], { encoding: "utf8", windowsHide: true, env: { ...process.env, LOCALAPPDATA: state, APPDATA: state } });
  return { code: result.status, out: result.stdout, err: result.stderr };
}

describe("nosh model", () => {
  it("shows, validates, and saves the default model without a running daemon", () => {
    const state = mkdtempSync(join(tmpdir(), "nosh-cli-model-"));
    try {
      expect(nosh(state, "model").err).toContain("not initialized");
      const root = join(state, "NOSH"); mkdirSync(root, { recursive: true });
      // Port 1 is never a NOSH daemon, so validation against Pi's model list is skipped.
      writeFileSync(join(root, "config.json"), JSON.stringify({ dataDirectory: join(root, "data"), port: 1, bootstrapToken: "test", currentProjectId: null }));
      expect(nosh(state, "model").out).toContain("none");
      expect(nosh(state, "model", "set", "nope").err).toContain("provider/id");
      expect(nosh(state, "model", "set", "openai-codex/gpt-6-luna:low").out).toContain("Default model set to openai-codex/gpt-6-luna:low");
      expect(JSON.parse(readFileSync(join(root, "config.json"), "utf8")).defaultModel).toBe("openai-codex/gpt-6-luna:low");
      expect(nosh(state, "model").out.trim()).toBe("openai-codex/gpt-6-luna:low");
    } finally { rmSync(state, { recursive: true, force: true }); }
  }, 60_000);
});
