import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { exportPaper, initializePaperWorkspace } from "./paper.js";

describe("paper export", () => {
  it("creates a canonical workspace and reproducible LaTeX source", () => { const directory = mkdtempSync(join(tmpdir(), "nosh-paper-")); try { initializePaperWorkspace(directory, "Test Paper"); const manifest = exportPaper(join(directory, "docs", "paper.md"), join(directory, "docs", "paper.bib"), join(directory, "build", "paper")); expect(readFileSync(manifest.latexPath, "utf8")).toContain("\\title{Test Paper}\\maketitle"); expect(manifest.sourceHash).toMatch(/^sha256:[0-9a-f]{64}$/); expect(manifest.command).toEqual(["latexmk", "-pdf", "-interaction=nonstopmode", "-halt-on-error", "paper.tex"]); } finally { rmSync(directory, { recursive: true, force: true }); } });
});
