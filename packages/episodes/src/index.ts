import { createId } from "@nosh/core";
import { episodeSchema, schemaUri, sha256, type Episode, type EpisodeDraft } from "@nosh/wire";

export type EpisodeInput = Omit<Episode, "$schema" | "schemaVersion" | "episodeId" | "episodeHash"> & { episodeId?: string };

export function createEpisode(input: EpisodeInput): Episode {
  const unhashed = { $schema: schemaUri("episode"), schemaVersion: 1 as const, ...input, episodeId: input.episodeId ?? createId("epi") };
  return episodeSchema.parse({ ...unhashed, episodeHash: sha256(unhashed) });
}

export function fallbackEpisodeDraft(episodeType: string, summary: string): EpisodeDraft {
  return { $schema: schemaUri("episode-draft"), schemaVersion: 1, episodeType, summary: summary.trim() || "The execution step completed without a semantic summary.", facts: [], decisions: [], artifactIds: [], evidenceIds: [], changedFiles: [], unresolvedQuestions: [], recommendedNextActions: [] };
}

export function renderEpisode(episode: Episode): string {
  const facts = episode.facts.slice(0, 20).map((item) => `- ${clip(item.statement, 500)} [${item.confidence}]${item.evidenceRefs.length ? ` (${item.evidenceRefs.slice(0, 20).join(", ")})` : ""}`).join("\n");
  const decisions = episode.decisions.slice(0, 20).map((item) => `- ${clip(item.statement, 300)}: ${clip(item.rationale, 500)}`).join("\n");
  return [`Episode ${episode.episodeId} (${episode.episodeType}, ${episode.status})`, `Objective: ${clip(episode.objective, 1_000)}`, `Summary: ${clip(episode.summary, 1_000)}`, facts ? `Facts:\n${facts}` : "", decisions ? `Decisions:\n${decisions}` : "", episode.artifactIds.length ? `Artifacts: ${episode.artifactIds.slice(0, 20).join(", ")}` : "", episode.evidenceIds.length ? `Evidence: ${episode.evidenceIds.slice(0, 20).join(", ")}` : "", episode.unresolvedQuestions.length ? `Open questions: ${episode.unresolvedQuestions.slice(0, 10).map((item) => clip(item, 300)).join(" | ")}` : ""].filter(Boolean).join("\n");
}

export function episodeIntegrity(episode: Episode): boolean {
  const { episodeHash: _episodeHash, ...unhashed } = episode;
  return episode.episodeHash === sha256(unhashed);
}

function clip(value: string, maximum: number): string { return value.length > maximum ? `${value.slice(0, maximum - 1)}…` : value; }
