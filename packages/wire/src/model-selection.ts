import { z } from "zod";

export const thinkingLevelSchema = z.enum(["off", "minimal", "low", "medium", "high", "xhigh", "max"]);
export type ThinkingLevel = z.infer<typeof thinkingLevelSchema>;

export const modelSelectionSchema = z.object({
  provider: z.string().trim().min(1),
  id: z.string().trim().min(1),
  thinkingLevel: thinkingLevelSchema.optional(),
}).strict();
export type ModelSelection = z.infer<typeof modelSelectionSchema>;

/** Parses "provider/id" or "provider/id:thinkingLevel" (e.g. openai-codex/gpt-6-luna:low). */
export function parseModelSelection(text: string): ModelSelection {
  const match = /^([^/\s]+)\/([^:\s]+)(?::([a-z]+))?$/.exec(text.trim());
  if (!match) throw new Error(`Model must be provider/id or provider/id:thinkingLevel, got "${text}"`);
  return modelSelectionSchema.parse({ provider: match[1], id: match[2], ...(match[3] ? { thinkingLevel: match[3] } : {}) });
}
