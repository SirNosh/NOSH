/** Budgeted tokens weigh cache reads at 10%, close to what providers bill for them. Without a breakdown, the provider total counts. */
export function budgetTokens(total: number, input: number, output: number, cacheRead: number, cacheWrite: number): number {
  return input + output + cacheRead + cacheWrite > 0 ? input + output + cacheWrite + Math.ceil(cacheRead / 10) : total;
}

/** Budget tokens of one usage event payload, recomputed from its raw counts so every budget uses one formula (including events recorded before weighting). */
export function eventBudgetTokens(payload: unknown): number {
  const value = (payload && typeof payload === "object" ? payload : {}) as Record<string, unknown>;
  const count = (key: string): number => { const raw = value[key]; return typeof raw === "number" && Number.isFinite(raw) ? Math.max(0, raw) : 0; };
  return budgetTokens(count("providerTotalTokens") || count("modelTokens"), count("inputTokens"), count("outputTokens"), count("cacheReadTokens"), count("cacheWriteTokens"));
}
