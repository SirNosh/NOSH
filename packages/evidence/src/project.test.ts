import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { amendProjectContract, approveProjectContract, commitProjectContract, detectRunnableCommands, initializeResearchProject, readProjectContract } from "./project.js";

describe("project contract reads", () => {
  it("rejects metadata that redirects the active contract outside .nosh/contracts", () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-project-"));
    try {
      const { repositoryRoot } = initializeResearchProject({ path: join(directory, "repo"), dataDirectory: join(directory, "data"), createRepository: true });
      expect(readProjectContract(repositoryRoot).contractVersion).toBe(1);
      const metadataPath = join(repositoryRoot, ".nosh", "project.json");
      const metadata = JSON.parse(readFileSync(metadataPath, "utf8")) as Record<string, unknown>;
      writeFileSync(metadataPath, JSON.stringify({ ...metadata, activeProjectContractPath: "../outside.json" }));
      expect(() => readProjectContract(repositoryRoot)).toThrow("activeProjectContractPath must name .nosh/contracts/project.v<N>.json");
    } finally { rmSync(directory, { recursive: true, force: true }); }
  });

  it("stamps approvedAt on host approval so agents never invent approval metadata", () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-project-"));
    try {
      const { repositoryRoot } = initializeResearchProject({ path: join(directory, "repo"), dataDirectory: join(directory, "data"), createRepository: true });
      const draft = readProjectContract(repositoryRoot);
      const before = Date.now();
      const approved = approveProjectContract(repositoryRoot, { ...draft, contractVersion: 2, workingTitle: "Approved", approvedAt: null });
      expect(Date.parse(approved.approvedAt!)).toBeGreaterThanOrEqual(before);
      expect(readProjectContract(repositoryRoot)).toMatchObject({ contractVersion: 2, workingTitle: "Approved", approvedAt: approved.approvedAt });
      expect(() => approveProjectContract(repositoryRoot, { ...approved, contractVersion: 3 })).toThrow("already complete");
    } finally { rmSync(directory, { recursive: true, force: true }); }
  });

  it("lets the user amend an approved contract to declare runnable commands, never before approval", () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-project-"));
    try {
      const { repositoryRoot } = initializeResearchProject({ path: join(directory, "repo"), dataDirectory: join(directory, "data"), createRepository: true });
      const draft = readProjectContract(repositoryRoot);
      const execution = { runner: "native" as const, commands: [{ commandId: "command_test", description: "Unit tests", argv: ["node", "--test"], timeoutSeconds: 120 }] };
      expect(() => amendProjectContract(repositoryRoot, { ...draft, contractVersion: 2, execution })).toThrow("Complete Project discovery");
      const approved = approveProjectContract(repositoryRoot, { ...draft, contractVersion: 2 });
      expect(() => amendProjectContract(repositoryRoot, { ...approved, contractVersion: 4, execution })).toThrow("identity or version");
      expect(() => amendProjectContract(repositoryRoot, { ...approved, contractVersion: 3, execution: { ...execution, commands: [{ ...execution.commands[0], argv: [] }] } })).toThrow();
      const amended = amendProjectContract(repositoryRoot, { ...approved, contractVersion: 3, execution });
      expect(readProjectContract(repositoryRoot)).toMatchObject({ contractVersion: 3, execution, approvedAt: amended.approvedAt });
    } finally { rmSync(directory, { recursive: true, force: true }); }
  });

  it("commits exactly the approved contract files and leaves other staged work alone", () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-contract-commit-"));
    try {
      const { repositoryRoot } = initializeResearchProject({ path: join(directory, "repo"), dataDirectory: join(directory, "data"), createRepository: true });
      const git = (...args: string[]) => spawnSync("git", ["-C", repositoryRoot, ...args], { encoding: "utf8", windowsHide: true }).stdout.trim();
      writeFileSync(join(repositoryRoot, "notes.txt"), "work in progress"); git("add", "notes.txt");
      approveProjectContract(repositoryRoot, { ...readProjectContract(repositoryRoot), contractVersion: 2 });
      const first = commitProjectContract(repositoryRoot);
      expect(first.committed).toBe(true);
      expect(git("show", "--name-only", "--format=", "HEAD").split(/\s+/).sort()).toEqual([".nosh/contracts/project.v2.json", ".nosh/project.json"]);
      expect(git("diff", "--cached", "--name-only")).toBe("notes.txt");
      expect(commitProjectContract(repositoryRoot)).toEqual({ committed: false, detail: "already committed" });
    } finally { rmSync(directory, { recursive: true, force: true }); }
  });

  it("detects runnable test and evaluation commands that need no shell", () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-detect-"));
    try {
      writeFileSync(join(directory, "package.json"), JSON.stringify({ scripts: { test: "node --test test/a.test.mjs", evaluate: "node evaluate.mjs", "test:unit": "vitest run && echo ok", bench: "npx bench", build: "node build.mjs" } }));
      mkdirSync(join(directory, "tests")); writeFileSync(join(directory, "tests", "test_model.py"), "def test_ok():\n    pass\n");
      writeFileSync(join(directory, "Makefile"), "eval:\n\tpython eval.py\n");
      expect(detectRunnableCommands(directory)).toEqual([
        { commandId: "command_test", description: 'package.json script "test"', argv: ["node", "--test", "test/a.test.mjs"], timeoutSeconds: 600 },
        { commandId: "command_evaluate", description: 'package.json script "evaluate"', argv: ["node", "evaluate.mjs"], timeoutSeconds: 600 },
        { commandId: "command_pytest", description: "pytest suite", argv: ["python", "-m", "pytest", "-q"], timeoutSeconds: 1_800 },
        { commandId: "command_make-eval", description: "make eval", argv: ["make", "eval"], timeoutSeconds: 1_800 },
      ]);
      expect(detectRunnableCommands(join(directory, "missing"))).toEqual([]);
    } finally { rmSync(directory, { recursive: true, force: true }); }
  });

  it("explains the reference format when a contract field is malformed", () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-project-"));
    try {
      const { repositoryRoot } = initializeResearchProject({ path: join(directory, "repo"), dataDirectory: join(directory, "data"), createRepository: true });
      const draft = readProjectContract(repositoryRoot);
      expect(() => approveProjectContract(repositoryRoot, { ...draft, contractVersion: 2, northStar: { ...draft.northStar, contributionType: "contribution_method_measurement_fixture" } })).toThrow("exactly one underscore");
    } finally { rmSync(directory, { recursive: true, force: true }); }
  });

  it("does not echo file content from a JSON parse failure", () => {
    const directory = mkdtempSync(join(tmpdir(), "nosh-project-"));
    try {
      const { repositoryRoot } = initializeResearchProject({ path: join(directory, "repo"), dataDirectory: join(directory, "data"), createRepository: true });
      writeFileSync(join(repositoryRoot, ".nosh", "contracts", "project.v1.json"), "{ secret-token-value");
      const failure = (() => { try { readProjectContract(repositoryRoot); } catch (error) { return (error as Error).message; } return ""; })();
      expect(failure).toContain("file is not valid JSON");
      expect(failure).not.toContain("secret-token-value");
    } finally { rmSync(directory, { recursive: true, force: true }); }
  });
});
