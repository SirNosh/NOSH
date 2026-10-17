import { createHash } from "node:crypto";
import { copyFileSync, existsSync, lstatSync, mkdirSync, readFileSync, renameSync, rmSync, statSync, writeFileSync } from "node:fs";
import { dirname, extname, isAbsolute, join, relative, resolve, sep } from "node:path";
export * from "./paper.js";
export * from "./project.js";

export type Artifact = { artifactId: string; projectId: string; kind: string; mediaType: string; contentHash: string; sizeBytes: number; version: number; sourcePath: string; storedPath: string; retentionClass: "accepted_evidence" | "negative_evidence" | "checkpoint" | "cache" | "temporary"; createdAt: string };

export class ArtifactStore {
  private readonly manifestPath: string;
  constructor(private readonly root: string) { mkdirSync(root, { recursive: true }); assertSafePath(root, root); this.manifestPath = join(root, "manifest.json"); }
  add(input: Omit<Artifact, "contentHash" | "sizeBytes" | "version" | "storedPath" | "createdAt">): Artifact {
    const source = resolve(input.sourcePath); assertUnlinkedAbsolute(source, "Artifact source"); assertSafePath(this.root, this.root); assertRegularFile(source, "Artifact source");
    const bytes = readFileSync(source); const contentHash = `sha256:${createHash("sha256").update(bytes).digest("hex")}`; const hash = contentHash.slice(7); const storedRelativePath = join(hash.slice(0, 2), `${hash}${extname(source)}`); const storedPath = join(this.root, storedRelativePath); assertSafePath(this.root, storedPath); mkdirSync(dirname(storedPath), { recursive: true }); if (existsSync(storedPath)) assertRegularFile(storedPath, "Artifact object"); else copyFileSync(source, storedPath);
    const records = this.rawList(); const prior = records.filter((artifact) => artifact.artifactId === input.artifactId).sort((a, b) => b.version - a.version)[0];
    if (prior?.contentHash === contentHash) return this.publicArtifact(prior);
    const artifact: Artifact = { ...input, sourcePath: source, contentHash, sizeBytes: statSync(source).size, version: (prior?.version ?? 0) + 1, storedPath: storedRelativePath, createdAt: new Date().toISOString() };
    records.push(artifact); this.save(records); return this.publicArtifact(artifact);
  }
  resolve(artifactId: string, version?: number): Artifact { const matches = this.rawList().filter((artifact) => artifact.artifactId === artifactId && (version === undefined || artifact.version === version)).sort((a, b) => b.version - a.version); const artifact = matches[0]; if (!artifact) throw new Error(`Artifact ${artifactId} does not resolve to its stored hash`); const storedPath = this.absoluteStoredPath(artifact.storedPath); assertRegularFile(storedPath, "Artifact object"); if (hashFile(storedPath) !== artifact.contentHash) throw new Error(`Artifact ${artifactId} does not resolve to its stored hash`); return { ...artifact, storedPath }; }
  cleanup(retentionClasses: Artifact["retentionClass"][]): Artifact[] { return this.list().filter((artifact) => !retentionClasses.includes(artifact.retentionClass) || artifact.retentionClass === "accepted_evidence" || artifact.retentionClass === "negative_evidence"); }
  list(): Artifact[] { assertSafePath(this.root, this.root); if (existsSync(this.manifestPath)) assertRegularFile(this.manifestPath, "Artifact manifest"); return this.rawList().map((artifact) => this.publicArtifact(artifact)); }
  private rawList(): Artifact[] { return existsSync(this.manifestPath) ? JSON.parse(readFileSync(this.manifestPath, "utf8")) as Artifact[] : []; }
  private absoluteStoredPath(storedPath: string): string { const absolute = isAbsolute(storedPath) ? resolve(storedPath) : resolve(this.root, storedPath); assertSafePath(this.root, absolute); return absolute; }
  private publicArtifact(artifact: Artifact): Artifact { return { ...artifact, storedPath: this.absoluteStoredPath(artifact.storedPath) }; }
  private save(records: Artifact[]): void {
    assertSafePath(this.root, this.manifestPath);
    const temporary = `${this.manifestPath}.tmp`; assertUnlinkedAbsolute(temporary, "Artifact manifest temporary");
    if (existsSync(temporary)) { const stat = lstatSync(temporary); if (stat.isSymbolicLink() || !stat.isFile()) throw new Error("Artifact manifest temporary is not a regular file"); rmSync(temporary); }
    const persisted = records.map((artifact) => ({ ...artifact, storedPath: relative(resolve(this.root), this.absoluteStoredPath(artifact.storedPath)) }));
    let created = false;
    try { writeFileSync(temporary, `${JSON.stringify(persisted)}\n`, { encoding: "utf8", flag: "wx" }); created = true; renameSync(temporary, this.manifestPath); }
    catch (error) { if (created) { const stat = lstatSync(temporary, { throwIfNoEntry: false }); if (stat?.isFile() && !stat.isSymbolicLink()) rmSync(temporary); } throw error; }
  }
}


export function appendCanonicalJsonl(path: string, record: unknown): void { mkdirSync(dirname(path), { recursive: true }); writeFileSync(path, `${JSON.stringify(record)}\n`, { encoding: "utf8", flag: "a" }); }
export function recoverJsonl(path: string): unknown[] { if (!existsSync(path)) return []; const lines = readFileSync(path, "utf8").split("\n"); const records: unknown[] = []; for (const line of lines) { if (!line) continue; try { records.push(JSON.parse(line)); } catch { break; } } const repaired = records.map((record) => JSON.stringify(record)).join("\n"); writeFileSync(path, repaired ? `${repaired}\n` : "", "utf8"); return records; }
function hashFile(path: string): string { return `sha256:${createHash("sha256").update(readFileSync(path)).digest("hex")}`; }
function assertRegularFile(path: string, label: string): void { const stat = lstatSync(path, { throwIfNoEntry: false }); if (!stat || stat.isSymbolicLink() || !stat.isFile()) throw new Error(`${label} is not a regular file`); }
function assertUnlinkedAbsolute(candidate: string, label: string): void {
  const target = resolve(candidate); const chain: string[] = []; let current = target;
  while (true) { chain.unshift(current); const parent = dirname(current); if (parent === current) break; current = parent; }
  for (const path of chain) { const stat = lstatSync(path, { throwIfNoEntry: false }); if (stat?.isSymbolicLink()) throw new Error(`${label} traverses a symlink or reparse point`); }
}
function contained(root: string, candidate: string): boolean { const value = relative(resolve(root), resolve(candidate)); return value === "" || !isAbsolute(value) && value !== ".." && !value.startsWith(`..${sep}`); }
function assertSafePath(root: string, candidate: string): void {
  const base = resolve(root); const target = resolve(candidate);
  if (!contained(base, target)) throw new Error("Artifact path escapes artifact root");
  assertUnlinkedAbsolute(target, "Artifact path");
}
