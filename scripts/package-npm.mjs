import { cpSync, existsSync, mkdirSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { dirname, join, relative, sep } from "node:path";
import { spawnSync } from "node:child_process";

// Assemble an installable npm package: `npm install -g <tarball>` gives `nosh` (CLI, daemon, TUI) with no
// pnpm, build step, or separate Bun install. Internal @nosh/* packages ship under lib/ with their imports
// rewritten to relative paths (npm's bundleDependencies breaks Pi's shrinkwrapped dependency tree);
// third-party packages, including the `bun` runtime for the TUI, install from npm.
const args = process.argv.slice(2);
const option = (name, fallback) => { const index = args.indexOf(`--${name}`); return index === -1 ? fallback : args[index + 1]; };
const root = JSON.parse(readFileSync("package.json", "utf8"));
const name = option("name", "nosh-runtime");
const output = option("output", join("release", "npm"));
const build = spawnSync(process.execPath, [join("node_modules", "typescript", "bin", "tsc"), "--build"], { stdio: "inherit", windowsHide: true });
if (build.status !== 0) throw new Error(`Build failed: ${build.error?.message ?? build.status}`);

const staging = join(output, "package");
rmSync(staging, { recursive: true, force: true });
mkdirSync(staging, { recursive: true });
const manifest = (path) => JSON.parse(readFileSync(join(path, "package.json"), "utf8"));
const runtimeFile = (path) => path.endsWith(".js") && !path.endsWith(".test.js") && !path.endsWith("smoke.js");
const copyDist = (from, to) => { for (const file of walk(join(from, "dist")).filter(runtimeFile)) cpSync(file, join(to, "dist", relative(join(from, "dist"), file))); };
const dependencies = { bun: "^1.3.0" }; const internal = new Map();
const addDependencies = (path, packageJson) => {
  for (const [dependency, version] of Object.entries(packageJson.dependencies ?? {})) {
    if (String(version).startsWith("workspace:")) continue;
    if (dependencies[dependency] && dependencies[dependency] !== version) throw new Error(`${path} needs ${dependency}@${version}, another package needs ${dependencies[dependency]}`);
    dependencies[dependency] = version;
  }
};

// Applications keep the repository layout, so the CLI finds noshd and the TUI, and noshd finds pi-package, by relative path.
for (const app of ["apps/cli", "apps/noshd", "apps/tui"]) {
  const packageJson = manifest(app); addDependencies(app, packageJson);
  copyDist(app, join(staging, app));
  writeFileSync(join(staging, app, "package.json"), `${JSON.stringify({ name: packageJson.name, version: packageJson.version, private: true, type: "module" }, null, 2)}\n`);
}
for (const directory of readdirSync("packages")) {
  const path = join("packages", directory); const packageJson = manifest(path); addDependencies(path, packageJson);
  const target = join(staging, "lib", directory); internal.set(packageJson.name, target);
  copyDist(path, target);
  if (existsSync(join(path, "src", "schemas"))) cpSync(join(path, "src", "schemas"), join(target, "src", "schemas"), { recursive: true });
  const { dependencies: _dependencies, devDependencies: _devDependencies, scripts: _scripts, ...rest } = packageJson;
  writeFileSync(join(target, "package.json"), `${JSON.stringify(rest, null, 2)}\n`);
}
// Every internal package exports only "." -> ./dist/index.js, so a bare "@nosh/x" import becomes a relative path to it.
for (const file of walk(staging).filter((path) => path.endsWith(".js"))) {
  const source = readFileSync(file, "utf8");
  const rewritten = source.replace(/(from\s*|import\(\s*)(["'])(@nosh\/[a-z-]+)\2/g, (match, prefix, quote, specifier) => {
    const target = internal.get(specifier); if (!target) throw new Error(`${file} imports unknown ${specifier}`);
    const path = relative(dirname(file), join(target, "dist", "index.js")).split(sep).join("/");
    return `${prefix}${quote}${path.startsWith(".") ? path : `./${path}`}${quote}`;
  });
  if (rewritten !== source) writeFileSync(file, rewritten);
}
const leftover = walk(staging).filter((path) => path.endsWith(".js") && /["']@nosh\//.test(readFileSync(path, "utf8")));
if (leftover.length) throw new Error(`Unrewritten @nosh imports in ${leftover.join(", ")}`);
cpSync("pi-package", join(staging, "pi-package"), { recursive: true });
for (const file of ["README.md", "LICENSE", "THIRD_PARTY_NOTICES.md"]) cpSync(file, join(staging, file));
const pi = manifest("pi-package");
writeFileSync(join(staging, "package.json"), `${JSON.stringify({
  name, version: root.version, description: `${root.description}: a local research harness and terminal UI`, license: root.license, type: "module",
  repository: pi.repository && { type: pi.repository.type, url: pi.repository.url }, homepage: pi.homepage?.replace("#pi-package", ""),
  bin: { nosh: "apps/cli/dist/main.js" }, engines: root.engines,
  dependencies: Object.fromEntries(Object.entries(dependencies).sort(([left], [right]) => left.localeCompare(right))),
}, null, 2)}\n`);

const npm = process.platform === "win32" ? "npm.cmd" : "npm";
const pack = spawnSync(npm, ["pack", "--pack-destination", relative(staging, output) || "."], { cwd: staging, encoding: "utf8", windowsHide: true, shell: process.platform === "win32" });
if (pack.status !== 0) throw new Error(`npm pack failed: ${pack.stderr || pack.error?.message}`);
process.stdout.write(`Packed ${join(output, pack.stdout.trim().split(/\r?\n/).at(-1))}\nInstall: npm install -g ${join(output, pack.stdout.trim().split(/\r?\n/).at(-1))}\n`);

function walk(directory) { return readdirSync(directory).flatMap((entry) => { const path = join(directory, entry); return statSync(path).isDirectory() ? walk(path) : [path]; }); }
