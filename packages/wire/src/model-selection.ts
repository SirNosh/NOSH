import { z } from "zod";

export const thinkingLevelSchema = z.enum(["off", "minimal", "low", "medium", "high", "xhigh", "max"]);
export type ThinkingLevel = z.infer<typeof thinkingLevelSchema>;

export const modelSelectionSchema = z.object({
  provider: z.string().trim().min(1),
  id: z.string().trim().min(1),
  thinkingLevel: thinkingLevelSchema.optional(),
}).strict();
export type ModelSelection = z.infer<typeof modelSelectionSchema>;
