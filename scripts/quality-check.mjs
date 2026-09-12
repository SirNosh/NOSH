import { existsSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { spawnSync } from "node:child_process";

const tsc = join("node_modules", "typescript", "bin", "tsc");
const projects = [...["apps", "packages"].flatMap((root) => findConfigs(root))];
for (const config of projects) run(process.execPath, [tsc, "-p", config, "--noEmit", "--pretty", "false", "--noUnusedLocals", "--noUnusedParameters"]);
run(process.execPath, [join("node_modules", "eslint", "bin", "eslint.js"), "apps/cli/src", "apps/noshd/src", "apps/tui/src", "packages", "scripts"]);
run(process.execPath, [join("node_modules", "knip", "bin", "knip.js"), "--no-progress", "--include", "exports"]);
process.stdout.write(`quality: ESLint, TypeScript unused-symbol checks (${projects.length} projects), and Knip dead-export analysis passed.\n`);

function findConfigs(root) {
  return readdirSync(root, { withFileTypes: true }).filter((entry) => entry.isDirectory()).map((entry) => join(root, entry.name, "tsconfig.json")).filter((path) => existsSync(path));
}
function run(command, args) {
  const result = spawnSync(command, args, { encoding: "utf8", windowsHide: true });
  if (result.status !== 0) { process.stderr.write(`${result.stdout ?? ""}${result.stderr ?? ""}`); process.exit(result.status ?? 1); }
}
