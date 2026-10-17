import { existsSync, mkdtempSync, readdirSync, rmSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { spawnSync } from "node:child_process";

const packageOnly = process.argv.includes("--package-only");
if (process.argv.slice(2).some(arg => arg !== "--package-only")) throw new Error("Usage: release-smoke.mjs [--package-only]");
if (!packageOnly && process.platform !== "win32") throw new Error("Installer smoke requires native Windows Node. Use --package-only for package/native renderer checks only.");
const required = ["package.json", "pnpm-lock.yaml", "apps/cli", "apps/noshd", "apps/tui", "packages", "scripts/install.ps1", "scripts/uninstall.ps1", "BUILD-METADATA.json", "apps/cli/dist/main.js", "apps/noshd/dist/main.js", "apps/tui/dist/index.js", "apps/tui/dist/main.js", "apps/tui/dist/smoke.js"];
const forbidden = ["apps/web", "apps/relay", "packages/crypto", "apps/noshd/dist/terminal.js", "apps/noshd/dist/remote-control.js", "apps/noshd/dist/relay-client.js", "timeline.md", "project.md"];
const isolated = mkdtempSync(join(tmpdir(), "nosh-release-smoke-"));
const target = join(isolated, "package");
const bin = join(isolated, "bin");
const env = { ...process.env, LOCALAPPDATA: isolated };
try {
  run(process.execPath, ["scripts/package-release.mjs", "--output", target]);
  for (const path of required) if (!existsSync(join(target, path))) throw new Error(`release missing required path: ${path}`);
  for (const path of forbidden) if (existsSync(join(target, path))) throw new Error(`release contains forbidden path: ${path}`);
  inspect(target);
  run(process.env.NOSH_BUN ?? "bun", ["apps/tui/dist/smoke.js"]);
  if (!packageOnly) {
    runPowerShell("scripts/install.ps1", ["-NoPath", "-NoScheduledTask", "-BinRoot", bin]);
    const wrapper = join(bin, "nosh.cmd");
    if (!existsSync(wrapper)) throw new Error("installer did not create isolated NOSH wrapper");
    const help = run("cmd.exe", ["/d", "/c", "call", wrapper, "help"], { env });
    if (!(help.stdout + help.stderr).includes("nosh setup")) throw new Error("installed wrapper did not print CLI help");
    runPowerShell("scripts/uninstall.ps1", ["-NoPath", "-NoScheduledTask", "-BinRoot", bin]);
    if (existsSync(wrapper)) throw new Error("uninstaller left isolated NOSH wrapper behind");
  }
  process.stdout.write(`release smoke: clean package and native renderer passed (${required.length} required paths, ${forbidden.length} forbidden paths absent); Windows installer ${packageOnly ? "NOT TESTED" : "passed"}\n`);
} finally {
  rmSync(isolated, { recursive: true, force: true });
}

function inspect(root) {
  for (const entry of readdirSync(root, { withFileTypes: true })) {
    const path = join(root, entry.name);
    if (entry.isSymbolicLink() || ["node_modules", "dist-types", "coverage", ".git", ".nosh"].includes(entry.name) || /^\.env(?:\..*)?$/.test(entry.name) || /\.(?:map|tsbuildinfo)$/.test(entry.name) || /\.(test|spec)(?:\.d)?\.[cm]?[jt]sx?$/.test(entry.name)) throw new Error(`release contains private/build/test residue: ${path}`);
    if (entry.isDirectory()) inspect(path);
  }
}
function run(command, args, options = {}) {
  const result = spawnSync(command, args, { encoding: "utf8", windowsHide: true, ...options });
  if (result.status !== 0) throw new Error(`${command} failed (${result.status ?? result.error?.message ?? "unknown"})\n${result.stdout ?? ""}${result.stderr ?? ""}`);
  process.stdout.write(result.stdout ?? "");
  return result;
}
function runPowerShell(script, args) {
  run("powershell.exe", ["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", join(target, script), ...args], { env });
}
