import { describe, expect, it } from "vitest";
import { renderResponseHtml, renderResponseMarkdown, type ResponseCard } from "./index.js";

const card: ResponseCard = { status: "completed", scope: "Direction", outcome: "Baseline reproduced.", produced: ["art_1"], validation: ["Postflight passed"], issues: [], next: [] };
describe("deterministic response renderer", () => {
  it("uses the uniform section order without an LLM", () => { const markdown = renderResponseMarkdown(card); expect(markdown.indexOf("Outcome")).toBeLessThan(markdown.indexOf("Produced or changed")); expect(markdown).toContain("- No action required"); });
  it("renders equivalent desktop and mobile semantic content", () => { expect(renderResponseHtml(card, false).replace("nosh-response\"", "nosh-response nosh-response--compact\"")).toBe(renderResponseHtml(card, true)); });
});
