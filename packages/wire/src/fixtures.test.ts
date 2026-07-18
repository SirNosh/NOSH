import { readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { schemaFor, schemaUris } from "./registry.js";
import { sha256 } from "./json.js";

describe("golden schema registry", () => {
  for (const uri of schemaUris()) {
    const name = /\/schemas\/([^/]+)\/v1$/.exec(uri)![1]!; const fixture = (...parts: string[]) => join(import.meta.dirname, "..", "..", "..", "fixtures", "schemas", name, ...parts);
    it(`${name} keeps runtime, fixtures, hashes, and JSON Schema aligned`, () => {
      const schema = schemaFor(uri)!; const minimal = JSON.parse(readFileSync(fixture("minimal.valid.json"), "utf8")); const full = JSON.parse(readFileSync(fixture("full.valid.json"), "utf8"));
      expect(schema.safeParse(minimal).success).toBe(true); expect(schema.safeParse(full).success).toBe(true); expect(schema.safeParse(JSON.parse(readFileSync(fixture("missing.invalid.json"), "utf8"))).success).toBe(false); expect(schema.safeParse(JSON.parse(readFileSync(fixture("unknown.invalid.json"), "utf8"))).success).toBe(false); expect(readFileSync(fixture("full.sha256.txt"), "utf8").trim()).toBe(sha256(full));
      const document = JSON.parse(readFileSync(join(import.meta.dirname, "schemas", `${name}.v1.schema.json`), "utf8")); expect(document.$schema).toBe("https://json-schema.org/draft/2020-12/schema"); expect(document.$id).toBe(uri);
    });
  }
});
