import { describe, expect, it } from "vitest";
import { authProviders } from "./index.js";

describe("model-provider sign-in options", () => {
  it("offer ChatGPT/Claude subscriptions and API keys from Pi's providers", async () => {
    const providers = await authProviders();
    const byId = new Map(providers.map((provider) => [provider.id, provider]));
    expect(byId.get("openai-codex")?.subscription).toContain("ChatGPT");
    expect(byId.get("anthropic")?.subscription).toContain("Claude");
    expect(byId.get("openai")?.apiKey).toContain("API key");
    expect(providers.every((provider) => typeof provider.connected === "boolean")).toBe(true);
  }, 60_000);
});
