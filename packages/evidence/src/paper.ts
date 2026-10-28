import { createHash } from "node:crypto";
import { copyFileSync, existsSync, lstatSync, mkdirSync, readFileSync, renameSync, rmSync, statSync, writeFileSync } from "node:fs";
import { basename, dirname, extname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { spawnSync } from "node:child_process";
export type PaperClaim = { claimId: string; supportingEvidenceIds: string[]; contradictingEvidenceIds: string[]; qualifyingEvidenceIds: string[]; limitations: string[]; paperLocations: string[] };

export type PaperManifest = { markdownPath: string; bibliographyPath: string; latexPath: string; pdfPath: string | null; sourceHash: string; command: string[]; warnings: string[]; exportedAt: string };
const activeExports = new Set<string>();

export function initializePaperWorkspace(repositoryRoot: string, title: string): void {
  const docs = join(repositoryRoot, "docs"); assertUnlinkedContained(repositoryRoot, docs, "Paper docs");
  mkdirSync(join(docs, "figures"), { recursive: true });
  const markdownPath = join(docs, "paper.md"); const bibliographyPath = join(docs, "paper.bib");
  if (existsSync(markdownPath)) assertRegularFile(markdownPath, "Paper Markdown"); else writeFileSync(markdownPath, `# ${title}\n\n## Abstract\n\n## Introduction\n\n## Related work\n\n## Method\n\n## Experiments\n\n## Limitations\n\n## Conclusion\n`, "utf8");
  if (existsSync(bibliographyPath)) assertRegularFile(bibliographyPath, "Paper bibliography"); else writeFileSync(bibliographyPath, "", "utf8");
}

export function exportPaper(markdownPath: string, bibliographyPath: string, outputDirectory: string): PaperManifest {
  const output = resolve(outputDirectory);
  if (activeExports.has(output)) throw new Error(`Paper export is already running for ${output}`);
  activeExports.add(output);
  const staging = `${output}.staging-${process.pid}-${Date.now()}-${Math.random().toString(16).slice(2)}`;
  const rollback = `${output}.rollback-${process.pid}-${Date.now()}-${Math.random().toString(16).slice(2)}`;
  const repositoryRoot = dirname(dirname(resolve(markdownPath)));
  assertUnlinkedContained(repositoryRoot, staging, "Paper export staging");
  try {
    assertUnlinkedContained(repositoryRoot, markdownPath, "Paper Markdown");
    assertUnlinkedContained(repositoryRoot, bibliographyPath, "Paper bibliography");
    assertRegularFile(markdownPath, "Paper Markdown");
    if (existsSync(bibliographyPath)) assertRegularFile(bibliographyPath, "Paper bibliography");
    const markdown = readFileSync(markdownPath, "utf8"); const bibliography = existsSync(bibliographyPath) ? readFileSync(bibliographyPath, "utf8") : "";
    mkdirSync(staging, { recursive: true });
    const stagedLatexPath = join(staging, "paper.tex"); const stagedPdfPath = join(staging, "paper.pdf"); const warnings: string[] = [];
    if (existsSync(bibliographyPath)) copyFileSync(bibliographyPath, join(staging, "paper.bib"));
    const latex = markdownToLatex(markdown, warnings, bibliographyPath, markdownPath, staging); writeFileSync(stagedLatexPath, latex, "utf8");
    const command = ["latexmk", "-pdf", "-interaction=nonstopmode", "-halt-on-error", "paper.tex"];
    const built = spawnSync(command[0]!, command.slice(1), { cwd: staging, encoding: "utf8", windowsHide: true });
    writeFileSync(join(staging, "export.log"), `${built.stdout ?? ""}\n${built.stderr ?? ""}`, "utf8");
    const hasPdf = built.status === 0 && existsSync(stagedPdfPath);
    if (!hasPdf) warnings.push("PDF was not produced; install latexmk and a TeX distribution, then rerun the same command.");
    const manifest: PaperManifest = { markdownPath: resolve(markdownPath), bibliographyPath: resolve(bibliographyPath), latexPath: join(output, "paper.tex"), pdfPath: hasPdf ? join(output, "paper.pdf") : null, sourceHash: `sha256:${createHash("sha256").update(markdown).update("\0").update(bibliography).digest("hex")}`, command, warnings, exportedAt: new Date().toISOString() };
    writeFileSync(join(staging, "manifest.json"), `${JSON.stringify(manifest)}\n`, "utf8");
    const prior = existsSync(output);
    if (prior) renameSync(output, rollback);
    try { renameSync(staging, output); } catch (error) { if (prior && existsSync(rollback)) renameSync(rollback, output); throw error; }
    if (prior) try { rmSync(rollback, { recursive: true, force: true }); } catch (error) { process.stderr.write(`nosh paper export retained ${rollback}: ${error instanceof Error ? error.message : "unknown cleanup failure"}\n`); }
    return manifest;
  } catch (error) {
    rmSync(staging, { recursive: true, force: true });
    throw error;
  } finally { activeExports.delete(output); }
}

export function auditPaper(repositoryRoot: string, claims: PaperClaim[], figureArtifacts: Array<{ path: string; artifactId: string; contentHash: string }>): string[] {
  const issues: string[] = [];
  for (const claim of claims) {
    if (!claim.paperLocations.length) issues.push(`${claim.claimId}: no paper location`);
    if (!claim.supportingEvidenceIds.length && !claim.contradictingEvidenceIds.length && !claim.qualifyingEvidenceIds.length && !claim.limitations.length) issues.push(`${claim.claimId}: no evidence or limitation`);
  }
  for (const figure of figureArtifacts) {
    try { assertUnlinkedContained(join(repositoryRoot, "docs", "figures"), resolve(repositoryRoot, figure.path), `Figure ${figure.artifactId}`); if (!statSync(resolve(repositoryRoot, figure.path)).isFile()) throw new Error("not a regular file"); }
    catch { issues.push(`${figure.artifactId}: figure does not resolve safely inside Project docs/figures`); }
    if (!/^sha256:[0-9a-f]{64}$/.test(figure.contentHash)) issues.push(`${figure.artifactId}: invalid provenance hash`);
  }
  return issues;
}

function markdownToLatex(markdown: string, warnings: string[], bibliographyPath: string, markdownPath: string, outputDirectory: string): string {
  const body: string[] = [];
  let inCode = false; let table: string[] = [];
  const flushTable = () => { if (table.length) { body.push(latexTable(table, warnings)); table = []; } };
  for (const source of markdown.replaceAll("\r\n", "\n").split("\n")) {
    if (!inCode && /^\|.*\|$/.test(source.trim())) { table.push(source.trim()); continue; }
    flushTable();
    if (source.startsWith("```")) { body.push(inCode ? "\\end{verbatim}" : "\\begin{verbatim}"); inCode = !inCode; continue; }
    if (inCode) { body.push(source); continue; }
    const heading = /^(#{1,3})\s+(.+)$/.exec(source);
    if (heading) { const command = heading[1]!.length === 1 ? "title" : heading[1]!.length === 2 ? "section" : "subsection"; body.push(`\\${command}{${escapeLatex(heading[2]!)}}`); continue; }
    const image = /^!\[([^\]]*)\]\(([^)]+)\)$/.exec(source);
    if (image) { body.push(`\\begin{figure}[ht]\\centering\\includegraphics[width=\\linewidth]{${escapeLatex(copyPaperFigure(markdownPath, outputDirectory, image[2]!))}}\\caption{${escapeLatex(image[1]!)}}\\end{figure}`); continue; }
    if (source.startsWith("- ")) { body.push(`\\begin{itemize}\\item ${inline(source.slice(2))}\\end{itemize}`); continue; }
    body.push(source ? `${inline(source)}\n` : "");
  }
  flushTable();
  if (inCode) warnings.push("Unclosed fenced code block.");
  const bibliography = existsSync(bibliographyPath) && readFileSync(bibliographyPath, "utf8").trim() ? "\\bibliographystyle{plain}\\bibliography{paper}" : "";
  return `\\documentclass[11pt]{article}\n\\usepackage[margin=1in]{geometry}\n\\usepackage[T1]{fontenc}\n\\usepackage{graphicx}\n\\usepackage{tabularx}\n\\usepackage{hyperref}\n\\begin{document}\n${body.join("\n").replace("\\title{", "\\title{").replace(/(\\title\{[^}]+\})/, "$1\\maketitle")}\n${bibliography}\n\\end{document}\n`;
}
/** GitHub-style pipe table -> tabular scaled to the text width; alignment from the separator row (---: right, :---: center). */
function latexTable(rows: string[], warnings: string[]): string {
  const cells = (row: string) => row.slice(1, -1).split("|").map((cell) => cell.trim());
  const separator = rows[1] && cells(rows[1]).every((cell) => /^:?-{3,}:?$/.test(cell)) ? cells(rows[1]) : null;
  if (!separator) { warnings.push(`Markdown table without a separator row was kept as text: ${rows[0]!.slice(0, 80)}`); return rows.map((row) => `${inline(row)}\n`).join("\n"); }
  // Left-aligned (text) columns wrap; long identifiers may break after underscores and slashes instead of shrinking the whole table.
  const columns = separator.map((cell) => cell.startsWith(":") && cell.endsWith(":") ? "c" : cell.endsWith(":") ? "r" : ">{\\raggedright\\arraybackslash}X").join("");
  const line = (row: string) => `${cells(row).map((cell) => inline(cell).replaceAll("\\_", "\\_\\allowbreak{}").replaceAll(" / ", " /\\allowbreak{} ")).join(" & ")} \\\\`;
  const [header, , ...data] = rows;
  // In place (not a float): generated tables belong where the Markdown put them.
  return `\\begin{center}\\footnotesize\\begin{tabularx}{\\linewidth}{${columns}}\\hline\n${line(header!)}\n\\hline\n${data.map(line).join("\n")}\n\\hline\\end{tabularx}\\end{center}`;
}
function inline(value: string): string { return escapeLatex(value).replace(/\*\*([^*]+)\*\*/g, "\\textbf{$1}").replace(/`([^`]+)`/g, (_match, code: string) => `\\texttt{${code.replace(/([0-9a-f]{8})(?=[0-9a-f]{8})/g, "$1\\allowbreak{}")}}`).replace(/\[([^\]]+)\]\(([^)]+)\)/g, (_match, text, url) => /^https?:\/\//i.test(url.replaceAll("\\%", "%")) ? `\\href{${url}}{${text}}` : text).replace(/\[@([^\]]+)\]/g, "\\cite{$1}"); }
function escapeLatex(value: string): string { const replacements: Record<string, string> = { "\\": "\\textbackslash{}", "&": "\\&", "%": "\\%", "$": "\\$", "#": "\\#", "_": "\\_", "{": "\\{", "}": "\\}" }; return value.replace(/[\\&%$#_{}]/g, (character) => replacements[character]!); }
function copyPaperFigure(markdownPath: string, outputDirectory: string, reference: string): string {
  if (isAbsolute(reference) || /^(?:[a-z][a-z0-9+.-]*:|\/\/)/i.test(reference)) throw new Error(`Paper image path is not a local figure: ${reference}`);
  const docsRoot = dirname(markdownPath); const figuresRoot = resolve(docsRoot, "figures"); const source = resolve(docsRoot, reference); const inside = relative(figuresRoot, source);
  if (!inside || inside === ".." || inside.startsWith(`..${sep}`) || isAbsolute(inside) || !existsSync(source)) throw new Error(`Paper image is not a readable docs/figures asset: ${reference}`);
  assertUnlinkedContained(docsRoot, source, `Paper image ${reference}`);
  assertUnlinkedContained(dirname(outputDirectory), outputDirectory, "Paper export output");
  assertRegularFile(source, `Paper image ${reference}`);
  if (![".eps", ".jpeg", ".jpg", ".pdf", ".png", ".svg"].includes(extname(source).toLowerCase())) throw new Error(`Paper image is not a supported regular figure file: ${reference}`);
  const targetName = `${createHash("sha256").update(source).digest("hex").slice(0, 16)}-${basename(source)}`; const target = join(outputDirectory, "figures", targetName);
  assertUnlinkedContained(outputDirectory, target, "Paper export figure output");
  mkdirSync(dirname(target), { recursive: true }); assertUnlinkedContained(outputDirectory, target, "Paper export figure output"); copyFileSync(source, target); return `figures/${targetName}`;
}
function assertRegularFile(path: string, label: string): void { const stat = lstatSync(path, { throwIfNoEntry: false }); if (!stat || stat.isSymbolicLink() || !stat.isFile()) throw new Error(`${label} is not a regular file`); }
function assertUnlinkedContained(root: string, candidate: string, label: string): void {
  const base = resolve(root); const target = resolve(candidate); const inside = relative(base, target);
  if (inside === ".." || inside.startsWith(`..${sep}`) || isAbsolute(inside)) throw new Error(`${label} escapes its allowed root`);
  const chain: string[] = []; let current = base;
  while (true) { chain.unshift(current); const parent = dirname(current); if (parent === current) break; current = parent; }
  current = base;
  for (const segment of inside.split(sep).filter(Boolean)) { current = join(current, segment); chain.push(current); }
  for (const path of chain) { const stat = lstatSync(path, { throwIfNoEntry: false }); if (stat?.isSymbolicLink()) throw new Error(`${label} traverses a symlink or reparse point`); }
}
