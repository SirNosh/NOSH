import { readFileSync } from "node:fs";
import { resolve } from "node:path";

const paths = process.argv.slice(2).length ? process.argv.slice(2) : ["pi-package/prompts/nosh-director.md", "pi-package/prompts/nosh-worker.md", "pi-package/prompts/nosh-reviewer.md", "pi-package/skills/nosh-control/SKILL.md"];
const files = paths.map((path) => { const text = readFileSync(resolve(path), "utf8"); return { path, characters: text.length, estimatedTokens: Math.ceil(text.length / 4) }; });
const total = files.reduce((sum, file) => sum + file.estimatedTokens, 0);
process.stdout.write(`${JSON.stringify({ note: "Token counts are conservative character-based estimates; provider tokenization varies.", files, estimatedTokens: total }, null, 2)}\n`);
