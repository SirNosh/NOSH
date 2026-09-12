import { spawnSync } from "node:child_process";
import { writeFileSync } from "node:fs";

const licenses = pnpm(["licenses", "list", "--prod", "--json"]);
const lines = ["# Third-party notices", "", "Generated from the production dependency graph in `pnpm-lock.yaml`. NOSH source is Apache-2.0; dependencies retain their own licenses.", ""];
for (const license of Object.keys(licenses).sort()) {
  lines.push(`## ${license}`, "");
  for (const item of licenses[license].sort((a, b) => a.name.localeCompare(b.name))) lines.push(`- ${item.name} ${item.versions.join(", ")}${item.homepage ? ` — ${item.homepage}` : ""}`);
  lines.push("");
}
lines.push("## OpenCode terminal UI adaptations", "", "Theme values, border characters, layout, and interaction adaptations are based on OpenCode (MIT, Copyright (c) 2025 opencode).", "See `apps/tui/UPSTREAM.md` for pinned source paths, modifications, and the full MIT permission notice.", "");
writeFileSync("THIRD_PARTY_NOTICES.md", `${lines.join("\n")}\n`, "utf8");

function pnpm(args) { const result = spawnSync("corepack", ["pnpm", ...args], { encoding: "utf8", windowsHide: true, shell: process.platform === "win32" }); if (result.status !== 0) throw new Error(result.stderr || result.error?.message || "pnpm license scan failed"); return JSON.parse(result.stdout); }
