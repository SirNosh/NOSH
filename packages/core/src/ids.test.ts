import { describe, expect, it } from "vitest";
import { createId, isNoshId } from "./ids.js";

describe("NOSH IDs", () => {
  it("creates opaque, typed IDs", () => {
    const projectId = createId("prj");
    expect(isNoshId(projectId)).toBe(true);
    expect(isNoshId(projectId, "prj")).toBe(true);
    expect(isNoshId(projectId, "mis")).toBe(false);
  });
});
