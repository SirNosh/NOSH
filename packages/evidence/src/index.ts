import { createHash } from "node:crypto";
import { copyFileSync, existsSync, mkdirSync, readFileSync, renameSync, statSync, writeFileSync } from "node:fs";
import { dirname, extname, join, relative, resolve } from "node:path";

export type Artifact = { artifactId: string; projectId: string; kind: string; mediaType: string; contentHash: string; sizeBytes: number; version: number; sourcePath: string; storedPath: string; retentionClass: "accepted_evidence" | "negative_evidence" | "checkpoint" | "cache" | "temporary"; createdAt: string };
export type Evidence = { evidenceId: string; statement: string; polarity: "supports" | "contradicts" | "qualifies"; sourceRefs: Array<{ refType: string; refId: string; locator: string }>; evaluationContractHash: string | null; limitations: string[]; reviewed: boolean };
export type Claim = { claimId: string; text: string; status: "hypothesis" | "under_test" | "supported" | "partially_supported" | "not_supported" | "contradicted" | "limited" | "withdrawn"; supportingEvidenceIds: string[]; contradictingEvidenceIds: string[]; qualifyingEvidenceIds: string[]; limitations: string[]; paperLocations: string[] };

export class ArtifactStore {
  private readonly manifestPath: string;
  constructor(private readonly root: string) { mkdirSync(root, { recursive: true }); this.manifestPath = join(root, "manifest.json"); }
  add(input: Omit<Artifact, "contentHash" | "sizeBytes" | "version" | "storedPath" | "createdAt">): Artifact {
    const source = resolve(input.sourcePath); const bytes = readFileSync(source); const contentHash = `sha256:${createHash("sha256").update(bytes).digest("hex")}`; const hash = contentHash.slice(7); const storedPath = join(this.root, hash.slice(0, 2), `${hash}${extname(source)}`); mkdirSync(dirname(storedPath), { recursive: true }); if (!existsSync(storedPath)) copyFileSync(source, storedPath);
    const records = this.list(); const prior = records.filter((artifact) => artifact.artifactId === input.artifactId).sort((a, b) => b.version - a.version)[0];
    if (prior?.contentHash === contentHash) return prior;
    const artifact: Artifact = { ...input, sourcePath: source, contentHash, sizeBytes: statSync(source).size, version: (prior?.version ?? 0) + 1, storedPath, createdAt: new Date().toISOString() };
    records.push(artifact); this.save(records); return artifact;
  }
  resolve(artifactId: string, version?: number): Artifact { const matches = this.list().filter((artifact) => artifact.artifactId === artifactId && (version === undefined || artifact.version === version)).sort((a, b) => b.version - a.version); const artifact = matches[0]; const storedPath = artifact ? resolve(artifact.storedPath) : ""; if (!artifact || relative(resolve(this.root), storedPath).startsWith("..") || !existsSync(storedPath) || hashFile(storedPath) !== artifact.contentHash) throw new Error(`Artifact ${artifactId} does not resolve to its stored hash`); return { ...artifact, storedPath }; }
  cleanup(retentionClasses: Artifact["retentionClass"][]): Artifact[] { return this.list().filter((artifact) => !retentionClasses.includes(artifact.retentionClass) || artifact.retentionClass === "accepted_evidence" || artifact.retentionClass === "negative_evidence"); }
  list(): Artifact[] { return existsSync(this.manifestPath) ? JSON.parse(readFileSync(this.manifestPath, "utf8")) as Artifact[] : []; }
  private save(records: Artifact[]): void { const temporary = `${this.manifestPath}.tmp`; writeFileSync(temporary, `${JSON.stringify(records)}\n`, "utf8"); renameSync(temporary, this.manifestPath); }
}

export class ClaimEvidenceGraph {
  private claims = new Map<string, Claim>(); private evidence = new Map<string, Evidence>(); private derived = new Map<string, Set<string>>();
  addEvidence(record: Evidence): void { if (!record.sourceRefs.length || record.sourceRefs.some((source) => !source.locator)) throw new Error("Evidence requires exact source references and locators"); if (this.evidence.has(record.evidenceId)) throw new Error("Evidence records are immutable"); this.evidence.set(record.evidenceId, structuredClone(record)); }
  addClaim(record: Claim): void { for (const id of [...record.supportingEvidenceIds, ...record.contradictingEvidenceIds, ...record.qualifyingEvidenceIds]) if (!this.evidence.has(id)) throw new Error(`Claim references unresolved evidence ${id}`); this.claims.set(record.claimId, structuredClone(record)); }
  derive(fromId: string, toId: string): void { if (!this.evidence.has(fromId) || !this.evidence.has(toId)) throw new Error("Derived evidence edge must resolve"); const edges = this.derived.get(fromId) ?? new Set<string>(); edges.add(toId); this.derived.set(fromId, edges); if (this.hasPath(toId, fromId)) { edges.delete(toId); throw new Error("Evidence derivation cannot contain a cycle"); } }
  audit(): Array<{ claimId: string; issue: string }> { const issues: Array<{ claimId: string; issue: string }> = []; for (const claim of this.claims.values()) { const evidenceCount = claim.supportingEvidenceIds.length + claim.contradictingEvidenceIds.length + claim.qualifyingEvidenceIds.length; if (!evidenceCount && !claim.limitations.length) issues.push({ claimId: claim.claimId, issue: "unsupported_and_unqualified" }); if (["supported", "partially_supported"].includes(claim.status) && !claim.supportingEvidenceIds.some((id) => this.evidence.get(id)?.reviewed)) issues.push({ claimId: claim.claimId, issue: "no_reviewed_support" }); if (!claim.paperLocations.length) issues.push({ claimId: claim.claimId, issue: "not_linked_to_paper" }); } return issues; }
  claim(claimId: string): Claim { const claim = this.claims.get(claimId); if (!claim) throw new Error("Unknown claim"); return structuredClone(claim); }
  private hasPath(from: string, target: string, visited = new Set<string>()): boolean { if (from === target) return true; if (visited.has(from)) return false; visited.add(from); return [...(this.derived.get(from) ?? [])].some((next) => this.hasPath(next, target, visited)); }
}

export function appendCanonicalJsonl(path: string, record: unknown): void { mkdirSync(dirname(path), { recursive: true }); writeFileSync(path, `${JSON.stringify(record)}\n`, { encoding: "utf8", flag: "a" }); }
export function recoverJsonl(path: string): unknown[] { if (!existsSync(path)) return []; const lines = readFileSync(path, "utf8").split("\n"); const records: unknown[] = []; for (const line of lines) { if (!line) continue; try { records.push(JSON.parse(line)); } catch { break; } } const repaired = records.map((record) => JSON.stringify(record)).join("\n"); writeFileSync(path, repaired ? `${repaired}\n` : "", "utf8"); return records; }
function hashFile(path: string): string { return `sha256:${createHash("sha256").update(readFileSync(path)).digest("hex")}`; }

export * from "./paper.js";
export * from "./project.js";
