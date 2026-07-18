import { createHash } from "node:crypto";
import { copyFileSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, relative, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import type { Claim } from "./index.js";

export type PaperManifest = { markdownPath: string; bibliographyPath: string; latexPath: string; pdfPath: string | null; sourceHash: string; command: string[]; warnings: string[]; exportedAt: string };

export function initializePaperWorkspace(repositoryRoot: string, title: string): void {
  const docs = join(repositoryRoot, "docs"); mkdirSync(join(docs, "figures"), { recursive: true });
  if (!existsSync(join(docs, "paper.md"))) writeFileSync(join(docs, "paper.md"), `# ${title}\n\n## Abstract\n\n## Introduction\n\n## Related work\n\n## Method\n\n## Experiments\n\n## Limitations\n\n## Conclusion\n`, "utf8");
  if (!existsSync(join(docs, "paper.bib"))) writeFileSync(join(docs, "paper.bib"), "", "utf8");
}

export function exportPaper(markdownPath: string, bibliographyPath: string, outputDirectory: string): PaperManifest {
  const markdown = readFileSync(markdownPath, "utf8"); mkdirSync(outputDirectory, { recursive: true });
  const latexPath = join(outputDirectory, "paper.tex"); const pdfPath = join(outputDirectory, "paper.pdf"); const warnings: string[] = [];
  if (existsSync(bibliographyPath)) copyFileSync(bibliographyPath, join(outputDirectory, "paper.bib"));
  const latex = markdownToLatex(markdown, warnings, bibliographyPath); writeFileSync(latexPath, latex, "utf8");
  const command = ["latexmk", "-pdf", "-interaction=nonstopmode", "-halt-on-error", "paper.tex"];
  const built = spawnSync(command[0]!, command.slice(1), { cwd: outputDirectory, encoding: "utf8", windowsHide: true });
  writeFileSync(join(outputDirectory, "export.log"), `${built.stdout ?? ""}\n${built.stderr ?? ""}`, "utf8");
  if (built.status !== 0) warnings.push("PDF was not produced; install latexmk and a TeX distribution, then rerun the same command.");
  const bibliography = existsSync(bibliographyPath) ? readFileSync(bibliographyPath, "utf8") : "";
  return { markdownPath: resolve(markdownPath), bibliographyPath: resolve(bibliographyPath), latexPath, pdfPath: built.status === 0 && existsSync(pdfPath) ? pdfPath : null, sourceHash: `sha256:${createHash("sha256").update(markdown).update("\0").update(bibliography).digest("hex")}`, command, warnings, exportedAt: new Date().toISOString() };
}

export function auditPaper(repositoryRoot: string, claims: Claim[], figureArtifacts: Array<{ path: string; artifactId: string; contentHash: string }>): string[] {
  const issues: string[] = [];
  for (const claim of claims) {
    if (!claim.paperLocations.length) issues.push(`${claim.claimId}: no paper location`);
    if (!claim.supportingEvidenceIds.length && !claim.contradictingEvidenceIds.length && !claim.qualifyingEvidenceIds.length && !claim.limitations.length) issues.push(`${claim.claimId}: no evidence or limitation`);
  }
  for (const figure of figureArtifacts) { const path = resolve(repositoryRoot, figure.path); if (relative(resolve(repositoryRoot), path).startsWith("..") || !existsSync(path)) issues.push(`${figure.artifactId}: figure does not resolve inside the Project`); if (!/^sha256:[0-9a-f]{64}$/.test(figure.contentHash)) issues.push(`${figure.artifactId}: invalid provenance hash`); }
  return issues;
}

function markdownToLatex(markdown: string, warnings: string[], bibliographyPath: string): string {
  const body: string[] = [];
  let inCode = false;
  for (const source of markdown.replaceAll("\r\n", "\n").split("\n")) {
    if (source.startsWith("```")) { body.push(inCode ? "\\end{verbatim}" : "\\begin{verbatim}"); inCode = !inCode; continue; }
    if (inCode) { body.push(source); continue; }
    const heading = /^(#{1,3})\s+(.+)$/.exec(source);
    if (heading) { const command = heading[1]!.length === 1 ? "title" : heading[1]!.length === 2 ? "section" : "subsection"; body.push(`\\${command}{${escapeLatex(heading[2]!)}}`); continue; }
    const image = /^!\[([^\]]*)\]\(([^)]+)\)$/.exec(source);
    if (image) { body.push(`\\begin{figure}[ht]\\centering\\includegraphics[width=\\linewidth]{${escapeLatex(image[2]!)}}\\caption{${escapeLatex(image[1]!)}}\\end{figure}`); continue; }
    if (source.startsWith("- ")) { body.push(`\\begin{itemize}\\item ${inline(source.slice(2))}\\end{itemize}`); continue; }
    if (/^\|.*\|$/.test(source)) { warnings.push(`Markdown table requires manual LaTeX review: ${source.slice(0, 80)}`); body.push(`% TABLE_REVIEW_REQUIRED ${escapeLatex(source)}`); continue; }
    body.push(source ? `${inline(source)}\n` : "");
  }
  if (inCode) warnings.push("Unclosed fenced code block.");
  const bibliography = existsSync(bibliographyPath) && readFileSync(bibliographyPath, "utf8").trim() ? "\\bibliographystyle{plain}\\bibliography{paper}" : "";
  return `\\documentclass[11pt]{article}\n\\usepackage[margin=1in]{geometry}\n\\usepackage{graphicx}\n\\usepackage{hyperref}\n\\begin{document}\n${body.join("\n").replace("\\title{", "\\title{").replace(/(\\title\{[^}]+\})/, "$1\\maketitle")}\n${bibliography}\n\\end{document}\n`;
}
function inline(value: string): string { return escapeLatex(value).replace(/\*\*([^*]+)\*\*/g, "\\textbf{$1}").replace(/`([^`]+)`/g, "\\texttt{$1}").replace(/\[([^\]]+)\]\(([^)]+)\)/g, "\\href{$2}{$1}").replace(/\[@([^\]]+)\]/g, "\\cite{$1}"); }
function escapeLatex(value: string): string { const replacements: Record<string, string> = { "\\": "\\textbackslash{}", "&": "\\&", "%": "\\%", "$": "\\$", "#": "\\#", "_": "\\_", "{": "\\{", "}": "\\}" }; return value.replace(/[\\&%$#_{}]/g, (character) => replacements[character]!); }
